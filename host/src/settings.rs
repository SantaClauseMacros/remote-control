//! Persistent host configuration (`%APPDATA%\RemoteControl\config.toml`).
//!
//! Every field maps to a control in the settings window. Missing keys fall back
//! to [`Default`] via `#[serde(default)]`, so a config written by an older
//! version still loads after an upgrade, and unknown keys from a *newer*
//! version are ignored rather than rejected.

use std::path::Path;

use anyhow::{Context, Result};
use rc_protocol::QualityMode;
use serde::{Deserialize, Serialize};

pub use crate::display::MultiMonitorMode;

/// Relay a fresh install parks at, so a PC is reachable from the web app with
/// no setup. Override at build time with `RC_DEFAULT_RELAY`; a
/// `network.signaling_url` already in config.toml always wins.
pub const DEFAULT_RELAY: &str = match option_env!("RC_DEFAULT_RELAY") {
    Some(url) => url,
    None => "wss://remote-control.bloxvault8436200.workers.dev/relay",
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Friendly name shown in clients' device lists.
    pub computer_name: String,
    /// Master switch. While `false` the host never listens for connections.
    pub enable_remote_access: bool,
    /// Mirrors the platform autostart entry so the UI can show its state.
    /// The registry is the source of truth; this is reconciled on startup.
    pub start_with_windows: bool,
    pub performance: PerformanceSettings,
    pub network: NetworkSettings,
    pub security: SecuritySettings,
    pub update: UpdateSettings,
    pub audio: AudioSettings,
    pub mic: MicSettings,
    pub power: PowerSettings,
    pub display: DisplaySettings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MicSettings {
    /// Play the connected device's microphone on this PC. Off by default: it
    /// needs a one-time mic permission on the device, and — without a virtual
    /// audio cable installed (see the README) — it plays out loud on this
    /// PC's speakers rather than being usable as a mic in Discord, a game,
    /// etc, which would be a surprise to turn on silently.
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerSettings {
    /// Keep this PC from sleeping while Remote Control is running, so it's
    /// still reachable if nobody touches it for a while. Off by default —
    /// most people don't want their PC awake all night for this.
    pub prevent_sleep: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplaySettings {
    /// How to handle a PC with more than one monitor for a session — see
    /// `crate::display`.
    pub multi_monitor: MultiMonitorMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Stream whatever this PC is playing to the connected device.
    pub enabled: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceSettings {
    pub mode: QualityMode,
    /// Hard cap on frames per second sent to a client.
    pub max_fps: u16,
    /// Hard cap on video bitrate. `0` = uncapped (congestion control decides).
    pub max_bitrate_kbps: u32,
    /// Prefer a GPU encoder (NVENC / AMF / QSV) when one is available.
    pub use_hardware_encoder: bool,
    /// Once a session is on a direct (same-network) path, double the frame
    /// rate target — a direct link usually has plenty of headroom, so this is
    /// where a game actually benefits from more than 30-60 FPS. Ignored on
    /// the relay, which is exactly the link this can't help.
    pub uncap_fps_on_direct: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSettings {
    /// UDP port the host binds for direct / LAN connections. `0` = OS-assigned.
    pub listen_port: u16,
    /// Advertise on the local network via mDNS for one-tap discovery.
    pub lan_discovery: bool,
    /// Rendezvous relay address for connecting across networks. When set, the
    /// host dials out and parks a connection there so a client on any network
    /// can reach it. Defaults to [`DEFAULT_RELAY`]; `""` keeps the host
    /// LAN-only.
    ///
    /// Two forms: `host:port` dials a self-hosted `rc-relay` over raw TCP;
    /// `wss://…` (or `ws://…`) dials a relay that only speaks HTTP — a
    /// Cloudflare Worker, for instance — over a WebSocket instead. Same wire
    /// protocol either way, so a client never needs to know which the host used.
    pub signaling_url: Option<String>,
    /// Optional shared secret the relay requires (`--key` on `rc-relay`).
    pub relay_key: Option<String>,
    /// Permit a TURN relay for direct-P2P fallback (WebRTC path, milestone 4b).
    pub allow_relay: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecuritySettings {
    /// Require a numeric PIN in addition to a paired device key.
    pub require_pin: bool,
    /// Argon2 hash of the PIN. Never the PIN itself; `None` when unset.
    pub pin_hash: Option<String>,
    /// Drop a session after this many minutes with no input. `0` = never.
    pub session_timeout_minutes: u32,
    /// Show the persistent on-screen "someone is connected" indicator.
    pub connection_indicator: bool,
    /// Sync clipboard **text** between host and client while connected. When
    /// off, nothing is sent or applied in either direction.
    pub clipboard_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// Check this URL for a version manifest (`{"version","url","notes"}` JSON).
    /// Release builds default it to the GitHub release's `latest.json`, baked
    /// in at build time via `RC_UPDATE_URL`. A build without that — or `""`
    /// here — makes no update calls at all.
    pub check_url: Option<String>,
    /// How often to check, in hours. Ignored while `check_url` is `None`.
    pub check_interval_hours: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            computer_name: default_computer_name(),
            enable_remote_access: true,
            start_with_windows: false,
            performance: PerformanceSettings::default(),
            network: NetworkSettings::default(),
            security: SecuritySettings::default(),
            update: UpdateSettings::default(),
            audio: AudioSettings::default(),
            mic: MicSettings::default(),
            power: PowerSettings::default(),
            display: DisplaySettings::default(),
        }
    }
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check_url: option_env!("RC_UPDATE_URL").map(str::to_string),
            check_interval_hours: 12,
        }
    }
}

impl Default for PerformanceSettings {
    fn default() -> Self {
        Self {
            mode: QualityMode::Balanced,
            // A *target*, not a promise: the capture loop skips frames on its
            // own while the link is saturated (see session::capture_loop's
            // net_busy gate), so aiming high costs nothing on a slow link and
            // is what makes games feel smooth on a fast one. Capping this
            // instead just made everything choppy everywhere.
            max_fps: 60,
            max_bitrate_kbps: 0,
            use_hardware_encoder: true,
            uncap_fps_on_direct: true,
        }
    }
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            listen_port: 0,
            lan_discovery: true,
            signaling_url: Some(DEFAULT_RELAY.to_string()),
            relay_key: None,
            allow_relay: true,
        }
    }
}

