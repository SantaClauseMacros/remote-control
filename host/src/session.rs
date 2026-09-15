//! One connected remote-control session: capture → encode → send video,
//! receive control → inject input, and (optionally) sync clipboard text.
//!
//! The "require confirmation" prompt is still to come; the GPU-only
//! capture→encode path is milestone 6.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rc_capture::{Capturer, D3dContext, DesktopRect, Grab};
use rc_clipboard::ClipboardWatcher;
use rc_encode::{EncodeConfig, StreamEncoder};
use rc_input::{Injector, Rect};
use rc_protocol::{ClientMessage, DisplayInfo, HostMessage, InputEvent, QualityMode};
use rc_transport::lan::LanSession;
use rc_transport::{EncodedFrame, Session};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};

use crate::display::{self, MultiMonitorMode};
use crate::gamepad::GamepadHub;

/// Host-initiated actions a running session accepts from outside its own
/// control loop — right now just "send this file to the connected device"
/// (the dashboard's "Send file to phone").
pub enum SessionCommand {
    SendFile(PathBuf),
}

/// Reject a device→PC file offer larger than this outright, before any of it
/// is written to disk.
const MAX_INCOMING_FILE: u64 = 500 * 1024 * 1024;

/// One file transfer in progress from the connected device.
struct IncomingFile {
    file: std::fs::File,
    name: String,
    path: PathBuf,
    written: u64,
    total: u64,
}

/// `%USERPROFILE%\Downloads\RemoteControl` — created on first use.
fn incoming_files_dir() -> Result<PathBuf> {
    let base = std::env::var_os("USERPROFILE").context("no USERPROFILE")?;
    let dir = PathBuf::from(base).join("Downloads").join("RemoteControl");
    std::fs::create_dir_all(&dir).context("creating the downloads folder")?;
    Ok(dir)
}

