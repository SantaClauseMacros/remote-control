//! Single-instance guard backed by a named mutex.
//!
//! A second launch of the host detects the existing instance and (see
//! [`crate::platform::message_window`]) nudges it to show its settings instead
//! of starting a duplicate tray icon.

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

/// Owns the mutex handle for the lifetime of the process.
pub struct InstanceGuard(HANDLE);

impl InstanceGuard {
    /// `Ok(Some(guard))` — this is the first instance.
    /// `Ok(None)` — another instance already holds the mutex.
    pub fn acquire(name: &str) -> windows::core::Result<Option<Self>> {
        unsafe {
            let handle = CreateMutexW(None, true, &HSTRING::from(name))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                Ok(None)
            } else {
                Ok(Some(Self(handle)))
            }
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
