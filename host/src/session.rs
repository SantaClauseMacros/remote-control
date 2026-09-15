//! One connected remote-control session: capture → encode → send video,
//! receive control → inject input, and (optionally) sync clipboard text.
//!
//! The "require confirmation" prompt is still to come; the GPU-only
//! capture→encode path is milestone 6.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rc_capture::{Capturer, D3dContext, DesktopRect, Grab};
use rc_clipboard::ClipboardWatcher;
use rc_encode::{EncodeConfig, StreamEncoder};
use rc_input::{Injector, Rect};
use rc_protocol::{ClientMessage, DisplayInfo, HostMessage, InputEvent, QualityMode};
use rc_transport::lan::LanSession;
use rc_transport::{EncodedFrame, Session};
use tokio::sync::watch;

/// Tunables for a session, taken from host settings.
#[derive(Debug, Clone, Copy)]
pub struct SessionParams {
    pub fps: u32,
    /// `0` = let [`EncodeConfig::balanced`] pick from the resolution.
    pub max_bitrate_kbps: u32,
    /// Starting point for [`ClientMessage::SetQuality`] — the client can
    /// still change it mid-session (see `quality_config`).
    pub mode: QualityMode,
    /// Sync clipboard text in both directions.
    pub clipboard_sync: bool,
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
) -> Result<()> {
    let session = Arc::new(session);

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
    let cap_fps = params.fps;
    let capture_thread = std::thread::Builder::new()
        .name("rc-capture".into())
        .spawn(move || capture_loop(cap_fps, &geo_tx, &enc_rx, &cap_stop, &cap_gate))
        .context("spawn capture thread")?;

    let (width, height, rect) = geo_rx
        .recv()
        .context("capture thread exited before reporting geometry")??;
    tracing::info!(width, height, "session capture ready");

    // ── encoder ────────────────────────────────────────────────────────────
    let cfg = quality_config(params.mode, width, height, params.fps, params.max_bitrate_kbps);
    let (pkt_tx, mut pkt_rx) = tokio::sync::mpsc::unbounded_channel();
    let encoder = StreamEncoder::new(cfg, pkt_tx.clone()).context("start encoder")?;
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
    let frames_sent = Arc::new(AtomicU64::new(0));
    let sv = session.clone();
    let vc = frames_sent.clone();
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
            let sent = sv.video().send(frame).await;
            // Caught up only once nothing else is already waiting behind us.
            nb.net_busy.store(!pkt_rx.is_empty(), Ordering::Release);
            if let Err(e) = sent {
                tracing::warn!(error = %e, "video send failed");
                break;
            }
            vc.fetch_add(1, Ordering::Relaxed);
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
    let _ = session
        .control()
        .send(rc_protocol::encode(&HostMessage::Displays(displays)))
        .await;

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
        let sc = session.clone();
        tokio::spawn(async move {
            while let Some(text) = clip_rx.recv().await {
                tracing::debug!(len = text.len(), "host clipboard → client");
                let _ = sc
                    .control()
                    .send(rc_protocol::encode(&HostMessage::ClipboardText(text)))
                    .await;
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

    let mut input_events: u64 = 0;
    let mut current_mode = params.mode;
    let mut stats = tokio::time::interval(Duration::from_secs(5));
    stats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
        let cfg = quality_config(mode, width, height, params.fps, params.max_bitrate_kbps);
        match StreamEncoder::new(cfg, pkt_tx.clone()) {
            Ok(new_encoder) => {
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

    let reason;
    loop {
        let release_due = injector.next_release_due();
        tokio::select! {
            r = session.control().recv() => match r {
                Ok(bytes) => {
                    match handle_client_msg(&bytes, &mut injector, clipboard.as_ref(), &session).await {
                        ClientAction::Input => input_events += 1,
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
                        ClientAction::None => {}
                    }
                }
                Err(e) => { reason = format!("client disconnected: {e}"); break; }
            },
            _ = keyframe_poll.tick() => {
                if session.video().take_key_frame_request() {
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
                let rtt = session.video().feedback().rtt_ms;
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
                    tracing::debug!(captured, "cursor capture changed");
                    let _ = session
                        .control()
                        .send(rc_protocol::encode(&HostMessage::CursorCaptured(captured)))
                        .await;
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
                            let _ = session
                                .control()
                                .send(rc_protocol::encode(&HostMessage::GameArea {
                                    x: area.0,
                                    y: area.1,
                                    w: area.2,
                                    h: area.3,
                                }))
                                .await;
                        }
                    }
                }
            }
            _ = stats.tick() => {
                tracing::info!(
                    frames_sent = frames_sent.load(Ordering::Relaxed),
                    input_events,
                    rtt_ms = session.video().feedback().rtt_ms,
                    "session stats"
                );
            }
            _ = stop.changed() => {
                if *stop.borrow() { reason = "host stopping".into(); break; }
            }
            _ = session.closed() => { reason = "connection closed".into(); break; }
        }
    }

    // ── teardown ──────────────────────────────────────────────────────────
    tracing::info!(%reason, "session ending");
    capture_stop.store(true, Ordering::SeqCst);
    clip_stop.store(true, Ordering::SeqCst);
    let _ = capture_thread.join();
    drop(encoder_slot); // last ref → flushes and joins the encoder thread
    injector.release_all();
    video_task.abort();
    let _ = session
        .control()
        .send(rc_protocol::encode(&HostMessage::Disconnect { reason }))
        .await;
    Ok(())
}

/// What the control loop should do in response to one decoded client message.
enum ClientAction {
    None,
    /// Counted in the periodic session stats.
    Input,
    /// The client wants the video stream renegotiated to this target.
    SetQuality(QualityMode),
}

async fn handle_client_msg(
    bytes: &[u8],
    injector: &mut Injector,
    clipboard: Option<&Arc<StdMutex<ClipboardWatcher>>>,
    session: &Arc<LanSession>,
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
            let _ = session
                .control()
                .send(rc_protocol::encode(&HostMessage::Pong { nonce }))
                .await;
        }
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
        Err(e) => tracing::warn!(error = %e, "undecodable control message"),
    }
    ClientAction::None
}

fn capture_loop(
    fps: u32,
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

    let frame_dur = Duration::from_secs_f64(1.0 / fps as f64);
    let mut next = Instant::now();
    let mut ts_us: u64 = 0;
    let mut last: Option<Vec<u8>> = None;
    let mut dirty = false;
    let mut last_submit = Instant::now() - Duration::from_secs(1);

    while !stop.load(Ordering::SeqCst) {
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
