//! Direct connections: carry a session over a WebRTC data channel when the
//! device can reach this PC on the local network, instead of through the relay.
//!
//! A browser can't open a plain socket to a LAN address from a secure page, but
//! it can do WebRTC. The device connects through the relay as usual, sends a
//! WebRTC offer over that (already end-to-end encrypted) session, and this
//! module answers it with this PC's local address. If the data channel opens,
//! a second Noise handshake runs over it and the session moves across; if not
//! — different networks, a firewall, anything — nothing changes.
//!
//! The PC is an ICE-lite peer: it only answers connectivity checks the browser
//! sends to its host candidate, which is all a same-network path needs.

use std::net::{IpAddr, SocketAddr};
use std::sync::Once;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Data channel message size. Browsers accept much larger, but small messages
/// keep one big video frame from monopolising the channel.
const CHUNK: usize = 16 * 1024;
/// Session bytes waiting for the channel before the session is made to wait —
/// that backpressure is what lets the capture loop skip frames on a slow link.
const MAX_QUEUED: usize = 2 * 1024 * 1024;
/// How long the data channel gets to open before giving up.
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

static CRYPTO: Once = Once::new();

/// Answer a browser's SDP offer. Returns the SDP answer to send back, and this
/// end of a byte stream that carries the data channel once it opens (the
/// session runs its handshake over it). The WebRTC connection runs in its own
/// task until either side goes away.
pub fn accept(offer_sdp: &str) -> Result<(String, DuplexStream)> {
    CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());

    let ip = lan_ip().context("no local network address")?;
    let socket = std::net::UdpSocket::bind(SocketAddr::new(ip, 0)).context("binding a UDP port")?;
    socket.set_nonblocking(true)?;
    let local = socket.local_addr()?;

    let mut rtc = Rtc::builder().set_ice_lite(true).build(Instant::now());
    let candidate = Candidate::host(local, "udp").map_err(|e| anyhow!("host candidate: {e:?}"))?;
    rtc.add_local_candidate(candidate);
    let offer = SdpOffer::from_sdp_string(offer_sdp).map_err(|e| anyhow!("parsing the offer: {e:?}"))?;
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| anyhow!("accepting the offer: {e:?}"))?;

    let (session_end, bridge_end) = tokio::io::duplex(1 << 20);
    let socket = UdpSocket::from_std(socket)?;
    tracing::info!(%local, "offering a direct connection");
    tokio::spawn(async move {
        match drive(rtc, socket, local, bridge_end).await {
            Ok(()) => tracing::info!("direct path closed"),
            Err(e) => tracing::info!(error = %e, "direct path ended"),
        }
    });
    Ok((answer.to_sdp_string(), session_end))
}

/// Run the WebRTC connection: UDP in and out, timers, and shuttling bytes
/// between the data channel and the session's stream.
async fn drive(mut rtc: Rtc, socket: UdpSocket, local: SocketAddr, bridge: DuplexStream) -> Result<()> {
    let (mut from_session, mut to_session) = tokio::io::split(bridge);
    // Incoming channel data goes to the session through a queue, so the
    // WebRTC loop never waits on the session.
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(chunk) = in_rx.recv().await {
            if to_session.write_all(&chunk).await.is_err() {
                break;
            }
        }
    });

    let started = Instant::now();
    let mut channel: Option<ChannelId> = None;
    let mut outgoing: Vec<u8> = Vec::new();
    let mut packet = vec![0u8; 2000];
    let mut read_buf = vec![0u8; 64 * 1024];

    loop {
        // Drain everything the engine has to say until it only wants a timer.
        let deadline = loop {
            match rtc.poll_output().map_err(|e| anyhow!("{e:?}"))? {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Output::Event(Event::ChannelOpen(id, _)) => {
                    tracing::info!(after_ms = started.elapsed().as_millis() as u64, "direct data channel open");
                    channel = Some(id);
                }
                Output::Event(Event::ChannelData(d)) => {
                    if in_tx.send(d.data).is_err() {
                        return Ok(());
                    }
                }
                Output::Event(Event::ChannelClose(_)) => return Ok(()),
                Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => {
                    return Ok(());
                }
                Output::Event(_) => {}
            }
        };
        if !rtc.is_alive() {
            return Ok(());
        }
        if channel.is_none() && started.elapsed() > OPEN_TIMEOUT {
            bail!("the direct path never opened");
        }

        // Hand queued session bytes to the channel: one write, then drain again.
        if let Some(id) = channel {
            if !outgoing.is_empty() {
                let n = outgoing.len().min(CHUNK);
                if let Some(mut ch) = rtc.channel(id) {
                    if ch.write(true, &outgoing[..n]).map_err(|e| anyhow!("{e:?}"))? {
                        outgoing.drain(..n);
                        continue;
                    }
                }
                // Not accepted: the channel's send buffer is full. Carry on
                // with network input, which frees it as the browser acks.
            }
        }

        let wait = deadline.saturating_duration_since(Instant::now());
        let input = tokio::select! {
            r = socket.recv_from(&mut packet) => Some(r?),
            _ = tokio::time::sleep(wait) => None,
            r = from_session.read(&mut read_buf), if channel.is_some() && outgoing.len() < MAX_QUEUED => {
                let n = r?;
                if n == 0 {
                    return Ok(()); // the session let go of the direct path
                }
                outgoing.extend_from_slice(&read_buf[..n]);
                continue;
            }
        };
        match input {
            Some((n, source)) => {
                let contents = packet[..n].try_into().map_err(|e| anyhow!("{e:?}"))?;
                rtc.handle_input(Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local,
                        contents,
                    },
                ))
                .map_err(|e| anyhow!("{e:?}"))?;
            }
            None => rtc.handle_input(Input::Timeout(Instant::now())).map_err(|e| anyhow!("{e:?}"))?,
        }
    }
}

/// This PC's address on its main network — the local end of the route toward
/// the internet (working it out sends nothing).
fn lan_ip() -> Option<IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:53").ok()?;
    let ip = probe.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}
