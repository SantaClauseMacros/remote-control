/* tslint:disable */
/* eslint-disable */

/**
 * One encrypted end-to-end session with a host, over a browser WebSocket that
 * the relay bridges to the host.
 */
export class Session {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Feed bytes received after the handshake. Returns an array of
     * `[channel:number, payload:Uint8Array]` for every complete message now
     * available (pass an empty array just to drain buffered bytes).
     */
    feed(bytes: Uint8Array): Array<any>;
    /**
     * The first bytes to send: `[mode byte] ++ [u16 len][noise msg1]`.
     */
    first_message(): Uint8Array;
    /**
     * First contact: authenticate with the 6-digit pairing code (XXpsk0).
     */
    static new_pair(static_sk: Uint8Array, pairing_code: string): Session;
    /**
     * Reconnect: authenticate by our pinned copy of the host's static key (XX).
     */
    static new_resume(static_sk: Uint8Array, host_key: Uint8Array): Session;
    /**
     * The host's static key, once the handshake has finished. Pin it after a
     * pairing so future connects can `new_resume`.
     */
    peer_key(): Uint8Array | undefined;
    /**
     * Feed handshake bytes. Returns the next framed message to send (or `null`
     * when nothing more to send). Call [`Session::ready`] to know when done;
     * leftover bytes are kept for [`Session::feed`].
     */
    read_handshake(bytes: Uint8Array): Uint8Array | undefined;
    /**
     * `true` once the handshake has finished.
     */
    ready(): boolean;
    /**
     * Encrypt+frame a control payload (from the `enc_*` helpers) for sending.
     */
    seal_control(payload: Uint8Array): Uint8Array;
    /**
     * Ask the host for a fresh keyframe — same empty message on the
     * keyframe-request channel that `rc_transport::lan`'s native
     * `request_key_frame()` sends. Call this whenever the decoder reports
     * corrupt/undecodable video: without it there is no way to recover
     * from a lost or garbled frame short of reconnecting.
     */
    seal_keyframe_request(): Uint8Array;
    /**
     * Encrypt+frame a microphone packet (already-encoded ADPCM bytes, same
     * layout as `crates/audio/src/adpcm.rs`) — the browser's side of the
     * native client's `LanSession::send_mic`. Only the host reads this
     * channel; nothing is ever sent back on it.
     */
    seal_mic(payload: Uint8Array): Uint8Array;
    /**
     * Framed bytes queued by `feed()` echoing a keepalive ping — call once
     * right after `feed()` and send each one over the WebSocket. Separate
     * from `feed()`'s return value because sending is JS's job everywhere
     * else in this API (`seal_control`, `seal_keyframe_request`, …); this
     * keeps that split instead of having Rust reach back into the socket.
     */
    take_keepalive_replies(): Array<any>;
}

export function ack_bad_key(): number;

export function ack_host_offline(): number;

/**
 * Expose the ACK constants to JS.
 */
export function ack_ok(): number;

/**
 * Extract `avc1.PPCCLL` (profile / constraints / level) from an Annex-B buffer
 * that contains an SPS NAL, for `VideoDecoder.configure`.
 */
export function avc_codec_string(annexb: Uint8Array): string | undefined;

/**
 * Decode a channel-0 payload into a plain JS object, e.g.
 * `{ kind: "displays", displays: [{ width, height, ... }] }`.
 */
export function decode_host_message(payload: Uint8Array): any;

export function enc_clipboard(text: string): Uint8Array;

/**
 * Offer the host a direct (same-network) path: this browser's WebRTC SDP offer.
 */
export function enc_direct_offer(sdp: string): Uint8Array;

/**
 * First message over a finished direct path: move the session onto it.
 */
export function enc_direct_use(): Uint8Array;

export function enc_disconnect(): Uint8Array;

export function enc_file_chunk(id: number, offset: number, data: Uint8Array): Uint8Array;

export function enc_file_done(id: number): Uint8Array;

/**
 * Offer a file to the host: the start of a transfer. Chunk it with
 * `enc_file_chunk` afterward and finish with `enc_file_done`.
 */
export function enc_file_offer(id: number, name: string, size: number): Uint8Array;

export function enc_gamepad_disconnect(): Uint8Array;

/**
 * One controller frame. `buttons` is already XInput's bit layout (see
 * `GAMEPAD_BUTTON_*` constants below) — the host hands it straight to a
 * virtual Xbox 360 controller with no remapping.
 */
export function enc_gamepad_state(buttons: number, left_trigger: number, right_trigger: number, thumb_lx: number, thumb_ly: number, thumb_rx: number, thumb_ry: number): Uint8Array;