/// A path under `dir` for `name` that doesn't already exist — `name` itself
/// stripped to just its file name (no directories a malicious/buggy client
/// could use to escape `dir`), with " (2)", " (3)", … appended on collision.
fn unique_download_path(dir: &Path, name: &str) -> PathBuf {
    let leaf = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("file");
    let leaf_path = Path::new(leaf);
    let stem = leaf_path.file_stem().and_then(|s| s.to_str()).unwrap_or("file").to_string();
    let ext = leaf_path.extension().and_then(|s| s.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    let mut candidate = dir.join(leaf);
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    candidate
}

/// Tunables for a session, taken from host settings. (Not `Debug`: it holds
/// the host's secret key.)
#[derive(Clone)]
pub struct SessionParams {
    pub fps: u32,
    /// `0` = let [`EncodeConfig::balanced`] pick from the resolution.
    pub max_bitrate_kbps: u32,
    /// Starting point for [`ClientMessage::SetQuality`] — the client can
    /// still change it mid-session (see `quality_config`).
    pub mode: QualityMode,
    /// Sync clipboard text in both directions.
    pub clipboard_sync: bool,
    /// Stream the PC's sound.
    pub audio: bool,
    /// Play the connected device's microphone on this PC (see `rc_audio::render`).
    pub mic: bool,
    /// Double the frame rate target once the session is on a direct path.
    pub uncap_fps_on_direct: bool,
    /// How to handle a PC with more than one monitor for this session.
    pub multi_monitor: MultiMonitorMode,
    /// This host's static key and PC ID — what a direct path's handshake needs.
    pub host_static: [u8; 32],
    pub device_id: String,
    /// The connected device's static key; a direct path must present the same.
    pub peer_key: [u8; 32],
}

/// Live numbers from a running session, shared with the dashboard. Written by
/// the session loop, read whenever the dashboard polls — atomics, so neither
/// side ever waits on the other.
#[derive(Debug, Default)]
pub struct LiveStats {
    pub frames_sent: AtomicU64,
    pub input_events: AtomicU64,
    /// Round-trip time in milliseconds × 100.
    pub rtt_ms_x100: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub hardware_encoder: AtomicBool,
    /// A game currently has the mouse captured.
    pub game_captured: AtomicBool,
    /// The session is on a direct same-network path rather than the relay.
    pub direct: AtomicBool,
}

/// The path a session's traffic takes. Starts as whatever the device connected
/// over (usually the relay) and can move to a direct path mid-session — and
/// back again if that drops. Senders fetch the current path for every message.
struct Link {
    current: StdMutex<Arc<LanSession>>,
}

impl Link {
    fn new(session: Arc<LanSession>) -> Self {
        Self {
            current: StdMutex::new(session),
        }
    }

    fn get(&self) -> Arc<LanSession> {
        self.current.lock().unwrap().clone()
    }

    fn set(&self, session: Arc<LanSession>) {
        *self.current.lock().unwrap() = session;
    }

    async fn send(&self, msg: &HostMessage) {
        let _ = self.get().control().send(rc_protocol::encode(msg)).await;
    }
}

/// Backpressure signals the capture thread checks before handing the encoder
/// another frame. Both exist because the only thing that reliably keeps a
/// remote desktop feeling live is *not encoding* what the link can't carry —
/// anything already encoded has to be delivered, since dropping it corrupts
/// every frame after it.
struct CaptureGate {
    /// Set while a frame is still on its way out (or more are queued behind
    /// it). Immediate and exact, but only reacts once the kernel's socket
    /// buffer is already full — which is what `throttle` is for.
    net_busy: AtomicBool,
    /// Submit at most one frame per `frame_dur × throttle`. Raised when
    /// round-trip time climbs above its own floor, which is the tell-tale of
    /// a queue building *somewhere* on the path — a router, the relay, a
    /// socket buffer we can't see. Waiting for `net_busy` alone would leave
    /// however many hundred milliseconds of video the send buffer happens to
    /// hold sitting in front of every new frame.
    throttle: AtomicU32,
}

/// Map a requested [`QualityMode`] onto encoder settings. Scales off the same
/// per-resolution "balanced" baseline the session starts with, fps included,
/// and never exceeds `admin_cap_kbps` (`0` = no host-configured cap) — that's
/// a hard limit from the host's own settings, not something a client's
/// quality request can raise.
fn quality_config(
    mode: QualityMode,
    width: u32,
    height: u32,
    base_fps: u32,
    admin_cap_kbps: u32,
) -> EncodeConfig {
    // Auto has no bandwidth/RTT-driven controller yet (that's the deferred
    // adaptive-bitrate work described in ARCHITECTURE.md) — it's Balanced
    // until that lands.
    let (fps, scale) = match mode {
        QualityMode::Low => (base_fps.min(30), 0.35),
        QualityMode::Balanced | QualityMode::Auto => (base_fps, 1.0),
        QualityMode::High => (base_fps, 1.6),
    };
    let mut cfg = EncodeConfig::balanced(width, height, fps);
    cfg.bitrate_bps = (cfg.bitrate_bps as f64 * scale) as u32;
    if admin_cap_kbps > 0 {
        cfg.bitrate_bps = cfg.bitrate_bps.min(admin_cap_kbps.saturating_mul(1000));
    }
    cfg
}

/// Run until the client disconnects or `stop` flips to `true`.
pub async fn run(
    session: LanSession,
    params: SessionParams,
    mut stop: watch::Receiver<bool>,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    stats: Arc<LiveStats>,
) -> Result<()> {
    // The path the device connected over, kept to fall back to if a direct
    // path is set up and later lost.
    let relay = Arc::new(session);
    let link = Arc::new(Link::new(relay.clone()));

    // A PC with more than one monitor: mirror or move windows for the
    // session, undone in the teardown block below.
    let (monitor_restore, monitor_warning) = display::apply(params.multi_monitor);
    if let Some(warning) = monitor_warning {
        link.send(&HostMessage::Notice(warning)).await;
    }

    // ── capture thread: reports geometry, then waits for the encoder ─────────
    let capture_stop = Arc::new(AtomicBool::new(false));
    let (geo_tx, geo_rx) = std_mpsc::channel::<Result<(u32, u32, DesktopRect)>>();
    // A shared, swappable slot rather than a one-shot handoff: `SetQuality`
    // needs to replace the running encoder out from under the capture loop
    // mid-session (see the `ClientAction::SetQuality` arm below).
    let (enc_tx, enc_rx) = std_mpsc::channel::<Arc<StdMutex<Arc<StreamEncoder>>>>();
    // Declared up here so the capture thread can take a handle; the video
    // forwarding task and the control loop below are what set it.
    let gate = Arc::new(CaptureGate {
        net_busy: AtomicBool::new(false),
        throttle: AtomicU32::new(1),
    });
    let cap_stop = capture_stop.clone();
    let cap_gate = gate.clone();
    // Read fresh every frame by the capture loop, so switching to a direct
    // path (see `uncap_fps_on_direct`) can raise the target without
    // restarting the thread.
    let target_fps = Arc::new(AtomicU32::new(params.fps.clamp(5, 240)));
    let cap_fps = target_fps.clone();
    let capture_thread = std::thread::Builder::new()
        .name("rc-capture".into())
        .spawn(move || capture_loop(&cap_fps, &geo_tx, &enc_rx, &cap_stop, &cap_gate))
        .context("spawn capture thread")?;

    let (width, height, rect) = geo_rx
        .recv()
        .context("capture thread exited before reporting geometry")??;
    tracing::info!(width, height, "session capture ready");
    stats.width.store(width, Ordering::Relaxed);
    stats.height.store(height, Ordering::Relaxed);

    // ── encoder ────────────────────────────────────────────────────────────
    let cfg = quality_config(params.mode, width, height, params.fps, params.max_bitrate_kbps);
    let (pkt_tx, mut pkt_rx) = tokio::sync::mpsc::unbounded_channel();
    let encoder = StreamEncoder::new(cfg, pkt_tx.clone()).context("start encoder")?;
    stats.hardware_encoder.store(encoder.is_hardware(), Ordering::Relaxed);
    let encoder_slot = Arc::new(StdMutex::new(Arc::new(encoder)));
    enc_tx.send(encoder_slot.clone()).ok();

    // ── video forwarding task ──────────────────────────────────────────────
    // `net_busy` is the whole overload story: true while a frame is still
    // going out (or more are already queued behind it), which tells the
    // capture thread to skip submitting — so under a slow link we simply
    // encode *fewer* frames of the current screen rather than building a
    // backlog of stale ones. Self-correcting with no counters to drift, and
    // nothing already encoded is ever discarded, which is what matters: a
    // dropped delta frame corrupts every frame after it until the next
    // keyframe.
    //
    // `resync` guards the one case where discarding *is* correct. Swapping
    // the encoder (a quality change, or a keyframe request) abandons a stream
    // mid-flight, and the outgoing encoder drains whatever the MFT still held
    // into this same channel as it shuts down — arriving after the new
    // encoder's first keyframe if the capture thread was holding a reference
    // when the swap happened. Those trailing frames belong to a sequence the
    // decoder is no longer following, so forwarding them tears the picture,
    // which makes the client ask for a keyframe, which swaps the encoder
    // again. Drop everything until the next keyframe instead: that's a clean
    // cut between two streams rather than a hole punched in one.
    let resync = Arc::new(AtomicBool::new(false));
    let sv = link.clone();
    let vc = stats.clone();
    let nb = gate.clone();
    let rs = resync.clone();
    let video_task = tokio::spawn(async move {
        while let Some(p) = pkt_rx.recv().await {
            if rs.load(Ordering::Acquire) {
                if !p.key_frame {
                    continue;
                }
                rs.store(false, Ordering::Release);
            }
            nb.net_busy.store(true, Ordering::Release);
            let frame = EncodedFrame {
                data: p.data,
                key_frame: p.key_frame,
                timestamp_us: p.timestamp_us,
            };
            let sent = sv.get().video().send(frame).await;
            // Caught up only once nothing else is already waiting behind us.
            nb.net_busy.store(!pkt_rx.is_empty(), Ordering::Release);
            if let Err(e) = sent {
                // Keep going: the control loop either moves the session back
                // to the relay or ends it.
                tracing::debug!(error = %e, "video send failed");
                continue;
            }
            vc.frames_sent.fetch_add(1, Ordering::Relaxed);
        }
        nb.net_busy.store(false, Ordering::Release);
        tracing::debug!("video forwarding task ending");
    });

    // ── tell the client about the display ──────────────────────────────────
    let displays = vec![DisplayInfo {
        id: 0,
        x: rect.x,
        y: rect.y,
        width,
        height,
        primary: true,
        scale: 1.0,
    }];
    link.send(&HostMessage::Displays(displays)).await;

    // ── clipboard sync ────────────────────────────────────────────────────
    let clipboard = params
        .clipboard_sync
        .then(|| Arc::new(StdMutex::new(ClipboardWatcher::new())));
    let clip_stop = Arc::new(AtomicBool::new(false));
    if let Some(watcher) = &clipboard {
        let (clip_tx, mut clip_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let w = watcher.clone();
        let cs = clip_stop.clone();
        std::thread::Builder::new()
            .name("rc-clipboard".into())
            .spawn(move || {
                while !cs.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(400));
                    if let Ok(mut w) = w.lock() {
                        if let Some(text) = w.poll() {
                            let _ = clip_tx.send(text);
                        }
                    }
                }
            })
            .ok();
        let sc = link.clone();
        tokio::spawn(async move {
            while let Some(text) = clip_rx.recv().await {
                tracing::debug!(len = text.len(), "host clipboard → client");
                sc.send(&HostMessage::ClipboardText(text)).await;
            }
        });
    }

    // ── sound ─────────────────────────────────────────────────────────────
    // Captured on its own thread (WASAPI blocks) and written to the session by
    // an async task. A full queue drops sound rather than delaying it: a gap is
    // a click, but a backlog is sound that trails behind the picture.
    let audio_stop = Arc::new(AtomicBool::new(false));
    if params.audio {
        let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let stop_flag = audio_stop.clone();
        std::thread::Builder::new()
            .name("rc-audio".into())
            .spawn(move || {
                let sent = rc_audio::stream_loopback(&stop_flag, |packet| {
                    let _ = audio_tx.try_send(packet);
                });
                if let Err(e) = sent {
                    tracing::warn!(error = ?e, "sound capture stopped");
                }
            })
            .ok();
        let sa = link.clone();
        tokio::spawn(async move {
            while let Some(packet) = audio_rx.recv().await {
                let _ = sa.get().send_audio(&packet).await;
            }
        });
    }

    // ── control loop ──────────────────────────────────────────────────────
    let mut injector = Injector::new();
    injector.set_source_rect(Rect {
        x: rect.x,
        y: rect.y,
        w: rect.width as i32,
        h: rect.height as i32,
    });

    let mut current_mode = params.mode;
    let mut stats_tick = tokio::time::interval(Duration::from_secs(5));
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Frequent enough that a client's keyframe request (see the arm below)
    // gets acted on promptly — a slow decoder recovery is as bad as not
    // recovering at all — and to keep the congestion check responsive.
    let mut keyframe_poll = tokio::time::interval(Duration::from_millis(300));
    keyframe_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Lowest RTT this link has shown us: the value with nothing queued on
    // the path. Everything above it is queueing delay we're causing.
    let mut rtt_floor = 0.0f32;

    // Recreate the encoder in place (swapped into `encoder_slot`, picked up
    // by the capture thread on its next frame). A brand-new encoder's first
    // output is always a keyframe, which is what both a quality change and a
    // client's keyframe request actually need — reuse the one mechanism for
    // both instead of teaching the encoder a separate "force an IDR" path.
    let recreate_encoder = |mode: QualityMode| -> Option<EncodeConfig> {
        let fps = target_fps.load(Ordering::Relaxed);
        let cfg = quality_config(mode, width, height, fps, params.max_bitrate_kbps);
        match StreamEncoder::new(cfg, pkt_tx.clone()) {
            Ok(new_encoder) => {
                stats.hardware_encoder.store(new_encoder.is_hardware(), Ordering::Relaxed);
                // Set before the swap: the outgoing encoder drains into the
                // shared channel as it's dropped, and none of that belongs to
                // the stream the client is about to follow. See `resync`.
                resync.store(true, Ordering::Release);
                *encoder_slot.lock().unwrap() = Arc::new(new_encoder);
                Some(cfg)
            }
            Err(e) => {
                tracing::warn!(error = ?e, "encoder recreate failed; keeping current encoder");
                None
            }
        }
    };

    // Whether a game has the cursor captured — polled rather than hooked, and
    // only while a session is live. Entering needs two polls in a row so a
    // cursor that merely blinks out (hide-while-typing) doesn't flip every
    // client into aim mode; leaving is immediate.
    let mut capture_poll = tokio::time::interval(Duration::from_millis(100));
    capture_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut captured = false;
    let mut capture_streak: u8 = 0;
    // Last `GameArea` sent, so it only goes out when the window moves.
    let mut last_area: Option<(f32, f32, f32, f32)> = None;

    // A direct path: a session handshaken over WebRTC that's waiting for the
    // device's "use this one" (`pending`), then carrying traffic (`on_direct`).
    let (direct_tx, mut direct_rx) = tokio::sync::mpsc::unbounded_channel::<Result<LanSession>>();
    let mut pending: Option<Arc<LanSession>> = None;
    let mut on_direct = false;

    // ── controller passthrough ────────────────────────────────────────────
    let mut gamepad = GamepadHub::new();

    // ── incoming files (device → PC) ──────────────────────────────────────
    let mut incoming_files: HashMap<u32, IncomingFile> = HashMap::new();

    // ── mic playback (device → PC) ─────────────────────────────────────────
    let mic_stop = Arc::new(AtomicBool::new(false));
    let (mic_render_tx, mic_render_rx) = std_mpsc::sync_channel::<rc_audio::render::MicPacket>(8);
    if params.mic {
        let ms = mic_stop.clone();
        std::thread::Builder::new()
            .name("rc-mic".into())
            .spawn(move || rc_audio::render_mic(&ms, &mic_render_rx))
            .ok();
    }

    let reason;
    loop {
        let release_due = injector.next_release_due();
        let cur = link.get();
        tokio::select! {
            r = cur.control().recv() => match r {
                Ok(bytes) => {
                    match handle_client_msg(&bytes, &mut injector, clipboard.as_ref(), &link, &mut gamepad, &mut incoming_files).await {
                        ClientAction::Input => {
                            stats.input_events.fetch_add(1, Ordering::Relaxed);
                        }
                        ClientAction::SetQuality(mode) => {
                            current_mode = mode;
                            if let Some(cfg) = recreate_encoder(mode) {
                                tracing::info!(
                                    ?mode,
                                    bitrate_kbps = cfg.bitrate_bps / 1000,
                                    fps = cfg.fps,
                                    "quality changed"
                                );
                            }
                        }
                        ClientAction::DirectOffer(sdp) => {
                            tokio::spawn(offer_direct(sdp, link.clone(), params.clone(), direct_tx.clone()));
                        }
                        ClientAction::None => {}
                    }
                }
                Err(e) => {
                    // The direct path dropped: carry on over the relay if it's
                    // still up, with a fresh keyframe for the switch.
                    if on_direct && !relay.is_closed() {
                        tracing::info!("direct connection lost; back to the relay");
                        link.set(relay.clone());
                        on_direct = false;
                        stats.direct.store(false, Ordering::Relaxed);
                        target_fps.store(params.fps.clamp(5, 240), Ordering::Relaxed);
                        recreate_encoder(current_mode);
                        continue;
                    }
                    reason = format!("client disconnected: {e}");
                    break;
                }
            },
            r = async { pending.as_ref().expect("guarded").control().recv().await }, if pending.is_some() => {
                match r.map(|b| rc_protocol::decode::<ClientMessage>(&b)) {
                    Ok(Ok(ClientMessage::DirectUse)) => {
                        if let Some(direct) = pending.take() {
                            link.set(direct);
                            on_direct = true;
                            stats.direct.store(true, Ordering::Relaxed);
                            if params.uncap_fps_on_direct {
                                target_fps.store((params.fps.clamp(5, 240) * 2).min(120), Ordering::Relaxed);
                            }
                            tracing::info!("switched to a direct connection");
                            recreate_encoder(current_mode);
                        }
                    }
                    Ok(_) => {} // nothing else belongs before the switch
                    Err(_) => pending = None,
                }
            }
            Some(result) = direct_rx.recv() => match result {
                Ok(sess) if sess.peer_key() == params.peer_key => pending = Some(Arc::new(sess)),
                Ok(_) => tracing::warn!("a direct path authenticated a different device; ignored"),
                Err(e) => tracing::info!(error = %e, "direct connection didn't come up; staying on the relay"),
            },
            mic = cur.recv_mic(), if params.mic => {
                if let Ok(packet) = mic {
                    if let Some(d) = rc_audio::adpcm::decode(&packet) {
                        let _ = mic_render_tx.try_send((d.samples, d.sample_rate, d.channels));
                    }
                }
            }
            Some(cmd) = commands.recv() => match cmd {
                SessionCommand::SendFile(path) => {
                    tokio::spawn(send_file_to_client(link.clone(), path));
                }
            },
            _ = keyframe_poll.tick() => {
                if cur.video().take_key_frame_request() {
                    tracing::debug!("client requested a keyframe; recreating encoder");
                    recreate_encoder(current_mode);
                }

                // Congestion control, such as it is. Round-trip time sitting
                // well above this link's own floor means video is queueing up
                // somewhere ahead of us, and every millisecond of that queue
                // is added to input latency too. Back the frame rate off until
                // it drains. Deliberately crude — it only paces the capture
                // gate, never touches the encoder, so there's no re-keying
                // and nothing to see beyond a lower frame rate while a link
                // is struggling.
                let rtt = cur.video().feedback().rtt_ms;
                stats.rtt_ms_x100.store((rtt * 100.0) as u32, Ordering::Relaxed);
                if rtt > 0.0 {
                    rtt_floor = if rtt_floor == 0.0 || rtt < rtt_floor {
                        rtt
                    } else {
                        // Let the floor drift up slowly so a genuinely slower
                        // network eventually reads as normal rather than as
                        // permanent congestion.
                        rtt_floor + (rtt - rtt_floor) * 0.01
                    };
                    let excess = rtt - rtt_floor;
                    let want = if excess > 150.0 {
                        4
                    } else if excess > 60.0 {
                        2
                    } else {
                        1
                    };
                    // React to congestion at once, recover a step at a time:
                    // backing off slowly means riding a full queue for
                    // seconds, while recovering fast just rebuilds it.
                    let now = gate.throttle.load(Ordering::Acquire);
                    let next = if want > now { want } else { now.saturating_sub(1).max(1) };
                    if next != now {
                        tracing::debug!(rtt_ms = rtt, floor_ms = rtt_floor, throttle = next, "congestion throttle changed");
                        gate.throttle.store(next, Ordering::Release);
                    }
                }
            }
            _ = async { tokio::time::sleep_until(release_due.unwrap().into()).await },
                if release_due.is_some() =>
            {
                if let Err(e) = injector.flush_due() {
                    tracing::warn!(error = %e, "deferred release failed");
                }
            }
            _ = capture_poll.tick() => {
                let now = rc_input::cursor_captured();
                capture_streak = if now { capture_streak.saturating_add(1) } else { 0 };
                let next = if captured { now } else { capture_streak >= 2 };
                if next != captured {
                    captured = next;
                    injector.set_captured(captured);
                    stats.game_captured.store(captured, Ordering::Relaxed);
                    tracing::debug!(captured, "cursor capture changed");
                    link.send(&HostMessage::CursorCaptured(captured)).await;
                }
                // While a game has the mouse, tell the client where that game
                // is drawn, so overlays line up with its UI whether it's
                // fullscreen, maximized or in a window.
                if captured {
                    if let Some(r) = rc_input::foreground_client_rect() {
                        let (dw, dh) = (rect.width.max(1) as f32, rect.height.max(1) as f32);
                        let area = (
                            (r.x - rect.x) as f32 / dw,
                            (r.y - rect.y) as f32 / dh,
                            r.w as f32 / dw,
                            r.h as f32 / dh,
                        );
                        if last_area != Some(area) {
                            last_area = Some(area);
                            tracing::debug!(?area, "game area changed");
                            link.send(&HostMessage::GameArea {
                                x: area.0,
                                y: area.1,
                                w: area.2,
                                h: area.3,
                            })
                            .await;
                        }
                    }
                }
            }
            _ = stats_tick.tick() => {
                tracing::info!(
                    frames_sent = stats.frames_sent.load(Ordering::Relaxed),
                    input_events = stats.input_events.load(Ordering::Relaxed),
                    rtt_ms = cur.video().feedback().rtt_ms,
                    direct = on_direct,
                    "session stats"
                );
            }
            _ = stop.changed() => {
                if *stop.borrow() { reason = "host stopping".into(); break; }
            }
        }
    }

    // ── teardown ──────────────────────────────────────────────────────────
    tracing::info!(%reason, "session ending");
    capture_stop.store(true, Ordering::SeqCst);
    clip_stop.store(true, Ordering::SeqCst);
    audio_stop.store(true, Ordering::SeqCst);
    mic_stop.store(true, Ordering::SeqCst);
    let _ = capture_thread.join();
    drop(encoder_slot); // last ref → flushes and joins the encoder thread
    injector.release_all();
    gamepad.disconnect();
    display::undo(monitor_restore);
    video_task.abort();
    link.send(&HostMessage::Disconnect { reason }).await;
    Ok(())
}

