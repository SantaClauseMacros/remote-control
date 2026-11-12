//! A mic-only link: a device that connected just to be this PC's microphone.
//!
//! No screen capture, no encoder, no input - the only thing it does is play
//! the device's mic on the PC (into the virtual cable if one is installed, so
//! Discord or a game can pick it as a microphone). It lives beside the
//! controlling sessions rather than replacing one, so a phone can keep
//! driving the PC while another device supplies the mic.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rc_protocol::{ClientMessage, HostMessage};
use rc_transport::lan::LanSession;
use rc_transport::Session;
use tokio::sync::watch;

/// `true` while a mic-only link is connected. Controlling sessions check it
/// and stop playing their own device's mic, so two mics never mix into the
/// same virtual cable.
pub static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Clears [`ACTIVE`] however the session ends.
struct ActiveGuard;

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// Run until the device leaves, goes quiet, or `stop` flips to `true`.
pub async fn run(session: LanSession, mut stop: watch::Receiver<bool>) -> Result<()> {
    ACTIVE.store(true, Ordering::SeqCst);
    let _active = ActiveGuard;

    let virtual_cable = rc_audio::has_virtual_cable();
    let mic_stop = Arc::new(AtomicBool::new(false));
    let (render_tx, render_rx) = std_mpsc::sync_channel::<rc_audio::render::MicPacket>(8);
    {
        let ms = mic_stop.clone();
        std::thread::Builder::new()
            .name("rc-mic-only".into())
            .spawn(move || rc_audio::render_mic(&ms, &render_rx))
            .ok();
    }

    let send = |msg: HostMessage| {
        let session = &session;
        async move {
            let _ = session.control().send(rc_protocol::encode(&msg)).await;
        }
    };

    let mut mic_last: Option<Instant> = None;
    let mut mic_reported = false;
    let mut last_heard = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let reason: String;
    loop {
        tokio::select! {
            r = session.control().recv() => match r {
                Ok(bytes) => {
                    last_heard = Instant::now();
                    match rc_protocol::decode::<ClientMessage>(&bytes) {
                        Ok(ClientMessage::Ping { nonce }) => send(HostMessage::Pong { nonce }).await,
                        Ok(ClientMessage::RequestInfo) => {
                            send(HostMessage::Info {
                                version: env!("CARGO_PKG_VERSION").to_string(),
                                virtual_mic: virtual_cable,
                            })
                            .await;
                        }
                        Ok(ClientMessage::Disconnect) => {
                            reason = "device left".into();
                            break;
                        }
                        // MicOnly itself, and anything a mic-only device has
                        // no business sending: ignored.
                        _ => {}
                    }
                }
                Err(e) => {
                    reason = format!("device disconnected: {e}");
                    break;
                }
            },
            mic = session.recv_mic() => match mic {
                Ok(packet) => {
                    last_heard = Instant::now();
                    mic_last = Some(Instant::now());
                    if let Some(d) = rc_audio::adpcm::decode(&packet) {
                        let _ = render_tx.try_send((d.samples, d.sample_rate, d.channels));
                    }
                }
                Err(e) => {
                    reason = format!("device disconnected: {e}");
                    break;
                }
            },
            _ = tick.tick() => {
                let receiving = mic_last.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                if receiving || mic_reported {
                    mic_reported = receiving;
                    send(HostMessage::MicStatus { receiving, virtual_cable }).await;
                }
                if last_heard.elapsed() > Duration::from_secs(60) {
                    reason = "device stopped responding (lost connection?)".into();
                    break;
                }
            }
            _ = stop.changed() => {
                if *stop.borrow() {
                    reason = "host stopping".into();
                    break;
                }
            }
        }
    }

    tracing::info!(%reason, "mic-only link ending");
    mic_stop.store(true, Ordering::SeqCst);
    send(HostMessage::Disconnect { reason }).await;
    Ok(())
}
