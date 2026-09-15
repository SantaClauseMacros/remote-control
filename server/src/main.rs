//! Rendezvous relay for Remote Control.
//!
//! A host with no reachable inbound port dials **out** to this server and parks
//! a connection under its device id. A client dials the server and asks for that
//! id; the server writes one status byte to each side and then does nothing but
//! `copy_bidirectional` between the two sockets. Everything it forwards is
//! already end-to-end encrypted with a Noise key derived from the pairing code,
//! which the relay never sees — so a hostile or compromised relay can drop or
//! delay traffic but cannot read or inject it.
//!
//! ```text
//! rc-relay --bind 0.0.0.0:9878 [--key SHARED_SECRET] [--max-conns 512]
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rc_protocol::relay::{Hello, ACK_BAD_KEY, ACK_BUSY, ACK_HOST_OFFLINE, ACK_OK, DEFAULT_PORT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

/// A host connection waiting to be matched.
struct Parked {
    stream: TcpStream,
    since: Instant,
    /// Held so a parked host counts against `--max-conns`.
    _permit: OwnedSemaphorePermit,
}

type Waiting = Arc<Mutex<HashMap<String, Parked>>>;

struct Config {
    bind: String,
    key: Option<String>,
    max_conns: usize,
}

fn parse_args() -> Config {
    let mut cfg = Config {
        bind: format!("0.0.0.0:{DEFAULT_PORT}"),
        key: None,
        max_conns: 512,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => cfg.bind = args.next().unwrap_or(cfg.bind),
            "--key" => cfg.key = args.next(),
            "--max-conns" => {
                cfg.max_conns = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(cfg.max_conns)
            }
            "-h" | "--help" => {
                eprintln!("rc-relay --bind ADDR [--key SECRET] [--max-conns N]");
                std::process::exit(0);
            }
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }
    cfg
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RC_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Arc::new(parse_args());
    let listener = TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("binding {}", cfg.bind))?;
    tracing::info!(bind = %cfg.bind, auth = cfg.key.is_some(), "relay listening");

    let waiting: Waiting = Arc::new(Mutex::new(HashMap::new()));
    let slots = Arc::new(Semaphore::new(cfg.max_conns));

    // Evict stale parked hosts (dead NAT mappings) every minute.
    {
        let waiting = waiting.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let mut map = waiting.lock().await;
                map.retain(|id, p| {
                    let fresh = p.since.elapsed() < Duration::from_secs(15 * 60);
                    if !fresh {
                        tracing::info!(device_id = %id, "evicting stale parked host");
                    }
                    fresh
                });
            }
        });
    }

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let permit = match slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(%peer, "connection limit reached; dropping");
                continue;
            }
        };
        let cfg = cfg.clone();
        let waiting = waiting.clone();
        tokio::spawn(async move {
            let res = match is_websocket(&stream).await {
                Ok(true) => handle_ws(stream, permit, &cfg, &waiting).await,
                Ok(false) => handle(stream, permit, &cfg, &waiting).await,
                Err(e) => Err(e),
            };
            if let Err(e) = res {
                tracing::debug!(%peer, error = %e, "connection ended");
            }
        });
    }
}

/// Peek the first bytes: an HTTP/WebSocket handshake starts with `GET `.
async fn is_websocket(stream: &TcpStream) -> Result<bool> {
    let mut buf = [0u8; 4];
    // A native peer sends a 4-byte length prefix first, never "GET ".
    let n = tokio::time::timeout(Duration::from_secs(10), stream.peek(&mut buf))
        .await
        .context("client sent nothing")??;
    Ok(n >= 4 && &buf == b"GET ")
}

async fn read_hello(stream: &mut TcpStream) -> Result<Hello> {
    let len = tokio::time::timeout(Duration::from_secs(10), stream.read_u32())
        .await
        .context("hello timed out")??;
    anyhow::ensure!(len <= 4096, "hello too large");
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    rc_protocol::decode::<Hello>(&buf).context("decoding hello")
}

fn key_ok(cfg: &Config, provided: &Option<String>) -> bool {
    match &cfg.key {
        None => true,
        Some(want) => provided.as_deref() == Some(want.as_str()),
    }
}

