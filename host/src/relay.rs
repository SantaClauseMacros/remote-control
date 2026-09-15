//! Keeps a connection parked at the rendezvous relay so clients on other
//! networks can reach this host without any inbound port.
//!
//! Each time the relay splices in a client, the raw pre-handshake stream is
//! handed to the engine, which runs the same Noise handshake and session path
//! it uses for a LAN accept.
//!
//! `relay_addr` picks the transport: `ws://`/`wss://` parks over a WebSocket
//! (what a Cloudflare Worker — or anything else that only speaks HTTP — needs),
//! anything else parks a raw TCP connection to a self-hosted `rc-relay`. Both
//! come out the other side as the same [`rc_transport::relay::BoxedIo`], so
//! the rest of the engine never needs to know which one it got.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rc_transport::relay::BoxedIo;
use tokio::sync::{mpsc, oneshot, watch};

/// A spliced stream plus a channel the engine fires the instant it *takes* the
/// stream, so the parker only re-parks once the previous one is being handled
/// (never buffering a stream the client has already given up on).
pub type Splice = (BoxedIo, oneshot::Sender<()>);

/// Spawn the parker. Returns the channel the engine polls for spliced streams.
/// `online` tracks whether the host is currently parked and reachable.
pub fn spawn(
    relay_addr: String,
    device_id: String,
    key: Option<String>,
    mut stop: watch::Receiver<bool>,
    online: Arc<AtomicBool>,
) -> mpsc::Receiver<Splice> {
    let (tx, rx) = mpsc::channel::<Splice>(1);

    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        tracing::info!(relay = %relay_addr, "relay parker started");

        loop {
            if *stop.borrow() {
                break;
            }

            let is_ws = relay_addr.starts_with("ws://") || relay_addr.starts_with("wss://");
            let park = async {
                if is_ws {
                    rc_transport::relay::park_as_host_ws(&relay_addr, &device_id, key.clone()).await
                } else {
                    rc_transport::relay::park_as_host(&relay_addr, &device_id, key.clone())
                        .await
                        .map(|s| Box::new(s) as BoxedIo)
                }
            };
            tokio::pin!(park);
            // A park only resolves once a client arrives, so a park still
            // waiting a few seconds in is what "connected and reachable"
            // looks like — a refused or broken connection fails well before.
            let reachable_after = tokio::time::sleep(Duration::from_secs(3));
            tokio::pin!(reachable_after);
            let mut marked = false;
            let outcome = loop {
                tokio::select! {
                    _ = stop.changed() => break None,
                    res = &mut park => break Some(res),
                    _ = &mut reachable_after, if !marked => {
                        marked = true;
                        online.store(true, Ordering::Relaxed);
                    }
                }
            };
            let Some(parked) = outcome else { continue };

            match parked {
                Ok(stream) => {
                    backoff = Duration::from_secs(1);
                    tracing::info!("relay spliced a client in");
                    let (ack_tx, ack_rx) = oneshot::channel();
                    if tx.send((stream, ack_tx)).await.is_err() {
                        break; // engine gone
                    }
                    // Wait until the engine has actually picked the stream up
                    // (i.e. no session is running) before parking again.
                    tokio::select! {
                        _ = ack_rx => {}
                        _ = stop.changed() => break,
                    }
                }
                Err(e) => {
                    online.store(false, Ordering::Relaxed);
                    tracing::warn!(error = %e, retry_in_s = backoff.as_secs(), "relay park failed");
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = stop.changed() => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
        online.store(false, Ordering::Relaxed);
        tracing::info!("relay parker stopped");
    });

    rx
}
