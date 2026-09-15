//! System-tray icon, context menu, and the Win32 message loop that drives them.
//!
//! This runs on the process's main thread. The async [`crate::core`] runs on a
//! separate thread; the two communicate over the channels stored in
//! [`AppContext`]. Keeping the UI on its own OS thread means a slow core task
//! can never make the tray menu unresponsive, and vice versa.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rc_common::{APP_NAME, HOST_WINDOW_CLASS, WM_APP_SHOW_SETTINGS};
use tokio::sync::{mpsc, watch};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, KillTimer, LoadIconW,
    PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer,
    SetMenuDefaultItem, SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, GWLP_USERDATA, HICON,
    MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, SW_SHOWNORMAL, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_COMMAND, WM_DESTROY, WM_ENDSESSION, WM_LBUTTONDBLCLK,
    WM_LBUTTONUP, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

use crate::engine::{CoreCommand, CoreState, CoreStatus};
use crate::platform::autostart;
use crate::platform::elevate;
use crate::platform::single_instance::InstanceGuard;
use crate::settings::Settings;

/// Tray-icon callback message (`WM_APP` + 2).
const WM_TRAYICON: u32 = 0x8000 + 2;
/// Our single tray icon's id.
const TRAY_ICON_ID: u32 = 1;

// Context-menu command ids.
const ID_OPEN_SETTINGS: usize = 1;
const ID_RELOAD_SETTINGS: usize = 2;
const ID_TOGGLE_AUTOSTART: usize = 3;
const ID_DISCONNECT_ALL: usize = 4;
const ID_COPY_CODE: usize = 5;
const ID_FORGET_PAIRED: usize = 6;
const ID_COPY_DIAGNOSTICS: usize = 8;
const ID_DOWNLOAD_UPDATE: usize = 9;
pub(crate) const ID_RESTART_ELEVATED: usize = 10;
const ID_COPY_PHONE_LINK: usize = 11;
const ID_OPEN_APP: usize = 12;
const ID_INSTALL_UPDATE: usize = 13;
pub(crate) const ID_EXIT: usize = 7;

/// Timer that refreshes the tray tooltip / fires connect notifications.
const STATUS_TIMER_ID: usize = 1;