async fn handle(
    mut stream: TcpStream,
    permit: OwnedSemaphorePermit,
    cfg: &Config,
    waiting: &Waiting,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    let hello = read_hello(&mut stream).await?;

    match hello {
        Hello::Host { device_id, key } => {
            if !key_ok(cfg, &key) {
                stream.write_all(&[ACK_BAD_KEY]).await.ok();
                return Ok(());
            }
            let mut map = waiting.lock().await;
            let replaced = map
                .insert(
                    device_id.clone(),
                    Parked {
                        stream,
                        since: Instant::now(),
                        _permit: permit,
                    },
                )
                .is_some();
            tracing::info!(device_id = %device_id, replaced, parked = map.len(), "host parked");
            // The stream now lives in the map; a Client handler will take it.
            Ok(())
        }
        Hello::Query { device_id, key } => {
            let ack = if !key_ok(cfg, &key) {
                ACK_BAD_KEY
            } else if waiting.lock().await.contains_key(&device_id) {
                ACK_OK
            } else {
                ACK_HOST_OFFLINE
            };
            stream.write_all(&[ack]).await.ok();
            Ok(())
        }
        Hello::Client { device_id, key } => {
            if !key_ok(cfg, &key) {
                stream.write_all(&[ACK_BAD_KEY]).await.ok();
                return Ok(());
            }
            let parked = waiting.lock().await.remove(&device_id);
            let Some(Parked {
                stream: mut host,
                _permit: host_permit,
                ..
            }) = parked
            else {
                stream.write_all(&[ACK_HOST_OFFLINE]).await.ok();
                tracing::info!(device_id = %device_id, "client asked for an offline host");
                return Ok(());
            };
            let _keep = (permit, host_permit); // both slots held for the session

            // Green-light both ends, then become a dumb pipe.
            if host.write_all(&[ACK_OK]).await.is_err() {
                // Parked host was actually dead.
                stream.write_all(&[ACK_HOST_OFFLINE]).await.ok();
                return Ok(());
            }
            stream.write_all(&[ACK_OK]).await?;
            tracing::info!(device_id = %device_id, "spliced host ↔ client");

            match tokio::io::copy_bidirectional(&mut host, &mut stream).await {
                Ok((a, b)) => {
                    tracing::info!(device_id = %device_id, up = a, down = b, "session closed")
                }
                Err(e) => tracing::debug!(device_id = %device_id, error = %e, "session error"),
            }
            let _ = ACK_BUSY; // reserved for a future "host already in a session" reply
            Ok(())
        }
    }
}

/// A browser client speaks WebSocket. The first WS binary message is the
/// (unframed) `Hello::Client` postcard; the ACK is a 1-byte WS binary message;
/// after that every WS binary message is forwarded verbatim to/from the host's
/// raw stream — the relay still only sees ciphertext.
async fn handle_ws(
    stream: TcpStream,
    permit: OwnedSemaphorePermit,
    cfg: &Config,
    waiting: &Waiting,
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    stream.set_nodelay(true).ok();
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .context("websocket upgrade")?;

    // First message: the client Hello (no length prefix — WS frames it).
    let first = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .context("ws hello timed out")?
        .context("ws closed before hello")?
        .context("ws hello error")?;
    let hello_bytes = match first {
        Message::Binary(b) => b,
        _ => anyhow::bail!("ws hello was not binary"),
    };
    let (device_id, key, query_only) = match rc_protocol::decode::<Hello>(&hello_bytes)
        .context("decode ws hello")?
    {
        Hello::Client { device_id, key } => (device_id, key, false),
        Hello::Query { device_id, key } => (device_id, key, true),
        Hello::Host { .. } => anyhow::bail!("a browser client cannot register as a host"),
    };

    if !key_ok(cfg, &key) {
        ws.send(Message::Binary(vec![ACK_BAD_KEY])).await.ok();
        return Ok(());
    }

    if query_only {
        let ack = if waiting.lock().await.contains_key(&device_id) {
            ACK_OK
        } else {
            ACK_HOST_OFFLINE
        };
        ws.send(Message::Binary(vec![ack])).await.ok();
        return Ok(());
    }

    let parked = waiting.lock().await.remove(&device_id);
    let Some(Parked { stream: mut host, _permit: host_permit, .. }) = parked else {
        ws.send(Message::Binary(vec![ACK_HOST_OFFLINE])).await.ok();
        tracing::info!(device_id = %device_id, "ws client asked for an offline host");
        return Ok(());
    };
    let _keep = (permit, host_permit);

    if host.write_all(&[ACK_OK]).await.is_err() {
        ws.send(Message::Binary(vec![ACK_HOST_OFFLINE])).await.ok();
        return Ok(());
    }
    ws.send(Message::Binary(vec![ACK_OK])).await?;
    tracing::info!(device_id = %device_id, "spliced host ↔ ws client");

    let (mut host_rd, mut host_wr) = host.split();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            r = host_rd.read(&mut buf) => match r {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws.send(Message::Binary(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            },
            m = ws.next() => match m {
                Some(Ok(Message::Binary(b))) => {
                    if host_wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}         // ignore text/ping/pong
                Some(Err(_)) => break,
            },
        }
    }
    tracing::info!(device_id = %device_id, "ws session closed");
    Ok(())
}
