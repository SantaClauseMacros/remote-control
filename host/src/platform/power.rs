//! Keep the PC awake while Remote Control is running, so it's still reachable
//! if nobody touches it for a while — the opposite of what Windows' own power
//! plan wants for an idle machine. Opt-in (Settings → Features): most people
//! don't want their PC humming all night for this.
//!
//! This also keeps the *display* from turning off, not just the system from
//! sleeping. That's not just for the picture — DXGI Desktop Duplication (the
//! capture API this app uses) commonly stops delivering frames once Windows
//! blanks the monitor via its power-save timeout, even though the PC itself
//! is still fully awake and signed in. Screen capture tools generally hit
//! this same wall; keeping the display "on" (even though nothing's there to
//! see it) sidesteps it entirely.

use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};

/// `true`: prevent the system from sleeping — and the display from turning
/// off — until this is called again with `false`.
pub fn set_prevent_sleep(on: bool) {
    let flags = if on {
        ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    unsafe {
        // Failure here isn't actionable (no parameters to get wrong) and
        // isn't worth surfacing to the user — just note it for diagnostics.
        if SetThreadExecutionState(flags).0 == 0 {
            tracing::debug!("SetThreadExecutionState failed");
        }
    }
    tracing::info!(prevent_sleep = on, "sleep prevention updated");
}