thread_local! {
    /// Last `CoreState` we reflected in the UI, as a small int (see `state_code`).
    static LAST_UI_STATE: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
    /// Whether we've already balloon-notified about the current update.
    static UPDATE_NOTIFIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Everything the message loop / WNDPROC needs. Stored behind the window's
/// `GWLP_USERDATA` pointer for the lifetime of the window.
pub struct AppContext {
    pub paths: rc_common::AppPaths,
    pub settings: Mutex<Settings>,
    pub core_tx: mpsc::UnboundedSender<CoreCommand>,
    pub status: watch::Receiver<CoreStatus>,
    pub exe_path: PathBuf,
    /// Held for the process's lifetime — except across a "Restart as
    /// Administrator", which must release it before the elevated relaunch so
    /// that instance can acquire it in turn (see `ID_RESTART_ELEVATED`).
    pub instance_guard: Mutex<Option<InstanceGuard>>,
    /// The app window's local server, started the first time it's opened.
    pub dashboard: std::sync::OnceLock<crate::dashboard::Dashboard>,
    /// Open the app window as soon as the tray is up (first run after install).
    pub open_on_start: bool,
}

/// Register the window class, create the hidden window, add the tray icon and
/// pump messages until the user chooses Exit (or Windows ends the session).
pub fn run(ctx: AppContext) -> Result<()> {
    unsafe {
        let hmodule = GetModuleHandleW(PCWSTR::null()).context("GetModuleHandleW")?;
        let hinstance = HINSTANCE(hmodule.0);
        let class_name = HSTRING::from(HOST_WINDOW_CLASS);

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_win32()).context("RegisterClassW");
        }

        // Hidden, never shown. Top-level (not message-only) so `FindWindowW`
        // locates it and it still receives WM_ENDSESSION at logoff/shutdown.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(class_name.as_ptr()),
            &HSTRING::from(APP_NAME),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None, // no parent window
            None, // no menu
            hinstance,
            None,
        )
        .context("CreateWindowExW")?;

        let open_on_start = ctx.open_on_start;
        // Hand ownership of the context to the window.
        let boxed = Box::into_raw(Box::new(ctx));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, boxed as isize);

        add_tray_icon(hwnd);
        if open_on_start {
            let _ = PostMessageW(hwnd, WM_APP_SHOW_SETTINGS, WPARAM(0), LPARAM(0));
        }
        // Refresh the tooltip / fire connect notifications once a second.
        SetTimer(hwnd, STATUS_TIMER_ID, 1000, None);
        tracing::info!("tray icon active; entering message loop");

        let mut msg = MSG::default();
        loop {
            let got = GetMessageW(&mut msg, None, 0, 0);
            if got.0 <= 0 {
                break; // 0 = WM_QUIT, -1 = error
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Reclaim and drop the context.
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppContext;
        if !ptr.is_null() {
            drop(Box::from_raw(ptr));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
    }
    Ok(())
}

fn context<'a>(hwnd: HWND) -> Option<&'a AppContext> {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AppContext;
        ptr.as_ref()
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAYICON => {
            // The mouse event is in the low word of lParam.
            match (lparam.0 as u32) & 0xFFFF {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => open_dashboard(hwnd),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            handle_command(hwnd, wparam.0 & 0xFFFF);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == STATUS_TIMER_ID => {
            refresh_status_ui(hwnd);
            LRESULT(0)
        }
        WM_APP_SHOW_SETTINGS => {
            open_dashboard(hwnd);
            LRESULT(0)
        }
        WM_QUERYENDSESSION => LRESULT(1), // allow logoff/shutdown
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                tracing::info!("session ending; shutting host down");
                remove_tray_icon(hwnd);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, STATUS_TIMER_ID);
            remove_tray_icon(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn handle_command(hwnd: HWND, id: usize) {
    let Some(ctx) = context(hwnd) else { return };
    match id {
        ID_OPEN_APP => open_dashboard(hwnd),
        ID_OPEN_SETTINGS => open_config_file(hwnd),
        ID_RELOAD_SETTINGS => {
            let _ = ctx.core_tx.send(CoreCommand::ReloadSettings);
            tracing::info!("settings reload requested from tray");
        }
        ID_TOGGLE_AUTOSTART => {
            let target = !autostart::is_enabled();
            match autostart::set(target, &ctx.exe_path) {
                Ok(()) => {
                    if let Ok(mut s) = ctx.settings.lock() {
                        // Re-read first: the app window may have changed other
                        // settings since this copy was loaded.
                        if let Ok(fresh) = Settings::load(&ctx.paths.config_file()) {
                            *s = fresh;
                        }
                        s.start_with_windows = target;
                        if let Err(e) = s.save(&ctx.paths.config_file()) {
                            tracing::warn!(error = %e, "could not persist autostart preference");
                        }
                    }
                    tracing::info!(enabled = target, "autostart toggled");
                }
                Err(e) => tracing::warn!(error = %e, "toggling autostart failed"),
            }
        }
        ID_DISCONNECT_ALL => {
            let _ = ctx.core_tx.send(CoreCommand::DisconnectAll);
        }
        ID_FORGET_PAIRED => {
            let _ = ctx.core_tx.send(CoreCommand::ForgetPaired);
        }
        ID_COPY_DIAGNOSTICS => {
            let status = ctx.status.borrow().clone();
            let report = crate::diagnostics::build_report(&ctx.paths, &status);
            match rc_clipboard::set_text(&report) {
                Ok(()) => {
                    tracing::info!("diagnostic log copied to clipboard");
                    balloon(
                        hwnd,
                        "Diagnostics copied",
                        "Log contents were copied to the clipboard. Paste them wherever you're reporting the issue.",
                    );
                }
                Err(e) => tracing::warn!(error = %e, "copying diagnostics failed"),
            }
        }
        ID_COPY_CODE => {
            let id = ctx.status.borrow().device_id.clone();
            if !id.is_empty() {
                if let Err(e) = rc_clipboard::set_text(&id) {
                    tracing::warn!(error = %e, "copying PC ID failed");
                } else {
                    tracing::info!("PC ID copied to clipboard");
                }
            }
        }
        ID_COPY_PHONE_LINK => {
            let id = ctx.status.borrow().device_id.clone();
            let link = Settings::load(&ctx.paths.config_file())
                .ok()
                .and_then(|s| s.network.phone_link(&id));
            match link {
                Some(link) => match rc_clipboard::set_text(&link) {
                    Ok(()) => {
                        tracing::info!("phone link copied to clipboard");
                        balloon(
                            hwnd,
                            "Phone link copied",
                            "Send it to your phone and open it there. It adds this PC to the Remote Control app.",
                        );
                    }
                    Err(e) => tracing::warn!(error = %e, "copying phone link failed"),
                },
                None => balloon(
                    hwnd,
                    "No relay configured",
                    "Set network.signaling_url in Settings to connect from other networks.",
                ),
            }
        }
        ID_DOWNLOAD_UPDATE => {
            if let Some((_, url, _, _)) = ctx.status.borrow().update_available.clone() {
                let target = HSTRING::from(url);
                let _ = ShellExecuteW(
                    hwnd,
                    w!("open"),
                    &target,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                );
            }
        }
        ID_INSTALL_UPDATE => {
            if let Some((_, _, _, download_url)) = ctx.status.borrow().update_available.clone() {
                if !download_url.is_empty() {
                    let _ = ctx.core_tx.send(CoreCommand::InstallUpdate(download_url));
                    balloon(hwnd, "Installing update", "Downloading the update. Remote Control will restart in a moment.");
                }
            }
        }
        ID_RESTART_ELEVATED => restart_elevated(hwnd, ctx),
        ID_EXIT => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

/// Relaunch the host elevated so it can reach windows that auto-elevate
/// (Task Manager) or otherwise run at a higher integrity level (UAC dialogs,
/// elevated installers) — see `platform::elevate` for why that's necessary.
unsafe fn restart_elevated(hwnd: HWND, ctx: &AppContext) {
    // Release the single-instance mutex *before* launching the elevated
    // relaunch, or it'll see the name still taken and refuse to become the
    // running instance, leaving nothing listening at all.
    let Ok(mut guard_slot) = ctx.instance_guard.lock() else {
        return;
    };
    let held = guard_slot.take();

    let args = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    match elevate::relaunch_elevated(&ctx.exe_path, &args) {
        Ok(()) => {
            drop(held); // release the mutex now the elevated copy is starting
            let _ = DestroyWindow(hwnd);
        }
        Err(code) => {
            // Most commonly the user clicked "No" on the UAC prompt. Keep
            // running normally — put the guard back so we're still the one
            // legitimate instance.
            *guard_slot = held;
            tracing::info!(code, "elevated relaunch not started (declined or failed)");
            balloon(
                hwnd,
                "Still running without Administrator",
                "The elevation prompt was cancelled or failed, so Remote Control keeps running as before.",
            );
        }
    }
}

/// Small stable int per [`CoreState`] for change detection.
fn state_code(s: CoreState) -> i32 {
    match s {
        CoreState::Starting => 0,
        CoreState::Disabled => 1,
        CoreState::Idle => 2,
        CoreState::Listening => 3,
        CoreState::Connected => 4,
    }
}

/// Called on the status timer: keep the tray tooltip current and fire a
/// balloon on connect / disconnect.
unsafe fn refresh_status_ui(hwnd: HWND) {
    let Some(ctx) = context(hwnd) else { return };
    let status = ctx.status.borrow().clone();

    if let Some((version, _, notes, _)) = &status.update_available {
        let already = UPDATE_NOTIFIED.with(|n| n.replace(true));
        if !already {
            let body = if notes.is_empty() {
                format!("Remote Control {version} is available. Right-click the tray icon to download it.")
            } else {
                format!("Remote Control {version} is available: {notes}")
            };
            balloon(hwnd, "Update available", &body);
        }
    }

    let code = state_code(status.state);
    let prev = LAST_UI_STATE.with(|c| c.replace(code));
    if prev == code {
        return;
    }

    set_tray_tooltip(hwnd, &tooltip_for(&status));

    // Notify only on real transitions (skip the initial Starting → …).
    if prev >= 0 {
        match (prev, code) {
            (_, 4) => balloon(
                hwnd,
                "Remote session started",
                "A device is now connected and can control this PC.",
            ),
            (4, _) => balloon(hwnd, "Remote session ended", "The device has disconnected."),
            _ => {}
        }
    }
}

fn tooltip_for(status: &CoreStatus) -> String {
    match status.state {
        CoreState::Connected => format!("{APP_NAME} — CONNECTED ({} device)", status.sessions),
        CoreState::Listening => {
            format!("{APP_NAME} — ready, PC ID {}", status.device_id)
        }
        CoreState::Idle => format!("{APP_NAME} — enabled, listener down"),
        CoreState::Disabled => format!("{APP_NAME} — remote access off"),
        CoreState::Starting => APP_NAME.to_string(),
    }
}

unsafe fn set_tray_tooltip(hwnd: HWND, text: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_TIP,
        ..Default::default()
    };
    for (dst, src) in nid.szTip.iter_mut().zip(text.encode_utf16()) {
        *dst = src;
    }
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

unsafe fn balloon(hwnd: HWND, title: &str, body: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_INFO,
        ..Default::default()
    };
    for (dst, src) in nid.szInfoTitle.iter_mut().zip(title.encode_utf16()) {
        *dst = src;
    }
    for (dst, src) in nid.szInfo.iter_mut().zip(body.encode_utf16()) {
        *dst = src;
    }
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// Show the Remote Control app window, starting its local server the first
/// time. Falls back to the config file if the server can't start.
unsafe fn open_dashboard(hwnd: HWND) {
    let Some(ctx) = context(hwnd) else { return };
    if ctx.dashboard.get().is_none() {
        let started = crate::dashboard::Dashboard::start(crate::dashboard::DashboardCtx {
            paths: ctx.paths.clone(),
            core_tx: ctx.core_tx.clone(),
            status: ctx.status.clone(),
            exe_path: ctx.exe_path.clone(),
            preview: false,
        });
        match started {
            Ok(dashboard) => {
                let _ = ctx.dashboard.set(dashboard);
            }
            Err(e) => {
                tracing::error!(error = ?e, "could not start the app window");
                open_config_file(hwnd);
                return;
            }
        }
    }
    if let Some(dashboard) = ctx.dashboard.get() {
        dashboard.open_window();
    }
}

/// Open config.toml in the user's default editor (advanced settings).
unsafe fn open_config_file(hwnd: HWND) {
    let Some(ctx) = context(hwnd) else { return };
    let file = HSTRING::from(ctx.paths.config_file().as_os_str());
    let result = ShellExecuteW(
        hwnd,
        w!("open"),
        &file,
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
    // ShellExecuteW returns a value <= 32 on failure.
    if result.0 as isize <= 32 {
        tracing::warn!(code = result.0 as isize, "opening settings file failed");
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Some(ctx) = context(hwnd) else { return };
    let status = ctx.status.borrow().clone();

    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "CreatePopupMenu failed");
            return;
        }
    };

    // Status lines (greyed, non-interactive).
    let _ = AppendMenuW(
        menu,
        MF_STRING | MF_GRAYED,
        0,
        &HSTRING::from(status_line(&status)),
    );
    if !status.device_id.is_empty() {
        let id_line = format!("PC ID:  {}   (port {})", status.device_id, status.listen_port);
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &HSTRING::from(id_line));
        let _ = AppendMenuW(menu, MF_STRING, ID_COPY_CODE, w!("Copy PC ID"));
        let _ = AppendMenuW(menu, MF_STRING, ID_COPY_PHONE_LINK, w!("Copy link for your phone"));
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_APP, w!("Open Remote Control"));
    let _ = SetMenuDefaultItem(menu, ID_OPEN_APP as u32, 0);
    let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_SETTINGS, w!("Edit config file (advanced)"));
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        ID_RELOAD_SETTINGS,
        w!("Reload settings from file"),
    );

    let mut autostart_flags = MF_STRING;
    if autostart::is_enabled() {
        autostart_flags |= MF_CHECKED;
    }
    let _ = AppendMenuW(
        menu,
        autostart_flags,
        ID_TOGGLE_AUTOSTART,
        w!("Start with Windows"),
    );

    let mut disconnect_flags = MF_STRING;
    if status.sessions == 0 {
        disconnect_flags |= MF_GRAYED;
    }
    let _ = AppendMenuW(
        menu,
        disconnect_flags,
        ID_DISCONNECT_ALL,
        w!("Disconnect all sessions"),
    );

    let mut forget_flags = MF_STRING;
    if status.paired_count == 0 {
        forget_flags |= MF_GRAYED;
    }
    let forget_label = if status.paired_count > 0 {
        format!("Forget {} paired device(s)", status.paired_count)
    } else {
        "Forget paired devices".to_string()
    };
    let _ = AppendMenuW(menu, forget_flags, ID_FORGET_PAIRED, &HSTRING::from(forget_label));

    let _ = AppendMenuW(
        menu,
        MF_STRING,
        ID_COPY_DIAGNOSTICS,
        w!("Copy Diagnostic Logs"),
    );

    // Task Manager (and any UAC dialog / elevated installer) auto-elevates
    // and silently drops synthetic input from this non-elevated process —
    // see platform::elevate. Offer the escape hatch, but only show it when
    // it would actually change something.
    if elevate::is_elevated() {
        let _ = AppendMenuW(
            menu,
            MF_STRING | MF_GRAYED,
            0,
            w!("Running as Administrator"),
        );
    } else {
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            ID_RESTART_ELEVATED,
            w!("Restart as Administrator (fixes clicks on Task Manager, UAC, etc.)"),
        );
    }

    if let Some((version, _, _, download_url)) = &status.update_available {
        if !download_url.is_empty() {
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                ID_INSTALL_UPDATE,
                &HSTRING::from(format!("Update available: v{version} — Install now")),
            );
        }
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            ID_DOWNLOAD_UPDATE,
            &HSTRING::from(format!("v{version} release notes")),
        );
    }

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit Remote Control"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu dismisses correctly when the user clicks elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = PostMessageW(hwnd, 0x0000 /* WM_NULL */, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
}

