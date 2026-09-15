//! Byte-stream transport, secured with a Noise `NNpsk0` handshake keyed by the
//! pairing code. It runs over anything that is `AsyncRead + AsyncWrite` — a LAN
//! TCP connection ([`LanSession::connect`] / [`LanListener`]) or a relayed
//! stream from the rendezvous server ([`LanSession::over_stream`]).
//!
//! Wire format, after the handshake, per logical message:
//! ```text
//!   u32  chunk_count
//!   chunk_count ×:  u16 len  |  len bytes of Noise ciphertext
//! ```
//! The concatenated plaintext of a message is `channel_byte ++ payload`.
//! Chunking keeps every Noise message within its 65535-byte limit while still
//! delivering an arbitrarily large H.264 frame as one logical unit.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use snow::TransportState;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

/// Type-erased halves so [`LanSession`] itself isn't generic over the carrier.
type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

use crate::{
    ControlChannel, EncodedFrame, LinkFeedback, Result as TResult, Session, TransportError,
    VideoChannel,
};

/// First pairing: authenticate with the 6-digit code as a PSK, and learn each
/// other's static keys (trust on first use).
const NOISE_PAIR: &str = "Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s";
/// Reconnect: no code — mutual authentication is by the pinned static keys.
const NOISE_RESUME: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
/// One byte the initiator sends before the handshake to pick the pattern.
const MODE_PAIR: u8 = 0x01;
const MODE_RESUME: u8 = 0x02;

/// Plaintext bytes per Noise message (max 65535 − 16-byte tag, rounded down).
const MAX_CHUNK: usize = 48 * 1024;
/// Reject absurd frame headers early.
const MAX_CHUNKS_PER_MSG: u32 = 8192;

/// How a client authenticates itself to a host.
pub enum ClientAuth {
    /// First contact — authenticate with the 6-digit pairing code. Both ends
    /// learn (and should then persist) each other's static key.
    Pair { code: String },
    /// A reconnect — no code. The handshake aborts unless the host presents the
    /// `host_key` we pinned during pairing.
    Resume { host_key: [u8; 32] },
}

const CH_CONTROL: u8 = 0;
const CH_VIDEO: u8 = 1;
const CH_KEEPALIVE: u8 = 2;
const CH_KEYFRAME_REQ: u8 = 3;
/// Encoded sound packets, host → client (see `rc_audio::adpcm`).
const CH_AUDIO: u8 = 4;

fn err<E: std::fmt::Display>(e: E) -> TransportError {
    TransportError::Other(e.to_string())
}
fn hs_err<E: std::fmt::Display>(e: E) -> TransportError {
    TransportError::Handshake(e.to_string())
}

// ── handshake ────────────────────────────────────────────────────────────────

async fn write_hs<S: AsyncWrite + Unpin>(s: &mut S, msg: &[u8]) -> TResult<()> {
    s.write_u16(msg.len() as u16).await.map_err(hs_err)?;
    s.write_all(msg).await.map_err(hs_err)?;
    s.flush().await.map_err(hs_err)
}

async fn read_hs<S: AsyncRead + Unpin>(s: &mut S) -> TResult<Vec<u8>> {
    // A peer that rejects our PSK simply drops the socket, so EOF here reads as
    // an authentication failure rather than a generic transport error.
    let n = s.read_u16().await.map_err(|_| {
        TransportError::Handshake("peer closed during handshake (wrong PC ID?)".into())
    })? as usize;
    if n > 4096 {
        return Err(hs_err("handshake message too large"));
    }
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).await.map_err(|_| {
        TransportError::Handshake("peer closed during handshake (wrong PC ID?)".into())
    })?;
    Ok(buf)
}

/// Outcome of a completed handshake.
struct Handshaken {
    ts: TransportState,
    /// The peer's static X25519 public key.
    peer_key: [u8; 32],
    /// `true` if this was a first-time pairing (code-authenticated).
    was_pairing: bool,
}

