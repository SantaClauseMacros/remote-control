//! Shared primitives for every Remote Control component:
//! on-disk layout, logging setup, and a few cross-crate constants.
//!
//! This crate is deliberately tiny and dependency-light so it can be pulled in
//! by the host, the desktop client and the signaling server alike.

use std::fs;
use std::path::{Path, PathBuf};

#[cfg(windows)]
pub mod dpapi;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Human-facing product name (tray tooltip, installer, window titles).
pub const APP_NAME: &str = "Remote Control";
/// Internal short name — safe for registry values, mutex and window-class names.
pub const APP_SLUG: &str = "RemoteControl";
/// `HKCU\...\Run` value name used for the "Start with Windows" feature.
pub const RUN_VALUE_NAME: &str = "RemoteControlHost";
/// Win32 window-class name for the host's hidden message window.
pub const HOST_WINDOW_CLASS: &str = "RemoteControlHostWnd";
/// Per-user named mutex enforcing a single running host instance.
pub const HOST_SINGLE_INSTANCE_MUTEX: &str = "Local\\RemoteControlHost.SingleInstance";
/// Custom window message: "bring the settings UI to the foreground".
/// `WM_APP` (0x8000) + 1.
pub const WM_APP_SHOW_SETTINGS: u32 = 0x8000 + 1;

/// Resolved per-user directories for config, data and logs.
///
/// On Windows these land under `%APPDATA%\RemoteControl` (config) and
/// `%LOCALAPPDATA%\RemoteControl\data` (identity, logs) — never under
/// Program Files, so the host needs no elevation to run.
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl AppPaths {
    /// Resolve the standard locations, creating any that are missing.
    pub fn resolve() -> Result<Self> {
        let pd = ProjectDirs::from("dev", "RemoteControl", APP_SLUG)
            .context("could not determine user profile directories")?;
        let config_dir = pd.config_dir().to_path_buf();
        let data_dir = pd.data_local_dir().to_path_buf();
        let logs_dir = data_dir.join("logs");
        for d in [&config_dir, &data_dir, &logs_dir] {
            fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(Self {
            config_dir,
            data_dir,
            logs_dir,
        })
    }

    /// Human-editable configuration file.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// DPAPI-sealed device identity blob (never plaintext key material).
    pub fn identity_file(&self) -> PathBuf {
        self.data_dir.join("identity.bin")
    }
}

/// Initialise `tracing` with a rotating daily log file in `logs_dir`, plus an
/// optional stderr layer for foreground / debug runs.
///
/// The returned [`WorkerGuard`] **must** stay alive for the whole process or
/// buffered log lines are dropped on exit.
///
/// Level can be overridden with the `RC_LOG` environment variable
/// (e.g. `RC_LOG=rc_host=trace`).
pub fn init_logging(logs_dir: &Path, to_console: bool) -> Result<WorkerGuard> {
    let file_appender = tracing_appender::rolling::daily(logs_dir, "host.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_env("RC_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,rc_host=debug,rc_transport=debug"));

    let file_layer = fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true);

    let subscriber = tracing_subscriber::registry().with(filter).with(file_layer);

    if to_console {
        subscriber
            .with(fmt::layer().with_writer(std::io::stderr).with_target(false))
            .init();
    } else {
        subscriber.init();
    }

    Ok(guard)
}

/// Redact all but the last `keep` characters of a secret-ish string for logs.
pub fn redact(s: &str, keep: usize) -> String {
    let n = s.chars().count();
    if n <= keep {
        return "*".repeat(n);
    }
    let tail: String = s.chars().skip(n - keep).collect();
    format!("{}{}", "*".repeat(n - keep), tail)
}
