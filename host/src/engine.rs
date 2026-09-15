//! The async "core" of the host: everything that is *not* the Win32 UI thread.
//!
//! When remote access is enabled it accepts sessions two ways — a LAN
//! [`LanListener`], and (if `network.signaling_url` is set) a connection parked
//! at the rendezvous relay so clients on other networks can reach it. One
//! session runs at a time. When idle its cost is a per-minute heartbeat.

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
use crate::paired::PairedStore;
use crate::session::{self, SessionParams};
use crate::settings::Settings;

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
    /// Port the LAN listener is bound to (0 if not listening).
    pub listen_port: u16,
    /// `true` while relay parking is enabled.
    pub relay_parked: bool,
    /// Live: parked at the relay right now, so reachable from any network.
    pub relay_online: Arc<AtomicBool>,
    /// Number of remembered (paired) devices.
    pub paired_count: usize,
    /// `Some((version, download_url, notes))` once the update checker finds a
    /// newer release. Never populated unless `update.check_url` is configured.
    pub update_available: Option<(String, String, String)>,
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
    let mut session_task: Option<JoinHandle<()>> = None;
    let mut session_stop: Option<watch::Sender<bool>> = None;
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
                    status.update_available = Some((info.version, info.url, info.notes));
                    publish(&status_tx, &status);
                }
            }

            // LAN accept — only while listening and idle.
            accepted = async { listener.as_ref().unwrap().accept().await },
                if listener.is_some() && session_task.is_none() =>
            {
                match accepted {
                    Ok((sess, peer)) => {
                        begin_session(
                            sess, peer.to_string(), host_static, &device_id, &settings, &mut paired,
                            &mut status, &status_tx, &mut session_task, &mut session_stop,
                        ).await;
                    }
                    Err(e) => tracing::warn!(error = %e, "LAN accept/handshake failed"),
                }
            }

            // Relay splice — a client reached us through the rendezvous server.
            relayed = async { relay_rx.as_mut().unwrap().recv().await },
                if relay_rx.is_some() && session_task.is_none() =>
            {
                if let Some((stream, ack)) = relayed {
                    // Tell the parker we've taken this stream so it can re-park;
                    // we only reach this branch when no session is running, so
                    // the parker never buffers a stream a client has abandoned.
                    let _ = ack.send(());
                    let hs = tokio::time::timeout(
                        Duration::from_secs(6),
                        LanSession::over_stream_responder(stream, &host_static, &device_id),
                    ).await;
                    match hs {
                        Ok(Ok(sess)) => {
                            begin_session(
                                sess, "via relay".to_string(), host_static, &device_id, &settings, &mut paired,
                                &mut status, &status_tx, &mut session_task, &mut session_stop,
                            ).await;
                        }
                        Ok(Err(e)) => tracing::warn!(error = %e, "relayed handshake failed"),
                        Err(_) => tracing::warn!("relayed handshake timed out"),
                    }
                }
            }

            // The active session finished.
            _ = async { session_task.as_mut().unwrap().await.ok(); }, if session_task.is_some() => {
                tracing::info!("session task finished");
                session_task = None;
                session_stop = None;
                status.sessions = 0;
                status.session = None;
                status.state = if listener.is_some() {
                    CoreState::Listening
                } else {
                    state_without_listener(&settings)
                };
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
                            settings = new;
                            if session_task.is_none() {
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
                    if let Some(stop) = &session_stop {
                        tracing::info!("disconnecting active session");
                        let _ = stop.send(true);
                    } else {
                        tracing::info!("disconnect-all: nothing connected");
                    }
                }
                Some(CoreCommand::ForgetPaired) => {
                    paired.forget_all();
                    status.paired_count = 0;
                    tracing::info!("all paired devices revoked");
                    publish(&status_tx, &status);
                }
                Some(CoreCommand::Shutdown) | None => {
                    tracing::info!("core shutting down");
                    if let Some(s) = &relay_stop { let _ = s.send(true); }
                    if let Some(stop) = &session_stop { let _ = stop.send(true); }
                    if let Some(task) = session_task.take() {
                        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
                    }
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Authenticate (the PC ID, or the paired allow-list for older clients) then
/// spawn a session for an accepted connection.
#[allow(clippy::too_many_arguments)]
async fn begin_session(
    sess: LanSession,
    peer_label: String,
    host_static: [u8; 32],
    device_id: &str,
    settings: &Settings,
    paired: &mut PairedStore,
    status: &mut CoreStatus,
    status_tx: &watch::Sender<CoreStatus>,
    session_task: &mut Option<JoinHandle<()>>,
    session_stop: &mut Option<watch::Sender<bool>>,
) {
    let peer_key = sess.peer_key();
    let key_id = rc_crypto::DeviceIdentity::key_short_id(&peer_key);

    if sess.was_pairing() {
        // The client proved it knows this PC's ID — that's all that's needed.
        paired.upsert(&peer_key, &peer_label);
        status.paired_count = paired.count();
        tracing::info!(%key_id, "device connected with the PC ID");
    } else {
        // Reconnect: must be on the allow-list.
        if !paired.is_paired(&peer_key) {
            tracing::warn!(peer = %peer_label, %key_id, "unpaired device tried to reconnect — rejected");
            drop(sess);
            return;
        }
        paired.touch(&peer_key);
        status.paired_count = paired.count();
    }

    tracing::info!(peer = %peer_label, %key_id, "connection accepted; starting session");
    let (stop_tx, stop_rx) = watch::channel(false);
    let params = SessionParams {
        fps: settings.performance.max_fps.clamp(5, 120) as u32,
        max_bitrate_kbps: settings.performance.max_bitrate_kbps,
        mode: settings.performance.mode,
        clipboard_sync: settings.security.clipboard_sync,
        audio: settings.audio.enabled,
        host_static,
        device_id: device_id.to_string(),
        peer_key,
    };
    let stats = Arc::new(session::LiveStats::default());
    let task_stats = stats.clone();
    *session_stop = Some(stop_tx);
    *session_task = Some(tokio::spawn(async move {
        if let Err(e) = session::run(sess, params, stop_rx, task_stats).await {
            tracing::error!(error = ?e, "session ended with error");
        }
    }));
    status.session = Some(SessionInfo {
        route: peer_label,
        key_id,
        since: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        stats,
    });
    status.state = CoreState::Connected;
    status.sessions = 1;
    publish(status_tx, status);
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
