//! WASAPI loopback capture of the default output device — exactly what the
//! PC's speakers or headphones are playing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::adpcm::Encoder;

/// Packets per second: 20 ms each.
const PACKETS_PER_SECOND: u32 = 50;

/// Capture the PC's sound until `stop` is set, calling `on_packet` with each
/// encoded packet (stereo, at the output device's own sample rate). Reopens
/// the device when it goes away — headphones plugged in, output switched —
/// and waits quietly when there's no output device at all.
pub fn stream_loopback(stop: &AtomicBool, mut on_packet: impl FnMut(Vec<u8>)) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut retry = Duration::from_millis(250);
    while !stop.load(Ordering::Relaxed) {
        match capture_device(stop, &mut on_packet) {
            Ok(()) => retry = Duration::from_millis(250),
            Err(e) => {
                tracing::debug!(error = ?e, retry_ms = retry.as_millis() as u64, "sound capture interrupted");
                std::thread::sleep(retry);
                retry = (retry * 2).min(Duration::from_secs(5));
            }
        }
    }
    Ok(())
}

fn capture_device(stop: &AtomicBool, on_packet: &mut impl FnMut(Vec<u8>)) -> Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("audio device enumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("default output device")?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("audio client")?;

        let mix = client.GetMixFormat().context("mix format")?;
        let format = SampleFormat::parse(mix);
        // 100 ms buffer (in 100-ns units) — plenty, since it's drained every few ms.
        let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 1_000_000, 0, mix, None);
        CoTaskMemFree(Some(mix as *const _));
        let format = format?;
        init.context("starting loopback capture")?;

        let capture: IAudioCaptureClient = client.GetService().context("capture client")?;
        client.Start().context("start")?;
        tracing::info!(
            rate = format.rate,
            channels = format.channels,
            bits = format.bits,
            float = format.float,
            "sound capture started"
        );

        let block = (format.rate / PACKETS_PER_SECOND).max(1) as usize;
        let mut encoder = Encoder::new(2, format.rate);
        let mut pending: Vec<i16> = Vec::with_capacity(block * 8);

        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
            loop {
                let next = capture.GetNextPacketSize().context("packet size")?;
                if next == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .context("reading captured sound")?;
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                format.append_stereo(data, frames as usize, silent, &mut pending);
                capture.ReleaseBuffer(frames).context("releasing captured sound")?;
            }
            // Nothing is captured while nothing plays, so packets only flow
            // when there's actually sound.
            while pending.len() >= block * 2 {
                on_packet(encoder.encode(&pending[..block * 2]));
                pending.drain(..block * 2);
            }
            // Never let a backlog build if the consumer stalls.
            if pending.len() > block * 2 * 10 {
                pending.clear();
            }
        }
        let _ = client.Stop();
        Ok(())
    }
}

/// The shared-mode mix format, which loopback capture always delivers.
struct SampleFormat {
    channels: usize,
    rate: u32,
    bits: u16,
    float: bool,
    block_align: usize,
}

impl SampleFormat {
    unsafe fn parse(fmt: *const WAVEFORMATEX) -> Result<Self> {
        ensure!(!fmt.is_null(), "no mix format");
        let f = std::ptr::read_unaligned(fmt);
        let (tag, bits, channels, rate, block_align) =
            (f.wFormatTag, f.wBitsPerSample, f.nChannels, f.nSamplesPerSec, f.nBlockAlign);
        let float = match tag {
            3 => true,  // WAVE_FORMAT_IEEE_FLOAT
            1 => false, // WAVE_FORMAT_PCM
            0xFFFE => {
                // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID's first field is
                // the classic tag (3 = float, 1 = PCM).
                let ext = std::ptr::read_unaligned(fmt as *const WAVEFORMATEXTENSIBLE);
                let sub = ext.SubFormat;
                match sub.data1 {
                    3 => true,
                    1 => false,
                    other => bail!("unsupported sound sub-format {other:#x}"),
                }
            }
            other => bail!("unsupported sound format tag {other:#x}"),
        };
        ensure!(
            matches!((float, bits), (true, 32) | (false, 16) | (false, 24) | (false, 32)),
            "unsupported sample format: {bits}-bit {}",
            if float { "float" } else { "integer" }
        );
        let sample_bytes = usize::from(bits / 8);
        ensure!(channels >= 1, "no channels");
        ensure!(
            usize::from(block_align) >= sample_bytes * usize::from(channels),
            "inconsistent block alignment"
        );
        Ok(Self {
            channels: usize::from(channels),
            rate,
            bits,
            float,
            block_align: usize::from(block_align),
        })
    }

    /// Convert `frames` of captured sound to interleaved 16-bit stereo. Mono is
    /// doubled; beyond two channels, front left/right are kept.
    unsafe fn append_stereo(&self, data: *const u8, frames: usize, silent: bool, out: &mut Vec<i16>) {
        if silent || data.is_null() {
            out.resize(out.len() + frames * 2, 0);
            return;
        }
        let bytes = std::slice::from_raw_parts(data, frames * self.block_align);
        let size = usize::from(self.bits / 8);
        let sample = |frame: usize, ch: usize| -> f32 {
            let o = frame * self.block_align + ch * size;
            let b = &bytes[o..o + size];
            match (self.float, self.bits) {
                (true, _) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                (false, 16) => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
                (false, 24) => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
                _ => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            }
        };
        let to_i16 = |v: f32| (v.clamp(-1.0, 1.0) * 32_767.0) as i16;
        out.reserve(frames * 2);
        for frame in 0..frames {
            let left = sample(frame, 0);
            let right = if self.channels > 1 { sample(frame, 1) } else { left };
            out.push(to_i16(left));
            out.push(to_i16(right));
        }
    }
}