fn status_line(status: &CoreStatus) -> String {
    match status.state {
        CoreState::Starting => "Starting…".to_string(),
        CoreState::Disabled => "Remote access is OFF".to_string(),
        CoreState::Idle => "Enabled, but not listening (port busy?)".to_string(),
        CoreState::Listening => "Ready — waiting for a device".to_string(),
        CoreState::Connected => format!("● Connected — {} device(s)", status.sessions),
    }
}

unsafe fn add_tray_icon(hwnd: HWND) {
    // Resource id "1" is the app icon embedded by build.rs (winresource).
    // Fall back to the generic system icon so a dev build without an RC
    // toolchain (icon not embedded) still shows *something* in the tray.
    // MAKEINTRESOURCEW(1) — not a real pointer, just the resource id "1"
    // (build.rs / winresource) encoded the way Win32 resource APIs expect it.
    #[allow(clippy::manual_dangling_ptr)]
    let resource_1 = PCWSTR(1usize as *const u16);
    let hicon: HICON = GetModuleHandleW(PCWSTR::null())
        .ok()
        .and_then(|hmodule| LoadIconW(HINSTANCE(hmodule.0), resource_1).ok())
        .or_else(|| LoadIconW(None, windows::Win32::UI::WindowsAndMessaging::IDI_APPLICATION).ok())
        .unwrap_or_default();

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAYICON,
        hIcon: hicon,
        ..Default::default()
    };
    for (dst, src) in nid.szTip.iter_mut().zip(APP_NAME.encode_utf16()) {
        *dst = src;
    }

    if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
        tracing::error!("Shell_NotifyIconW(NIM_ADD) failed");
    }
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}
