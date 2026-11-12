//! The async "core" of the host: everything that is *not* the Win32 UI thread.
//!
//! When remote access is enabled it accepts sessions two ways — a LAN
//! [`LanListener`], and (if `network.signaling_url` is set) a connection parked
//! at the rendezvous relay so clients on other networks can reach it. One
//! session runs at a time. When idle its cost is a per-minute heartbeat.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rc_common::AppPaths;
use rc_transport::lan::{LanListener, LanSession};
use tokio::sync::{mpsc, watch};

use crate::relay::Splice;
use tokio::task::JoinHandle;

use crate::identity;
use crate::mic_only;
use crate::paired::PairedStore;
use crate::session::{self, SessionParams};
use crate::settings::Settings;
use rc_protocol::ClientMessage;

/// How many devices can control the PC at the same time. A third connection
/// replaces the one that has been connected longest.
const MAX_SESSIONS: usize = 2;
/// How long a new connection gets to say it is mic-only (its first message)
/// before it is treated as a normal controlling device.
const FIRST_MESSAGE_WAIT: Duration = Duration::from_millis(1500);

/// One running controlling session.
struct Slot {
    id: u64,
    peer_key: [u8; 32],
    task: JoinHandle<()>,
    stop: watch::Sender<bool>,
    cmd_tx: mpsc::UnboundedSender<session::SessionCommand>,
    info: SessionInfo,
}

/// The running mic-only link, if any (see [`mic_only`]).
struct MicSlot {
    id: u64,
    task: JoinHandle<()>,
    stop: watch::Sender<bool>,
}

/// Everything that is connected right now.
struct Live {
    slots: Vec<Slot>,
    mic: Option<MicSlot>,
    next_id: u64,
    /// Each task reports its id here when it ends.
    ended_tx: mpsc::UnboundedSender<u64>,
}

/// Fallback TCP port when `network.listen_port` is 0. From the IANA dynamic
/// range; not registered, not privileged.
pub const DEFAULT_LISTEN_PORT: u16 = 9877;

/// Commands sent from the UI thread to the core.
#[derive(Debug)]
pub enum CoreCommand {
    ReloadSettings,
    DisconnectAll,
    /// Clear the remembered-device list (they can still connect with the PC ID).
    ForgetPaired,
    /// Send a file to the currently connected device, if any.
    SendFileToClient(PathBuf),
    /// Download the given URL (a `latest.json` `downloadUrl`) and install it
    /// silently, then restart into it.
    InstallUpdate(String),
    Shutdown,
}

/// Coarse lifecycle state, surfaced in the tray menu and (later) the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoreState {
    #[default]
    Starting,
    Disabled,
    /// Enabled but the listener is not up (e.g. the port is in use).
    Idle,
    /// Listening / discoverable, waiting for a client.
    Listening,
    /// A client is connected.
    Connected,
}

/// Snapshot of core status shared with the UI thread via a [`watch`] channel.
#[derive(Debug, Clone, Default)]
pub struct CoreStatus {
    pub state: CoreState,
    pub device_id: String,
    pub sessions: u32,
    /// A mic-only link is connected (it is not counted in `sessions`).
    pub mic_only: bool,
    /// Port the LAN listener is bound to (0 if not listening).
    pub listen_port: u16,
    /// `true` while relay parking is enabled.
    pub relay_parked: bool,
    /// Live: parked at the relay right now, so reachable from any network.
    pub relay_online: Arc<AtomicBool>,
    /// Number of remembered (paired) devices.
    pub paired_count: usize,
    /// `Some((version, release_url, notes, installer_url))` once the update
    /// checker finds a newer release. `installer_url` is empty when the
    /// manifest didn't provide one (older manifest, or a hand-edited one) —
    /// there's just no one-click install to offer then. Never populated
    /// unless `update.check_url` is configured.
    pub update_available: Option<(String, String, String, String)>,
    /// The connected device, while a session is running.
    pub session: Option<SessionInfo>,
}

