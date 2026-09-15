//! Client of the rendezvous relay ([`rc-relay`](../../rc_relay/index.html)).
//!
//! These functions do the plaintext preamble only. The returned [`TcpStream`]
//! is then handed to [`crate::lan::LanSession::over_stream`], which runs the
//! end-to-end Noise handshake the relay can't see.

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio_tungstenite::tungstenite::Message;

use rc_protocol::relay::{Hello, ACK_BAD_KEY, ACK_BUSY, ACK_HOST_OFFLINE, ACK_OK};

use crate::ws_io::WsIo;
use crate::{Result as TResult, TransportError};

/// Object-safe union of the traits a spliced connection needs, so both a raw
/// `TcpStream` and a [`WsIo`]-wrapped WebSocket can be boxed to the same type.
pub trait AsyncIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AsyncIo for T {}

/// A relay-spliced connection, whichever transport reached it: a raw TCP
/// socket to a self-hosted `rc-relay`, or a WebSocket to one that only speaks
/// HTTP (a Cloudflare Worker, say). [`crate::lan::LanSession`] doesn't care
/// which — it only needs `AsyncRead + AsyncWrite`.
pub type BoxedIo = Box<dyn AsyncIo>;

/// Why a client couldn't reach a host through the relay — mapped to the spec's
/// user-facing connection messages.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("the PC is offline")]
    HostOffline,
    #[error("the relay rejected the access key")]
    BadKey,
    #[error("the host is already in a session")]
    Busy,
    #[error("relay unavailable: {0}")]
    Unavailable(String),
}

async fn send_hello(stream: &mut TcpStream, hello: &Hello) -> TResult<()> {
    let bytes = rc_protocol::encode(hello);
    stream.write_u32(bytes.len() as u32).await.map_err(io)?;
    stream.write_all(&bytes).await.map_err(io)?;
    stream.flush().await.map_err(io)?;
    Ok(())
}

fn io<E: std::fmt::Display>(e: E) -> TransportError {
    TransportError::Other(e.to_string())
}

/// Park a connection at the relay as a host and block until a client arrives.
/// Returns the spliced stream (ready for `LanSession::over_stream(.., false)`).
pub async fn park_as_host<A: ToSocketAddrs>(
    relay: A,
    device_id: &str,
    key: Option<String>,
) -> TResult<TcpStream> {
    let mut stream = TcpStream::connect(relay).await.map_err(io)?;
    stream.set_nodelay(true).ok();
    send_hello(
        &mut stream,
        &Hello::Host {
            device_id: device_id.to_string(),
            key,
        },
    )
    .await?;

    // Blocks (possibly for minutes) until the relay splices us to a client.
    let ack = stream.read_u8().await.map_err(io)?;
    match ack {
        ACK_OK => Ok(stream),
        ACK_BAD_KEY => Err(TransportError::Handshake(
            "relay rejected the access key".into(),
        )),
        other => Err(io(format!("relay sent unexpected ack {other} to a host"))),
    }
}

/// Connect through the relay as a client to the host parked under `device_id`.
pub async fn connect_as_client<A: ToSocketAddrs>(
    relay: A,
    device_id: &str,
    key: Option<String>,
) -> std::result::Result<TcpStream, RelayError> {
    let mut stream = TcpStream::connect(relay)
        .await
        .map_err(|e| RelayError::Unavailable(e.to_string()))?;
    stream.set_nodelay(true).ok();
    send_hello(
        &mut stream,
        &Hello::Client {
            device_id: device_id.to_string(),
            key,
        },
    )
    .await
    .map_err(|e| RelayError::Unavailable(e.to_string()))?;

    let ack = stream
        .read_u8()
        .await
        .map_err(|e| RelayError::Unavailable(e.to_string()))?;
    match ack {
        ACK_OK => Ok(stream),
        ACK_HOST_OFFLINE => Err(RelayError::HostOffline),
        ACK_BAD_KEY => Err(RelayError::BadKey),
        ACK_BUSY => Err(RelayError::Busy),
        other => Err(RelayError::Unavailable(format!("unexpected ack {other}"))),
    }
}

