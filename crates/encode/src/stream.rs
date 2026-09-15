//! Low-latency streaming H.264 encoder built directly on an encoder MFT.
//!
//! Unlike [`crate::Mp4Recorder`] (which wraps `IMFSinkWriter` and writes a
//! container), this emits a raw **Annex-B elementary stream** — the bytes that
//! go straight onto the transport's video channel. It prefers a hardware
//! encoder MFT (NVENC / QSV / AMF, driven with the async event model) and falls
//! back to the OS software encoder (driven synchronously).
//!
//! The MFT runs on its own thread. Callers push BGRA frames with
//! [`StreamEncoder::submit_bgra`] and receive [`EncodedPacket`]s on the
//! `tokio` channel handed to [`StreamEncoder::new`].

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::mpsc as tok_mpsc;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_WRITE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSampleExtension_CleanPoint,
    MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_LOW_LATENCY, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MAX_KEYFRAME_SPACING, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK,
};

use crate::color::{bgra_to_nv12, nv12_len};
use crate::EncodeConfig;

/// H.264 Main profile.
const H264_PROFILE_MAIN: u32 = 77;

/// One coded frame: a run of Annex-B NAL units (with start codes). An IDR frame
/// is prefixed with SPS/PPS by the encoder.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub key_frame: bool,
    /// Presentation timestamp, microseconds (echoes the value passed to
    /// [`StreamEncoder::submit_bgra`]).
    pub timestamp_us: u64,
}

enum Job {
    Frame { bgra: Vec<u8>, ts_us: u64 },
    Stop,
}

/// Handle to the encoder thread.
pub struct StreamEncoder {
    job_tx: std_mpsc::Sender<Job>,
    worker: Option<JoinHandle<()>>,
    hardware: bool,
}