/// Who's connected and how it's going — for the dashboard.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// `"via relay"`, or the LAN peer's address.
    pub route: String,
    /// Short id of the connecting device's key.
    pub key_id: String,
    /// Unix seconds.
    pub since: u64,
    pub stats: Arc<session::LiveStats>,
}

/// Run the core until [`CoreCommand::Shutdown`] (or the command channel closes).
pub async fn run(
    paths: AppPaths,
    mut settings: Settings,
    mut cmd_rx: mpsc::UnboundedReceiver<CoreCommand>,
    status_tx: watch::Sender<CoreStatus>,
) -> anyhow::Result<()> {
    let identity = identity::load_or_create(&paths).context("initialising device identity")?;
    let device_id = identity.device_id();
    let host_static = *identity.x25519_secret();
    let mut paired = PairedStore::load(&paths.data_dir);
    let mut status = CoreStatus {
        device_id: device_id.clone(),
        paired_count: paired.count(),
        ..Default::default()
    };
    tracing::info!(%device_id, paired = paired.count(), "device identity ready");

    let mut listener: Option<LanListener> = None;
    let mut advertiser: Option<rc_discovery::Advertiser> = None;
    let mut relay_rx: Option<mpsc::Receiver<Splice>> = None;
    let mut relay_stop: Option<watch::Sender<bool>> = None;
    let (ended_tx, mut ended_rx) = mpsc::unbounded_channel::<u64>();
    let mut live = Live { slots: Vec::new(), mic: None, next_id: 1, ended_tx };
    let mut update_rx = crate::update::spawn(paths.clone());

    apply_listening_state(
        &settings,
        &device_id,
        host_static,
        &mut listener,
        &mut advertiser,
        &mut status,
    )
    .await;
    apply_relay_state(
        &settings,
        &device_id,
        &mut relay_rx,
        &mut relay_stop,
        &mut status,
    );
    crate::platform::power::set_prevent_sleep(settings.power.prevent_sleep);
    publish(&status_tx, &status);

    let mut heartbeat = tokio::time::interval(Duration::from_secs(60));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                tracing::debug!(state = ?status.state, sessions = status.sessions, "heartbeat");
            }

            Ok(()) = update_rx.changed() => {
                if let Some(info) = update_rx.borrow().clone() {
                    status.update_available = Some((info.version, info.url, info.notes, info.download_url));
                    publish(&status_tx, &status);
                }
            }

            // LAN accept — while listening and there's room for another device.
            accepted = async { listener.as_ref().unwrap().accept().await },
                if listener.is_some() && live.slots.len() < MAX_SESSIONS =>
            {
                match accepted {
                    Ok((sess, peer)) => {
                        live.admit(
                            sess, peer.to_string(), host_static, &device_id, &settings, &mut paired, &mut status,
                        ).await;
                        live.sync_status(&mut status, listener.is_some(), &settings);
                        publish(&status_tx, &status);
                    }
                    Err(e) => tracing::warn!(error = %e, "LAN accept/handshake failed"),
                }
            }

            // Relay splice — a client reached us through the rendezvous server.
            // Also taken while sessions are "running": a phone that lost wifi
            // never says goodbye, so its dead session would otherwise sit
            // here and the same phone could never get back in. The new client
            // has to pass the handshake (it knows the PC's secret) before any
            // old session is replaced.
            relayed = async { relay_rx.as_mut().unwrap().recv().await },
                if relay_rx.is_some() =>
            {
                if let Some((stream, ack)) = relayed {
                    // Tell the parker we've taken this stream so it can re-park.
                    let _ = ack.send(());
                    let hs = tokio::time::timeout(
                        Duration::from_secs(6),
                        LanSession::over_stream_responder(stream, &host_static, &device_id),
                    ).await;
                    match hs {
                        Ok(Ok(sess)) => {
                            live.admit(
                                sess, "via relay".to_string(), host_static, &device_id, &settings, &mut paired, &mut status,
                            ).await;
                            live.sync_status(&mut status, listener.is_some(), &settings);
                            publish(&status_tx, &status);
                        }
                        Ok(Err(e)) => tracing::warn!(error = %e, "relayed handshake failed"),
                        Err(_) => tracing::warn!("relayed handshake timed out"),
                    }
                }
            }

            // A session (or the mic-only link) finished.
            Some(id) = ended_rx.recv() => {
                tracing::info!(id, "session task finished");
                live.slots.retain(|s| s.id != id);
                if live.mic.as_ref().is_some_and(|m| m.id == id) {
                    live.mic = None;
                }
                live.sync_status(&mut status, listener.is_some(), &settings);
                publish(&status_tx, &status);
            }

            cmd = cmd_rx.recv() => match cmd {
                Some(CoreCommand::ReloadSettings) => {
                    match Settings::load(&paths.config_file()) {
                        Ok(new) => {
                            let relisten = new.enable_remote_access != settings.enable_remote_access
                                || new.network.listen_port != settings.network.listen_port
                                || new.network.lan_discovery != settings.network.lan_discovery
                                || new.computer_name != settings.computer_name;
                            let re_relay = new.enable_remote_access != settings.enable_remote_access
                                || new.network.signaling_url != settings.network.signaling_url
                                || new.network.relay_key != settings.network.relay_key;
                            if new.power.prevent_sleep != settings.power.prevent_sleep {
                                crate::platform::power::set_prevent_sleep(new.power.prevent_sleep);
                            }
                            settings = new;
                            if live.slots.is_empty() && live.mic.is_none() {
                                if relisten {
                                    listener = None;
                                    advertiser = None;
                                    apply_listening_state(
                                        &settings, &device_id, host_static,
                                        &mut listener, &mut advertiser, &mut status,
                                    ).await;
                                }
                                if re_relay {
                                    if let Some(s) = relay_stop.take() { let _ = s.send(true); }
                                    relay_rx = None;
                                    apply_relay_state(
                                        &settings, &device_id, &mut relay_rx, &mut relay_stop, &mut status,
                                    );
                                }
                            }
                            publish(&status_tx, &status);
                            tracing::info!("settings reloaded");
                        }
                        Err(e) => tracing::warn!(error = %e, "settings reload failed; keeping current"),
                    }
                }
                Some(CoreCommand::DisconnectAll) => {
                    if live.slots.is_empty() && live.mic.is_none() {
                        tracing::info!("disconnect-all: nothing connected");
                    } else {
                        tracing::info!("disconnecting active sessions");
                        live.stop_all();
                    }
                }
                Some(CoreCommand::ForgetPaired) => {
                    paired.forget_all();
                    status.paired_count = 0;
                    tracing::info!("all paired devices revoked");
                    publish(&status_tx, &status);
                }
                Some(CoreCommand::SendFileToClient(path)) => {
                    // To the device that connected most recently.
                    match live.slots.last() {
                        Some(s) => { let _ = s.cmd_tx.send(session::SessionCommand::SendFile(path)); }
                        None => tracing::info!("send file requested, but nothing is connected"),
                    }
                }
                Some(CoreCommand::InstallUpdate(download_url)) => {
                    tokio::spawn(async move {
                        match tokio::task::spawn_blocking(move || crate::update::apply_update(&download_url)).await {
                            Ok(Ok(())) => {
                                tracing::info!("update downloaded; restarting to install it");
                                crate::platform::message_window::post_command(crate::platform::tray::ID_EXIT);
                            }
                            Ok(Err(e)) => tracing::warn!(error = ?e, "installing the update failed"),
                            Err(e) => tracing::warn!(error = ?e, "update install task panicked"),
                        }
                    });
                }
                Some(CoreCommand::Shutdown) | None => {
                    tracing::info!("core shutting down");
                    crate::platform::power::set_prevent_sleep(false);
                    if let Some(s) = &relay_stop { let _ = s.send(true); }
                    live.stop_all();
                    for task in live.take_tasks() {
                        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
                    }
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Check who is connecting: a first-time device proves it by knowing the PC
/// ID; a returning one must be on the allow-list. Returns the device's short
/// key id, or `None` if it was turned away.
fn authorize(
    sess: &LanSession,
    peer_label: &str,
    paired: &mut PairedStore,
    status: &mut CoreStatus,
) -> Option<String> {
    let peer_key = sess.peer_key();
    let key_id = rc_crypto::DeviceIdentity::key_short_id(&peer_key);

    if sess.was_pairing() {
        // The client proved it knows this PC's ID — that's all that's needed.
        paired.upsert(&peer_key, peer_label);
        status.paired_count = paired.count();
        tracing::info!(%key_id, "device connected with the PC ID");
    } else {
        // Reconnect: must be on the allow-list.
        if !paired.is_paired(&peer_key) {
            tracing::warn!(peer = %peer_label, %key_id, "unpaired device tried to reconnect — rejected");
            return None;
        }
        paired.touch(&peer_key);
        status.paired_count = paired.count();
    }
    Some(key_id)
}

impl Live {
    /// Take in a connection that has passed the handshake: a mic-only link
    /// (which never disturbs anything already connected) or a controlling
    /// session (up to [`MAX_SESSIONS`] at once).
    #[allow(clippy::too_many_arguments)]
    async fn admit(
        &mut self,
        sess: LanSession,
        peer_label: String,
        host_static: [u8; 32],
        device_id: &str,
        settings: &Settings,
        paired: &mut PairedStore,
        status: &mut CoreStatus,
    ) {
        let Some(key_id) = authorize(&sess, &peer_label, paired, status) else {
            return;
        };

        // A mic-only device says so first. The message stays queued, so a
        // normal device's first message is still read by its session.
        let mic_only = match sess.peek_first_control(FIRST_MESSAGE_WAIT).await {
            Some(first) => matches!(rc_protocol::decode::<ClientMessage>(&first), Ok(ClientMessage::MicOnly)),
            None => false,
        };

        let id = self.next_id;
        self.next_id += 1;
        let ended_tx = self.ended_tx.clone();
        let (stop_tx, stop_rx) = watch::channel(false);

        if mic_only {
            // A new mic-only link replaces the old one (a dead connection never
            // says goodbye).
            if let Some(old) = self.mic.take() {
                tracing::info!("a new mic-only connection arrived; replacing the previous one");
                let _ = old.stop.send(true);
                let _ = tokio::time::timeout(Duration::from_secs(3), old.task).await;
            }
            tracing::info!(peer = %peer_label, %key_id, "connection accepted; mic-only");
            let task = tokio::spawn(async move {
                if let Err(e) = mic_only::run(sess, stop_rx).await {
                    tracing::error!(error = ?e, "mic-only link ended with error");
                }
                let _ = ended_tx.send(id);
            });
            self.mic = Some(MicSlot { id, task, stop: stop_tx });
            return;
        }

        // The same device reconnecting replaces its own old session; with no
        // room left, the one connected longest makes way.
        let peer_key = sess.peer_key();
        let victim = self
            .slots
            .iter()
            .position(|s| s.peer_key == peer_key)
            .or_else(|| (self.slots.len() >= MAX_SESSIONS).then_some(0));
        if let Some(i) = victim {
            tracing::info!("a new connection arrived; replacing a previous session");
            let old = self.slots.remove(i);
            let _ = old.stop.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(8), old.task).await;
        }

        tracing::info!(peer = %peer_label, %key_id, "connection accepted; starting session");
        let params = SessionParams {
            fps: settings.performance.max_fps.clamp(5, 120) as u32,
            max_bitrate_kbps: settings.performance.max_bitrate_kbps,
            mode: settings.performance.mode,
            clipboard_sync: settings.security.clipboard_sync,
            audio: settings.audio.enabled,
            mic: settings.mic.enabled,
            uncap_fps_on_direct: settings.performance.uncap_fps_on_direct,
            multi_monitor: settings.display.multi_monitor,
            host_static,
            device_id: device_id.to_string(),
            peer_key,
        };
        let stats = Arc::new(session::LiveStats::default());
        let task_stats = stats.clone();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            if let Err(e) = session::run(sess, params, stop_rx, cmd_rx, task_stats).await {
                tracing::error!(error = ?e, "session ended with error");
            }
            let _ = ended_tx.send(id);
        });
        let info = SessionInfo {
            route: peer_label,
            key_id,
            since: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            stats,
        };
        self.slots.push(Slot { id, peer_key, task, stop: stop_tx, cmd_tx, info });
    }

    /// Ask everything that is connected to end.
    fn stop_all(&self) {
        for s in &self.slots {
            let _ = s.stop.send(true);
        }
        if let Some(m) = &self.mic {
            let _ = m.stop.send(true);
        }
    }

    /// Hand back every running task (to wait for them on shutdown).
    fn take_tasks(&mut self) -> Vec<JoinHandle<()>> {
        let mut tasks: Vec<JoinHandle<()>> = self.slots.drain(..).map(|s| s.task).collect();
        if let Some(m) = self.mic.take() {
            tasks.push(m.task);
        }
        tasks
    }

    /// Bring the UI-facing status in line with what is connected.
    fn sync_status(&self, status: &mut CoreStatus, listening: bool, settings: &Settings) {
        status.sessions = self.slots.len() as u32;
        status.session = self.slots.last().map(|s| s.info.clone());
        status.mic_only = self.mic.is_some();
        status.state = if !self.slots.is_empty() {
            CoreState::Connected
        } else if listening {
            CoreState::Listening
        } else {
            state_without_listener(settings)
        };
    }
}

