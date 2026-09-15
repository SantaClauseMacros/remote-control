//! The Remote Control app window.
//!
//! A small dashboard served by the host itself on `127.0.0.1` and shown in a
//! Microsoft Edge app window (no tabs, no address bar), which every Windows 10
//! and 11 PC has. Nothing runs until the window is first opened, and the server
//! is a single blocking thread, so an app that nobody is looking at costs
//! nothing.
//!
//! Security: the server only listens on loopback, checks the `Host` header
//! (so a web page can't reach it through DNS rebinding), and requires a random
//! per-launch token — in the URL for the page itself, and in a custom header
//! for every API call, which a cross-site request can't set.

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use rc_common::AppPaths;
use rc_protocol::QualityMode;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};
use tokio::sync::{mpsc, watch};

use crate::engine::{CoreCommand, CoreState, CoreStatus, SessionInfo};
use crate::paired::PairedStore;
use crate::session::LiveStats;
use crate::settings::{Settings, UpdateSettings};

const UI_HTML: &str = include_str!("ui.html");
const FAVICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#4c8dff"/><stop offset="1" stop-color="#7a5cff"/></linearGradient></defs><rect width="32" height="32" rx="8" fill="url(#g)"/><g fill="none" stroke="#fff" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="8" width="15" height="11" rx="2"/><path d="M9 23h7M12.5 19v4"/><rect x="20" y="12" width="7" height="12" rx="1.6"/></g></svg>"##;

/// What the dashboard needs from the rest of the host.
pub struct DashboardCtx {
    pub paths: AppPaths,
    pub core_tx: mpsc::UnboundedSender<CoreCommand>,
    pub status: watch::Receiver<CoreStatus>,
    pub exe_path: PathBuf,
    /// `--dashboard-preview`: never touch the registry or the running app.
    pub preview: bool,
}

/// A running dashboard server.
pub struct Dashboard {
    url: String,
}

