//! System sound for a remote session: capture whatever the PC is playing
//! (WASAPI loopback on the default output device) and pack it into small
//! IMA ADPCM packets.
//!
//! ADPCM rather than Opus: it's a few dozen lines with no native dependency,
//! costs next to no CPU, adds no codec delay, and the browser client decodes it
//! in plain JavaScript on every platform — including iPhone Safari, which has
//! no WebCodecs audio decoder. At 4 bits per sample, 48 kHz stereo is about
//! 390 kbit/s: small beside the video.

pub mod adpcm;
#[cfg(windows)]
mod loopback;

#[cfg(windows)]
pub use loopback::stream_loopback;
