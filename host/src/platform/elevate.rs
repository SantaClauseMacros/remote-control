//! Detecting and escalating to Administrator privileges.
//!
//! The host deliberately runs without admin rights (see `README.md`) — but
//! Windows' UIPI (User Interface Privilege Isolation) silently drops any
//! `SendInput` event aimed at a *higher*-integrity window than the sender.
//! Task Manager auto-elevates to full Administrator for admin accounts with
//! no prompt, so it (and any UAC dialog, elevated installer, etc.) is
//! unreachable to a non-elevated host: the input is dropped by the OS before
//! the target window ever sees it, and nothing reports an error on either
//! side. Relaunching the whole host elevated is the only way past that.

use std::path::Path;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Whether *this* process is running elevated (full Administrator token).
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

/// Relaunch `exe` elevated (triggers a UAC consent prompt) with the given
/// space-joined `args`. Does not touch the current process — the caller
/// decides what to do once this returns. `Err` means Windows didn't launch
/// it (most commonly: the user clicked "No" on the UAC prompt).
pub fn relaunch_elevated(exe: &Path, args: &str) -> Result<(), i32> {
    let exe_h = HSTRING::from(exe.as_os_str());
    let args_h = HSTRING::from(args);
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            &exe_h,
            &args_h,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Per ShellExecuteW's documented (legacy) contract: values <= 32 mean
    // failure — the same check `tray::open_settings` already uses.
    let code = result.0 as isize;
    if code <= 32 {
        Err(code as i32)
    } else {
        Ok(())
    }
}