/// Send a file to the connected device: an offer, then chunks, then done. The
/// device shows a download once it sees `FileDone`. Runs detached so a large
/// file doesn't hold up the control loop; errors just end the transfer quietly
/// (the device already knows the session if the link itself dropped).
async fn send_file_to_client(link: Arc<Link>, path: PathBuf) {
    const CHUNK: usize = 32 * 1024;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file").to_string();
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "can't send file: reading its metadata failed");
            return;
        }
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "can't send file: opening it failed");
            return;
        }
    };
    let id = rand::random::<u32>();
    let size = meta.len();
    link.send(&HostMessage::FileOffer { id, name: name.clone(), size }).await;
    let mut sent = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match file.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "sending file failed mid-transfer");
                return;
            }
        };
        link.send(&HostMessage::FileChunk { id, offset: sent, data: buf[..n].to_vec() }).await;
        sent += n as u64;
    }
    link.send(&HostMessage::FileDone { id }).await;
    tracing::info!(name, size, "sent file to the connected device");
}

/// Answer a device's offer of a direct path. The answer goes back over the
/// current link; the direct session — once its own handshake completes, or
/// fails — arrives on `direct_tx`.
async fn offer_direct(
    sdp: String,
    link: Arc<Link>,
    params: SessionParams,
    direct_tx: tokio::sync::mpsc::UnboundedSender<Result<LanSession>>,
) {
    match crate::direct::accept(&sdp).await {
        Ok((answer, stream)) => {
            link.send(&HostMessage::DirectAnswer(answer)).await;
            let tx = direct_tx.clone();
            let (key, id) = (params.host_static, params.device_id.clone());
            tokio::spawn(async move {
                let handshake = LanSession::over_stream_responder(stream, &key, &id);
                let result = match tokio::time::timeout(Duration::from_secs(20), handshake).await {
                    Ok(Ok(sess)) => Ok(sess),
                    Ok(Err(e)) => Err(anyhow!(e)),
                    Err(_) => Err(anyhow!("timed out")),
                };
                let _ = tx.send(result);
            });
        }
        Err(e) => {
            tracing::info!(error = ?e, "can't offer a direct connection");
            link.send(&HostMessage::DirectUnavailable).await;
        }
    }
}

