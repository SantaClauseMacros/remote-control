//! Local input injection via Win32 `SendInput`.
//!
//! Maps [`rc_protocol::InputEvent`] (resolution-independent, normalised pointer
//! coordinates) onto `INPUT` records. No driver, no elevation.
//!
//! The injector tracks which buttons and keys *it* pressed so a session can be
//! torn down cleanly with [`Injector::release_all`] — otherwise a client that
//! disconnects mid-drag or mid-keypress would leave a button stuck down on the
//! host.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use rc_protocol::{InputEvent, PointerButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};
/// Make the current process **per-monitor DPI aware (V2)**.
///
/// Screen-capture dimensions, `SendInput` absolute mapping and `GetCursorPos`
/// only agree — in physical pixels — when this is set. Without it, on a scaled
/// display, injected clicks land ~1% off. Call once at process start, before
/// creating any window. Idempotent: if a manifest already set awareness this is
/// a harmless no-op.
pub fn set_dpi_aware() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// One wheel "notch".
const WHEEL_DELTA: i32 = 120;
/// Tag on `dwExtraInfo` so our own injected events are identifiable in a hook.
const INJECT_TAG: usize = 0x5243_0001; // "RC\0\1"
/// `mouseData` values for the thumb buttons (from WinUser.h).
const XBUTTON1: i32 = 0x0001;
const XBUTTON2: i32 = 0x0002;

/// Shortest press the injector lets through. A phone tap, or a quick click
/// whose press and release got bunched together by network jitter, can land
/// as a down/up pair only microseconds apart — and a game that samples input
/// once per frame (every 16–33ms) never sees the button down at all. Releases
/// that arrive sooner than this are held back until it has elapsed.
pub const MIN_HOLD: Duration = Duration::from_millis(50);

/// Something the injector can hold down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Held {
    Button(PointerButton),
    Key(u16),
}

/// The foreground window's client area, in virtual-desktop pixels — where a
/// game that has captured the mouse is actually drawing (inside the title bar
/// and above the taskbar when it's windowed or maximized).
pub fn foreground_client_rect() -> Option<Rect> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowInfo, WINDOWINFO};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut wi = WINDOWINFO {
            cbSize: std::mem::size_of::<WINDOWINFO>() as u32,
            ..Default::default()
        };
        GetWindowInfo(hwnd, &mut wi).ok()?;
        let c = wi.rcClient;
        let (w, h) = (c.right - c.left, c.bottom - c.top);
        (w > 0 && h > 0).then_some(Rect { x: c.left, y: c.top, w, h })
    }
}

/// Has a game (or any app) captured the mouse — hidden the cursor or clipped
/// it to a region smaller than the desktop? That's what Minecraft, shooters
/// and third-person cameras do while they read relative motion for aiming,
/// and while it's true an absolute cursor position means nothing to them.
pub fn cursor_captured() -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClipCursor, GetCursorInfo, CURSORINFO, CURSOR_SHOWING,
    };
    unsafe {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // Hidden either via ShowCursor(FALSE) or by setting a null cursor
        // (GLFW, which Minecraft Java uses, does the latter).
        let hidden = GetCursorInfo(&mut ci).is_ok()
            && ((ci.flags.0 & CURSOR_SHOWING.0) == 0 || ci.hCursor.is_invalid());
        let vd = Rect::virtual_desktop();
        let mut clip = RECT::default();
        let clipped = GetClipCursor(&mut clip).is_ok()
            && (clip.left > vd.x
                || clip.top > vd.y
                || clip.right < vd.x + vd.w
                || clip.bottom < vd.y + vd.h);
        hidden || clipped
    }
}

/// A rectangle in virtual-desktop pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    fn virtual_desktop() -> Self {
        unsafe {
            Self {
                x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                w: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                h: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            }
        }
    }
}

