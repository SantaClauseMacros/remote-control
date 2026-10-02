//! Play back a connected device's microphone on this PC — the reverse of
//! [`crate::stream_loopback`].
//!
//! WASAPI shared mode alone can't turn this into something Discord or a game
//! picks up *as a microphone*: Windows has no built-in virtual input device.
//! What this can do is render the decoded audio to whichever output device
//! looks like a virtual audio cable (its name contains "CABLE" — the free
//! [VB-CABLE](https://vb-audio.com/Cable/) driver is the common one, paired
//! with a matching "CABLE Output" recording device other apps can select as
//! their microphone). Without one installed, it falls back to this PC's
//! normal speakers — audible, but not selectable as a mic anywhere. See the
//! README for the one-time setup.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};

/// One decoded mic packet: interleaved PCM samples, sample rate, channel
/// count (1 or 2 — see `adpcm::Decoded`, which this is built from).
pub type MicPacket = (Vec<i16>, u32, usize);

/// Render decoded microphone packets from `rx` until `stop` is set. Opens the
/// output device lazily (only once a packet actually arrives) and closes it
/// again after a few seconds of silence, so an enabled-but-unused mic feature
/// costs nothing and a later reconnect at a different sample rate just
/// reopens cleanly.
pub fn render_mic(stop: &AtomicBool, rx: &Receiver<MicPacket>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut retry = Duration::from_millis(250);
    let mut warned_no_cable = false;
    while !stop.load(Ordering::Relaxed) {
        let first = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match render_session(stop, &first, rx, &mut warned_no_cable) {
            Ok(()) => retry = Duration::from_millis(250),
            Err(e) => {
                tracing::debug!(error = ?e, retry_ms = retry.as_millis() as u64, "mic render interrupted");
                std::thread::sleep(retry);
                retry = (retry * 2).min(Duration::from_secs(5));
            }
        }
    }
}

/// Whether a virtual audio cable (a render device with "cable" in its name,
/// e.g. VB-CABLE's "CABLE Input") is installed — i.e. whether a phone's mic
/// can actually show up as a microphone ("CABLE Output") in Discord or a game.
pub fn has_virtual_cable() -> bool {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return false;
        };
        pick_device(&enumerator).map(|(_, cable)| cable).unwrap_or(false)
    }
}

/// Pick a render endpoint: prefer one whose friendly name contains "cable"
/// (a virtual audio cable's input side), else this PC's default output.
/// Returns whether a cable device was found.
fn pick_device(enumerator: &IMMDeviceEnumerator) -> Result<(IMMDevice, bool)> {
    unsafe {
        if let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
            let count = collection.GetCount().unwrap_or(0);
            for i in 0..count {
                let Ok(dev) = collection.Item(i) else { continue };
                if device_name(&dev).unwrap_or_default().to_ascii_lowercase().contains("cable") {
                    return Ok((dev, true));
                }
            }
        }
        let dev = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("default output device")?;
        Ok((dev, false))
    }
}

unsafe fn device_name(dev: &IMMDevice) -> Result<String> {
    let store = dev.OpenPropertyStore(STGM_READ)?;
    let val = store.GetValue(&PKEY_Device_FriendlyName)?;
    Ok(val.to_string())
}

/// Open a device at `first`'s sample rate and keep feeding it packets from
/// `rx` until the rate changes (caller reopens), silence times out, or `rx`
/// closes.
fn render_session(
    stop: &AtomicBool,
    first: &MicPacket,
    rx: &Receiver<MicPacket>,
    warned_no_cable: &mut bool,
) -> Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("audio device enumerator")?;
        let (device, is_cable) = pick_device(&enumerator)?;
        let name = device_name(&device).unwrap_or_else(|_| "(unknown device)".to_string());
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).context("audio client")?;

        let rate = first.1;
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        // AUTOCONVERTPCM + SRC_DEFAULT_QUALITY: let the audio engine resample
        // from our fixed format to whatever the device's mix format actually
        // is, rather than us tracking every device's native rate.
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                1_000_000, // 100ms shared buffer
                0,
                &fmt,
                None,
            )
            .context("initializing mic playback")?;
        let render: IAudioRenderClient = client.GetService().context("render client")?;
        let buffer_frames = client.GetBufferSize().context("buffer size")?;
        client.Start().context("starting mic playback")?;
        tracing::info!(rate, device = %name, cable = is_cable, "mic playback started");
        if !is_cable && !*warned_no_cable {
            *warned_no_cable = true;
            tracing::warn!(
                "no virtual audio cable found (looked for a device with \"cable\" in its name) — \
                 the phone's mic will play out loud on this PC's speakers instead of being usable \
                 as a microphone in other apps; see the README for a free driver that fixes this"
            );
        }

        let write = |samples: &[i16], src_channels: usize| -> Result<()> {
            let stereo: std::borrow::Cow<[i16]> = if src_channels == 2 {
                std::borrow::Cow::Borrowed(samples)
            } else {
                std::borrow::Cow::Owned(samples.iter().flat_map(|&s| [s, s]).collect())
            };
            let total_frames = (stereo.len() / 2) as u32;
            let mut offset = 0u32;
            while offset < total_frames {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let padding = client.GetCurrentPadding().context("padding")?;
                let available = buffer_frames.saturating_sub(padding);
                if available == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let want = available.min(total_frames - offset);
                if want == 0 {
                    continue;
                }
                let data = render.GetBuffer(want).context("render buffer")?;
                let dst = std::slice::from_raw_parts_mut(data as *mut i16, want as usize * 2);
                let src = &stereo[offset as usize * 2..(offset + want) as usize * 2];
                dst.copy_from_slice(src);
                render.ReleaseBuffer(want, 0).context("release render buffer")?;
                offset += want;
            }
            Ok(())
        };
        write(&first.0, first.2)?;

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok((samples, packet_rate, channels)) => {
                    if packet_rate != rate {
                        // Reopen at the new rate — the source changed (e.g. a
                        // different browser tab's AudioContext rate).
                        return Ok(());
                    }
                    write(&samples, channels)?;
                }
                Err(RecvTimeoutError::Timeout) => break, // idle: close the device
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = client.Stop();
        Ok(())
    }
}
