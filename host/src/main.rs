//! Remote Control — Windows host (tray application).
//!
//! Milestone 1 scope: installs, starts with Windows, runs quietly in the tray,
//! creates/loads a sealed device identity, and stays near-idle. Screen capture,
//! networking, pairing and sessions arrive in later milestones behind the same
//! [`core`] loop.
//!
//! Threading model:
//!   * **main thread** — Win32 message loop + tray icon ([`platform::tray`]).
//!   * **`rc-core` thread** — a single-threaded Tokio runtime running
//!     [`engine::run`]. Talks to the UI thread over channels only.

// Release builds have no console window; debug builds keep one for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dashboard;
mod diagnostics;
mod engine;
mod identity;
mod paired;
mod platform;
mod relay;
mod session;
mod settings;
mod update;

use std::sync::Mutex;

use anyhow::{Context, Result};
use rc_common::{AppPaths, HOST_SINGLE_INSTANCE_MUTEX};

use crate::platform::single_instance::InstanceGuard;
use crate::settings::Settings;

fn main() -> Result<()> {
    let paths = AppPaths::resolve().context("resolving application directories")?;

    let show_console = cfg!(debug_assertions) || std::env::args().any(|a| a == "--console");
    let _log_guard =
        rc_common::init_logging(&paths.logs_dir, show_console).context("initialising logging")?;

    std::panic::set_hook(Box::new(|info| {
        tracing::error!(panic = %info, "unhandled panic on some thread");
    }));

    // Developer tool: the app window with made-up data, alongside (not instead
    // of) any running host — see `dashboard::run_preview`.
    if std::env::args().any(|a| a == "--dashboard-preview") {
        return dashboard::run_preview();
    }

    // Physical-pixel coordinates everywhere (capture size, input mapping,
    // cursor). Must happen before any window is created.
    rc_input::set_dpi_aware();

    let launched_at_logon = std::env::args().any(|a| a == platform::autostart::AUTOSTART_ARG);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        launched_at_logon,
        "host starting"
    );
    // Measured on a 1080p60 capture: a debug build encodes roughly a quarter
    // of the frames a release build does, because the BGRA→NV12 conversion is
    // a per-pixel loop over two million pixels every frame. It looks like a
    // network problem from the viewer's side, so say so plainly here.
    #[cfg(debug_assertions)]
    tracing::warn!(
        "this is a DEBUG build — the video pipeline runs several times slower \
         than release and will drop most frames; build with --release for real use"
    );

    // Single instance: a second launch just asks the first to show settings.
    let instance = match InstanceGuard::acquire(HOST_SINGLE_INSTANCE_MUTEX)
        .context("acquiring single-instance mutex")?
    {
        Some(guard) => guard,
        None => {
            tracing::info!("another instance is already running; signalling it");
            platform::message_window::signal_show_settings();
            return Ok(());
        }
    };

    let config_existed = paths.config_file().exists();
    let mut settings = Settings::load(&paths.config_file()).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "config unreadable; using defaults");
        Settings::default()
    });

    // Installer-only flag: the user ticked "start with Windows" in the setup
    // wizard. We still route it through the same preference the tray checkbox
    // writes, rather than the installer touching the registry itself, so
    // there's exactly one place that decides autostart.
    let enable_autostart_flag = std::env::args().any(|a| a == "--enable-autostart");
    if enable_autostart_flag {
        settings.start_with_windows = true;
    }

    // First run: materialise config.toml so "Open Settings" has a file to show
    // and the user can see every available option with its default. Also
    // persist here if the installer's autostart flag just changed the
    // in-memory default, so it survives past this one process launch.
    if !config_existed || enable_autostart_flag {
        if let Err(e) = settings.save(&paths.config_file()) {
            tracing::warn!(error = %e, "could not write initial config.toml");
        } else {
            tracing::info!(path = %paths.config_file().display(), "wrote initial config");
        }
    }

    // Keep the registry autostart entry consistent with the saved preference.
    if let Err(e) = platform::autostart::reconcile(settings.start_with_windows) {
        tracing::warn!(error = %e, "could not reconcile autostart entry");
    }

    // Channels between the UI thread and the async core.
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (status_tx, status_rx) = tokio::sync::watch::channel(engine::CoreStatus::default());

    let core_paths = paths.clone();
    let core_settings = settings.clone();
    let core_thread = std::thread::Builder::new()
        .name("rc-core".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build core Tokio runtime");
            if let Err(e) = rt.block_on(engine::run(core_paths, core_settings, cmd_rx, status_tx)) {
                tracing::error!(error = %e, "core exited with an error");
            }
        })
        .context("spawning core thread")?;

    let ctx = platform::tray::AppContext {
        paths,
        settings: Mutex::new(settings),
        core_tx: cmd_tx.clone(),
        status: status_rx,
        exe_path: std::env::current_exe().context("locating own executable")?,
        instance_guard: Mutex::new(Some(instance)),
        dashboard: std::sync::OnceLock::new(),
        // A fresh install: show the app so the PC ID and QR code are right there.
        open_on_start: !config_existed,
    };

    // Blocks on the Win32 message loop until Exit / logoff.
    let ui_result = platform::tray::run(ctx);

    // Orderly shutdown: tell the core to stop, then wait for the thread so all
    // resources unwind before the process exits.
    let _ = cmd_tx.send(engine::CoreCommand::Shutdown);
    if core_thread.join().is_err() {
        tracing::error!("core thread panicked during shutdown");
    }

    tracing::info!("host stopped");
    ui_result
}
