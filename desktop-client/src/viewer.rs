//! The Win32 viewer window: blits decoded frames and turns local mouse/keyboard
//! input into [`ClientMessage`]s.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use rc_protocol::{ClientMessage, InputEvent, PointerButton};
use tokio::sync::mpsc::UnboundedSender;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, FillRect, GetStockObject, InvalidateRect,
    SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, DIB_RGB_COLORS,
    HBRUSH, HDC, PAINTSTRUCT, SRCCOPY, STRETCH_HALFTONE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    ClipCursor, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetForegroundWindow, GetMessageW, GetWindowLongPtrW, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetCursor, SetCursorPos, SetWindowLongPtrW, SetWindowTextW,
    ShowWindow, TranslateMessage, CREATESTRUCTW, CW_USEDEFAULT, GWLP_USERDATA, HTCLIENT,
    IDC_ARROW, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_ACTIVATE, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOVE, WM_NCCREATE, WM_PAINT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

/// The latest decoded frame, top-down BGRA.
pub struct FrameBuf {
    pub w: i32,
    pub h: i32,
    pub bgra: Vec<u8>,
}

/// Shared between the Win32 UI thread and the async networking tasks.
pub struct ViewerState {
    pub input_tx: UnboundedSender<ClientMessage>,
    pub frame: Mutex<Option<FrameBuf>>,
}

static HWND_PTR: AtomicIsize = AtomicIsize::new(0);
/// The host says a game has captured its mouse.
static CAPTURED: AtomicBool = AtomicBool::new(false);
/// We're currently holding the local cursor hidden and clipped to the window,
/// turning mouse motion into relative deltas (only while focused).
static CLIPPED: AtomicBool = AtomicBool::new(false);
const WM_APP_CAPTURE: u32 = WM_APP + 1;

/// The host's cursor-capture state changed (safe from any thread).
pub fn set_captured(on: bool) {
    CAPTURED.store(on, Ordering::Relaxed);
    let h = HWND_PTR.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_APP_CAPTURE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Ask the window to repaint (safe from any thread).
pub fn request_repaint() {
    let h = HWND_PTR.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = InvalidateRect(HWND(h as *mut _), None, false);
        }
    }
}

/// Close the window from another thread (host disconnected, socket dropped).
pub fn post_close() {
    let h = HWND_PTR.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Update the title bar (e.g. to show "connected — 1920×1080").
pub fn set_title(text: &str) {
    let h = HWND_PTR.load(Ordering::Relaxed);
    if h != 0 {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = SetWindowTextW(HWND(h as *mut _), PCWSTR(wide.as_ptr()));
        }
    }
}

/// Create the window and pump messages until it closes.
pub fn run(state: Arc<ViewerState>) -> anyhow::Result<()> {
    unsafe {
        let hmod = GetModuleHandleW(PCWSTR::null())?;
        let hinstance = HINSTANCE(hmod.0);
        let class = w!("RcDesktopClientWnd");

        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class,
            hCursor: cursor,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            return Err(anyhow::anyhow!(
                "RegisterClassW failed: {:?}",
                windows::core::Error::from_win32()
            ));
        }

        // One Arc ref is handed to the window; reclaimed in WM_DESTROY.
        let raw = Arc::into_raw(state);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Remote Control"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            760,
            None,
            None,
            hinstance,
            Some(raw as *const _),
        )
        .map_err(|e| anyhow::anyhow!("CreateWindowExW failed: {e}"))?;

        HWND_PTR.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = ShowWindow(hwnd, SW_SHOW);
        tracing::info!("viewer window shown; entering message loop");

        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        tracing::info!("viewer message loop exited");
    }
    Ok(())
}