impl NetworkSettings {
    /// A link that opens the web app — served by the same Worker as the relay —
    /// with this PC ready to add: `wss://host/relay` → `https://host/#add=<ID>`.
    /// `None` without a WebSocket relay (a raw `host:port` `rc-relay` serves no
    /// web app).
    pub fn phone_link(&self, device_id: &str) -> Option<String> {
        let url = self.signaling_url.as_deref()?.trim();
        let (scheme, rest) = if let Some(r) = url.strip_prefix("wss://") {
            ("https", r)
        } else if let Some(r) = url.strip_prefix("ws://") {
            ("http", r)
        } else {
            return None;
        };
        let host = rest.split('/').next().filter(|h| !h.is_empty())?;
        (!device_id.is_empty()).then(|| format!("{scheme}://{host}/#add={device_id}"))
    }
}

impl Default for SecuritySettings {
    fn default() -> Self {
        Self {
            require_pin: false,
            pin_hash: None,
            session_timeout_minutes: 30,
            connection_indicator: true,
            clipboard_sync: true,
        }
    }
}

impl Settings {
    /// Load from `path`, returning [`Settings::default`] if the file does not
    /// exist yet (first run).
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).context("parsing config.toml"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context("reading config.toml"),
        }
    }

    /// Write atomically: serialise to a temp file in the same directory, then
    /// rename over the target so a crash mid-write can't corrupt the config.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serialising config")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes()).context("writing temp config")?;
        std::fs::rename(&tmp, path).context("replacing config")?;
        Ok(())
    }
}

fn default_computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "My PC".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_relay(url: Option<&str>) -> NetworkSettings {
        NetworkSettings {
            signaling_url: url.map(str::to_string),
            ..NetworkSettings::default()
        }
    }

    #[test]
    fn phone_link_opens_the_relays_web_app() {
        assert_eq!(
            with_relay(Some("wss://example.workers.dev/relay"))
                .phone_link("ABCDE-FGHIJ-KLMNO-P")
                .as_deref(),
            Some("https://example.workers.dev/#add=ABCDE-FGHIJ-KLMNO-P")
        );
        assert_eq!(
            with_relay(Some("ws://127.0.0.1:8787/relay")).phone_link("ID").as_deref(),
            Some("http://127.0.0.1:8787/#add=ID")
        );
    }

    #[test]
    fn no_phone_link_without_a_websocket_relay() {
        assert_eq!(with_relay(Some("relay.example.com:9878")).phone_link("ID"), None);
        assert_eq!(with_relay(None).phone_link("ID"), None);
        assert_eq!(with_relay(Some("wss://x/relay")).phone_link(""), None);
    }

    #[test]
    fn fresh_install_uses_the_default_relay() {
        assert_eq!(
            Settings::default().network.signaling_url.as_deref(),
            Some(DEFAULT_RELAY)
        );
    }
}