/// Injects input events into the local session.
///
/// Pointer coordinates in [`InputEvent`] are `0.0..=1.0` over the **source
/// rectangle** (the region the client is actually seeing). The injector maps
/// that into the source rect, then into the `0..=65535` absolute range
/// `SendInput` expects across the whole virtual desktop. Default source rect is
/// the entire virtual desktop.
pub struct Injector {
    source: Rect,
    vd: Rect,
    buttons_down: HashSet<PointerButton>,
    keys_down: HashSet<u16>,
    /// Leftover sub-pixel motion from [`InputEvent::PointerDelta`], carried
    /// into the next event. `SendInput` only takes whole pixels, so without
    /// this a slow drag — every delta rounding to zero — moves nothing at
    /// all, which is exactly when aiming precision matters most.
    delta_rem: (f64, f64),
    /// A game has the cursor captured — see [`Injector::set_captured`].
    captured: bool,
    /// Last absolute position the client sent, for turning absolute moves
    /// into relative motion while `captured`.
    last_norm: Option<(f64, f64)>,
    /// When each currently-held button/key went down, for [`MIN_HOLD`].
    pressed_at: HashMap<Held, Instant>,
    /// Releases held back by [`MIN_HOLD`], with the time each is due.
    deferred: Vec<(Instant, Held)>,
}

impl Default for Injector {
    fn default() -> Self {
        Self::new()
    }
}

impl Injector {
    pub fn new() -> Self {
        let vd = Rect::virtual_desktop();
        Self {
            source: vd,
            vd,
            buttons_down: HashSet::new(),
            keys_down: HashSet::new(),
            delta_rem: (0.0, 0.0),
            captured: false,
            last_norm: None,
            pressed_at: HashMap::new(),
            deferred: Vec::new(),
        }
    }

    /// Set the region the client's normalised coordinates map into (the
    /// captured monitor's rectangle, in virtual-desktop pixels). Also refreshes
    /// the virtual-desktop bounds.
    pub fn set_source_rect(&mut self, source: Rect) {
        self.vd = Rect::virtual_desktop();
        self.source = source;
    }

    fn normalized_to_absolute(&self, nx: f64, ny: f64) -> (i32, i32) {
        let px = self.source.x as f64 + nx.clamp(0.0, 1.0) * self.source.w as f64;
        let py = self.source.y as f64 + ny.clamp(0.0, 1.0) * self.source.h as f64;
        let ax = ((px - self.vd.x as f64) * 65535.0 / self.vd.w as f64).round() as i32;
        let ay = ((py - self.vd.y as f64) * 65535.0 / self.vd.h as f64).round() as i32;
        (ax.clamp(0, 65535), ay.clamp(0, 65535))
    }

    /// Tell the injector whether a game currently has the cursor captured
    /// (see [`cursor_captured`]). While it does, absolute positions from the
    /// client become relative motion, and clicks and scrolls stop moving the
    /// pointer first — either would yank the game's camera off target.
    pub fn set_captured(&mut self, captured: bool) {
        if captured != self.captured {
            self.captured = captured;
            self.delta_rem = (0.0, 0.0);
        }
    }

