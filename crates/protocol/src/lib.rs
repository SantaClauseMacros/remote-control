//! Wire protocol shared between the host and its clients.
//!
//! The [`transport`](../rc_transport/index.html) layer owns framing, ordering
//! and encryption. This crate only defines the *payloads* exchanged over the
//! authenticated control channel, plus the small enums the settings UI needs.
//!
//! Keep every type here `serde`-serialisable and free of platform types so the
//! same definitions compile for the host, the desktop client and (via FFI /
//! codegen) the mobile client.

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Bumped on any breaking change to the message set below. The peers exchange
/// this during the handshake and refuse to proceed on mismatch.
pub const PROTOCOL_VERSION: u16 = 1;

/// Compact binary encoding for one control-channel message.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_allocvec(msg).expect("control messages are always serialisable")
}

/// Decode one control-channel message.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

/// Messages sent from a controlling client to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Keep-alive; the host replies with [`HostMessage::Pong`] carrying the
    /// same nonce so the client can measure round-trip time.
    Ping { nonce: u64 },
    /// A single mouse / keyboard / text action.
    Input(InputEvent),
    /// The client's clipboard changed and text sync is enabled on both ends.
    ClipboardText(String),
    /// Ask the host to (re)negotiate the video stream to this quality target.
    SetQuality(QualityMode),
    /// Client is leaving; the host should tear the session down promptly.
    Disconnect,
    /// Offer to move the session onto a direct (same-network) path: the
    /// browser's WebRTC SDP offer. The host replies `DirectAnswer` or
    /// `DirectUnavailable`. Appended last so older hosts just ignore it.
    DirectOffer(String),
    /// The first message over a direct path once its own handshake is done:
    /// "carry the session over this one now".
    DirectUse,
    /// One controller's state this frame — the field layout matches XInput's
    /// `XINPUT_GAMEPAD` exactly, so the host can hand it straight to a virtual
    /// Xbox 360 controller (see `host::gamepad`) with no remapping.
    GamepadState {
        buttons: u16,
        left_trigger: u8,
        right_trigger: u8,
        thumb_lx: i16,
        thumb_ly: i16,
        thumb_rx: i16,
        thumb_ry: i16,
    },
    /// The device's controller was unplugged or the tab lost it.
    GamepadDisconnect,
    /// Offer to send a file to the host: a new transfer `id` (unique for this
    /// session), its name and total size. Chunks follow on `FileChunk`.
    FileOffer { id: u32, name: String, size: u64 },
    /// One piece of a file offered by `FileOffer`. `offset` is only for the
    /// receiver's progress reporting — chunks arrive in order over this
    /// reliable, ordered transport, so it's never used to seek.
    FileChunk { id: u32, offset: u64, data: Vec<u8> },
    /// All chunks for `id` have been sent.
    FileDone { id: u32 },
}

/// Messages sent from the host to a controlling client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMessage {
    Pong {
        nonce: u64,
    },
    /// The host clipboard changed and text sync is enabled.
    ClipboardText(String),
    /// Layout of the host's monitors — sent on connect and whenever it changes.
    Displays(Vec<DisplayInfo>),
    /// Non-fatal notice to surface to the user
    /// (e.g. "clipboard sync is turned off").
    Notice(String),
    /// The host is ending the session; `reason` is human-readable.
    Disconnect {
        reason: String,
    },
    /// A game on the host has captured the mouse (hidden or clipped the
    /// cursor) — `true` — or released it. While captured, clients should send
    /// relative motion (`PointerDelta`) and in-place clicks. Appended last so
    /// older clients' variant numbering is unchanged.
    CursorCaptured(bool),
    /// Where the window that captured the mouse draws, as fractions of the
    /// streamed display (a window partly off-screen can fall outside
    /// `0.0..=1.0`). Lets a client pin game-UI overlays — a hotbar — to the
    /// game itself rather than the screen, which differ for a windowed or
    /// maximized game. Sent while captured, whenever it changes.
    GameArea {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
    /// The host's SDP answer to a `DirectOffer`.
    DirectAnswer(String),
    /// The host can't set up a direct path right now.
    DirectUnavailable,
    /// The host is offering to send a file to the device (see
    /// `ClientMessage::FileOffer` for the field meanings — same shape, other
    /// direction).
    FileOffer { id: u32, name: String, size: u64 },
    /// One piece of a host-initiated file transfer.
    FileChunk { id: u32, offset: u64, data: Vec<u8> },
    /// All chunks for `id` have been sent.
    FileDone { id: u32 },
}

