//! WASM bridge for the browser client.
//!
//! Reuses the *exact* Noise handshake, chunk framing and protocol codec the
//! native clients use, so the wire is guaranteed compatible and no crypto is
//! re-implemented in JavaScript. JS handles the WebSocket, WebCodecs decode and
//! the touch UI; everything security-relevant happens here.

use serde::Serialize;
use snow::{HandshakeState, TransportState};
use wasm_bindgen::prelude::*;

use rc_protocol::{ClientMessage, HostMessage, InputEvent, PointerButton, QualityMode};

// Must match rc_transport::lan.
const NOISE_PAIR: &str = "Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s";
const NOISE_RESUME: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const MODE_PAIR: u8 = 0x01;
const MODE_RESUME: u8 = 0x02;
const MAX_CHUNK: usize = 48 * 1024;
const MAX_CHUNKS: usize = 8192;

/// Transport channels — must match `rc_transport::lan`.
pub const CH_CONTROL: u8 = 0;
pub const CH_VIDEO: u8 = 1;
const CH_KEEPALIVE: u8 = 2;
const CH_KEYFRAME_REQ: u8 = 3;
/// Encoded microphone packets, browser → host — must match `rc_transport::lan`.
const CH_MIC: u8 = 5;

fn js(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}
fn jss(s: &str) -> JsValue {
    JsValue::from_str(s)
}

/// SHA-256 domain-separated pairing-code KDF — identical to
/// `rc_crypto::derive_pairing_psk`.
fn derive_psk(code: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"remote-control/lan-pairing-psk/v1\0");
    h.update(code.trim().as_bytes());
    h.finalize().into()
}

/// 32 random bytes for the browser's persistent static X25519 secret. Store in
/// `localStorage` and reuse; it's this device's identity to every host.
#[wasm_bindgen]
pub fn gen_static_key() -> Vec<u8> {
    let mut k = [0u8; 32];
    getrandom::getrandom(&mut k).expect("browser RNG");
    k.to_vec()
}

/// One encrypted end-to-end session with a host, over a browser WebSocket that
/// the relay bridges to the host.
#[wasm_bindgen]
pub struct Session {
    hs: Option<HandshakeState>,
    ts: Option<TransportState>,
    rx: Vec<u8>,
    mode: u8,
    expected_host_key: Option<[u8; 32]>,
    peer_key: Option<[u8; 32]>,
    /// Keepalive echoes queued by `feed()`, waiting for `take_keepalive_replies`
    /// to hand them to JS for sending. See that method for why this exists.
    keepalive_replies: Vec<Vec<u8>>,
}

fn build(params: &str, static_sk: &[u8], psk: Option<[u8; 32]>) -> Result<HandshakeState, JsValue> {
    if static_sk.len() != 32 {
        return Err(jss("static key must be 32 bytes"));
    }
    let mut b = snow::Builder::new(params.parse().map_err(js)?).local_private_key(static_sk);
    if let Some(k) = &psk {
        b = b.psk(0, k);
    }
    b.build_initiator().map_err(js)
}

#[wasm_bindgen]
impl Session {
    /// First contact: authenticate with the 6-digit pairing code (XXpsk0).
    pub fn new_pair(static_sk: &[u8], pairing_code: &str) -> Result<Session, JsValue> {
        Ok(Session {
            hs: Some(build(NOISE_PAIR, static_sk, Some(derive_psk(pairing_code)))?),
            ts: None,
            rx: Vec::new(),
            mode: MODE_PAIR,
            expected_host_key: None,
            peer_key: None,
            keepalive_replies: Vec::new(),
        })
    }

    /// Reconnect: authenticate by our pinned copy of the host's static key (XX).
    pub fn new_resume(static_sk: &[u8], host_key: &[u8]) -> Result<Session, JsValue> {
        let hk: [u8; 32] = host_key.try_into().map_err(|_| jss("host key must be 32 bytes"))?;
        Ok(Session {
            hs: Some(build(NOISE_RESUME, static_sk, None)?),
            ts: None,
            rx: Vec::new(),
            mode: MODE_RESUME,
            expected_host_key: Some(hk),
            peer_key: None,
            keepalive_replies: Vec::new(),
        })
    }

