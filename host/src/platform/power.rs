//! Keep the PC awake while Remote Control is running, so it's still reachable
//! if nobody touches it for a while — the opposite of what Windows' own power
//! plan wants for an idle machine. Opt-in (Settings → Features): most people
//! don't want their PC humming all night for this.

use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};

/// `true`: prevent the system from sleeping until this is called again with
/// `false`. Doesn't touch the display — only whether Windows suspends.
pub fn set_prevent_sleep(on: bool) {
    let flags = if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS };
    unsafe {
        // Failure here isn't actionable (no parameters to get wrong) and
        // isn't worth surfacing to the user — just note it for diagnostics.
        if SetThreadExecutionState(flags).0 == 0 {
            tracing::debug!("SetThreadExecutionState failed");
        }
    }
    tracing::info!(prevent_sleep = on, "sleep prevention updated");
}