/// What the control loop should do in response to one decoded client message.
enum ClientAction {
    None,
    /// Counted in the periodic session stats.
    Input,
    /// The client wants the video stream renegotiated to this target.
    SetQuality(QualityMode),
    /// The client offers a direct path (WebRTC SDP).
    DirectOffer(String),
}

#[allow(clippy::too_many_arguments)]
async fn handle_client_msg(
    bytes: &[u8],
    injector: &mut Injector,
    clipboard: Option<&Arc<StdMutex<ClipboardWatcher>>>,
    link: &Link,
    gamepad: &mut GamepadHub,
    incoming: &mut HashMap<u32, IncomingFile>,
) -> ClientAction {
    match rc_protocol::decode::<ClientMessage>(bytes) {
        Ok(ClientMessage::Input(ev)) => {
            if let InputEvent::Text(_) | InputEvent::Key { .. } = ev {
                tracing::trace!(?ev, "inject key/text");
            }
            if let Err(e) = injector.inject(&ev) {
                tracing::warn!(error = %e, "input injection failed");
            }
            return ClientAction::Input;
        }
        Ok(ClientMessage::Ping { nonce }) => {
            link.send(&HostMessage::Pong { nonce }).await;
        }
        Ok(ClientMessage::DirectOffer(sdp)) => return ClientAction::DirectOffer(sdp),
        // Only meaningful as the first message on a direct path.
        Ok(ClientMessage::DirectUse) => {}
        Ok(ClientMessage::ClipboardText(text)) => {
            if let Some(watcher) = clipboard {
                if let Ok(mut w) = watcher.lock() {
                    w.note_local_set(&text);
                }
                if let Err(e) = rc_clipboard::set_text(&text) {
                    tracing::warn!(error = %e, "applying client clipboard failed");
                } else {
                    tracing::debug!(len = text.len(), "client clipboard → host");
                }
            }
        }
        Ok(ClientMessage::SetQuality(mode)) => return ClientAction::SetQuality(mode),
        Ok(ClientMessage::Disconnect) => {
            tracing::info!("client requested disconnect");
        }
        Ok(ClientMessage::GamepadState {
            buttons,
            left_trigger,
            right_trigger,
            thumb_lx,
            thumb_ly,
            thumb_rx,
            thumb_ry,
        }) => {
            if let Some(notice) = gamepad.update(buttons, left_trigger, right_trigger, thumb_lx, thumb_ly, thumb_rx, thumb_ry) {
                link.send(&HostMessage::Notice(notice)).await;
            }
            return ClientAction::Input;
        }
        Ok(ClientMessage::GamepadDisconnect) => gamepad.disconnect(),
        Ok(ClientMessage::FileOffer { id, name, size }) => {
            if size > MAX_INCOMING_FILE {
                link.send(&HostMessage::Notice(format!("\"{name}\" is too large to receive (over 500 MB)"))).await;
                return ClientAction::None;
            }
            match incoming_files_dir().and_then(|dir| {
                let path = unique_download_path(&dir, &name);
                std::fs::File::create(&path).map(|file| (file, path)).context("creating the file")
            }) {
                Ok((file, path)) => {
                    tracing::info!(name, size, path = %path.display(), "receiving a file from the connected device");
                    incoming.insert(id, IncomingFile { file, name, path, written: 0, total: size });
                }
                Err(e) => {
                    tracing::warn!(error = %e, name, "couldn't start receiving file");
                    link.send(&HostMessage::Notice(format!("Couldn't save \"{name}\": {e}"))).await;
                }
            }
        }
        Ok(ClientMessage::FileChunk { id, data, .. }) => {
            if let Some(f) = incoming.get_mut(&id) {
                use std::io::Write;
                if let Err(e) = f.file.write_all(&data) {
                    tracing::warn!(error = %e, name = %f.name, "writing received file failed");
                    incoming.remove(&id);
                } else {
                    f.written += data.len() as u64;
                }
            }
        }
        Ok(ClientMessage::FileDone { id }) => {
            if let Some(f) = incoming.remove(&id) {
                let ok = f.written >= f.total;
                tracing::info!(name = %f.name, bytes = f.written, complete = ok, "file received");
                let text = if ok {
                    format!("Received \"{}\" — saved to Downloads\\RemoteControl", f.name)
                } else {
                    format!("\"{}\" arrived incomplete and was kept anyway", f.name)
                };
                link.send(&HostMessage::Notice(text)).await;
                let _ = f.path; // kept on disk either way; nothing further to do with the handle
            }
        }
        Err(e) => tracing::warn!(error = %e, "undecodable control message"),
    }
    ClientAction::None
}

