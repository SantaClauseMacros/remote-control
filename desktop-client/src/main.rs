//! Remote Control — desktop client.
//!
//! Connect to a host using just its PC ID, show its screen in a window, and
//! forward this window's mouse/keyboard to it.
//!
//! ```text
//! rc-desktop-client --list                               # discover hosts on this network
//! rc-desktop-client --relay HOST:PORT ABCDE-FGHIJ-KLMNO-P # by PC ID, any network
//! rc-desktop-client "My Gaming PC"                       # by name on the LAN (ID via mDNS)
//! rc-desktop-client 192.168.1.20:9877 ABCDE-FGHIJ-KLMNO-P
//! rc-desktop-client                                      # prompts
//! ```

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod store;
mod viewer;

use std::io::Write;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rc_decode::StreamDecoder;
use rc_protocol::{ClientMessage, HostMessage};
use rc_transport::lan::{ClientAuth, LanSession};
use rc_transport::Session;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use viewer::{FrameBuf, ViewerState};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RC_LOG")
                // mdns-sd logs a spurious error while its daemon winds down.
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,mdns_sd=off")),
        )
        .with_target(false)
        .init();

    // args: [--relay ADDR] (--list | <target> [code])
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let mut relay: Option<String> = std::env::var("RC_RELAY").ok();
    if let Some(i) = argv.iter().position(|a| a == "--relay") {
        relay = argv.get(i + 1).cloned();
        argv.drain(i..=(i + 1).min(argv.len() - 1));
    }
    let mut args = argv.into_iter();
    let first = args.next();

    if first.as_deref() == Some("--list") {
        println!("Searching the local network for 3s…\n");
        match rc_discovery::discover(std::time::Duration::from_secs(3)) {
            Ok(hosts) if !hosts.is_empty() => {
                for h in hosts {
                    let id = h.device_id.filter(|s| !s.is_empty()).unwrap_or_default();
                    println!("  {:<22} {:<22} {}", h.name, h.addr, id);
                }
            }
            Ok(_) => println!("  (no hosts found — is the host running with LAN discovery on?)"),
            Err(e) => eprintln!("  discovery failed: {e}"),
        }
        return Ok(());
    }

    let target = match first {
        Some(a) => a,
        None => prompt("Host address, name, or device id: ")?,
    };

    // Decide how to reach the host, and the key under which we pin its identity.
    let (connect_to, pin_key): (Target, String) = if looks_like_device_id(&target) {
        match &relay {
            Some(r) => (
                Target::Relay {
                    relay: r.clone(),
                    device_id: target.clone(),
                    key: std::env::var("RC_RELAY_KEY").ok(),
                },
                target.to_uppercase(),
            ),
            None => {
                eprintln!("\"{target}\" is a device id — pass --relay <host:port> (or set RC_RELAY) to reach it across networks.");
                return Ok(());
            }
        }
    } else if looks_like_addr(&target) {
        let addr = if target.contains(':') { target.clone() } else { format!("{target}:9877") };
        (Target::Direct(addr.clone()), addr)
    } else {
        println!("Looking up \"{target}\" on the network…");
        match rc_discovery::discover(std::time::Duration::from_secs(3)) {
            Ok(hosts) => match hosts.iter().find(|h| h.name.eq_ignore_ascii_case(&target)) {
                Some(h) => {
                    // Pin by the host's device id if it advertised one, so a
                    // changing LAN IP doesn't lose the pairing.
                    let pk = h.device_id.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| h.addr.to_string());
                    (Target::Direct(h.addr.to_string()), pk)
                }
                None => { eprintln!("No host named \"{target}\" found on this network."); return Ok(()); }
            },
            Err(e) => { eprintln!("Discovery failed: {e}"); return Ok(()); }
        }
    };

    // The PC ID is all that's needed to connect. It's the target itself for a
    // relay connection, advertised over mDNS for a name, and asked for when
    // connecting to a bare IP address.
    let static_sk = store::load_identity().context("client identity")?;
    let mut pinned = store::PinnedHosts::load();
    let pc_id = if looks_like_device_id(&pin_key) {
        pin_key.clone()
    } else {
        match args.next() {
            Some(c) => c,
            None => prompt("PC ID: ")?,
        }
    };
    let auth = ClientAuth::Pair { code: rc_crypto::canonical_device_id(&pc_id) };
    let pinned_key = pinned.get(&pin_key);

    rc_input::set_dpi_aware();
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }

    let (input_tx, input_rx) = unbounded_channel::<ClientMessage>();
    let state = Arc::new(ViewerState {
        input_tx,
        frame: Mutex::new(None),
    });

    // Networking + decode run on a background Tokio runtime.
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
    let bg_state = state.clone();
    std::thread::Builder::new()
        .name("rc-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("build client runtime");
            rt.block_on(async move {
                match dial(connect_to, &static_sk, auth).await {
                    Ok(session) if pinned_key.is_some_and(|k| k != session.peer_key()) => {
                        drop(session);
                        let _ = ready_tx.send(Err(anyhow!(
                            "the PC presented a different identity key — not connecting"
                        )));
                    }
                    Ok(session) => {
                        pinned.set(&pin_key, &session.peer_key());
                        let _ = ready_tx.send(Ok(()));
                        run_session(session, bg_state, input_rx).await;
                        viewer::post_close();
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            });
        })
        .context("spawn net thread")?;

    match ready_rx.recv() {
        Ok(Ok(())) => tracing::info!("connected; opening viewer"),
        Ok(Err(e)) => {
            eprintln!("\nCould not connect: {e}");
            return Ok(());
        }
        Err(_) => return Err(anyhow!("net thread died")),
    }

    viewer::run(state)
}

