//! "Start with Windows" via the standard per-user `Run` key.
//!
//! We deliberately use `HKEY_CURRENT_USER\...\Run` rather than a service or a
//! scheduled task: it needs no administrator rights, is the mechanism Windows
//! itself documents for user apps, and is trivial for the user to inspect or
//! remove. The value points at our own exe with `--autostart` so the process
//! knows it was launched at logon and stays in the background.

use std::path::Path;

use anyhow::{Context, Result};
use rc_common::RUN_VALUE_NAME;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Flag passed to our exe when Windows starts it at logon.
pub const AUTOSTART_ARG: &str = "--autostart";

/// Ensure the registry entry matches `desired`, touching it only on a mismatch.
pub fn reconcile(desired: bool) -> Result<()> {
    if desired == is_enabled() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("locating own exe")?;
    set(desired, &exe)
}

/// Enable or disable autostart for the given executable.
pub fn set(enabled: bool, exe: &Path) -> Result<()> {
    if enabled {
        write_value(exe)
    } else {
        clear_value()
    }
}

/// Is the `Run` value currently present?
pub fn is_enabled() -> bool {
    let mut buf = [0u16; 1024];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            &HSTRING::from(RUN_VALUE_NAME),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS
}

fn open_run_key() -> Result<HKEY> {
    let mut hkey = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    status.ok().context("opening HKCU Run key")?;
    Ok(hkey)
}

fn write_value(exe: &Path) -> Result<()> {
    let hkey = open_run_key()?;
    let command = format!("\"{}\" {}", exe.display(), AUTOSTART_ARG);

    // REG_SZ wants a NUL-terminated UTF-16 string, as raw bytes.
    let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };

    let status =
        unsafe { RegSetValueExW(hkey, &HSTRING::from(RUN_VALUE_NAME), 0, REG_SZ, Some(bytes)) };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    status.ok().context("writing Run value")
}

fn clear_value() -> Result<()> {
    let hkey = open_run_key()?;
    let status = unsafe { RegDeleteValueW(hkey, &HSTRING::from(RUN_VALUE_NAME)) };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        status.ok().context("deleting Run value")
    }
}