async fn handshake_initiator<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    local_static: &[u8; 32],
    auth: &ClientAuth,
) -> TResult<Handshaken> {
    let (mode, params, psk): (u8, &str, Option<[u8; 32]>) = match auth {
        ClientAuth::Pair { code } => {
            (MODE_PAIR, NOISE_PAIR, Some(rc_crypto::derive_pairing_psk(code)))
        }
        ClientAuth::Resume { .. } => (MODE_RESUME, NOISE_RESUME, None),
    };
    stream.write_u8(mode).await.map_err(hs_err)?;
    stream.flush().await.map_err(hs_err)?;

    let mut b = snow::Builder::new(params.parse().map_err(hs_err)?).local_private_key(local_static);
    if let Some(k) = &psk {
        b = b.psk(0, k);
    }
    let mut hs = b.build_initiator().map_err(hs_err)?;
    run_handshake(stream, &mut hs).await?;

    let peer_key = remote_static(&hs)?;
    if let ClientAuth::Resume { host_key } = auth {
        if &peer_key != host_key {
            return Err(TransportError::Handshake(
                "the PC presented a different identity key — not connecting".into(),
            ));
        }
    }
    Ok(Handshaken {
        ts: hs.into_transport_mode().map_err(hs_err)?,
        peer_key,
        was_pairing: matches!(auth, ClientAuth::Pair { .. }),
    })
}

async fn handshake_responder<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    local_static: &[u8; 32],
    code: &str,
) -> TResult<Handshaken> {
    let mode = stream
        .read_u8()
        .await
        .map_err(|_| TransportError::Handshake("peer closed before the handshake".into()))?;
    let (params, psk) = match mode {
        MODE_PAIR => (NOISE_PAIR, Some(rc_crypto::derive_pairing_psk(code))),
        MODE_RESUME => (NOISE_RESUME, None),
        other => return Err(hs_err(format!("unknown handshake mode {other}"))),
    };

    let mut b = snow::Builder::new(params.parse().map_err(hs_err)?).local_private_key(local_static);
    if let Some(k) = &psk {
        b = b.psk(0, k);
    }
    let mut hs = b.build_responder().map_err(hs_err)?;
    run_handshake(stream, &mut hs).await?;

    Ok(Handshaken {
        peer_key: remote_static(&hs)?,
        was_pairing: mode == MODE_PAIR,
        ts: hs.into_transport_mode().map_err(hs_err)?,
    })
}

async fn run_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    hs: &mut snow::HandshakeState,
) -> TResult<()> {
    let mut buf = [0u8; 2048];
    while !hs.is_handshake_finished() {
        if hs.is_my_turn() {
            let n = hs.write_message(&[], &mut buf).map_err(hs_err)?;
            write_hs(stream, &buf[..n]).await?;
        } else {
            let msg = read_hs(stream).await?;
            hs.read_message(&msg, &mut buf).map_err(hs_err)?;
        }
    }
    Ok(())
}

fn remote_static(hs: &snow::HandshakeState) -> TResult<[u8; 32]> {
    let k = hs
        .get_remote_static()
        .ok_or_else(|| hs_err("peer did not present a static key"))?;
    k.try_into().map_err(|_| hs_err("bad static key length"))
}

// ── framed, encrypted wire ───────────────────────────────────────────────────

struct Wire {
    /// One shared Noise `TransportState` (both directions). The **writer** holds
    /// this across its socket write so that, when two messages are sent
    /// concurrently, the order their nonces were assigned equals the order their
    /// bytes hit the wire — which Noise AEAD requires for the peer to decrypt.
    /// The reader locks it only briefly, to decrypt bytes it has already read.
    noise: Mutex<TransportState>,
    writer: Mutex<BoxWrite>,
}

impl Wire {
    async fn write_message(&self, channel: u8, payload: &[u8]) -> TResult<()> {
        let mut plain = Vec::with_capacity(payload.len() + 1);
        plain.push(channel);
        plain.extend_from_slice(payload);

        // Hold `noise` for the whole encrypt+write so nonce order == wire order.
        let mut noise = self.noise.lock().await;
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        for chunk in plain.chunks(MAX_CHUNK) {
            let mut cbuf = vec![0u8; chunk.len() + 16];
            let n = noise.write_message(chunk, &mut cbuf).map_err(err)?;
            cbuf.truncate(n);
            chunks.push(cbuf);
        }

        // Serialise the whole frame before touching the socket and write it
        // once. Writing the header and each chunk separately meant a small
        // message — every mouse move and keystroke — went out as three or
        // more writes, and with TCP_NODELAY set (which it is, deliberately)
        // each of those can leave as its own packet.
        let total: usize = 4 + chunks.iter().map(|c| 2 + c.len()).sum::<usize>();
        let mut framed = Vec::with_capacity(total);
        framed.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
        for c in &chunks {
            framed.extend_from_slice(&(c.len() as u16).to_be_bytes());
            framed.extend_from_slice(c);
        }

        let mut w = self.writer.lock().await;
        w.write_all(&framed).await.map_err(err)?;
        w.flush().await.map_err(err)?;
        Ok(())
    }
}

