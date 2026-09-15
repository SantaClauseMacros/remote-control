//! Minimal Windows clipboard **text** support: read, write, and watch for
//! changes.
//!
//! Only `CF_UNICODETEXT` is handled — milestone 3 syncs text only. The watcher
//! polls `GetClipboardSequenceNumber` (cheap, no window/hook needed) so it can
//! run on any thread.

use std::time::Duration;

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// RAII guard: `OpenClipboard` on construction, `CloseClipboard` on drop.
struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Result<Self> {
        // A few retries: another process may hold the clipboard briefly.
        for _ in 0..10 {
            if unsafe { OpenClipboard(HWND(std::ptr::null_mut())) }.is_ok() {
                return Ok(Self);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(anyhow!("could not open the clipboard"))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Current clipboard text, or `None` if it holds no text.
pub fn get_text() -> Result<Option<String>> {
    let _guard = ClipboardGuard::open()?;
    unsafe {
        let handle = match GetClipboardData(CF_UNICODETEXT.0 as u32) {
            Ok(h) if !h.is_invalid() => h,
            _ => return Ok(None),
        };
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal) as *const u16;
        if ptr.is_null() {
            return Ok(None);
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        let _ = GlobalUnlock(hglobal);
        Ok(Some(text))
    }
}

/// Replace the clipboard contents with `text`.
pub fn set_text(text: &str) -> Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;

    let _guard = ClipboardGuard::open()?;
    unsafe {
        EmptyClipboard().map_err(|e| anyhow!("EmptyClipboard: {e}"))?;
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| anyhow!("GlobalAlloc: {e}"))?;
        let dst = GlobalLock(hmem) as *mut u16;
        if dst.is_null() {
            return Err(anyhow!("GlobalLock failed"));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(hmem);

        // Ownership of hmem transfers to the system on success.
        SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0))
            .map_err(|e| anyhow!("SetClipboardData: {e}"))?;
    }
    Ok(())
}

/// Monotonic-ish counter that changes whenever the clipboard is modified.
pub fn sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Polls the clipboard sequence number and yields new text as it appears.
///
/// Call [`ClipboardWatcher::poll`] on a timer. It tracks the last text that was
/// synced in **either** direction and never re-emits it, so a
/// host↔client↔host echo can't form even when both ends briefly observe the
/// same change (e.g. two watchers on one machine during testing).
pub struct ClipboardWatcher {
    last_seq: u32,
    last_synced: Option<String>,
}

impl Default for ClipboardWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardWatcher {
    pub fn new() -> Self {
        Self {
            last_seq: sequence_number(),
            last_synced: get_text().ok().flatten(),
        }
    }

    /// Record text we just wrote from the peer, so it is never sent back.
    pub fn note_local_set(&mut self, text: &str) {
        self.last_synced = Some(text.to_string());
        self.last_seq = sequence_number();
    }

    /// Returns `Some(text)` when the clipboard holds text we have not synced in
    /// either direction yet.
    pub fn poll(&mut self) -> Option<String> {
        let seq = sequence_number();
        if seq == self.last_seq {
            return None;
        }
        self.last_seq = seq;
        let text = get_text().ok().flatten()?;
        if self.last_synced.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.last_synced = Some(text.clone());
        Some(text)
    }
}
