//! Handle a PC with more than one monitor for a session: by default the
//! client only ever sees the primary display (capture is single-monitor —
//! see `ARCHITECTURE.md`), so anything open on a second screen is simply
//! unreachable. Two opt-in fixes, chosen in Settings:
//!
//!   * **Duplicate** — mirror every monitor onto the primary one for the
//!     session, so whatever's on the second screen shows up too.
//!   * **Move windows to the main screen** — move each window that's on a
//!     secondary monitor onto the primary one, preserving its position
//!     relative to its own monitor where that still fits.
//!
//! Both undo themselves when the session ends: `apply` returns a [`Restore`]
//! to hand back to `undo`.

use serde::{Deserialize, Serialize};
use windows::Win32::Devices::Display::{SetDisplayConfig, SDC_APPLY, SDC_TOPOLOGY_CLONE, SDC_TOPOLOGY_EXTEND};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetSystemMetrics, IsIconic, IsWindowVisible,
    SetWindowPos, GWL_EXSTYLE, SM_CMONITORS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_TOOLWINDOW,
};

/// How to handle a PC with more than one monitor for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MultiMonitorMode {
    /// Leave the setup alone — the client only sees the primary display.
    #[default]
    Ignore,
    /// Mirror every monitor onto the primary one for the session.
    Duplicate,
    /// Move windows on a secondary monitor onto the primary one.
    MoveToMain,
}

/// What a session needs to undo when it ends. Windows are kept as their raw
/// handle value (`isize`, not `HWND`) rather than the pointer-wrapping
/// `HWND` itself, purely so this type is `Send` — it has to cross into the
/// session's `tokio::spawn`ed task, and a window handle is just an opaque
/// number to Windows anyway, never dereferenced as a pointer.
pub enum Restore {
    None,
    /// Put the display topology back to Extend.
    Topology,
    /// Put each moved window back exactly where it was.
    Windows(Vec<(isize, RECT)>),
}

/// How many monitors Windows currently reports.
pub fn monitor_count() -> u32 {
    unsafe { GetSystemMetrics(SM_CMONITORS).max(1) as u32 }
}

/// Apply `mode` for a session that's about to start. A no-op (and `Restore::None`)
/// on a single-monitor PC regardless of `mode`. The second value is a
/// human-readable failure to show the connecting device — `None` on success
/// or when there was nothing to do.
pub fn apply(mode: MultiMonitorMode) -> (Restore, Option<String>) {
    if mode == MultiMonitorMode::Ignore || monitor_count() < 2 {
        return (Restore::None, None);
    }
    match mode {
        MultiMonitorMode::Ignore => (Restore::None, None),
        MultiMonitorMode::Duplicate => unsafe {
            let code = SetDisplayConfig(None, None, SDC_TOPOLOGY_CLONE | SDC_APPLY);
            if code == 0 {
                tracing::info!("switched displays to duplicate for this session");
                (Restore::Topology, None)
            } else {
                tracing::warn!(code, "couldn't switch to duplicate display mode");
                let why = match code {
                    31 => "your monitors don't support matching modes for it".to_string(),
                    87 => "Windows rejected the request".to_string(),
                    _ => format!("Windows error {code}"),
                };
                (Restore::None, Some(format!("Couldn't switch displays to duplicate — {why}.")))
            }
        },
        MultiMonitorMode::MoveToMain => (Restore::Windows(move_windows_to_primary()), None),
    }
}

/// Undo whatever `apply` did.
pub fn undo(restore: Restore) {
    match restore {
        Restore::None => {}
        Restore::Topology => unsafe {
            let code = SetDisplayConfig(None, None, SDC_TOPOLOGY_EXTEND | SDC_APPLY);
            if code != 0 {
                tracing::warn!(code, "couldn't restore extend display mode");
            } else {
                tracing::info!("restored extend display mode");
            }
        },
        Restore::Windows(moved) => {
            let n = moved.len();
            for (raw, rect) in moved {
                let hwnd = HWND(raw as *mut std::ffi::c_void);
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            if n > 0 {
                tracing::info!(count = n, "moved windows back to their monitors");
            }
        }
    }
}

struct EnumCtx {
    primary: HMONITOR,
    primary_work: RECT,
    moved: Vec<(isize, RECT)>,
}

fn move_windows_to_primary() -> Vec<(isize, RECT)> {
    unsafe {
        let primary = MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY);
        let mut primary_info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(primary, &mut primary_info).as_bool() {
            tracing::warn!("couldn't read the primary monitor's geometry");
            return Vec::new();
        }
        let mut ctx = EnumCtx { primary, primary_work: primary_info.rcWork, moved: Vec::new() };
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut EnumCtx as isize));
        tracing::info!(count = ctx.moved.len(), "moved windows onto the main monitor");
        ctx.moved
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut EnumCtx);
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return true.into();
    }
    if GetWindowTextLengthW(hwnd) == 0 {
        return true.into(); // no title: a helper/tool window, not something the user is "using"
    }
    if (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
        return true.into();
    }
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if monitor.0 == ctx.primary.0 {
        return true.into(); // already on the main monitor
    }
    let mut src_info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if !GetMonitorInfoW(monitor, &mut src_info).as_bool() {
        return true.into();
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return true.into();
    }

    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let pw = ctx.primary_work;
    // Preserve the window's offset from its own monitor's corner, then clamp
    // so it still lands fully inside the primary monitor's work area.
    let mut x = pw.left + (rect.left - src_info.rcMonitor.left);
    let mut y = pw.top + (rect.top - src_info.rcMonitor.top);
    x = x.min(pw.right - w.min(pw.right - pw.left)).max(pw.left);
    y = y.min(pw.bottom - h.min(pw.bottom - pw.top)).max(pw.top);

    if SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE).is_ok() {
        ctx.moved.push((hwnd.0 as isize, rect));
    }
    true.into()
}