    /// `true` once the handshake has finished.
    pub fn ready(&self) -> bool {
        self.ts.is_some()
    }

    /// The host's static key, once the handshake has finished. Pin it after a
    /// pairing so future connects can `new_resume`.
    pub fn peer_key(&self) -> Option<Vec<u8>> {
        self.peer_key.map(|k| k.to_vec())
    }

    /// The first bytes to send: `[mode byte] ++ [u16 len][noise msg1]`.
    pub fn first_message(&mut self) -> Result<Vec<u8>, JsValue> {
        let hs = self.hs.as_mut().ok_or_else(|| jss("handshake already finished"))?;
        let mut buf = [0u8; 2048];
        let n = hs.write_message(&[], &mut buf).map_err(js)?;
        let mut out = Vec::with_capacity(n + 3);
        out.push(self.mode);
        out.extend_from_slice(&frame_u16(&buf[..n]));
        Ok(out)
    }

    /// Feed handshake bytes. Returns the next framed message to send (or `null`
    /// when nothing more to send). Call [`Session::ready`] to know when done;
    /// leftover bytes are kept for [`Session::feed`].
    pub fn read_handshake(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>, JsValue> {
        self.rx.extend_from_slice(bytes);
        let mut buf = [0u8; 2048];
        let mut to_send: Option<Vec<u8>> = None;

        loop {
            let hs = match self.hs.as_mut() {
                Some(h) => h,
                None => break,
            };
            if hs.is_handshake_finished() {
                break;
            }
            if hs.is_my_turn() {
                let n = hs.write_message(&[], &mut buf).map_err(js)?;
                to_send = Some(frame_u16(&buf[..n]));
                continue;
            }
            // Need an inbound framed message.
            if self.rx.len() < 2 {
                break;
            }
            let len = u16::from_be_bytes([self.rx[0], self.rx[1]]) as usize;
            if self.rx.len() < 2 + len {
                break;
            }
            let msg = self.rx[2..2 + len].to_vec();
            self.rx.drain(0..2 + len);
            hs.read_message(&msg, &mut buf).map_err(js)?;
        }

        if let Some(hs) = self.hs.take_if(|h| h.is_handshake_finished()) {
            self.peer_key = hs
                .get_remote_static()
                .and_then(|k| k.try_into().ok());
            if self.mode == MODE_RESUME && self.peer_key != self.expected_host_key {
                return Err(jss("the PC presented a different identity key — not connecting"));
            }
            self.ts = Some(hs.into_transport_mode().map_err(js)?);
        }
        Ok(to_send)
    }

    /// Feed bytes received after the handshake. Returns an array of
    /// `[channel:number, payload:Uint8Array]` for every complete message now
    /// available (pass an empty array just to drain buffered bytes).
    pub fn feed(&mut self, bytes: &[u8]) -> Result<js_sys::Array, JsValue> {
        self.rx.extend_from_slice(bytes);
        let out = js_sys::Array::new();
        while let Some((ch, payload)) = self.take_message()? {
            if ch == CH_KEEPALIVE {
                // The host's RTT measurement (crates/transport::lan's
                // keepalive_loop) is otherwise dead for a browser session —
                // it works by exactly this echo, type 0 (ping) -> type 1
                // (reply) with the same 8-byte timestamp, same as the native
                // reader loop does. No need to originate our own pings: the
                // host already sends one every 2s.
                if payload.len() == 9 && payload[0] == 0 {
                    let mut echo = payload.clone();
                    echo[0] = 1;
                    if let Ok(sealed) = self.seal(CH_KEEPALIVE, &echo) {
                        self.keepalive_replies.push(sealed);
                    }
                }
                continue;
            }
            let entry = js_sys::Array::new();
            entry.push(&JsValue::from_f64(ch as f64));
            entry.push(&js_sys::Uint8Array::from(&payload[..]));
            out.push(&entry);
        }
        Ok(out)
    }

    /// Framed bytes queued by `feed()` echoing a keepalive ping — call once
    /// right after `feed()` and send each one over the WebSocket. Separate
    /// from `feed()`'s return value because sending is JS's job everywhere
    /// else in this API (`seal_control`, `seal_keyframe_request`, …); this
    /// keeps that split instead of having Rust reach back into the socket.
    pub fn take_keepalive_replies(&mut self) -> js_sys::Array {
        let out = js_sys::Array::new();
        for bytes in self.keepalive_replies.drain(..) {
            out.push(&js_sys::Uint8Array::from(&bytes[..]));
        }
        out
    }

    /// Encrypt+frame a control payload (from the `enc_*` helpers) for sending.
    pub fn seal_control(&mut self, payload: &[u8]) -> Result<Vec<u8>, JsValue> {
        self.seal(CH_CONTROL, payload)
    }

    /// Ask the host for a fresh keyframe — same empty message on the
    /// keyframe-request channel that `rc_transport::lan`'s native
    /// `request_key_frame()` sends. Call this whenever the decoder reports
    /// corrupt/undecodable video: without it there is no way to recover
    /// from a lost or garbled frame short of reconnecting.
    pub fn seal_keyframe_request(&mut self) -> Result<Vec<u8>, JsValue> {
        self.seal(CH_KEYFRAME_REQ, &[])
    }

    /// Encrypt+frame a microphone packet (already-encoded ADPCM bytes, same
    /// layout as `crates/audio/src/adpcm.rs`) — the browser's side of the
    /// native client's `LanSession::send_mic`. Only the host reads this
    /// channel; nothing is ever sent back on it.
    pub fn seal_mic(&mut self, payload: &[u8]) -> Result<Vec<u8>, JsValue> {
        self.seal(CH_MIC, payload)
    }

    fn seal(&mut self, channel: u8, payload: &[u8]) -> Result<Vec<u8>, JsValue> {
        let ts = self.ts.as_mut().ok_or_else(|| jss("not connected"))?;
        let mut plain = Vec::with_capacity(payload.len() + 1);
        plain.push(channel);
        plain.extend_from_slice(payload);

        let mut chunks: Vec<Vec<u8>> = Vec::new();
        for chunk in plain.chunks(MAX_CHUNK) {
            let mut ct = vec![0u8; chunk.len() + 16];
            let n = ts.write_message(chunk, &mut ct).map_err(js)?;
            ct.truncate(n);
            chunks.push(ct);
        }
        let mut out = Vec::with_capacity(4 + plain.len() + 32);
        out.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
        for c in &chunks {
            out.extend_from_slice(&(c.len() as u16).to_be_bytes());
            out.extend_from_slice(c);
        }
        Ok(out)
    }

    fn take_message(&mut self) -> Result<Option<(u8, Vec<u8>)>, JsValue> {
        let Some(ts) = self.ts.as_mut() else {
            return Ok(None);
        };
        if self.rx.len() < 4 {
            return Ok(None);
        }
        let n_chunks =
            u32::from_be_bytes([self.rx[0], self.rx[1], self.rx[2], self.rx[3]]) as usize;
        if n_chunks == 0 || n_chunks > MAX_CHUNKS {
            return Err(jss("bad chunk count"));
        }
        let mut off = 4;
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(n_chunks);
        for _ in 0..n_chunks {
            if self.rx.len() < off + 2 {
                return Ok(None);
            }
            let clen = u16::from_be_bytes([self.rx[off], self.rx[off + 1]]) as usize;
            off += 2;
            if self.rx.len() < off + clen {
                return Ok(None);
            }
            ranges.push((off, off + clen));
            off += clen;
        }

        let mut plain = Vec::new();
        for (a, b) in &ranges {
            let ct = &self.rx[*a..*b];
            let mut pt = vec![0u8; ct.len()];
            let n = ts.read_message(ct, &mut pt).map_err(js)?;
            plain.extend_from_slice(&pt[..n]);
        }
        self.rx.drain(0..off);

        if plain.is_empty() {
            return Err(jss("empty message"));
        }
        Ok(Some((plain[0], plain[1..].to_vec())))
    }
}

fn frame_u16(msg: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(msg.len() + 2);
    v.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    v.extend_from_slice(msg);
    v
}

// ── relay preamble ──────────────────────────────────────────────────────────

/// The `Hello::Client` a browser sends as its first WebSocket message
/// (no length prefix — the WS frame is the boundary).
#[wasm_bindgen]
pub fn encode_client_hello(device_id: &str, key: Option<String>) -> Vec<u8> {
    rc_protocol::encode(&rc_protocol::relay::Hello::Client {
        device_id: device_id.to_string(),
        key: key.filter(|s| !s.is_empty()),
    })
}

/// A `Hello::Query` — the relay answers with one ACK byte (online/offline) and
/// closes, without disturbing the parked host.
#[wasm_bindgen]
pub fn encode_query_hello(device_id: &str, key: Option<String>) -> Vec<u8> {
    rc_protocol::encode(&rc_protocol::relay::Hello::Query {
        device_id: device_id.to_string(),
        key: key.filter(|s| !s.is_empty()),
    })
}

pub use rc_protocol::relay::{ACK_BAD_KEY, ACK_HOST_OFFLINE, ACK_OK};

/// Expose the ACK constants to JS.
#[wasm_bindgen]
pub fn ack_ok() -> u8 {
    ACK_OK
}
#[wasm_bindgen]
pub fn ack_host_offline() -> u8 {
    ACK_HOST_OFFLINE
}
#[wasm_bindgen]
pub fn ack_bad_key() -> u8 {
    ACK_BAD_KEY
}

// ── control-message encoders (→ postcard bytes for `seal_control`) ───────────

fn enc(msg: &ClientMessage) -> Vec<u8> {
    rc_protocol::encode(msg)
}

#[wasm_bindgen]
pub fn enc_pointer_move(x: f64, y: f64) -> Vec<u8> {
    enc(&ClientMessage::Input(InputEvent::PointerMove { x, y }))
}

/// Relative motion for a captured cursor (mouse-look). See
/// `InputEvent::PointerDelta` — feed it `MouseEvent.movementX/Y` under
/// Pointer Lock on a desktop, or raw touch-drag deltas on a phone.
#[wasm_bindgen]
pub fn enc_pointer_delta(dx: f64, dy: f64) -> Vec<u8> {
    enc(&ClientMessage::Input(InputEvent::PointerDelta { dx, dy }))
}

/// Click without repositioning first — use this instead of
/// `enc_pointer_button` whenever the pointer is captured, or the click drags
/// the camera with it. See `InputEvent::PointerButtonInPlace`.
#[wasm_bindgen]
pub fn enc_pointer_button_in_place(button: u8, pressed: bool) -> Vec<u8> {
    enc(&ClientMessage::Input(InputEvent::PointerButtonInPlace {
        button: map_button(button),
        pressed,
    }))
}

#[wasm_bindgen]
pub fn enc_pointer_button(button: u8, pressed: bool, x: f64, y: f64) -> Vec<u8> {
    enc(&ClientMessage::Input(InputEvent::PointerButton {
        button: map_button(button),
        pressed,
        x,
        y,
    }))
}

#[wasm_bindgen]
pub fn enc_scroll(dx: f32, dy: f32, x: f64, y: f64) -> Vec<u8> {
    enc(&ClientMessage::Input(InputEvent::Scroll { dx, dy, x, y }))
}

#[wasm_bindgen]
pub fn enc_key(code: u32, pressed: bool) -> Vec<u8> {
    enc(&ClientMessage::Input(InputEvent::Key { code, pressed }))
}

#[wasm_bindgen]
pub fn enc_text(text: &str) -> Vec<u8> {
    enc(&ClientMessage::Input(InputEvent::Text(text.to_string())))
}

#[wasm_bindgen]
pub fn enc_ping(nonce: f64) -> Vec<u8> {
    enc(&ClientMessage::Ping { nonce: nonce as u64 })
}

#[wasm_bindgen]
pub fn enc_clipboard(text: &str) -> Vec<u8> {
    enc(&ClientMessage::ClipboardText(text.to_string()))
}

#[wasm_bindgen]
pub fn enc_quality(mode: &str) -> Vec<u8> {
    let m = match mode {
        "low" => QualityMode::Low,
        "high" => QualityMode::High,
        "auto" => QualityMode::Auto,
        _ => QualityMode::Balanced,
    };
    enc(&ClientMessage::SetQuality(m))
}

#[wasm_bindgen]
pub fn enc_disconnect() -> Vec<u8> {
    enc(&ClientMessage::Disconnect)
}

/// Offer the host a direct (same-network) path: this browser's WebRTC SDP offer.
#[wasm_bindgen]
pub fn enc_direct_offer(sdp: &str) -> Vec<u8> {
    enc(&ClientMessage::DirectOffer(sdp.to_string()))
}

/// First message over a finished direct path: move the session onto it.
#[wasm_bindgen]
pub fn enc_direct_use() -> Vec<u8> {
    enc(&ClientMessage::DirectUse)
}

/// One controller frame. `buttons` is already XInput's bit layout (see
/// `GAMEPAD_BUTTON_*` constants below) — the host hands it straight to a
/// virtual Xbox 360 controller with no remapping.
#[wasm_bindgen]
#[allow(clippy::too_many_arguments)]
pub fn enc_gamepad_state(
    buttons: u16,
    left_trigger: u8,
    right_trigger: u8,
    thumb_lx: i16,
    thumb_ly: i16,
    thumb_rx: i16,
    thumb_ry: i16,
) -> Vec<u8> {
    enc(&ClientMessage::GamepadState {
        buttons,
        left_trigger,
        right_trigger,
        thumb_lx,
        thumb_ly,
        thumb_rx,
        thumb_ry,
    })
}

#[wasm_bindgen]
pub fn enc_gamepad_disconnect() -> Vec<u8> {
    enc(&ClientMessage::GamepadDisconnect)
}

/// Offer a file to the host: the start of a transfer. Chunk it with
/// `enc_file_chunk` afterward and finish with `enc_file_done`.
#[wasm_bindgen]
pub fn enc_file_offer(id: u32, name: &str, size: f64) -> Vec<u8> {
    enc(&ClientMessage::FileOffer { id, name: name.to_string(), size: size as u64 })
}

#[wasm_bindgen]
pub fn enc_file_chunk(id: u32, offset: f64, data: &[u8]) -> Vec<u8> {
    enc(&ClientMessage::FileChunk { id, offset: offset as u64, data: data.to_vec() })
}

#[wasm_bindgen]
pub fn enc_file_done(id: u32) -> Vec<u8> {
    enc(&ClientMessage::FileDone { id })
}

/// Ask the PC app about itself (version, virtual mic) - a reply of
/// `kind: "info"` follows; an old PC app just never answers.
#[wasm_bindgen]
pub fn enc_request_info() -> Vec<u8> {
    enc(&ClientMessage::RequestInfo)
}

/// List a folder on the PC (empty string = the starting list).
#[wasm_bindgen]
pub fn enc_list_dir(path: &str) -> Vec<u8> {
    enc(&ClientMessage::ListDir { path: path.to_string() })
}

/// Ask the PC to send one of its files to this device.
#[wasm_bindgen]
pub fn enc_get_file(path: &str) -> Vec<u8> {
    enc(&ClientMessage::GetFile { path: path.to_string() })
}

/// Paste files this device already uploaded (by transfer id) into whatever
/// text box has focus on the PC.
#[wasm_bindgen]
pub fn enc_paste_files(ids: Vec<u32>) -> Vec<u8> {
    enc(&ClientMessage::PasteFiles { ids })
}

fn map_button(b: u8) -> PointerButton {
    match b {
        1 => PointerButton::Right,
        2 => PointerButton::Middle,
        3 => PointerButton::X1,
        4 => PointerButton::X2,
        _ => PointerButton::Left,
    }
}

// ── host-message decoders ──────────────────────────────────────────────────

/// Decode a channel-0 payload into a plain JS object, e.g.
/// `{ kind: "displays", displays: [{ width, height, ... }] }`.
#[wasm_bindgen]
pub fn decode_host_message(payload: &[u8]) -> Result<JsValue, JsValue> {
    let msg: HostMessage = rc_protocol::decode(payload).map_err(js)?;
    let v = match msg {
        HostMessage::Pong { nonce } => HostMsgJs::Pong { nonce: nonce as f64 },
        HostMessage::ClipboardText(text) => HostMsgJs::Clipboard { text },
        HostMessage::Notice(text) => HostMsgJs::Notice { text },
        HostMessage::Disconnect { reason } => HostMsgJs::Disconnect { reason },
        HostMessage::CursorCaptured(captured) => HostMsgJs::CursorCaptured { captured },
        HostMessage::GameArea { x, y, w, h } => HostMsgJs::GameArea { x, y, w, h },
        HostMessage::DirectAnswer(sdp) => HostMsgJs::DirectAnswer { sdp },
        HostMessage::DirectUnavailable => HostMsgJs::DirectUnavailable,
        HostMessage::FileOffer { id, name, size } => HostMsgJs::FileOffer { id, name, size: size as f64 },
        HostMessage::FileChunk { id, offset, data } => {
            // Chunk bytes go back as their own typed array, not through serde
            // (which would base64 or array-of-numbers them) — see the
            // `js_sys::Uint8Array` field handling in the caller.
            return Ok(file_chunk_js(id, offset as f64, &data));
        }
        HostMessage::FileDone { id } => HostMsgJs::FileDone { id },
        HostMessage::Info { version, virtual_mic } => HostMsgJs::Info { version, virtual_mic },
        HostMessage::TextFocus(focused) => HostMsgJs::TextFocus { focused },
        HostMessage::MicStatus { receiving, virtual_cable } => HostMsgJs::MicStatus { receiving, virtual_cable },
        HostMessage::DirListing { path, parent, entries, error } => HostMsgJs::DirListing {
            path,
            parent,
            entries: entries
                .into_iter()
                .map(|e| DirEntryJs { name: e.name, is_dir: e.is_dir, size: e.size as f64 })
                .collect(),
            error,
        },
        HostMessage::Displays(d) => HostMsgJs::Displays {
            displays: d
                .into_iter()
                .map(|x| DisplayJs {
                    width: x.width,
                    height: x.height,
                    x: x.x,
                    y: x.y,
                    scale: x.scale,
                    primary: x.primary,
                })
                .collect(),
        },
    };
    serde_wasm_bindgen::to_value(&v).map_err(js)
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum HostMsgJs {
    Pong { nonce: f64 },
    Clipboard { text: String },
    Notice { text: String },
    Disconnect { reason: String },
    Displays { displays: Vec<DisplayJs> },
    /// `kind: "cursorcaptured"`
    CursorCaptured { captured: bool },
    /// `kind: "gamearea"` — fractions of the streamed display.
    GameArea { x: f32, y: f32, w: f32, h: f32 },
    /// `kind: "directanswer"` — the host's WebRTC SDP answer.
    DirectAnswer { sdp: String },
    /// `kind: "directunavailable"`
    DirectUnavailable,
    /// `kind: "fileoffer"` — the host wants to send a file to this device.
    FileOffer { id: u32, name: String, size: f64 },
    /// `kind: "filedone"`
    FileDone { id: u32 },
    /// `kind: "info"` - reply to `enc_request_info`.
    Info { version: String, virtual_mic: bool },
    /// `kind: "textfocus"` - keyboard focus on the PC entered/left a text box.
    TextFocus { focused: bool },
    /// `kind: "micstatus"` - whether the PC is receiving this device's mic.
    MicStatus { receiving: bool, virtual_cable: bool },
    /// `kind: "dirlisting"` - reply to `enc_list_dir`.
    DirListing { path: String, parent: Option<String>, entries: Vec<DirEntryJs>, error: Option<String> },
}

#[derive(Serialize)]
struct DirEntryJs {
    name: String,
    is_dir: bool,
    size: f64,
}

/// A `HostMessage::FileChunk`'s bytes go out as a real `Uint8Array` rather
/// than through `serde_wasm_bindgen` (which would turn them into a JSON array
/// of numbers) — file chunks are the one payload here big enough for that to
/// matter.
fn file_chunk_js(id: u32, offset: f64, data: &[u8]) -> JsValue {
    let obj = js_sys::Object::new();
    let set = |k: &str, v: &JsValue| {
        let _ = js_sys::Reflect::set(&obj, &jss(k), v);
    };
    set("kind", &jss("filechunk"));
    set("id", &JsValue::from_f64(id as f64));
    set("offset", &JsValue::from_f64(offset));
    set("data", &js_sys::Uint8Array::from(data));
    obj.into()
}

#[derive(Serialize)]
struct DisplayJs {
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    scale: f32,
    primary: bool,
}

/// Split a channel-1 (video) payload into `{ key_frame, timestamp_us, data }`.
/// `data` is the raw Annex-B access unit for WebCodecs.
#[wasm_bindgen]
pub fn parse_video_payload(payload: &[u8]) -> Result<JsValue, JsValue> {
    if payload.len() < 9 {
        return Err(jss("short video payload"));
    }
    let key_frame = payload[0] != 0;
    let timestamp_us = u64::from_le_bytes(payload[1..9].try_into().unwrap());
    let obj = js_sys::Object::new();
    js_sys::Reflect::set(&obj, &"key_frame".into(), &JsValue::from_bool(key_frame))?;
    js_sys::Reflect::set(&obj, &"timestamp_us".into(), &JsValue::from_f64(timestamp_us as f64))?;
    js_sys::Reflect::set(&obj, &"data".into(), &js_sys::Uint8Array::from(&payload[9..]))?;
    Ok(obj.into())
}

/// Extract `avc1.PPCCLL` (profile / constraints / level) from an Annex-B buffer
/// that contains an SPS NAL, for `VideoDecoder.configure`.
#[wasm_bindgen]
pub fn avc_codec_string(annexb: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 4 < annexb.len() {
        let sc = if annexb[i..i + 3] == [0, 0, 1] {
            3
        } else if i + 4 <= annexb.len() && annexb[i..i + 4] == [0, 0, 0, 1] {
            4
        } else {
            i += 1;
            continue;
        };
        let nal_type = annexb[i + sc] & 0x1F;
        if nal_type == 7 && i + sc + 4 <= annexb.len() {
            let p = annexb[i + sc + 1];
            let c = annexb[i + sc + 2];
            let l = annexb[i + sc + 3];
            return Some(format!("avc1.{p:02x}{c:02x}{l:02x}"));
        }
        i += sc + 1;
    }
    None
}

/// Does this Annex-B access unit contain an IDR slice (NAL type 5)?
#[wasm_bindgen]
pub fn is_keyframe(annexb: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < annexb.len() {
        let sc = if annexb[i..i + 3] == [0, 0, 1] {
            3
        } else if i + 4 <= annexb.len() && annexb[i..i + 4] == [0, 0, 0, 1] {
            4
        } else {
            i += 1;
            continue;
        };
        if annexb[i + sc] & 0x1F == 5 {
            return true;
        }
        i += sc + 1;
    }
    false
}