fn capture_loop(
    target_fps: &AtomicU32,
    geo_tx: &std_mpsc::Sender<Result<(u32, u32, DesktopRect)>>,
    enc_rx: &std_mpsc::Receiver<Arc<StdMutex<Arc<StreamEncoder>>>>,
    stop: &AtomicBool,
    gate: &CaptureGate,
) {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }

    let mut cap = match D3dContext::new().and_then(|d3d| Capturer::new(d3d, 0)) {
        Ok(c) => c,
        Err(e) => {
            let _ = geo_tx.send(Err(e));
            return;
        }
    };
    let (w, h, rect) = (cap.width(), cap.height(), cap.desktop_rect());
    if geo_tx.send(Ok((w, h, rect))).is_err() {
        return;
    }

    // Re-read on every submit rather than once: `SetQuality` mid-session
    // swaps the `Arc<StreamEncoder>` this slot points at (see `session::run`).
    let encoder_slot = match enc_rx.recv() {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut fps = target_fps.load(Ordering::Relaxed).max(1);
    let mut frame_dur = Duration::from_secs_f64(1.0 / fps as f64);
    let mut next = Instant::now();
    let mut ts_us: u64 = 0;
    let mut last: Option<Vec<u8>> = None;
    let mut dirty = false;
    let mut last_submit = Instant::now() - Duration::from_secs(1);

    while !stop.load(Ordering::SeqCst) {
        // Picked up on every iteration so a mid-session change (switching to
        // or from a direct path — see `uncap_fps_on_direct`) takes effect
        // without restarting this thread.
        let new_fps = target_fps.load(Ordering::Relaxed).max(1);
        if new_fps != fps {
            fps = new_fps;
            frame_dur = Duration::from_secs_f64(1.0 / fps as f64);
        }
        // Pace *first*, then grab, then submit immediately. Grabbing before
        // the wait (as this did originally) meant every frame sat around for
        // up to a full frame period — 33ms at 30fps — going stale between
        // being captured and being handed to the encoder, for nothing.
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next += frame_dur;
        // Never let a slow iteration bank up "owed" frames and then burst.
        let now = Instant::now();
        if next < now {
            next = now;
        }

        // Short timeout: anything that changed during the wait above is
        // already queued, so this returns straight away when there's motion.
        match cap.grab(4) {
            Ok(Grab::Frame(f)) => {
                if !f.mouse_only {
                    last = Some(f.bgra);
                    dirty = true;
                }
            }
            Ok(Grab::Timeout) => {}
            Err(e) => {
                tracing::warn!(error = %e, "capture error");
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        // Send every changed frame; when the screen is static, still refresh at
        // ~5 fps so keyframes keep flowing for a late-joining decoder.
        let refresh = last_submit.elapsed() >= Duration::from_millis(200);
        // While the link is still busy with the previous frame, skip this one
        // entirely rather than encode something that would only queue up.
        // Whatever is on screen when it clears gets encoded then — fresher
        // than anything we'd have buffered, and the encoder's next delta is
        // still against a frame the decoder actually received.
        let busy = gate.net_busy.load(Ordering::Acquire);
        // Congestion backoff, set from measured RTT — see `CaptureGate`.
        let throttle = gate.throttle.load(Ordering::Acquire).max(1);
        let paced = last_submit.elapsed() >= frame_dur * throttle;
        if let Some(bytes) = &last {
            if (dirty || refresh) && !busy && paced {
                let encoder = encoder_slot.lock().unwrap().clone();
                let _ = encoder.submit_bgra(bytes.clone(), ts_us);
                dirty = false;
                last_submit = Instant::now();
            }
        }
        ts_us += (1_000_000 / fps.max(1)) as u64;
    }
}
