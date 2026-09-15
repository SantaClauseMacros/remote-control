//! Helpers for talking to an already-running host instance.
//!
//! The running instance owns a hidden top-level window whose class is
//! [`rc_common::HOST_WINDOW_CLASS`]. A second launch finds that window and posts
//! [`rc_common::WM_APP_SHOW_SETTINGS`] to it, then exits.

use rc_common::{HOST_WINDOW_CLASS, WM_APP_SHOW_SETTINGS};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_COMMAND};

/// Ask the running host's tray to run one of its menu commands. The app window
/// uses this for "Exit" and "Restart as Administrator", which have to happen on
/// the tray's own thread.
pub fn post_command(id: usize) {
    unsafe {
        let class = HSTRING::from(HOST_WINDOW_CLASS);
        match FindWindowW(&class, PCWSTR::null()) {
            Ok(hwnd) if !hwnd.is_invalid() => {
                let _ = PostMessageW(hwnd, WM_COMMAND, WPARAM(id), LPARAM(0));
            }
            _ => tracing::warn!("no running host window found for a menu command"),
        }
    }
}

/// Ask the running host instance to bring its settings UI to the foreground.
/// No-op if no instance is found (caller has already checked the mutex).
pub fn signal_show_settings() {
    unsafe {
        let class = HSTRING::from(HOST_WINDOW_CLASS);
        match FindWindowW(&class, PCWSTR::null()) {
            Ok(hwnd) if !hwnd.is_invalid() => {
                let _ = PostMessageW(hwnd, WM_APP_SHOW_SETTINGS, WPARAM(0), LPARAM(0));
            }
            _ => tracing::warn!("no running host window found to signal"),
        }
    }
}