async fn run_session(
    session: LanSession,
    state: Arc<ViewerState>,
    mut input_rx: UnboundedReceiver<ClientMessage>,
) {
    let session = Arc::new(session);

    // Local input → host.
    let s_in = session.clone();
    tokio::spawn(async move {
        while let Some(msg) = input_rx.recv().await {
            if s_in
                .control()
                .send(rc_protocol::encode(&msg))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Clipboard: local changes → host.
    let clip = Arc::new(std::sync::Mutex::new(rc_clipboard::ClipboardWatcher::new()));
    let (clip_tx, mut clip_rx) = unbounded_channel::<String>();
    {
        let clip = clip.clone();
        std::thread::Builder::new()
            .name("rc-clipboard".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let next = clip.lock().ok().and_then(|mut w| w.poll());
                if let Some(text) = next {
                    if clip_tx.send(text).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }
    let s_clip = session.clone();
    tokio::spawn(async move {
        while let Some(text) = clip_rx.recv().await {
            if s_clip
                .control()
                .send(rc_protocol::encode(&ClientMessage::ClipboardText(text)))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Host control messages.
    let s_ctl = session.clone();
    let ctl_clip = clip.clone();
    tokio::spawn(async move {
        while let Ok(bytes) = s_ctl.control().recv().await {
            match rc_protocol::decode::<HostMessage>(&bytes) {
                Ok(HostMessage::Displays(d)) => {
                    if let Some(p) = d.first() {
                        viewer::set_title(&format!("Remote Control — {}×{}", p.width, p.height));
                    }
                }
                Ok(HostMessage::ClipboardText(text)) => {
                    if let Ok(mut w) = ctl_clip.lock() {
                        w.note_local_set(&text);
                    }
                    if let Err(e) = rc_clipboard::set_text(&text) {
                        tracing::warn!(error = %e, "applying host clipboard failed");
                    }
                }
                Ok(HostMessage::Disconnect { reason }) => {
                    tracing::warn!(%reason, "host ended the session");
                    break;
                }
                Ok(HostMessage::Notice(n)) => tracing::info!(%n, "host notice"),
                Ok(HostMessage::CursorCaptured(on)) => viewer::set_captured(on),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "bad host message"),
            }
        }
        viewer::post_close();
    });

    // Video → decoder thread → viewer.
    let (raw_tx, raw_rx) = std_mpsc::channel::<Vec<u8>>();
    let dec_state = state.clone();
    std::thread::Builder::new()
        .name("rc-decode".into())
        .spawn(move || decode_thread(raw_rx, dec_state))
        .ok();

    while let Ok(frame) = session.video().recv().await {
        if raw_tx.send(frame.data).is_err() {
            break;
        }
    }
}

fn decode_thread(rx: std_mpsc::Receiver<Vec<u8>>, state: Arc<ViewerState>) {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    let mut dec = match StreamDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "cannot start H.264 decoder");
            return;
        }
    };
    // Optional: dump the Nth decoded frame to a BMP for verification.
    let dump_path = std::env::var("RC_DUMP_FRAME").ok();
    let mut decoded = 0u64;

    while let Ok(annexb) = rx.recv() {
        match dec.decode(&annexb) {
            Ok(Some(f)) => {
                decoded += 1;
                if let Some(p) = &dump_path {
                    if decoded == 30 {
                        if let Err(e) = write_bmp(p, f.width, f.height, &f.bgra) {
                            tracing::warn!(error = %e, "frame dump failed");
                        } else {
                            tracing::info!(path = %p, "dumped decoded frame 30");
                        }
                    }
                }
                *state.frame.lock().unwrap() = Some(FrameBuf {
                    w: f.width as i32,
                    h: f.height as i32,
                    bgra: f.bgra,
                });
                viewer::request_repaint();
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "decode error"),
        }
    }
}

fn write_bmp(path: &str, w: u32, h: u32, bgra: &[u8]) -> std::io::Result<()> {
    let px = (w * h * 4) as usize;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"BM")?;
    f.write_all(&(54 + px as u32).to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&(w as i32).to_le_bytes())?;
    f.write_all(&(-(h as i32)).to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&(px as u32).to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&bgra[..px])?;
    Ok(())
}

enum Target {
    Direct(String),
    Relay {
        relay: String,
        device_id: String,
        key: Option<String>,
    },
}

async fn dial(target: Target, static_sk: &[u8; 32], auth: ClientAuth) -> Result<LanSession> {
    match target {
        Target::Direct(addr) => LanSession::connect(&addr, static_sk, auth)
            .await
            .map_err(|e| anyhow!("{e}")),
        Target::Relay { relay, device_id, key } => {
            let stream = rc_transport::relay::connect_as_client(&relay, &device_id, key)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            LanSession::over_stream_initiator(stream, static_sk, auth)
                .await
                .map_err(|e| anyhow!("{e}"))
        }
    }
}

/// A device id looks like `XXXXX-XXXXX-XXXXX-X` (RFC-4648 base32).
fn looks_like_device_id(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 4
        && parts[..3].iter().all(|p| p.len() == 5)
        && parts[3].len() == 1
        && s.chars()
            .all(|c| c == '-' || c.is_ascii_uppercase() || ('2'..='7').contains(&c))
}

/// Heuristic: does this look like `1.2.3.4[:port]` or `host:port`, vs. a
/// friendly name like "My Gaming PC"?
fn looks_like_addr(s: &str) -> bool {
    let hostpart = s.rsplit_once(':').map(|(h, _)| h).unwrap_or(s);
    !hostpart.is_empty()
        && hostpart
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':')
        && !hostpart.contains(' ')
        && (s.contains(':') || hostpart.split('.').count() == 4)
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}