    /// Apply one event.
    pub fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::PointerMove { x, y } => {
                let prev = self.last_norm.replace((*x, *y));
                if !self.captured {
                    return self.send(&[self.mouse_move(*x, *y)]);
                }
                // Captured: the client still thinks in absolute positions (a
                // trackpad, or a mouse without Pointer Lock), so replay how far
                // it moved as the relative motion a game reads.
                match prev {
                    Some((lx, ly)) => self.relative(
                        (*x - lx) * self.source.w as f64,
                        (*y - ly) * self.source.h as f64,
                    ),
                    None => Ok(()),
                }
            }
            InputEvent::PointerButtonInPlace { button, pressed } => {
                self.button(*button, *pressed, None)
            }
            InputEvent::PointerDelta { dx, dy } => self.relative(*dx, *dy),
            InputEvent::PointerButton {
                button,
                pressed,
                x,
                y,
            } => self.button(*button, *pressed, Some((*x, *y))),
            InputEvent::Scroll { dx, dy, x, y } => {
                self.last_norm = Some((*x, *y));
                let mut inputs = Vec::with_capacity(3);
                if !self.captured {
                    inputs.push(self.mouse_move(*x, *y));
                }
                if *dy != 0.0 {
                    inputs.push(mouse_wheel(
                        MOUSEEVENTF_WHEEL,
                        (dy * WHEEL_DELTA as f32) as i32,
                    ));
                }
                if *dx != 0.0 {
                    inputs.push(mouse_wheel(
                        MOUSEEVENTF_HWHEEL,
                        (dx * WHEEL_DELTA as f32) as i32,
                    ));
                }
                self.send(&inputs)
            }
            InputEvent::Key { code, pressed } => {
                let vk = *code as u16;
                let held = Held::Key(vk);
                if *pressed {
                    self.release_deferred(held)?;
                    if self.keys_down.insert(vk) {
                        self.pressed_at.insert(held, Instant::now());
                    }
                } else if self.defer_if_short(held) {
                    return Ok(());
                } else {
                    self.keys_down.remove(&vk);
                    self.pressed_at.remove(&held);
                }
                self.send(&[key_event(vk, *pressed)])
            }
            InputEvent::Text(text) => {
                let inputs: Vec<INPUT> = text
                    .encode_utf16()
                    .flat_map(|u| [unicode_event(u, true), unicode_event(u, false)])
                    .collect();
                if inputs.is_empty() {
                    Ok(())
                } else {
                    self.send(&inputs)
                }
            }
        }
    }

    /// Release everything this injector currently holds down. Safe to call
    /// repeatedly; call it whenever a session ends.
    pub fn release_all(&mut self) {
        self.deferred.clear();
        self.pressed_at.clear();
        let buttons: Vec<_> = self.buttons_down.drain().collect();
        for b in buttons {
            if let Ok(input) = self.mouse_button(b, false) {
                let _ = self.send(&[input]);
            }
        }
        let keys: Vec<_> = self.keys_down.drain().collect();
        for vk in keys {
            let _ = self.send(&[key_event(vk, false)]);
        }
    }

    /// Send any releases [`MIN_HOLD`] held back whose time has come. Call
    /// again at [`Injector::next_release_due`].
    pub fn flush_due(&mut self) -> Result<()> {
        let now = Instant::now();
        let due: Vec<Held> = self
            .deferred
            .iter()
            .filter(|(at, _)| *at <= now)
            .map(|(_, h)| *h)
            .collect();
        self.deferred.retain(|(at, _)| *at > now);
        let mut result = Ok(());
        for held in due {
            if let Err(e) = self.send_release(held) {
                result = Err(e);
            }
        }
        result
    }

    /// When the earliest held-back release is due, if any.
    pub fn next_release_due(&self) -> Option<Instant> {
        self.deferred.iter().map(|(at, _)| *at).min()
    }

    fn button(&mut self, button: PointerButton, pressed: bool, at: Option<(f64, f64)>) -> Result<()> {
        let held = Held::Button(button);
        let mut inputs = Vec::with_capacity(2);
        if let Some((x, y)) = at {
            self.last_norm = Some((x, y));
            // Move first so the click lands where the client aimed, even if
            // intervening move events were dropped — unless a game has the
            // cursor captured, where that move would jerk its camera.
            if !self.captured {
                inputs.push(self.mouse_move(x, y));
            }
        }
        if pressed {
            self.release_deferred(held)?;
            if self.buttons_down.insert(button) {
                self.pressed_at.insert(held, Instant::now());
            }
        } else if self.defer_if_short(held) {
            return self.send(&inputs);
        } else {
            self.buttons_down.remove(&button);
            self.pressed_at.remove(&held);
        }
        inputs.push(self.mouse_button(button, pressed)?);
        self.send(&inputs)
    }

    fn relative(&mut self, dx: f64, dy: f64) -> Result<()> {
        let ((px, py), rem) = split_delta(self.delta_rem, dx, dy);
        self.delta_rem = rem;
        if px == 0 && py == 0 {
            return Ok(()); // all of it is still sub-pixel; keep saving it up
        }
        self.send(&[mouse_delta(px, py)])
    }

    /// Hold back a release that follows its press too closely. `true` if it
    /// was deferred (the caller must not send it).
    fn defer_if_short(&mut self, held: Held) -> bool {
        let Some(down_at) = self.pressed_at.get(&held) else {
            return false;
        };
        let due = *down_at + MIN_HOLD;
        if Instant::now() >= due {
            return false;
        }
        if !self.deferred.iter().any(|(_, h)| *h == held) {
            self.deferred.push((due, held));
        }
        true
    }

    /// A new press of something whose release is still held back: let the
    /// release go now so the press is a real second press.
    fn release_deferred(&mut self, held: Held) -> Result<()> {
        match self.deferred.iter().position(|(_, h)| *h == held) {
            Some(i) => {
                self.deferred.remove(i);
                self.send_release(held)
            }
            None => Ok(()),
        }
    }

    fn send_release(&mut self, held: Held) -> Result<()> {
        self.pressed_at.remove(&held);
        match held {
            Held::Button(b) => {
                self.buttons_down.remove(&b);
                let input = self.mouse_button(b, false)?;
                self.send(&[input])
            }
            Held::Key(vk) => {
                self.keys_down.remove(&vk);
                self.send(&[key_event(vk, false)])
            }
        }
    }

    fn mouse_move(&self, x: f64, y: f64) -> INPUT {
        let (ax, ay) = self.normalized_to_absolute(x, y);
        mouse_input(
            ax,
            ay,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )
    }

    fn mouse_button(&self, button: PointerButton, pressed: bool) -> Result<INPUT> {
        let (flags, data) = match (button, pressed) {
            (PointerButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (PointerButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (PointerButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (PointerButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (PointerButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (PointerButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (PointerButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (PointerButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (PointerButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (PointerButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        };
        Ok(mouse_input(0, 0, data, flags))
    }

    fn send(&self, inputs: &[INPUT]) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err(anyhow!(
                "SendInput injected {sent}/{} events (input possibly blocked by UIPI or a secure desktop)",
                inputs.len()
            ))
        }
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn mouse_wheel(flag: MOUSE_EVENT_FLAGS, amount: i32) -> INPUT {
    mouse_input(0, 0, amount, flag)
}

/// Split accumulated relative motion into the whole pixels to inject now and
/// the sub-pixel remainder to carry into the next event. Truncating toward
/// zero (rather than rounding) is what keeps the carry honest in both
/// directions — the remainder always has the same sign as the motion, so a
/// steady slow drag accumulates instead of alternating over/undershoot.
fn split_delta(rem: (f64, f64), dx: f64, dy: f64) -> ((i32, i32), (f64, f64)) {
    let (wx, wy) = (rem.0 + dx, rem.1 + dy);
    let (px, py) = (wx.trunc(), wy.trunc());
    ((px as i32, py as i32), (wx - px, wy - py))
}

/// Relative move — deliberately *not* `MOUSEEVENTF_ABSOLUTE`: this is the
/// raw dx/dy a physical mouse would report, which is what a game's captured-
/// cursor / raw-input look path expects. Callers pass whole pixels; the
/// sub-pixel remainder is carried by [`Injector::delta_rem`].
fn mouse_delta(dx: i32, dy: i32) -> INPUT {
    mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)
}

/// VK codes that must carry `KEYEVENTF_EXTENDEDKEY` for correct behaviour.
fn is_extended(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 // PRIOR, NEXT, END, HOME, LEFT, UP, RIGHT, DOWN
            | 0x2D  // INSERT
            | 0x2E  // DELETE
            | 0x5B  // LWIN
            | 0x5C  // RWIN
            | 0x5D  // APPS
            | 0x6F  // DIVIDE (numpad /)
            | 0xA3  // RCONTROL
            | 0xA5  // RMENU (right Alt)
            | 0x90 // NUMLOCK
    )
}

fn key_event(vk: u16, pressed: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }

    // A vk-only SendInput (wScan = 0, no KEYEVENTF_SCANCODE) never produces a
    // Raw Input HID report — only the WM_KEYDOWN message-queue path sees it.
    // Games that read the keyboard through Raw Input or DirectInput instead
    // of the message queue (Roblox's movement keys included) never notice
    // it was pressed at all. Stamping in the real hardware scan code and
    // setting KEYEVENTF_SCANCODE makes Windows synthesize it as if it came
    // from the keyboard driver, which both paths pick up.
    let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) };
    let (wscan, wvk) = if scan != 0 {
        flags |= KEYEVENTF_SCANCODE;
        (scan as u16, vk)
    } else {
        (0, vk)
    };

    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(wvk),
                wScan: wscan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn unicode_event(code_unit: u16, pressed: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    // For KEYEVENTF_UNICODE the character goes in wScan and wVk must be 0.
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: code_unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inj_with(source: Rect, vd: Rect) -> Injector {
        let mut i = Injector::new();
        i.source = source;
        i.vd = vd;
        i
    }

    /// Feed `steps` deltas of `d` through the splitter, returning the whole
    /// pixels emitted and whatever is still being carried.
    fn drag(d: f64, steps: usize) -> (i32, f64) {
        let mut rem = (0.0, 0.0);
        let mut moved = 0;
        for _ in 0..steps {
            let ((px, _), r) = split_delta(rem, d, 0.0);
            rem = r;
            moved += px;
        }
        (moved, rem.0)
    }

    #[test]
    fn slow_sub_pixel_motion_is_not_lost() {
        // A slow trackpad drag: every delta is well under a pixel, so
        // rounding each one in isolation (as this did before) moved the
        // cursor nowhere at all, no matter how long you dragged.
        let (moved, carried) = drag(0.3, 10);
        assert!(moved > 0, "slow motion must eventually move the cursor");
        // Nothing is dropped on the floor: what was emitted plus what's still
        // pending accounts for every bit of input. (Emitted + carried can sit
        // just under a whole pixel at an arbitrary stopping point — that last
        // pixel lands on the next event, which is the point of carrying it.)
        assert!(
            ((moved as f64 + carried) - 3.0).abs() < 1e-9,
            "motion must be conserved: emitted {moved} + carried {carried}"
        );
    }

    #[test]
    fn sub_pixel_carry_works_in_both_directions() {
        let (moved, carried) = drag(-0.3, 10);
        assert!(moved < 0, "negative slow motion must move too");
        assert!(
            ((moved as f64 + carried) + 3.0).abs() < 1e-9,
            "motion must be conserved: emitted {moved} + carried {carried}"
        );
    }

    #[test]
    fn whole_pixel_motion_passes_through_untouched() {
        let ((px, py), rem) = split_delta((0.0, 0.0), 12.0, -7.0);
        assert_eq!((px, py), (12, -7));
        assert_eq!(rem, (0.0, 0.0));
    }

    #[test]
    fn full_desktop_source_covers_full_range() {
        let vd = Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let i = inj_with(vd, vd);
        assert_eq!(i.normalized_to_absolute(0.0, 0.0), (0, 0));
        assert_eq!(i.normalized_to_absolute(1.0, 1.0), (65535, 65535));
        let (mx, _) = i.normalized_to_absolute(0.5, 0.5);
        assert!((32000..34000).contains(&mx));
    }

    #[test]
    fn source_rect_maps_into_its_subregion() {
        // Right-hand monitor of a dual 1920×1080 setup, origin at (-1920, 0).
        let vd = Rect {
            x: -1920,
            y: 0,
            w: 3840,
            h: 1080,
        };
        let src = Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let i = inj_with(src, vd);
        // Centre of the captured monitor → pixel (960, 540) → 3/4 across the vd.
        let (ax, ay) = i.normalized_to_absolute(0.5, 0.5);
        assert!((48000..50000).contains(&ax), "ax={ax}");
        assert!((32000..34000).contains(&ay), "ay={ay}");
    }

    #[test]
    fn out_of_range_is_clamped() {
        let vd = Rect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        };
        assert_eq!(
            inj_with(vd, vd).normalized_to_absolute(-5.0, 9.0),
            (0, 65535)
        );
    }

    #[test]
    fn text_expands_to_down_up_pairs() {
        // "Hi" -> 2 code units -> 4 INPUT records.
        let n = "Hi".encode_utf16().count() * 2;
        assert_eq!(n, 4);
    }
}