async fn read_message(rd: &mut BoxRead, noise: &Mutex<TransportState>) -> TResult<(u8, Vec<u8>)> {
    let n_chunks = rd.read_u32().await.map_err(|_| TransportError::Closed)?;
    if n_chunks == 0 || n_chunks > MAX_CHUNKS_PER_MSG {
        return Err(err(format!("bad chunk count {n_chunks}")));
    }
    let mut cts: Vec<Vec<u8>> = Vec::with_capacity(n_chunks as usize);
    for _ in 0..n_chunks {
        let len = rd.read_u16().await.map_err(|_| TransportError::Closed)? as usize;
        let mut b = vec![0u8; len];
        rd.read_exact(&mut b)
            .await
            .map_err(|_| TransportError::Closed)?;
        cts.push(b);
    }

    let mut plain = Vec::new();
    {
        let mut ns = noise.lock().await;
        for ct in &cts {
            let mut pbuf = vec![0u8; ct.len()];
            let n = ns.read_message(ct, &mut pbuf).map_err(err)?;
            plain.extend_from_slice(&pbuf[..n]);
        }
    }
    if plain.is_empty() {
        return Err(err("empty message"));
    }
    let channel = plain[0];
    Ok((channel, plain[1..].to_vec()))
}

// ── channel impls ────────────────────────────────────────────────────────────

struct Control {
    wire: Arc<Wire>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
}

#[async_trait::async_trait]
impl ControlChannel for Control {
    async fn send(&self, bytes: Vec<u8>) -> TResult<()> {
        self.wire.write_message(CH_CONTROL, &bytes).await
    }
    async fn recv(&self) -> TResult<Vec<u8>> {
        self.rx
            .lock()
            .await
            .recv()
            .await
            .ok_or(TransportError::Closed)
    }
}

struct Video {
    wire: Arc<Wire>,
    rx: Mutex<mpsc::Receiver<EncodedFrame>>,
    rtt_ms: Arc<AtomicU32>, // milliseconds × 100, fixed point
    key_frame_req: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl VideoChannel for Video {
    async fn send(&self, frame: EncodedFrame) -> TResult<()> {
        // header: [u8 key_frame][u64 timestamp_us] then the payload
        let mut buf = Vec::with_capacity(frame.data.len() + 9);
        buf.push(frame.key_frame as u8);
        buf.extend_from_slice(&frame.timestamp_us.to_le_bytes());
        buf.extend_from_slice(&frame.data);
        self.wire.write_message(CH_VIDEO, &buf).await
    }

    async fn recv(&self) -> TResult<EncodedFrame> {
        self.rx
            .lock()
            .await
            .recv()
            .await
            .ok_or(TransportError::Closed)
    }

    fn feedback(&self) -> LinkFeedback {
        LinkFeedback {
            rtt_ms: self.rtt_ms.load(Ordering::Relaxed) as f32 / 100.0,
            target_bitrate_bps: 0,
            packet_loss: 0.0,
        }
    }

    fn request_key_frame(&self) {
        let wire = self.wire.clone();
        tokio::spawn(async move {
            let _ = wire.write_message(CH_KEYFRAME_REQ, &[]).await;
        });
    }

    fn take_key_frame_request(&self) -> bool {
        self.key_frame_req.swap(false, Ordering::AcqRel)
    }
}

// ── session ──────────────────────────────────────────────────────────────────

/// One established connection, with the peer's pinned static key.
pub struct LanSession {
    control: Control,
    video: Video,
    closed: Arc<Notify>,
    /// Set once the connection is gone, for callers that need to check rather
    /// than wait (see [`LanSession::is_closed`]).
    closed_flag: Arc<AtomicBool>,
    peer_key: [u8; 32],
    was_pairing: bool,
    _reader: JoinHandle<()>,
    _keepalive: JoinHandle<()>,
}

impl LanSession {
    /// The peer's static X25519 public key. The client pins the host's; the
    /// host allowlists the client's.
    pub fn peer_key(&self) -> [u8; 32] {
        self.peer_key
    }
    /// Was this a first-time (code-authenticated) pairing?
    pub fn was_pairing(&self) -> bool {
        self.was_pairing
    }

    /// Has the underlying connection ended?
    pub fn is_closed(&self) -> bool {
        self.closed_flag.load(Ordering::Acquire)
    }