/// One monitor on the host, in virtual-desktop pixel coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// DPI scale factor (1.0 = 96 DPI).
    pub scale: f32,
}

/// A single input action.
///
/// Pointer positions are normalised to `0.0..=1.0` across the **virtual
/// desktop** so they stay correct regardless of the stream resolution the
/// client is currently receiving.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    PointerMove {
        x: f64,
        y: f64,
    },
    /// Relative mouse motion, in host-side pixels, for a **captured** cursor
    /// — a game reading raw/relative input for camera look (most FPS/
    /// third-person titles) rather than tracking the OS cursor position.
    /// Unlike `PointerMove` these deltas have no positional bound, so a
    /// client can send an unbroken stream of them (e.g. from the Pointer
    /// Lock API) the same way a real mouse would for an unlimited look
    /// gesture. The host injects them with `SendInput`'s relative
    /// (non-`MOUSEEVENTF_ABSOLUTE`) mode instead of repositioning the cursor.
    PointerDelta {
        dx: f64,
        dy: f64,
    },
    /// Press/release wherever the pointer already is, without positioning it
    /// first — the companion to `PointerDelta` for a captured cursor. Sending
    /// the ordinary `PointerButton` there would carry a now-meaningless
    /// absolute position, and Windows turns an absolute move into a *relative*
    /// delta for raw-input readers, so the game's camera would jerk on every
    /// click.
    PointerButtonInPlace {
        button: PointerButton,
        pressed: bool,
    },
    PointerButton {
        button: PointerButton,
        pressed: bool,
        x: f64,
        y: f64,
    },
    Scroll {
        /// Wheel deltas in notches; positive `dy` scrolls up.
        dx: f32,
        dy: f32,
        x: f64,
        y: f64,
    },
    /// A virtual-key press or release (Windows VK_* code).
    Key {
        code: u32,
        pressed: bool,
    },
    /// Unicode text entry from a soft keyboard / IME. The host injects this
    /// with `KEYEVENTF_UNICODE` rather than mapping to scan codes.
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    /// Back / forward thumb buttons.
    X1,
    X2,
}

/// Performance/quality target for the video stream. `Auto` lets the host's
/// adaptive controller pick based on measured bandwidth and RTT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum QualityMode {
    Low,
    #[default]
    Balanced,
    High,
    Auto,
}

/// The rendezvous-relay preamble, exchanged in the clear before the end-to-end
/// Noise handshake. The relay matches a parked host to an arriving client by
/// `device_id` (a public key fingerprint) and then only copies ciphertext — it
/// never has the pairing PSK, so it cannot read or forge the session.
pub mod relay {
    use serde::{Deserialize, Serialize};

    /// First framed message a peer sends to the relay (`u32` length prefix +
    /// `postcard` bytes).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum Hello {
        /// "Park this connection until a client asks for `device_id`."
        Host {
            device_id: String,
            key: Option<String>,
        },
        /// "Splice me to the host parked under `device_id`."
        Client {
            device_id: String,
            key: Option<String>,
        },
        /// "Is a host parked under `device_id`?" The relay replies with one ACK
        /// byte (`ACK_OK` / `ACK_HOST_OFFLINE`) and closes, without consuming
        /// the parked connection.
        Query {
            device_id: String,
            key: Option<String>,
        },
    }

    /// One status byte the relay writes back before it starts copying.
    pub const ACK_OK: u8 = 1;
    pub const ACK_HOST_OFFLINE: u8 = 2;
    pub const ACK_BAD_KEY: u8 = 3;
    pub const ACK_BUSY: u8 = 4;

    /// Default relay TCP port.
    pub const DEFAULT_PORT: u16 = 9878;
}