/// `rustls` 0.23 needs a process-wide crypto backend picked explicitly before
/// the first `wss://` connection — otherwise it panics rather than guessing.
/// Idempotent and cheap, so every WS dial-out just calls it first.
fn ensure_tls_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Park at a relay that only speaks WebSocket (e.g. a Cloudflare Worker) —
/// same protocol as [`park_as_host`], just carried over `wss://`/`ws://`
/// instead of a raw socket: the Hello is the first WS binary message (no
/// length prefix; the WS framing already delimits it), the ACK is the next
/// one, and everything after is forwarded as opaque binary messages.
///
/// Blocks (possibly for minutes) until the relay splices in a client.
/// A parked host can sit idle for minutes or hours waiting for a client, and
/// plenty of infrastructure between here and the relay (Cloudflare's own edge
/// included) will silently drop a connection that goes quiet for too long —
/// no close frame, no error, `poll_next` just never wakes again. These keep
/// the connection both alive (a proxy doesn't time out something that's
/// actively chattering) and *provably* alive (if a ping goes unanswered, we
/// know to give up and let the caller reconnect, instead of believing we're
/// still parked for hours after the relay forgot about us).
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
const PONG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub async fn park_as_host_ws(url: &str, device_id: &str, key: Option<String>) -> TResult<BoxedIo> {
    ensure_tls_provider();
    // `disable_nagle: true` — every message on this connection (mouse/
    // keyboard input included, once spliced) is latency-sensitive; without
    // it Nagle's algorithm can hold a small write for up to ~40ms waiting to
    // coalesce, which read as sluggish input on top of whatever the network
    // itself adds.
    let (mut ws, _) = tokio_tungstenite::connect_async_with_config(url, None, true)
        .await
        .map_err(io)?;
    let hello = rc_protocol::encode(&Hello::Host {
        device_id: device_id.to_string(),
        key,
    });
    ws.send(Message::Binary(hello)).await.map_err(io)?;

    loop {
        match tokio::time::timeout(PING_INTERVAL, ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) if !b.is_empty() => {
                return match b[0] {
                    ACK_OK => Ok(Box::new(WsIo::new(ws))),
                    ACK_BAD_KEY => Err(TransportError::Handshake(
                        "relay rejected the access key".into(),
                    )),
                    other => Err(io(format!("relay sent unexpected ack {other} to a host"))),
                };
            }
            Ok(Some(Ok(_))) => continue, // any other frame counts as activity
            Ok(Some(Err(e))) => return Err(io(e)),
            Ok(None) => return Err(io("relay closed the connection before acking")),
            Err(_elapsed) => {
                // Quiet for PING_INTERVAL — probe liveness rather than trust
                // a connection that might already be a silent black hole.
                if ws.send(Message::Ping(Vec::new())).await.is_err() {
                    return Err(io("relay connection is dead (ping failed)"));
                }
                match tokio::time::timeout(PONG_TIMEOUT, ws.next()).await {
                    Ok(Some(Ok(Message::Binary(b)))) if !b.is_empty() => {
                        return match b[0] {
                            ACK_OK => Ok(Box::new(WsIo::new(ws))),
                            ACK_BAD_KEY => Err(TransportError::Handshake(
                                "relay rejected the access key".into(),
                            )),
                            other => {
                                Err(io(format!("relay sent unexpected ack {other} to a host")))
                            }
                        };
                    }
                    Ok(Some(Ok(_))) => continue, // pong (or anything) — still alive
                    Ok(Some(Err(e))) => return Err(io(e)),
                    Ok(None) => return Err(io("relay closed the connection before acking")),
                    Err(_) => return Err(io("relay connection went silent (no pong)")),
                }
            }
        }
    }
}

/// Connect through a WebSocket-only relay as a client (mirrors
/// [`connect_as_client`]). Not currently used by any first-party client —
/// the web client speaks WebSocket natively from `rc-web`'s own WASM code,
/// and `rc-desktop-client` connects to self-hosted relays over raw TCP — but
/// kept symmetric for a future native client that wants a Worker relay too.
pub async fn connect_as_client_ws(
    url: &str,
    device_id: &str,
    key: Option<String>,
) -> std::result::Result<BoxedIo, RelayError> {
    ensure_tls_provider();
    let (mut ws, _) = tokio_tungstenite::connect_async_with_config(url, None, true)
        .await
        .map_err(|e| RelayError::Unavailable(e.to_string()))?;
    let hello = rc_protocol::encode(&Hello::Client {
        device_id: device_id.to_string(),
        key,
    });
    ws.send(Message::Binary(hello))
        .await
        .map_err(|e| RelayError::Unavailable(e.to_string()))?;

    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) if !b.is_empty() => {
                return match b[0] {
                    ACK_OK => Ok(Box::new(WsIo::new(ws))),
                    ACK_HOST_OFFLINE => Err(RelayError::HostOffline),
                    ACK_BAD_KEY => Err(RelayError::BadKey),
                    ACK_BUSY => Err(RelayError::Busy),
                    other => Err(RelayError::Unavailable(format!("unexpected ack {other}"))),
                };
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(RelayError::Unavailable(e.to_string())),
            None => return Err(RelayError::Unavailable("connection closed before ack".into())),
        }
    }
}
