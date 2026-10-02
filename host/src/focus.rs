//! Watches whether the PC's keyboard focus is in a text box, using Windows UI
//! Automation, so the phone can pop its keyboard up (and paste uploaded
//! pictures) only when there's somewhere to type.
//!
//! Best-effort: it recognises Edit controls (which is what Discord's and most
//! browsers' text boxes report) and rich-text Documents that accept typing.
//! Apps that expose nothing to UI Automation just read as "not a text box".

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use windows::core::Interface;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationValuePattern, UIA_DocumentControlTypeId,
    UIA_EditControlTypeId, UIA_TextPatternId, UIA_ValuePatternId,
};

/// Poll the focused element until `stop`, sending `true`/`false` whenever the
/// "is a text box focused" answer changes (and once at the start).
pub fn watch(stop: &AtomicBool, tx: &UnboundedSender<bool>) {
    let automation: IUIAutomation = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(error = ?e, "UI Automation unavailable; text-box detection is off");
                return;
            }
        }
    };
    let mut last: Option<bool> = None;
    while !stop.load(Ordering::Relaxed) {
        let now = unsafe { automation.GetFocusedElement().map(|el| is_text_box(&el)).unwrap_or(false) };
        if last != Some(now) {
            last = Some(now);
            if tx.send(now).is_err() {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

unsafe fn is_text_box(el: &IUIAutomationElement) -> bool {
    let Ok(kind) = el.CurrentControlType() else { return false };
    if kind == UIA_EditControlTypeId {
        return true;
    }
    if kind == UIA_DocumentControlTypeId {
        // A page's own document counts only if it takes typing: it has a text
        // pattern and isn't marked read-only.
        let has_text = el.GetCurrentPattern(UIA_TextPatternId).is_ok();
        if !has_text {
            return false;
        }
        if let Ok(p) = el.GetCurrentPattern(UIA_ValuePatternId) {
            if let Ok(v) = p.cast::<IUIAutomationValuePattern>() {
                return !v.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(true);
            }
        }
        return false;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The watcher starts, reads the focused element without crashing, and
    /// reports a first answer.
    #[test]
    fn reports_a_first_answer() {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s = stop.clone();
        let t = std::thread::spawn(move || watch(&s, &tx));
        std::thread::sleep(Duration::from_millis(900));
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(rx.try_recv().is_ok(), "expected an initial focus report");
    }
}