impl Dashboard {
    /// Bind a loopback port and start serving on a background thread.
    pub fn start(ctx: DashboardCtx) -> Result<Self> {
        let server = Server::http("127.0.0.1:0").map_err(|e| anyhow!("binding the dashboard server: {e}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .map(|a| a.port())
            .context("dashboard server address")?;
        let token = rc_crypto::random_token_hex(24);
        let url = format!("http://127.0.0.1:{port}/?t={token}");

        std::thread::Builder::new()
            .name("rc-dashboard".into())
            .spawn(move || {
                let app = App { ctx, token, port };
                for req in server.incoming_requests() {
                    app.handle(req);
                }
            })
            .context("spawning the dashboard thread")?;

        tracing::info!(port, "dashboard server started");
        Ok(Self { url })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Show the app window.
    pub fn open_window(&self) {
        open_app_window(&self.url);
    }
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

struct App {
    ctx: DashboardCtx,
    token: String,
    port: u16,
}

impl App {
    fn handle(&self, mut req: Request) {
        let host = header(&req, "Host").unwrap_or_default();
        if host != format!("127.0.0.1:{}", self.port) && host != format!("localhost:{}", self.port) {
            let _ = req.respond(reply(403, "text/plain; charset=utf-8", b"forbidden".to_vec()));
            return;
        }

        let url = req.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        let via_header = header(&req, "X-RC-Token") == Some(self.token.as_str());
        let via_query = query
            .split('&')
            .filter_map(|kv| kv.strip_prefix("t="))
            .any(|t| t == self.token);
        let method = req.method().clone();

        let result: Result<Resp> = match (&method, path) {
            (Method::Get, "/favicon.svg") => Ok(reply(200, "image/svg+xml", FAVICON_SVG.as_bytes().to_vec())),
            // The page and its QR image are loaded by URL, so the token rides
            // in the query; everything else needs the header.
            (Method::Get, "/") if via_query || via_header => Ok(reply(200, "text/html; charset=utf-8", UI_HTML.as_bytes().to_vec())),
            (Method::Get, "/api/qr.svg") if via_query || via_header => self.qr_svg().map(|svg| reply(200, "image/svg+xml", svg)),
            _ if !via_header => Ok(reply(
                403,
                "text/plain; charset=utf-8",
                b"This page only opens from the Remote Control app.".to_vec(),
            )),
            (Method::Get, "/api/status") => Ok(json_reply(self.status_json())),
            (Method::Get, "/api/settings") => self.settings_json().map(json_reply),
            (Method::Post, "/api/settings") => read_json(&mut req).and_then(|v| self.update_settings(v)).map(json_reply),
            (Method::Get, "/api/devices") => Ok(json_reply(self.devices_json())),
            (Method::Get, "/api/activity") => Ok(json_reply(self.activity_json())),
            (Method::Post, "/api/action") => read_json(&mut req).and_then(|v| self.action(v)).map(json_reply),
            _ => Ok(reply(404, "text/plain; charset=utf-8", b"not found".to_vec())),
        };

        let resp = result.unwrap_or_else(|e| {
            tracing::debug!(error = %e, path, "dashboard request failed");
            reply(400, "text/plain; charset=utf-8", e.to_string().into_bytes())
        });
        let _ = req.respond(resp);
    }

    fn settings(&self) -> Settings {
        Settings::load(&self.ctx.paths.config_file()).unwrap_or_default()
    }

    fn autostart_enabled(&self, settings: &Settings) -> bool {
        if self.ctx.preview {
            settings.start_with_windows
        } else {
            crate::platform::autostart::is_enabled()
        }
    }

    fn status_json(&self) -> Value {
        let status = self.ctx.status.borrow().clone();
        let settings = self.settings();
        let state = match status.state {
            CoreState::Starting => "starting",
            CoreState::Disabled => "disabled",
            CoreState::Idle => "idle",
            CoreState::Listening => "listening",
            CoreState::Connected => "connected",
        };
        let phone_link = settings.network.phone_link(&status.device_id);
        let web_app = phone_link
            .as_deref()
            .and_then(|l| l.split_once('#'))
            .map(|(base, _)| base.to_string());
        let update = status
            .update_available
            .as_ref()
            .map(|(version, url, notes)| json!({ "version": version, "url": url, "notes": notes }));

        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "computerName": settings.computer_name,
            "deviceId": status.device_id,
            "state": state,
            "relayEnabled": status.relay_parked,
            "relayConnected": status.relay_online.load(Ordering::Relaxed),
            "listenPort": status.listen_port,
            "pairedCount": status.paired_count,
            "phoneLink": phone_link,
            "webApp": web_app,
            "update": update,
            "updateChecks": update_checks_on(&settings),
            "elevated": crate::platform::elevate::is_elevated(),
            "autostart": self.autostart_enabled(&settings),
            "session": status.session.as_ref().map(session_json),
        })
    }

    fn settings_json(&self) -> Result<Value> {
        let settings = Settings::load(&self.ctx.paths.config_file())?;
        Ok(settings_view(&settings, self.autostart_enabled(&settings)))
    }

    fn update_settings(&self, patch: Value) -> Result<Value> {
        let path = self.ctx.paths.config_file();
        let mut s = Settings::load(&path)?;
        let fields = patch.as_object().context("expected a JSON object")?;
        let mut autostart = None;

        for (key, value) in fields {
            let as_bool = || value.as_bool().with_context(|| format!("{key} must be true or false"));
            let as_str = || value.as_str().with_context(|| format!("{key} must be text"));
            match key.as_str() {
                "computerName" => {
                    let name = as_str()?.trim();
                    ensure!(
                        (1..=40).contains(&name.chars().count()),
                        "The PC name must be 1 to 40 characters"
                    );
                    s.computer_name = name.to_string();
                }
                "enableRemoteAccess" => s.enable_remote_access = as_bool()?,
                "startWithWindows" => {
                    let on = as_bool()?;
                    s.start_with_windows = on;
                    autostart = Some(on);
                }
                "clipboardSync" => s.security.clipboard_sync = as_bool()?,
                "streamAudio" => s.audio.enabled = as_bool()?,
                "lanDiscovery" => s.network.lan_discovery = as_bool()?,
                "quality" => {
                    s.performance.mode = match as_str()? {
                        "low" => QualityMode::Low,
                        "balanced" => QualityMode::Balanced,
                        "high" => QualityMode::High,
                        "auto" => QualityMode::Auto,
                        other => bail!("unknown quality {other:?}"),
                    }
                }
                "maxFps" => {
                    let fps = value.as_u64().context("maxFps must be a number")?;
                    ensure!((5..=240).contains(&fps), "Frame rate must be between 5 and 240");
                    s.performance.max_fps = fps as u16;
                }
                "maxBitrateKbps" => {
                    let kbps = value.as_u64().context("maxBitrateKbps must be a number")?;
                    s.performance.max_bitrate_kbps = u32::try_from(kbps).context("bitrate too large")?;
                }
                // An empty relay is stored as "", not left out: a missing key
                // would fall back to the default relay on the next load.
                "relayUrl" => s.network.signaling_url = Some(as_str()?.trim().to_string()),
                "relayKey" => {
                    let key = as_str()?.trim();
                    s.network.relay_key = (!key.is_empty()).then(|| key.to_string());
                }
                "updateChecks" => {
                    s.update.check_url = if as_bool()? {
                        UpdateSettings::default()
                            .check_url
                            .or_else(|| s.update.check_url.clone().filter(|u| !u.is_empty()))
                    } else {
                        // Same reason as the relay: "" is "off", None is "default".
                        Some(String::new())
                    };
                }
                other => bail!("unknown setting {other:?}"),
            }
        }

        s.save(&path)?;
        if let Some(on) = autostart {
            if !self.ctx.preview {
                crate::platform::autostart::set(on, &self.ctx.exe_path)?;
            }
        }
        let _ = self.ctx.core_tx.send(CoreCommand::ReloadSettings);
        Ok(settings_view(&s, self.autostart_enabled(&s)))
    }

    fn devices_json(&self) -> Value {
        let devices = PairedStore::load(&self.ctx.paths.data_dir)
            .list()
            .into_iter()
            .map(|(id, d)| json!({ "id": id, "label": d.label, "added": d.added, "lastSeen": d.last_seen }))
            .collect();
        Value::Array(devices)
    }

    /// A readable timeline built from the log file — connections, drops,
    /// failures, updates — newest first.
    fn activity_json(&self) -> Value {
        let log = recent_log(&self.ctx.paths.logs_dir, 400_000);
        let mut events: VecDeque<Value> = VecDeque::new();
        let mut last: Option<(&'static str, String)> = None;

        for line in log.lines() {
            let Some((time, rest)) = line.split_once(' ') else { continue };
            if !time.starts_with("20") {
                continue; // a line cut in half by the tail
            }
            let Some((kind, text)) = describe_log_line(rest) else { continue };
            // Collapse a burst of identical events (the relay retrying, say).
            if last.as_ref().is_some_and(|(k, t)| *k == kind && *t == text) {
                continue;
            }
            last = Some((kind, text.clone()));
            events.push_front(json!({ "time": time, "kind": kind, "text": text }));
            if events.len() > 100 {
                events.pop_back();
            }
        }
        Value::Array(events.into())
    }

    fn qr_svg(&self) -> Result<Vec<u8>> {
        let status = self.ctx.status.borrow().clone();
        let link = self
            .settings()
            .network
            .phone_link(&status.device_id)
            .context("no phone link without a relay")?;
        let code = qrcode::QrCode::new(link.as_bytes()).map_err(|e| anyhow!("making the QR code: {e:?}"))?;
        let svg = code
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(200, 200)
            .quiet_zone(false)
            .dark_color(qrcode::render::svg::Color("#0b0d10"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .build();
        Ok(svg.into_bytes())
    }

    fn action(&self, body: Value) -> Result<Value> {
        let name = body.get("action").and_then(Value::as_str).context("missing action")?;
        let message = match name {
            "disconnect" => {
                let _ = self.ctx.core_tx.send(CoreCommand::DisconnectAll);
                "Disconnected"
            }
            "forgetDevices" => {
                let _ = self.ctx.core_tx.send(CoreCommand::ForgetPaired);
                "Device list cleared"
            }
            "copyDiagnostics" => {
                let status = self.ctx.status.borrow().clone();
                let report = crate::diagnostics::build_report(&self.ctx.paths, &status);
                rc_clipboard::set_text(&report).map_err(|e| anyhow!("copying: {e}"))?;
                "Diagnostic info copied — paste it wherever you're asking for help"
            }
            "openLogs" => {
                shell_open(self.ctx.paths.logs_dir.as_os_str());
                ""
            }
            "editConfig" => {
                shell_open(self.ctx.paths.config_file().as_os_str());
                ""
            }
            "restartAdmin" => {
                if !self.ctx.preview {
                    crate::platform::message_window::post_command(crate::platform::tray::ID_RESTART_ELEVATED);
                }
                "Restarting as Administrator…"
            }
            "exit" => {
                if !self.ctx.preview {
                    crate::platform::message_window::post_command(crate::platform::tray::ID_EXIT);
                }
                "Remote Control is closing"
            }
            other => bail!("unknown action {other:?}"),
        };
        Ok(json!({ "message": message }))
    }
}

fn session_json(s: &SessionInfo) -> Value {
    let st = &s.stats;
    json!({
        "route": s.route,
        "keyId": s.key_id,
        "since": s.since,
        "framesSent": st.frames_sent.load(Ordering::Relaxed),
        "inputEvents": st.input_events.load(Ordering::Relaxed),
        "rttMs": f64::from(st.rtt_ms_x100.load(Ordering::Relaxed)) / 100.0,
        "width": st.width.load(Ordering::Relaxed),
        "height": st.height.load(Ordering::Relaxed),
        "hardwareEncoder": st.hardware_encoder.load(Ordering::Relaxed),
        "gameCaptured": st.game_captured.load(Ordering::Relaxed),
        "direct": st.direct.load(Ordering::Relaxed),
    })
}

fn settings_view(s: &Settings, autostart: bool) -> Value {
    json!({
        "computerName": s.computer_name,
        "enableRemoteAccess": s.enable_remote_access,
        "startWithWindows": autostart,
        "clipboardSync": s.security.clipboard_sync,
        "streamAudio": s.audio.enabled,
        "lanDiscovery": s.network.lan_discovery,
        "quality": match s.performance.mode {
            QualityMode::Low => "low",
            QualityMode::Balanced => "balanced",
            QualityMode::High => "high",
            QualityMode::Auto => "auto",
        },
        "maxFps": s.performance.max_fps,
        "maxBitrateKbps": s.performance.max_bitrate_kbps,
        "relayUrl": s.network.signaling_url.clone().unwrap_or_default(),
        "relayKey": s.network.relay_key.clone().unwrap_or_default(),
        "updateChecks": update_checks_on(s),
        "updateChecksAvailable": UpdateSettings::default().check_url.is_some() || update_checks_on(s),
    })
}

fn update_checks_on(s: &Settings) -> bool {
    s.update.check_url.as_deref().is_some_and(|u| !u.trim().is_empty())
}

/// Turn one log line (minus its timestamp) into a dashboard event.
fn describe_log_line(rest: &str) -> Option<(&'static str, String)> {
    let field = |name: &str| -> String {
        rest.split_once(&format!("{name}="))
            .and_then(|(_, v)| v.split_whitespace().next())
            .unwrap_or("")
            .trim_matches('"')
            .to_string()
    };
    if rest.contains("host starting") {
        Some(("start", format!("Remote Control v{} started", field("version"))))
    } else if rest.contains("connection accepted; starting session") {
        let via = if rest.contains("peer=via relay") { "through the relay" } else { "on your local network" };
        Some(("connect", format!("Device {} connected {via}", field("key_id"))))
    } else if rest.contains("session ending") {
        let reason = rest.split_once("reason=").map(|(_, r)| r.trim()).unwrap_or("");
        let why = if reason.contains("host stopping") {
            "Remote Control closed"
        } else if reason.contains("disconnected") || reason.contains("connection closed") {
            "the device left"
        } else {
            reason
        };
        Some(("disconnect", format!("Session ended — {why}")))
    } else if rest.contains("relay park failed") {
        Some(("warn", "Lost the connection to the relay — reconnecting".to_string()))
    } else if rest.contains("handshake failed") || rest.contains("handshake timed out") {
        Some(("warn", "A device tried to connect but couldn't (wrong PC ID?)".to_string()))
    } else if rest.contains("update available") {
        Some(("update", format!("Update v{} is available", field("new_version"))))
    } else if rest.contains("switched to a direct connection") {
        Some(("connect", "Switched to a direct connection on your local network".to_string()))
    } else if rest.contains("direct connection lost; back to the relay") {
        Some(("warn", "Direct connection dropped — back on the relay".to_string()))
    } else if rest.contains("settings reloaded") {
        Some(("start", "Settings changed".to_string()))
    } else {
        None
    }
}

/// The tail of the two most recent log files, oldest first.
fn recent_log(dir: &Path, max_bytes_per_file: usize) -> String {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .filter(|(p, _)| p.is_file())
        .collect();
    files.sort_by_key(|(_, modified)| *modified);
    let mut out = String::new();
    for (path, _) in files.iter().rev().take(2).rev() {
        if let Ok(text) = std::fs::read_to_string(path) {
            out.push_str(crate::diagnostics::tail_str(&text, max_bytes_per_file));
            out.push('\n');
        }
    }
    out
}

// `HeaderField::equiv` only accepts a `&'static str`.
fn header<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

fn read_json(req: &mut Request) -> Result<Value> {
    let mut body = String::new();
    req.as_reader().take(64 * 1024).read_to_string(&mut body)?;
    serde_json::from_str(&body).context("invalid JSON")
}

fn reply(code: u16, content_type: &str, body: Vec<u8>) -> Resp {
    let h = |k: &str, v: &str| Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("static header");
    Response::from_data(body)
        .with_status_code(code)
        .with_header(h("Content-Type", content_type))
        .with_header(h("Cache-Control", "no-store"))
        .with_header(h("X-Frame-Options", "DENY"))
        .with_header(h("X-Content-Type-Options", "nosniff"))
        // Links out of the app (GitHub, the phone app) must not carry the
        // token along in a Referer header.
        .with_header(h("Referrer-Policy", "no-referrer"))
}

fn json_reply(v: Value) -> Resp {
    reply(200, "application/json; charset=utf-8", v.to_string().into_bytes())
}

fn shell_open(target: &std::ffi::OsStr) {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        let _ = ShellExecuteW(None, w!("open"), &HSTRING::from(target), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Open `url` as an app window in Microsoft Edge — no tabs or address bar, its
/// own taskbar button — or in the default browser if Edge can't be found.
fn open_app_window(url: &str) {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let edge = [std::env::var_os("ProgramFiles(x86)"), std::env::var_os("ProgramFiles"), std::env::var_os("LOCALAPPDATA")]
        .into_iter()
        .flatten()
        .map(|base| PathBuf::from(base).join(r"Microsoft\Edge\Application\msedge.exe"))
        .find(|p| p.exists());

    unsafe {
        if let Some(edge) = edge {
            let args = HSTRING::from(format!("--app={url} --window-size=1180,820"));
            let r = ShellExecuteW(None, w!("open"), &HSTRING::from(edge.as_os_str()), &args, PCWSTR::null(), SW_SHOWNORMAL);
            if r.0 as isize > 32 {
                return;
            }
            tracing::warn!(code = r.0 as isize, "launching Edge for the app window failed; using the default browser");
        }
        let _ = ShellExecuteW(None, w!("open"), &HSTRING::from(url), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// `rc-host --dashboard-preview`: serve the dashboard with made-up data and a
/// throwaway config folder, so the UI can be worked on without touching the
/// running app, the registry or real settings. Prints the URL and keeps
/// running until killed.
pub fn run_preview() -> Result<()> {
    let base = std::env::temp_dir().join("rc-dashboard-preview");
    let paths = AppPaths {
        config_dir: base.join("config"),
        data_dir: base.join("data"),
        logs_dir: base.join("logs"),
    };
    for dir in [&paths.config_dir, &paths.data_dir, &paths.logs_dir] {
        std::fs::create_dir_all(dir)?;
    }
    if !paths.config_file().exists() {
        Settings::default().save(&paths.config_file())?;
    }
    let mut paired = PairedStore::load(&paths.data_dir);
    paired.upsert(&[7u8; 32], "via relay");
    paired.upsert(&[42u8; 32], "192.168.1.20:50412");
    let demo_log = paths.logs_dir.join("host.log.preview");
    if !demo_log.exists() {
        std::fs::write(
            &demo_log,
            "2026-09-14T18:02:11.000000Z  INFO rc_host: host starting version=\"0.3.0\" launched_at_logon=true\n\
             2026-09-14T18:40:03.000000Z  INFO rc_host::engine: connection accepted; starting session peer=via relay key_id=UP5DV-52F6G-UIXZX-J\n\
             2026-09-14T19:12:47.000000Z  INFO rc_host::session: session ending reason=client disconnected: connection closed\n\
             2026-09-14T20:01:00.000000Z  WARN rc_host::relay: relay park failed error=transport error retry_in_s=1\n\
             2026-09-14T21:30:15.000000Z  INFO rc_host::engine: connection accepted; starting session peer=192.168.1.20:50412 key_id=VEBMB-FKXCB-TQEZX-P\n",
        )?;
    }

    let stats = Arc::new(LiveStats::default());
    stats.frames_sent.store(120_000, Ordering::Relaxed);
    stats.rtt_ms_x100.store(3_150, Ordering::Relaxed);
    stats.width.store(1920, Ordering::Relaxed);
    stats.height.store(1080, Ordering::Relaxed);
    stats.game_captured.store(true, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let status = CoreStatus {
        state: CoreState::Connected,
        device_id: "ABCDE-FGHIJ-KLMNO-P".into(),
        sessions: 1,
        listen_port: 9877,
        relay_parked: true,
        relay_online: Arc::new(AtomicBool::new(true)),
        paired_count: 2,
        session: Some(SessionInfo {
            route: "via relay".into(),
            key_id: "UP5DV-52F6G-UIXZX-J".into(),
            since: now.saturating_sub(754),
            stats: stats.clone(),
        }),
        ..Default::default()
    };
    let (_status_tx, status_rx) = watch::channel(status);
    let (core_tx, _core_rx) = mpsc::unbounded_channel();

    let dashboard = Dashboard::start(DashboardCtx {
        paths,
        core_tx,
        status: status_rx,
        exe_path: std::env::current_exe()?,
        preview: true,
    })?;
    println!("{}", dashboard.url());
    std::fs::write(std::env::temp_dir().join("rc-dashboard-preview.url"), dashboard.url())?;

    // Keep the numbers moving so FPS and the timer render like a live session.
    loop {
        std::thread::sleep(Duration::from_millis(100));
        stats.frames_sent.fetch_add(6, Ordering::Relaxed);
    }
}