    /// Send one encoded sound packet, on its own channel of the same
    /// encrypted stream as everything else.
    pub async fn send_audio(&self, packet: &[u8]) -> TResult<()> {
        self.control.wire.write_message(CH_AUDIO, packet).await
    }

    /// Connect over LAN TCP as the initiating client.
    pub async fn connect<A: ToSocketAddrs>(
        addr: A,
        local_static: &[u8; 32],
        auth: ClientAuth,
    ) -> TResult<Self> {
        let stream = TcpStream::connect(addr).await.map_err(err)?;
        stream.set_nodelay(true).ok();
        Self::over_stream_initiator(stream, local_static, auth).await
    }

    /// Client-side handshake over any duplex stream (relayed connection, TLS,
    /// WebRTC data channel…).
    pub async fn over_stream_initiator<S>(
        mut stream: S,
        local_static: &[u8; 32],
        auth: ClientAuth,
    ) -> TResult<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let hk = handshake_initiator(&mut stream, local_static, &auth).await?;
        let (rd, wr) = tokio::io::split(stream);
        Ok(Self::spawn(Box::new(rd), Box::new(wr), hk))
    }

    /// Host-side handshake over any duplex stream (used for the relay path).
    pub async fn over_stream_responder<S>(
        mut stream: S,
        local_static: &[u8; 32],
        current_code: &str,
    ) -> TResult<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let hk = handshake_responder(&mut stream, local_static, current_code).await?;
        let (rd, wr) = tokio::io::split(stream);
        Ok(Self::spawn(Box::new(rd), Box::new(wr), hk))
    }

    fn spawn(rd: BoxRead, wr: BoxWrite, hk: Handshaken) -> Self {
        let Handshaken { ts, peer_key, was_pairing } = hk;
        let wire = Arc::new(Wire {
            writer: Mutex::new(wr),
            noise: Mutex::new(ts),
        });

        let (ctrl_tx, ctrl_rx) = mpsc::channel::<Vec<u8>>(256);
        let (video_tx, video_rx) = mpsc::channel::<EncodedFrame>(8);
        let closed = Arc::new(Notify::new());
        let closed_flag = Arc::new(AtomicBool::new(false));
        let rtt_ms = Arc::new(AtomicU32::new(0));
        let key_frame_req = Arc::new(AtomicBool::new(false));

        let reader = tokio::spawn(reader_loop(
            rd,
            wire.clone(),
            ctrl_tx,
            video_tx,
            closed.clone(),
            closed_flag.clone(),
            rtt_ms.clone(),
            key_frame_req.clone(),
        ));

        // Both sides originate. The reply is what measures RTT, and it lands
        // in the *sender's* `rtt_ms` — so pinging from one side only left the
        // other end reading zero forever. On the host that's the number its
        // congestion control steers by, so it silently never engaged. Each
        // side pings, each side echoes, each side gets a real number.
        let keepalive = tokio::spawn(keepalive_loop(wire.clone()));

        Self {
            control: Control {
                wire: wire.clone(),
                rx: Mutex::new(ctrl_rx),
            },
            video: Video {
                wire,
                rx: Mutex::new(video_rx),
                rtt_ms,
                key_frame_req,
            },
            closed,
            closed_flag,
            peer_key,
            was_pairing,
            _reader: reader,
            _keepalive: keepalive,
        }
    }
}

#[async_trait::async_trait]
impl Session for LanSession {
    fn control(&self) -> &dyn ControlChannel {
        &self.control
    }
    fn video(&self) -> &dyn VideoChannel {
        &self.video
    }
    async fn closed(&self) {
        self.closed.notified().await
    }
}