export function enc_key(code: number, pressed: boolean): Uint8Array;

export function enc_ping(nonce: number): Uint8Array;

export function enc_pointer_button(button: number, pressed: boolean, x: number, y: number): Uint8Array;

/**
 * Click without repositioning first — use this instead of
 * `enc_pointer_button` whenever the pointer is captured, or the click drags
 * the camera with it. See `InputEvent::PointerButtonInPlace`.
 */
export function enc_pointer_button_in_place(button: number, pressed: boolean): Uint8Array;

/**
 * Relative motion for a captured cursor (mouse-look). See
 * `InputEvent::PointerDelta` — feed it `MouseEvent.movementX/Y` under
 * Pointer Lock on a desktop, or raw touch-drag deltas on a phone.
 */
export function enc_pointer_delta(dx: number, dy: number): Uint8Array;

export function enc_pointer_move(x: number, y: number): Uint8Array;

export function enc_quality(mode: string): Uint8Array;

export function enc_scroll(dx: number, dy: number, x: number, y: number): Uint8Array;

export function enc_text(text: string): Uint8Array;

/**
 * The `Hello::Client` a browser sends as its first WebSocket message
 * (no length prefix — the WS frame is the boundary).
 */
export function encode_client_hello(device_id: string, key?: string | null): Uint8Array;

/**
 * A `Hello::Query` — the relay answers with one ACK byte (online/offline) and
 * closes, without disturbing the parked host.
 */
export function encode_query_hello(device_id: string, key?: string | null): Uint8Array;

/**
 * 32 random bytes for the browser's persistent static X25519 secret. Store in
 * `localStorage` and reuse; it's this device's identity to every host.
 */
export function gen_static_key(): Uint8Array;

/**
 * Does this Annex-B access unit contain an IDR slice (NAL type 5)?
 */
export function is_keyframe(annexb: Uint8Array): boolean;

/**
 * Split a channel-1 (video) payload into `{ key_frame, timestamp_us, data }`.
 * `data` is the raw Annex-B access unit for WebCodecs.
 */
export function parse_video_payload(payload: Uint8Array): any;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_session_free: (a: number, b: number) => void;
    readonly ack_bad_key: () => number;
    readonly ack_host_offline: () => number;
    readonly ack_ok: () => number;
    readonly avc_codec_string: (a: number, b: number, c: number) => void;
    readonly decode_host_message: (a: number, b: number, c: number) => void;
    readonly enc_clipboard: (a: number, b: number, c: number) => void;
    readonly enc_direct_offer: (a: number, b: number, c: number) => void;
    readonly enc_direct_use: (a: number) => void;
    readonly enc_disconnect: (a: number) => void;
    readonly enc_file_chunk: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly enc_file_done: (a: number, b: number) => void;
    readonly enc_file_offer: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly enc_gamepad_disconnect: (a: number) => void;
    readonly enc_gamepad_state: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number) => void;
    readonly enc_key: (a: number, b: number, c: number) => void;
    readonly enc_ping: (a: number, b: number) => void;
    readonly enc_pointer_button: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly enc_pointer_button_in_place: (a: number, b: number, c: number) => void;
    readonly enc_pointer_delta: (a: number, b: number, c: number) => void;
    readonly enc_pointer_move: (a: number, b: number, c: number) => void;
    readonly enc_quality: (a: number, b: number, c: number) => void;
    readonly enc_scroll: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly enc_text: (a: number, b: number, c: number) => void;
    readonly encode_client_hello: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly encode_query_hello: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly gen_static_key: (a: number) => void;
    readonly is_keyframe: (a: number, b: number) => number;
    readonly parse_video_payload: (a: number, b: number, c: number) => void;
    readonly session_feed: (a: number, b: number, c: number, d: number) => void;
    readonly session_first_message: (a: number, b: number) => void;
    readonly session_new_pair: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly session_new_resume: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly session_peer_key: (a: number, b: number) => void;
    readonly session_read_handshake: (a: number, b: number, c: number, d: number) => void;
    readonly session_ready: (a: number) => number;
    readonly session_seal_control: (a: number, b: number, c: number, d: number) => void;
    readonly session_seal_keyframe_request: (a: number, b: number) => void;
    readonly session_seal_mic: (a: number, b: number, c: number, d: number) => void;
    readonly session_take_keepalive_replies: (a: number) => number;
    readonly __wbindgen_export: (a: number, b: number) => number;
    readonly __wbindgen_export2: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_export3: (a: number) => void;
    readonly __wbindgen_add_to_stack_pointer: (a: number) => number;
    readonly __wbindgen_export4: (a: number, b: number, c: number) => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