async fn apply_listening_state(
    settings: &Settings,
    device_id: &str,
    host_static: [u8; 32],
    listener: &mut Option<LanListener>,
    advertiser: &mut Option<rc_discovery::Advertiser>,
    status: &mut CoreStatus,
) {
    *advertiser = None;
    if !settings.enable_remote_access {
        *listener = None;
        status.state = CoreState::Disabled;
        status.listen_port = 0;
        return;
    }

    let port = if settings.network.listen_port != 0 {
        settings.network.listen_port
    } else {
        DEFAULT_LISTEN_PORT
    };
    // The PC ID is the pairing secret: knowing it is enough to connect.
    match LanListener::bind(("0.0.0.0", port), device_id, host_static).await {
        Ok(l) => {
            let bound = l.local_addr().map(|a| a.port()).unwrap_or(port);
            *listener = Some(l);
            status.state = CoreState::Listening;
            status.listen_port = bound;
            tracing::info!(port = bound, "listening for clients");

            if settings.network.lan_discovery {
                match rc_discovery::Advertiser::start(
                    &settings.computer_name,
                    bound,
                    Some(device_id),
                ) {
                    Ok(a) => *advertiser = Some(a),
                    Err(e) => tracing::warn!(error = %e, "mDNS advertise failed"),
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, port, "could not bind listener");
            *listener = None;
            status.state = CoreState::Idle;
            status.listen_port = 0;
        }
    }
}

fn apply_relay_state(
    settings: &Settings,
    device_id: &str,
    relay_rx: &mut Option<mpsc::Receiver<Splice>>,
    relay_stop: &mut Option<watch::Sender<bool>>,
    status: &mut CoreStatus,
) {
    *relay_rx = None;
    *relay_stop = None;
    status.relay_parked = false;
    status.relay_online.store(false, Ordering::Relaxed);

    if !settings.enable_remote_access {
        return;
    }
    let Some(addr) = settings
        .network
        .signaling_url
        .clone()
        .filter(|s| !s.trim().is_empty())
    else {
        return;
    };

    let (stop_tx, stop_rx) = watch::channel(false);
    let rx = crate::relay::spawn(
        addr.clone(),
        device_id.to_string(),
        settings.network.relay_key.clone(),
        stop_rx,
        status.relay_online.clone(),
    );
    *relay_rx = Some(rx);
    *relay_stop = Some(stop_tx);
    status.relay_parked = true;
    tracing::info!(relay = %addr, "relay parking enabled");
}

fn state_without_listener(settings: &Settings) -> CoreState {
    if settings.enable_remote_access {
        CoreState::Idle
    } else {
        CoreState::Disabled
    }
}

fn publish(tx: &watch::Sender<CoreStatus>, status: &CoreStatus) {
    let _ = tx.send(status.clone());
}