async fn reader_loop(
    mut rd: BoxRead,
    wire: Arc<Wire>,
    ctrl_tx: mpsc::Sender<Vec<u8>>,
    video_tx: mpsc::Sender<EncodedFrame>,
    closed: Arc<Notify>,
    closed_flag: Arc<AtomicBool>,
    rtt_ms: Arc<AtomicU32>,
    key_frame_req: Arc<AtomicBool>,
) {
    // Set after dropping a video frame: everything until the next keyframe
    // would decode against a frame that never arrived. See the CH_VIDEO arm.
    let mut awaiting_key = false;
    loop {
        match read_message(&mut rd, &wire.noise).await {
            Ok((CH_CONTROL, payload)) => {
                if ctrl_tx.send(payload).await.is_err() {
                    break;
                }
            }
            Ok((CH_VIDEO, payload)) => {
                if payload.len() < 9 {
                    tracing::warn!("short video frame header");
                    continue;
                }
                let key_frame = payload[0] != 0;
                let timestamp_us = u64::from_le_bytes(payload[1..9].try_into().unwrap());
                let frame = EncodedFrame {
                    data: payload[9..].to_vec(),
                    key_frame,
                    timestamp_us,
                };
                // Video must not block a slow consumer — but a delta frame is
                // decoded against the one before it, so quietly skipping one
                // corrupts every frame after it until the next keyframe, which
                // is what tearing and black bands across the picture actually
                // are. Once we have to drop, keep dropping until a keyframe
                // gives a clean restart, and ask for one straight away rather
                // than waiting out the encoder's own interval.
                if awaiting_key {
                    if !frame.key_frame {
                        continue;
                    }
                    awaiting_key = false;
                }
                match video_tx.try_send(frame) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        awaiting_key = true;
                        let _ = wire.write_message(CH_KEYFRAME_REQ, &[]).await;
                        continue;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            Ok((CH_KEEPALIVE, payload)) => {
                // Echo pings straight back; measure RTT on the echo.
                if payload.first() == Some(&0) && payload.len() == 9 {
                    let mut echo = payload.clone();
                    echo[0] = 1;
                    let _ = wire.write_message(CH_KEEPALIVE, &echo).await;
                } else if payload.first() == Some(&1) && payload.len() == 9 {
                    let sent = u64::from_le_bytes(payload[1..9].try_into().unwrap());
                    let now = now_micros();
                    let rtt = now.saturating_sub(sent) as f64 / 1000.0; // ms
                                                                        // EWMA, fixed point ×100
                    let prev = rtt_ms.load(Ordering::Relaxed) as f64 / 100.0;
                    let next = if prev == 0.0 {
                        rtt
                    } else {
                        prev * 0.8 + rtt * 0.2
                    };
                    rtt_ms.store((next * 100.0) as u32, Ordering::Relaxed);
                }
            }
            Ok((CH_KEYFRAME_REQ, _)) => key_frame_req.store(true, Ordering::Release),
            // Sound is played by the browser client; the native viewer doesn't
            // play it (yet), so it's dropped here rather than logged.
            Ok((CH_AUDIO, _)) => {}
            Ok((other, _)) => tracing::warn!(channel = other, "unknown channel"),
            Err(e) => {
                tracing::info!(error = %e, "reader loop ending");
                break;
            }
        }
    }
    closed_flag.store(true, Ordering::Release);
    closed.notify_waiters();
}

async fn keepalive_loop(wire: Arc<Wire>) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tick.tick().await;
        let mut msg = Vec::with_capacity(9);
        msg.push(0u8); // ping
        msg.extend_from_slice(&now_micros().to_le_bytes());
        if wire.write_message(CH_KEEPALIVE, &msg).await.is_err() {
            break;
        }
    }
}

fn now_micros() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

// ── listener ─────────────────────────────────────────────────────────────────

/// Accepts one connection at a time as the host (responder). The pairing code
/// can be rotated without rebinding the socket.
pub struct LanListener {
    listener: TcpListener,
    pairing_code: std::sync::Mutex<String>,
    local_static: [u8; 32],
}

impl LanListener {
    pub async fn bind<A: ToSocketAddrs>(
        addr: A,
        pairing_code: impl Into<String>,
        local_static: [u8; 32],
    ) -> TResult<Self> {
        Ok(Self {
            listener: TcpListener::bind(addr).await.map_err(err)?,
            pairing_code: std::sync::Mutex::new(pairing_code.into()),
            local_static,
        })
    }

    pub fn local_addr(&self) -> TResult<std::net::SocketAddr> {
        self.listener.local_addr().map_err(err)
    }

    /// Replace the code accepted for a *first* pairing. Reconnects by a paired
    /// device don't use it.
    pub fn set_pairing_code(&self, code: impl Into<String>) {
        *self.pairing_code.lock().unwrap() = code.into();
    }

    /// Wait for a peer and complete the handshake. A wrong code (on a first
    /// pairing) or an unknown resume fails here with
    /// [`TransportError::Handshake`].
    pub async fn accept(&self) -> TResult<(LanSession, std::net::SocketAddr)> {
        let (stream, peer) = self.listener.accept().await.map_err(err)?;
        stream.set_nodelay(true).ok();
        let code = self.pairing_code.lock().unwrap().clone();
        let session =
            LanSession::over_stream_responder(stream, &self.local_static, &code).await?;
        Ok((session, peer))
    }
}
