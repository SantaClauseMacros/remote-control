//! H.264 → BGRA decoding via the Media Foundation video decoder MFT.
//!
//! Software decode is used deliberately: it needs no D3D device, works over an
//! RDP/headless client session, and 1080p H.264 decode is cheap on any modern
//! CPU. The client is not the resource-constrained end of the link.
//!
//! Feed whole coded frames (the [`rc_encode::EncodedPacket::data`] blobs, which
//! already carry SPS/PPS before each IDR) to [`StreamDecoder::decode`].

use std::mem::ManuallyDrop;

use anyhow::{anyhow, Context, Result};
use rc_encode::color::nv12_to_bgra;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFTEnumEx, MFVideoArea, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE,
};

/// A decoded frame, top-down BGRA.
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub struct StreamDecoder {
    transform: IMFTransform,
    /// Display size (crop applied — what the host actually captured).
    width: u32,
    height: u32,
    /// Coded size — H.264 rounds up to a 16-pixel macroblock grid.
    coded_height: usize,
    stride: usize,
    bgra: Vec<u8>,
    negotiated: bool,
    /// Keeps Media Foundation initialised for this decoder's thread.
    _mf: rc_encode::MediaFoundation,
}

impl StreamDecoder {
    pub fn new() -> Result<Self> {
        let mf = rc_encode::MediaFoundation::init().context("MFStartup on decoder thread")?;
        let transform = find_decoder()?;

        unsafe {
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform
                .SetInputType(0, &in_type, 0)
                .context("decoder SetInputType(H264)")?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        Ok(Self {
            transform,
            width: 0,
            height: 0,
            coded_height: 0,
            stride: 0,
            bgra: Vec::new(),
            negotiated: false,
            _mf: mf,
        })
    }

    /// Decode one coded frame. Returns `Ok(None)` while the decoder is still
    /// buffering (typically only for the very first frame).
    pub fn decode(&mut self, annexb: &[u8]) -> Result<Option<DecodedFrame>> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(annexb.len() as u32)?;
            let mut dst: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(annexb.as_ptr(), dst, annexb.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(annexb.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .context("decoder ProcessInput")?;
        }

        // The MS decoder only exposes real output types once it has parsed an
        // SPS, so negotiate after feeding input, not before.
        if !self.negotiated && !self.negotiate_output()? {
            return Ok(None); // decoder still needs more data
        }

        loop {
            match self.pull()? {
                Pull::Frame(f) => return Ok(Some(f)),
                Pull::NeedInput => return Ok(None),
                Pull::FormatChanged => {
                    self.negotiated = false;
                    if !self.negotiate_output()? {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// Try to select an NV12 output type. Returns `false` if the decoder has no
    /// output types yet (needs more input).
    fn negotiate_output(&mut self) -> Result<bool> {
        unsafe {
            for i in 0.. {
                let t = match self.transform.GetOutputAvailableType(0, i) {
                    Ok(t) => t,
                    Err(_) => break,
                };
                let sub = t.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
                if sub == MFVideoFormat_NV12 {
                    let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                    let coded_w = (size >> 32) as u32;
                    let coded_h = (size & 0xFFFF_FFFF) as u32;

                    // Prefer the display aperture (SPS crop) over the coded size.
                    let mut area = MFVideoArea::default();
                    let mut got = 0u32;
                    let buf = std::slice::from_raw_parts_mut(
                        &mut area as *mut _ as *mut u8,
                        std::mem::size_of::<MFVideoArea>(),
                    );
                    let (disp_w, disp_h) =
                        match t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, buf, Some(&mut got)) {
                            Ok(()) if got as usize >= std::mem::size_of::<MFVideoArea>() => {
                                (area.Area.cx.max(1) as u32, area.Area.cy.max(1) as u32)
                            }
                            _ => (coded_w, coded_h),
                        };

                    self.width = disp_w;
                    self.height = disp_h;
                    self.coded_height = coded_h as usize;
                    self.stride = t
                        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                        .map(|s| s as usize)
                        .unwrap_or(coded_w as usize);
                    self.transform
                        .SetOutputType(0, &t, 0)
                        .context("decoder SetOutputType(NV12)")?;
                    self.bgra = vec![0u8; self.width as usize * self.height as usize * 4];
                    self.negotiated = true;
                    tracing::info!(
                        w = self.width,
                        h = self.height,
                        stride = self.stride,
                        "decoder output negotiated"
                    );
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn pull(&mut self) -> Result<Pull> {
        // The MS H.264 decoder does not allocate output samples for us.
        let out_sample = unsafe {
            let si = self.transform.GetOutputStreamInfo(0)?;
            let size = si.cbSize.max(self.width * self.height * 2).max(1 << 20);
            let buf = MFCreateMemoryBuffer(size)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            Some(s)
        };

        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(out_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;

        let r = unsafe { self.transform.ProcessOutput(0, &mut out, &mut status) };
        let sample = ManuallyDrop::into_inner(std::mem::replace(
            &mut out[0].pSample,
            ManuallyDrop::new(None),
        ));
        unsafe { ManuallyDrop::drop(&mut out[0].pEvents) };

        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(Pull::NeedInput),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => return Ok(Pull::FormatChanged),
            Err(e) => return Err(e).context("decoder ProcessOutput"),
        }

        let Some(sample) = sample else {
            return Ok(Pull::NeedInput);
        };
        let frame = unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut cur = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut cur))?;

            let (w, h, stride) = (self.width as usize, self.height as usize, self.stride);
            let data = std::slice::from_raw_parts(ptr, cur as usize);
            // Y is `stride * coded_height`; UV follows. We only read the top
            // `h` / `h/2` rows, which crops the macroblock padding.
            let y_size = stride * self.coded_height;
            if data.len() >= y_size + y_size / 2 {
                let (y, uv) = data.split_at(y_size);
                nv12_to_bgra(y, uv, w, h, stride, stride, &mut self.bgra);
            }
            buffer.Unlock()?;

            DecodedFrame {
                width: self.width,
                height: self.height,
                bgra: self.bgra.clone(),
            }
        };
        Ok(Pull::Frame(frame))
    }
}

enum Pull {
    Frame(DecodedFrame),
    NeedInput,
    FormatChanged,
}

fn find_decoder() -> Result<IMFTransform> {
    let in_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    unsafe {
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&in_info),
            Some(&out_info),
            &mut arr,
            &mut count,
        )
        .context("MFTEnumEx(decoder)")?;

        let slice = std::slice::from_raw_parts(arr, count as usize);
        let mut chosen = None;
        for activate in slice.iter().flatten() {
            if chosen.is_none() {
                if let Ok(t) = activate.ActivateObject::<IMFTransform>() {
                    chosen = Some(t);
                }
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(arr as *const _));
        chosen.ok_or_else(|| anyhow!("no H.264 decoder MFT available"))
    }
}

/// Kept for symmetry with the encoder crate's public surface.
pub fn h264_nal_type(annexb: &[u8]) -> Option<u8> {
    let mut i = 0;
    while i + 4 < annexb.len() {
        if annexb[i..i + 3] == [0, 0, 1] {
            return Some(annexb[i + 3] & 0x1F);
        }
        if annexb[i..i + 4] == [0, 0, 0, 1] {
            return Some(annexb[i + 4] & 0x1F);
        }
        i += 1;
    }
    None
}
