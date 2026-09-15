//! H.264 encoding via **Windows Media Foundation**.
//!
//! MF automatically binds a hardware encoder MFT (NVENC / AMD AMF / Intel QSV)
//! when one is present and falls back to the OS software H.264 encoder
//! otherwise — one code path, hardware-accelerated where possible. That keeps
//! host CPU low, which is the whole point.
//!
//! Milestone 2 wraps `IMFSinkWriter` and produces a playable `.mp4`, so the
//! capture → encode pipeline can be watched and measured end-to-end without any
//! networking. Milestone 3 swaps the sink writer for a directly-driven encoder
//! MFT that emits raw NAL units for the low-latency WebRTC path.

pub mod color;
pub mod stream;

pub use stream::{EncodedPacket, StreamEncoder};

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{anyhow, Context, Result};
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSinkWriter, IMFTransform, MFCreateAttributes, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Video, MFShutdown,
    MFStartup, MFT_ENUM_HARDWARE_URL_Attribute, MFVideoFormat_H264, MFVideoFormat_RGB32,
    MFVideoInterlace_Progressive, MFSTARTUP_FULL, MF_LOW_LATENCY, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_DISABLE_THROTTLING, MF_VERSION,
};

/// One-per-process Media Foundation runtime. Hold it for as long as any MF
/// object is alive; drop it (last) on shutdown.
pub struct MediaFoundation {
    _private: (),
}

static MF_REFCOUNT: AtomicUsize = AtomicUsize::new(0);

impl MediaFoundation {
    pub fn init() -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")? };
        MF_REFCOUNT.fetch_add(1, Ordering::SeqCst);
        Ok(Self { _private: () })
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        if MF_REFCOUNT.fetch_sub(1, Ordering::SeqCst) == 1 {
            unsafe {
                let _ = MFShutdown();
            }
        }
    }
}

/// Encoder settings. Resolution is taken from the first frame.
#[derive(Debug, Clone, Copy)]
pub struct EncodeConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

impl EncodeConfig {
    /// A reasonable "Balanced" default for the given resolution.
    pub fn balanced(width: u32, height: u32, fps: u32) -> Self {
        // ~0.1 bits per pixel per frame is a decent H.264 target for desktop.
        // The 8 Mbit ceiling matters more than the formula: past that you're
        // mostly buying detail nobody watching a remote desktop notices,
        // while walking straight into the upload limit of an ordinary home
        // connection — and a saturated uplink costs latency on *everything*,
        // input included. High frame rates stay affordable because screen
        // content compresses so well frame to frame.
        let bitrate =
            ((width as u64 * height as u64 * fps as u64) / 10).clamp(1_000_000, 8_000_000);
        Self {
            width,
            height,
            fps,
            bitrate_bps: bitrate as u32,
        }
    }
}

/// Stats gathered while recording.
#[derive(Debug, Default, Clone, Copy)]
pub struct EncodeStats {
    pub frames: u64,
    pub bytes_out: u64,
    pub hardware: bool,
}

/// Writes captured frames to an `.mp4` using MF's H.264 encoder.
pub struct Mp4Recorder {
    writer: IMFSinkWriter,
    stream: u32,
    cfg: EncodeConfig,
    frame_bytes: usize,
    hardware: bool,
    frames: u64,
}

