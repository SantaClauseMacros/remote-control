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

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
pub async fn accept(offer_sdp: &str) -> Result<(String, DuplexStream)> {
    CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());

    let ip = lan_ip().context("no local network address")?;
    let socket = std::net::UdpSocket::bind(SocketAddr::new(ip, 0)).context("binding a UDP port")?;
    socket.set_nonblocking(true)?;
    let local = socket.local_addr()?;
    let socket = UdpSocket::from_std(socket)?;

    let mut rtc = Rtc::builder().set_ice_lite(true).build(Instant::now());
    let candidate = Candidate::host(local, "udp").map_err(|e| anyhow!("host candidate: {e:?}"))?;
    rtc.add_local_candidate(candidate);

    // Best-effort: also offer a candidate at this router's public IP, learned
    // via one STUN request on the same socket. On a router that isn't doing
    // symmetric NAT, that lets a device reach this PC directly even from a
    // different network (mobile data, a friend's Wi-Fi) — not just the same
    // one. Skipped silently on any failure; the relay is always the fallback
    // regardless, so nothing here can make a connection worse.
    if let Some(public) = stun_public_addr(&socket).await {
        match Candidate::server_reflexive(public, local, "udp") {
            Ok(c) => {
                tracing::info!(%public, "adding a public candidate for off-network direct connections");
                rtc.add_local_candidate(c);
            }
            Err(e) => tracing::debug!(error = ?e, "server-reflexive candidate rejected"),
        }
    }

    let offer = SdpOffer::from_sdp_string(offer_sdp).map_err(|e| anyhow!("parsing the offer: {e:?}"))?;
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| anyhow!("accepting the offer: {e:?}"))?;

    let (session_end, bridge_end) = tokio::io::duplex(1 << 20);
    tracing::info!(%local, "offering a direct connection");
    tokio::spawn(async move {
        match drive(rtc, socket, local, bridge_end).await {
            Ok(()) => tracing::info!("direct path closed"),
            Err(e) => tracing::info!(error = %e, "direct path ended"),
        }
    });
    Ok((answer.to_sdp_string(), session_end))
}

/// One best-effort STUN Binding Request over `socket`, learning this NAT's
/// public mapping for the local address it's already bound to. Tries a
/// couple of public STUN servers with a short timeout each; `None` on any
/// failure (no reachable server, or the whole thing times out) rather than
/// holding up the direct-connection offer.
async fn stun_public_addr(socket: &UdpSocket) -> Option<SocketAddr> {
    const MAGIC: u32 = 0x2112_A442;
    for server in ["stun.l.google.com:19302", "stun1.l.google.com:19302"] {
        let Ok(mut addrs) = tokio::net::lookup_host(server).await else { continue };
        let Some(server_addr) = addrs.find(|a| a.is_ipv4()) else { continue };

        let mut txid = [0u8; 12];
        rand::Rng::fill(&mut rand::thread_rng(), &mut txid);
        let mut req = Vec::with_capacity(20);
        req.extend_from_slice(&1u16.to_be_bytes()); // Binding Request
        req.extend_from_slice(&0u16.to_be_bytes()); // no attributes
        req.extend_from_slice(&MAGIC.to_be_bytes());
        req.extend_from_slice(&txid);
        if socket.send_to(&req, server_addr).await.is_err() {
            continue;
        }

        let mut buf = [0u8; 512];
        let recv = tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buf)).await;
        if let Ok(Ok((n, _))) = recv {
            if let Some(addr) = parse_stun_binding_response(&buf[..n], &txid) {
                return Some(addr);
            }
        }
    }
    None
}

/// Pull the mapped address out of a STUN Binding Success Response — prefers
/// XOR-MAPPED-ADDRESS (RFC 5389), falls back to the older MAPPED-ADDRESS.
/// IPv4 only, which is all the LAN-IP path this pairs with ever produces.
fn parse_stun_binding_response(buf: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    const MAGIC: u32 = 0x2112_A442;
    if buf.len() < 20 || u16::from_be_bytes([buf[0], buf[1]]) != 0x0101 || &buf[8..20] != txid {
        return None;
    }
    let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = (20 + msg_len).min(buf.len());
    let mut mapped = None;
    let mut i = 20;
    while i + 4 <= end {
        let attr_type = u16::from_be_bytes([buf[i], buf[i + 1]]);
        let attr_len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        let Some(val) = buf.get(i + 4..i + 4 + attr_len) else { break };
        if attr_type == 0x0020 && val.len() >= 8 && val[1] == 0x01 {
            // XOR-MAPPED-ADDRESS
            let port = u16::from_be_bytes([val[2], val[3]]) ^ ((MAGIC >> 16) as u16);
            let magic_bytes = MAGIC.to_be_bytes();
            let ip = Ipv4Addr::new(val[4] ^ magic_bytes[0], val[5] ^ magic_bytes[1], val[6] ^ magic_bytes[2], val[7] ^ magic_bytes[3]);
            mapped = Some(SocketAddr::new(IpAddr::V4(ip), port));
        } else if attr_type == 0x0001 && mapped.is_none() && val.len() >= 8 && val[1] == 0x01 {
            // MAPPED-ADDRESS
            let port = u16::from_be_bytes([val[2], val[3]]);
            let ip = Ipv4Addr::new(val[4], val[5], val[6], val[7]);
            mapped = Some(SocketAddr::new(IpAddr::V4(ip), port));
        }
        i += 4 + attr_len.div_ceil(4) * 4;
    }
    mapped
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