impl StreamEncoder {
    /// Spawn the encoder. Encoded packets arrive on `packet_tx` in capture
    /// order. Returns once the MFT is configured and streaming.
    pub fn new(
        cfg: EncodeConfig,
        packet_tx: tok_mpsc::UnboundedSender<EncodedPacket>,
    ) -> Result<Self> {
        let (job_tx, job_rx) = std_mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<bool>>();

        let worker = std::thread::Builder::new()
            .name("rc-encoder".into())
            .spawn(move || {
                // Each MF worker thread wants its own COM apartment.
                unsafe {
                    let _ = windows::Win32::System::Com::CoInitializeEx(
                        None,
                        windows::Win32::System::Com::COINIT_MULTITHREADED,
                    );
                }
                match EncoderState::create(&cfg) {
                    Ok(mut st) => {
                        let hw = st.async_mode;
                        let _ = ready_tx.send(Ok(hw));
                        if let Err(e) = st.run(job_rx, packet_tx) {
                            tracing::error!(error = %e, "stream encoder loop failed");
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .context("spawning encoder thread")?;

        let hardware = ready_rx
            .recv()
            .context("encoder thread died during init")?
            .context("encoder init failed")?;

        tracing::info!(
            hardware,
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            bitrate_kbps = cfg.bitrate_bps / 1000,
            "stream encoder ready"
        );
        Ok(Self {
            job_tx,
            worker: Some(worker),
            hardware,
        })
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// Queue one top-down BGRA frame for encoding. Non-blocking.
    pub fn submit_bgra(&self, bgra: Vec<u8>, timestamp_us: u64) -> Result<()> {
        self.job_tx
            .send(Job::Frame {
                bgra,
                ts_us: timestamp_us,
            })
            .map_err(|_| anyhow!("encoder thread is gone"))
    }
}

impl Drop for StreamEncoder {
    fn drop(&mut self) {
        let _ = self.job_tx.send(Job::Stop);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

struct EncoderState {
    /// Kept so the MFT can be shut down properly (see `Drop`).
    activate: IMFActivate,
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    async_mode: bool,
    provides_samples: bool,
    /// Set when the encoder takes GPU textures (hardware MFTs).
    gpu: Option<GpuInput>,
    cfg: EncodeConfig,
    nv12: Vec<u8>,
    /// Keeps Media Foundation initialised for this worker thread's lifetime.
    _mf: crate::MediaFoundation,
}

impl EncoderState {
    fn create(cfg: &EncodeConfig) -> Result<Self> {
        let mf = crate::MediaFoundation::init().context("MFStartup on encoder thread")?;
        let candidates = find_encoders()?;
        let n = candidates.len();

        // Prefer hardware, but a hardware MFT can enumerate successfully and
        // still fail to *configure* — e.g. its GPU's video-encode block is
        // already saturated by another app (a game's own replay capture,
        // Xbox Game Bar, GeForce Experience/ShadowPlay, OBS, …). Rather than
        // aborting the whole session, fall through to the next candidate
        // (ultimately the software encoder) so a session can still start.
        let mut last_err = None;
        let mut candidates = candidates.into_iter().enumerate();
        while let Some((i, (activate, transform, async_mode))) = candidates.next() {
            match Self::configure(&transform, async_mode, cfg) {
                Ok((provides_samples, events, gpu)) => {
                    // Release the candidates we won't use.
                    for (_, (unused, _, _)) in candidates.by_ref() {
                        unsafe {
                            let _ = unused.ShutdownObject();
                        }
                    }
                    tracing::debug!(gpu_input = gpu.is_some(), "encoder configured");
                    return Ok(Self {
                        activate,
                        transform,
                        events,
                        async_mode,
                        provides_samples,
                        gpu,
                        cfg: *cfg,
                        nv12: vec![0u8; nv12_len(cfg.width as usize, cfg.height as usize)],
                        _mf: mf,
                    });
                }
                Err(e) => {
                    // Shut the MFT down properly. A hardware encoder holds a
                    // GPU encode session from activation until ShutdownObject;
                    // merely dropping it leaked one per attempt, until the
                    // driver's session limit was used up for every app.
                    unsafe {
                        let _ = activate.ShutdownObject();
                    }
                    tracing::warn!(
                        error = ?e,
                        hardware = async_mode,
                        "encoder MFT failed to configure{}",
                        if i + 1 < n { ", trying next candidate" } else { "" }
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("no H.264 encoder MFT available")))
    }

    /// Set input/output media types on `transform` and start streaming.
    /// Returns whether the MFT provides its own output samples, plus its
    /// event generator when `async_mode` (hardware MFTs only).
    fn configure(
        transform: &IMFTransform,
        async_mode: bool,
        cfg: &EncodeConfig,
    ) -> Result<(bool, Option<IMFMediaEventGenerator>, Option<GpuInput>)> {
        // Unlock async MFTs and ask for low latency.
        unsafe {
            let attrs = transform.GetAttributes().context("GetAttributes")?;
            if async_mode {
                attrs
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .context("unlock async MFT")?;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        // Hardware encoders — NVIDIA's among them — only take frames that are
        // already on the GPU, and refuse even the output type until they've
        // been given a D3D11 device ("The input type is not supported for D3D
        // device"). The software encoder needs none of this.
        let gpu = if async_mode {
            match GpuInput::new(transform, cfg.width, cfg.height) {
                Ok(g) => Some(g),
                Err(e) => {
                    tracing::debug!(error = ?e, "no D3D11 input path for this encoder");
                    None
                }
            }
        } else {
            None
        };

        // Output type (H.264) must be set before the input type.
        let out_type = media_type(&[
            (MF_MT_MAJOR_TYPE, V::Guid(MFMediaType_Video)),
            (MF_MT_SUBTYPE, V::Guid(MFVideoFormat_H264)),
            (MF_MT_AVG_BITRATE, V::U32(cfg.bitrate_bps)),
            (
                MF_MT_INTERLACE_MODE,
                V::U32(MFVideoInterlace_Progressive.0 as u32),
            ),
            (MF_MT_MPEG2_PROFILE, V::U32(H264_PROFILE_MAIN)),
            // Keyframe at least every 2 seconds.
            (MF_MT_MAX_KEYFRAME_SPACING, V::U32(cfg.fps.max(1) * 2)),
        ])?;
        set_ratio(&out_type, MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
        set_ratio(&out_type, MF_MT_FRAME_RATE, cfg.fps, 1)?;
        set_ratio(&out_type, MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        unsafe {
            transform
                .SetOutputType(0, &out_type, 0)
                .context("SetOutputType(H264)")?
        };

        let in_type = media_type(&[
            (MF_MT_MAJOR_TYPE, V::Guid(MFMediaType_Video)),
            (MF_MT_SUBTYPE, V::Guid(MFVideoFormat_NV12)),
            (
                MF_MT_INTERLACE_MODE,
                V::U32(MFVideoInterlace_Progressive.0 as u32),
            ),
        ])?;
        set_ratio(&in_type, MF_MT_FRAME_SIZE, cfg.width, cfg.height)?;
        set_ratio(&in_type, MF_MT_FRAME_RATE, cfg.fps, 1)?;
        unsafe {
            transform
                .SetInputType(0, &in_type, 0)
                .context("SetInputType(NV12)")?
        };

        let si = unsafe {
            transform
                .GetOutputStreamInfo(0)
                .context("GetOutputStreamInfo")?
        };
        let provides_samples = si.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

        let events = if async_mode {
            Some(
                transform
                    .cast::<IMFMediaEventGenerator>()
                    .context("event generator")?,
            )
        } else {
            None
        };

        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        Ok((provides_samples, events, gpu))
    }

    fn run(
        &mut self,
        job_rx: std_mpsc::Receiver<Job>,
        packet_tx: tok_mpsc::UnboundedSender<EncodedPacket>,
    ) -> Result<()> {
        if self.async_mode {
            self.run_async(job_rx, packet_tx)
        } else {
            self.run_sync(job_rx, packet_tx)
        }
    }

    fn run_async(
        &mut self,
        job_rx: std_mpsc::Receiver<Job>,
        packet_tx: tok_mpsc::UnboundedSender<EncodedPacket>,
    ) -> Result<()> {
        let events = self.events.clone().unwrap();
        let mut pending: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        let mut need_input: i32 = 0;
        let mut stopping = false;
        let mut draining = false;

        loop {
            loop {
                match job_rx.try_recv() {
                    Ok(Job::Frame { bgra, ts_us }) => pending.push_back((bgra, ts_us)),
                    Ok(Job::Stop) => stopping = true,
                    Err(std_mpsc::TryRecvError::Empty) => break,
                    Err(std_mpsc::TryRecvError::Disconnected) => stopping = true,
                }
            }
            // Bound the backlog: if the encoder can't keep up, drop the oldest
            // frames rather than growing memory without limit. Safe to drop
            // here precisely because these are still *raw* — nothing has been
            // encoded against them yet, so there's no reference chain to break
            // (contrast the encoded packets below, which must never be
            // dropped). Kept short: a queued raw frame is already stale by the
            // time it would be encoded.
            while pending.len() > 2 {
                pending.pop_front();
            }

            while need_input > 0 {
                let Some((bgra, ts)) = pending.pop_front() else {
                    break;
                };
                let sample = self.make_input_sample(&bgra, ts)?;
                unsafe { self.transform.ProcessInput(0, &sample, 0)? };
                need_input -= 1;
            }

            if stopping && pending.is_empty() && !draining {
                unsafe {
                    self.transform
                        .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?
                };
                draining = true;
            }

            let ev = unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            match ev {
                Ok(ev) => {
                    let kind = unsafe { ev.GetType()? };
                    if kind == METransformNeedInput.0 as u32 {
                        need_input += 1;
                    } else if kind == METransformHaveOutput.0 as u32 {
                        if let Some(p) = self.pull_output()? {
                            // Never drop an encoded packet: every delta frame
                            // is decoded against the one before it, so a hole
                            // here shows up on the viewer as tearing/black
                            // bands that persist until the next keyframe.
                            // Overload is handled by not *encoding* in the
                            // first place — see the capture loop's net_busy
                            // gate in `rc-host`'s session.
                            if packet_tx.send(p).is_err() {
                                break;
                            }
                        }
                    } else if kind == METransformDrainComplete.0 as u32 {
                        break;
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(Duration::from_micros(400));
                }
                Err(e) => return Err(e).context("encoder GetEvent"),
            }
        }

        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        Ok(())
    }

    fn run_sync(
        &mut self,
        job_rx: std_mpsc::Receiver<Job>,
        packet_tx: tok_mpsc::UnboundedSender<EncodedPacket>,
    ) -> Result<()> {
        while let Ok(job) = job_rx.recv() {
            let (bgra, ts) = match job {
                Job::Frame { bgra, ts_us } => (bgra, ts_us),
                Job::Stop => break,
            };
            let sample = self.make_input_sample(&bgra, ts)?;
            unsafe { self.transform.ProcessInput(0, &sample, 0)? };
            loop {
                match self.pull_output() {
                    Ok(Some(p)) => {
                        if packet_tx.send(p).is_err() {
                            return Ok(());
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Err(e),
                }
            }
        }
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        Ok(())
    }

    fn make_input_sample(&mut self, bgra: &[u8], ts_us: u64) -> Result<IMFSample> {
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        bgra_to_nv12(bgra, w, h, &mut self.nv12);
        let len = nv12_len(w, h);

        if let Some(gpu) = &self.gpu {
            let sample = gpu.upload(&self.nv12, w, h)?;
            unsafe {
                sample.SetSampleTime((ts_us * 10) as i64)?;
                sample.SetSampleDuration((10_000_000 / self.cfg.fps.max(1)) as i64)?;
            }
            return Ok(sample);
        }

        unsafe {
            let buffer = MFCreateMemoryBuffer(len as u32)?;
            let mut dst: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), dst, len);
            buffer.Unlock()?;
            buffer.SetCurrentLength(len as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((ts_us * 10) as i64)?; // 100-ns units
            sample.SetSampleDuration((10_000_000 / self.cfg.fps.max(1)) as i64)?;
            Ok(sample)
        }
    }

    /// Pull one encoded frame if the MFT has one ready.
    fn pull_output(&mut self) -> Result<Option<EncodedPacket>> {
        let preset = if self.provides_samples {
            None
        } else {
            // Software MFT: we must supply the output buffer.
            let si = unsafe { self.transform.GetOutputStreamInfo(0)? };
            let size = si.cbSize.max(1 << 20);
            unsafe {
                let buf = MFCreateMemoryBuffer(size)?;
                let s = MFCreateSample()?;
                s.AddBuffer(&buf)?;
                Some(s)
            }
        };

        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(preset),
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
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) => return Err(e).context("ProcessOutput"),
        }

        let Some(sample) = sample else {
            return Ok(None);
        };
        unsafe {
            let key_frame = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let ts_100ns = sample.GetSampleTime().unwrap_or(0);
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut cur = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut cur))?;
            let data = std::slice::from_raw_parts(ptr, cur as usize).to_vec();
            buffer.Unlock()?;
            Ok(Some(EncodedPacket {
                data,
                key_frame,
                timestamp_us: (ts_100ns / 10).max(0) as u64,
            }))
        }
    }
}

impl Drop for EncoderState {
    fn drop(&mut self) {
        // Releases the MFT's resources — for a hardware encoder, its GPU
        // encode session — which dropping the COM reference alone does not.
        unsafe {
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// Enumerate encoder MFTs, hardware first, then software as a fallback.
/// Returns each transform (with its activation object, needed to shut it down)
/// and whether it is async (hardware) — the caller tries them in order, since
/// a hardware MFT can enumerate fine but still fail to configure.
fn find_encoders() -> Result<Vec<(IMFActivate, IMFTransform, bool)>> {
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let in_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };

    let mut found = Vec::new();
    for (flags, is_async) in [
        (
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            true,
        ),
        (MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, false),
    ] {
        if let Some((activate, t)) = enum_first(flags, &in_info, &out_info)? {
            found.push((activate, t, is_async));
        }
    }
    if found.is_empty() {
        bail!("no H.264 encoder MFT available");
    }
    Ok(found)
}

fn enum_first(
    flags: MFT_ENUM_FLAG,
    in_info: &MFT_REGISTER_TYPE_INFO,
    out_info: &MFT_REGISTER_TYPE_INFO,
) -> Result<Option<(IMFActivate, IMFTransform)>> {
    unsafe {
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(in_info as *const _),
            Some(out_info as *const _),
            &mut arr,
            &mut count,
        )
        .context("MFTEnumEx")?;

        let mut chosen: Option<(IMFActivate, IMFTransform)> = None;
        for i in 0..count as usize {
            // Take ownership of each entry so every one is released, not just
            // the one we keep.
            let Some(activate) = std::ptr::read(arr.add(i)) else { continue };
            if chosen.is_none() {
                if let Ok(t) = activate.ActivateObject::<IMFTransform>() {
                    chosen = Some((activate, t));
                }
            }
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(arr as *const _));
        Ok(chosen)
    }
}

/// NV12 frames uploaded to D3D11 textures — the input hardware encoder MFTs
/// require. One staging texture is reused for the CPU upload; each frame gets
/// its own GPU texture, since an async encoder may still hold the last one.
struct GpuInput {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    _manager: IMFDXGIDeviceManager,
}

impl GpuInput {
    fn new(transform: &IMFTransform, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice (video)")?;
            let device: ID3D11Device = device.context("no D3D11 device")?;
            let context: ID3D11DeviceContext = context.context("no D3D11 context")?;
            // The MFT uses the device from its own threads.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).context("MFCreateDXGIDeviceManager")?;
            let manager: IMFDXGIDeviceManager = manager.context("no DXGI device manager")?;
            manager.ResetDevice(&device, token).context("ResetDevice")?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("MFT_MESSAGE_SET_D3D_MANAGER")?;

            let staging = nv12_texture(&device, width, height, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_WRITE.0 as u32)?;
            Ok(Self {
                device,
                context,
                staging,
                _manager: manager,
            })
        }
    }

    /// Upload one NV12 frame and wrap it in an input sample.
    fn upload(&self, nv12: &[u8], width: usize, height: usize) -> Result<IMFSample> {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .context("Map(NV12 staging)")?;
            let pitch = mapped.RowPitch as usize;
            let dst = mapped.pData as *mut u8;
            // Luma rows, then the half-height interleaved chroma plane, which
            // starts right after the luma plane at the same row pitch.
            for y in 0..height {
                std::ptr::copy_nonoverlapping(nv12.as_ptr().add(y * width), dst.add(y * pitch), width);
            }
            for y in 0..height / 2 {
                std::ptr::copy_nonoverlapping(
                    nv12.as_ptr().add(width * height + y * width),
                    dst.add((height + y) * pitch),
                    width,
                );
            }
            self.context.Unmap(&self.staging, 0);

            let texture = nv12_texture(&self.device, width as u32, height as u32, D3D11_USAGE_DEFAULT, 0)?;
            self.context.CopyResource(&texture, &self.staging);

            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &texture, 0, false)
                .context("MFCreateDXGISurfaceBuffer")?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                if let Ok(len) = b2.GetContiguousLength() {
                    let _ = buffer.SetCurrentLength(len);
                }
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            Ok(sample)
        }
    }
}

unsafe fn nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    usage: D3D11_USAGE,
    cpu_access: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: 0,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut texture = None;
    device
        .CreateTexture2D(&desc, None, Some(&mut texture))
        .context("CreateTexture2D (NV12)")?;
    texture.context("no NV12 texture")
}

enum V {
    Guid(GUID),
    U32(u32),
}

fn media_type(attrs: &[(GUID, V)]) -> Result<IMFMediaType> {
    unsafe {
        let mt = MFCreateMediaType()?;
        for (k, v) in attrs {
            match v {
                V::Guid(g) => mt.SetGUID(k, g)?,
                V::U32(n) => mt.SetUINT32(k, *n)?,
            }
        }
        Ok(mt)
    }
}

fn set_ratio(mt: &IMFMediaType, key: GUID, hi: u32, lo: u32) -> Result<()> {
    let packed = ((hi as u64) << 32) | lo as u64;
    unsafe { mt.SetUINT64(&key, packed).context("SetUINT64") }
}