impl Mp4Recorder {
    /// Create a recorder writing to `path` (must end in `.mp4`).
    pub fn create(path: &str, cfg: EncodeConfig) -> Result<Self> {
        let frame_bytes = cfg.width as usize * cfg.height as usize * 4;

        let writer = unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 3).context("MFCreateAttributes")?;
            let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes gave nothing"))?;
            // Let MF pick a hardware encoder; don't rate-limit WriteSample;
            // ask the encoder for low-latency behaviour.
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;

            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, &attrs)
                .context("MFCreateSinkWriterFromURL")?
        };

        let out_type = make_media_type(&[
            (MF_MT_MAJOR_TYPE, Attr::Guid(MFMediaType_Video)),
            (MF_MT_SUBTYPE, Attr::Guid(MFVideoFormat_H264)),
            (MF_MT_AVG_BITRATE, Attr::U32(cfg.bitrate_bps)),
            (
                MF_MT_INTERLACE_MODE,
                Attr::U32(MFVideoInterlace_Progressive.0 as u32),
            ),
        ])?;
        set_size(&out_type, MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
        set_size(&out_type, MF_MT_FRAME_RATE, cfg.fps, 1)?;
        set_size(&out_type, MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;

        let in_type = make_media_type(&[
            (MF_MT_MAJOR_TYPE, Attr::Guid(MFMediaType_Video)),
            (MF_MT_SUBTYPE, Attr::Guid(MFVideoFormat_RGB32)),
            (
                MF_MT_INTERLACE_MODE,
                Attr::U32(MFVideoInterlace_Progressive.0 as u32),
            ),
            // Positive stride => top-down, matching our capture buffer.
            (MF_MT_DEFAULT_STRIDE, Attr::U32(cfg.width * 4)),
        ])?;
        set_size(&in_type, MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
        set_size(&in_type, MF_MT_FRAME_RATE, cfg.fps, 1)?;
        set_size(&in_type, MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;

        let stream = unsafe { writer.AddStream(&out_type).context("AddStream")? };
        unsafe {
            writer
                .SetInputMediaType(stream, &in_type, None)
                .context("SetInputMediaType")?;
            writer.BeginWriting().context("BeginWriting")?;
        }

        let hardware = detect_hardware(&writer, stream);
        tracing::info!(
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            bitrate_kbps = cfg.bitrate_bps / 1000,
            hardware,
            "mp4 recorder ready"
        );

        Ok(Self {
            writer,
            stream,
            cfg,
            frame_bytes,
            hardware,
            frames: 0,
        })
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// Encode one top-down BGRA frame. `pts` and `dur` are in 100-ns units.
    pub fn push_bgra(&mut self, bgra: &[u8], pts_100ns: i64, dur_100ns: i64) -> Result<()> {
        if bgra.len() < self.frame_bytes {
            return Err(anyhow!(
                "frame is {} bytes, expected {}",
                bgra.len(),
                self.frame_bytes
            ));
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(self.frame_bytes as u32)?;
            let mut dst: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), dst, self.frame_bytes);
            buffer.Unlock()?;
            buffer.SetCurrentLength(self.frame_bytes as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts_100ns)?;
            sample.SetSampleDuration(dur_100ns)?;

            self.writer
                .WriteSample(self.stream, &sample)
                .context("WriteSample")?;
        }
        self.frames += 1;
        Ok(())
    }

    /// Flush and finalise the file.
    pub fn finish(self) -> Result<EncodeStats> {
        unsafe { self.writer.Finalize().context("Finalize")? };
        Ok(EncodeStats {
            frames: self.frames,
            bytes_out: 0, // filled in by the caller from the file size
            hardware: self.hardware,
        })
    }

    pub fn config(&self) -> EncodeConfig {
        self.cfg
    }
}

enum Attr {
    Guid(GUID),
    U32(u32),
}

fn make_media_type(attrs: &[(GUID, Attr)]) -> Result<IMFMediaType> {
    unsafe {
        let mt = MFCreateMediaType().context("MFCreateMediaType")?;
        for (key, val) in attrs {
            match val {
                Attr::Guid(g) => mt.SetGUID(key, g)?,
                Attr::U32(v) => mt.SetUINT32(key, *v)?,
            }
        }
        Ok(mt)
    }
}

/// `MF_MT_FRAME_SIZE`, `MF_MT_FRAME_RATE` and `MF_MT_PIXEL_ASPECT_RATIO` are
/// `UINT64` attributes packing two `u32`s (high word first). MF's `MFSet*`
/// helpers for this are header inlines, not exported APIs, so pack it here.
fn set_size(mt: &IMFMediaType, key: GUID, high: u32, low: u32) -> Result<()> {
    let packed = ((high as u64) << 32) | low as u64;
    unsafe { mt.SetUINT64(&key, packed).context("SetUINT64") }
}

/// Ask the sink writer which encoder MFT it bound and whether it is hardware.
fn detect_hardware(writer: &IMFSinkWriter, stream: u32) -> bool {
    unsafe {
        let mut obj: *mut c_void = std::ptr::null_mut();
        if writer
            .GetServiceForStream(stream, &GUID::zeroed(), &IMFTransform::IID, &mut obj)
            .is_err()
            || obj.is_null()
        {
            return false;
        }
        let transform = IMFTransform::from_raw(obj);
        match transform.GetAttributes() {
            Ok(a) => {
                // Present (even empty) => the MFT declared a hardware URL.
                let mut buf = [0u16; 512];
                a.GetString(&MFT_ENUM_HARDWARE_URL_Attribute, &mut buf, None)
                    .is_ok()
            }
            Err(_) => false,
        }
    }
}