fn state_ref<'a>(hwnd: HWND) -> Option<&'a ViewerState> {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ViewerState;
        p.as_ref()
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lp.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wp, lp);
    }

    let Some(state) = state_ref(hwnd) else {
        return DefWindowProcW(hwnd, msg, wp, lp);
    };

    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            let _ = InvalidateRect(hwnd, None, false);
            apply_clip(hwnd, GetForegroundWindow() == hwnd);
            LRESULT(0)
        }
        WM_MOVE => {
            apply_clip(hwnd, GetForegroundWindow() == hwnd);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_ACTIVATE => {
            apply_clip(hwnd, (wp.0 & 0xFFFF) != 0);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_APP_CAPTURE => {
            apply_clip(hwnd, GetForegroundWindow() == hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR if CLIPPED.load(Ordering::Relaxed) && (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
            SetCursor(None);
            LRESULT(1)
        }
        WM_PAINT => {
            paint(hwnd, state);
            LRESULT(0)
        }

        WM_MOUSEMOVE => {
            if CLIPPED.load(Ordering::Relaxed) {
                // Captured: report how far the mouse moved from the window's
                // centre, then put it back there — unbounded relative motion,
                // like the game would get from a local mouse.
                let (x, y) = lparam_xy(lp);
                let c = client_center(hwnd);
                let (dx, dy) = (x - c.x, y - c.y);
                if dx != 0 || dy != 0 {
                    let _ = state
                        .input_tx
                        .send(ClientMessage::Input(InputEvent::PointerDelta {
                            dx: dx as f64,
                            dy: dy as f64,
                        }));
                    recenter(hwnd);
                }
            } else {
                emit_move(hwnd, state, lp);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let _ = SetCapture(hwnd);
            emit_button(hwnd, state, lp, PointerButton::Left, true);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            emit_button(hwnd, state, lp, PointerButton::Left, false);
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            emit_button(hwnd, state, lp, PointerButton::Right, true);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            emit_button(hwnd, state, lp, PointerButton::Right, false);
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            emit_button(hwnd, state, lp, PointerButton::Middle, true);
            LRESULT(0)
        }
        WM_MBUTTONUP => {
            emit_button(hwnd, state, lp, PointerButton::Middle, false);
            LRESULT(0)
        }
        WM_XBUTTONDOWN => {
            let b = if hiword(wp.0) == 1 {
                PointerButton::X1
            } else {
                PointerButton::X2
            };
            emit_button(hwnd, state, lp, b, true);
            LRESULT(1)
        }
        WM_XBUTTONUP => {
            let b = if hiword(wp.0) == 1 {
                PointerButton::X1
            } else {
                PointerButton::X2
            };
            emit_button(hwnd, state, lp, b, false);
            LRESULT(1)
        }
        WM_MOUSEWHEEL => {
            let dy = (hiword(wp.0) as i16) as f32 / 120.0;
            emit_scroll(hwnd, state, lp, 0.0, dy);
            LRESULT(0)
        }
        WM_MOUSEHWHEEL => {
            let dx = (hiword(wp.0) as i16) as f32 / 120.0;
            emit_scroll(hwnd, state, lp, dx, 0.0);
            LRESULT(0)
        }

        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let _ = state.input_tx.send(ClientMessage::Input(InputEvent::Key {
                code: wp.0 as u32,
                pressed: true,
            }));
            LRESULT(0)
        }
        WM_KEYUP | WM_SYSKEYUP => {
            let _ = state.input_tx.send(ClientMessage::Input(InputEvent::Key {
                code: wp.0 as u32,
                pressed: false,
            }));
            LRESULT(0)
        }

        WM_CLOSE => {
            let _ = state.input_tx.send(ClientMessage::Disconnect);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            HWND_PTR.store(0, Ordering::Relaxed);
            if CLIPPED.swap(false, Ordering::Relaxed) {
                let _ = ClipCursor(None);
            }
            // Reclaim the Arc ref we leaked into the window.
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ViewerState;
            if !p.is_null() {
                drop(Arc::from_raw(p));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

#[inline]
fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}
#[inline]
fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    let x = (lp.0 & 0xFFFF) as i16 as i32;
    let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
    (x, y)
}

/// Client-pixel → 0.0..=1.0 over the window's client area (== the streamed
/// region, which the viewer stretches to fill).
unsafe fn normalized(hwnd: HWND, lp: LPARAM) -> (f64, f64) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (x, y) = lparam_xy(lp);
    let w = (rc.right - rc.left).max(1) as f64;
    let h = (rc.bottom - rc.top).max(1) as f64;
    (
        (x as f64 / w).clamp(0.0, 1.0),
        (y as f64 / h).clamp(0.0, 1.0),
    )
}

unsafe fn client_center(hwnd: HWND) -> POINT {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    POINT {
        x: (rc.right - rc.left) / 2,
        y: (rc.bottom - rc.top) / 2,
    }
}

unsafe fn recenter(hwnd: HWND) {
    let mut c = client_center(hwnd);
    let _ = ClientToScreen(hwnd, &mut c);
    let _ = SetCursorPos(c.x, c.y);
}

/// Hide + clip the local cursor to the window while the host's game has its
/// mouse captured and this window is focused; release it otherwise (so
/// alt-tab always gives the mouse back).
unsafe fn apply_clip(hwnd: HWND, active: bool) {
    if CAPTURED.load(Ordering::Relaxed) && active {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let mut tl = POINT { x: rc.left, y: rc.top };
        let mut br = POINT { x: rc.right, y: rc.bottom };
        let _ = ClientToScreen(hwnd, &mut tl);
        let _ = ClientToScreen(hwnd, &mut br);
        let screen = RECT { left: tl.x, top: tl.y, right: br.x, bottom: br.y };
        let _ = ClipCursor(Some(&screen as *const RECT));
        CLIPPED.store(true, Ordering::Relaxed);
        recenter(hwnd);
        SetCursor(None);
    } else if CLIPPED.swap(false, Ordering::Relaxed) {
        let _ = ClipCursor(None);
    }
}

unsafe fn emit_move(hwnd: HWND, state: &ViewerState, lp: LPARAM) {
    let (x, y) = normalized(hwnd, lp);
    let _ = state
        .input_tx
        .send(ClientMessage::Input(InputEvent::PointerMove { x, y }));
}
unsafe fn emit_button(
    hwnd: HWND,
    state: &ViewerState,
    lp: LPARAM,
    button: PointerButton,
    pressed: bool,
) {
    let event = if CLIPPED.load(Ordering::Relaxed) {
        InputEvent::PointerButtonInPlace { button, pressed }
    } else {
        let (x, y) = normalized(hwnd, lp);
        InputEvent::PointerButton { button, pressed, x, y }
    };
    let _ = state.input_tx.send(ClientMessage::Input(event));
}
unsafe fn emit_scroll(hwnd: HWND, state: &ViewerState, lp: LPARAM, dx: f32, dy: f32) {
    // Wheel messages carry screen coords, not client — but our injector only
    // needs a plausible anchor, and the host cursor is already tracking moves.
    let (x, y) = normalized(hwnd, lp);
    let _ = state
        .input_tx
        .send(ClientMessage::Input(InputEvent::Scroll { dx, dy, x, y }));
}

unsafe fn paint(hwnd: HWND, state: &ViewerState) {
    let mut ps = PAINTSTRUCT::default();
    let hdc: HDC = BeginPaint(hwnd, &mut ps);

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);

    let guard = state.frame.lock().unwrap();
    if let Some(fb) = guard.as_ref() {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: fb.w,
                biHeight: -fb.h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };
        SetStretchBltMode(hdc, STRETCH_HALFTONE);
        StretchDIBits(
            hdc,
            0,
            0,
            rc.right - rc.left,
            rc.bottom - rc.top,
            0,
            0,
            fb.w,
            fb.h,
            Some(fb.bgra.as_ptr() as *const _),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    } else {
        FillRect(hdc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));
    }

    let _ = EndPaint(hwnd, &ps);
}
