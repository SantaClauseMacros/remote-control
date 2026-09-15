import init, {
  Session, gen_static_key, encode_client_hello, encode_query_hello,
  enc_pointer_move, enc_pointer_delta, enc_pointer_button, enc_pointer_button_in_place,
  enc_scroll, enc_key, enc_text,
  enc_ping, enc_clipboard, enc_quality, enc_disconnect,
  decode_host_message, parse_video_payload, avc_codec_string, is_keyframe,
  ack_ok, ack_host_offline, ack_bad_key,
} from './pkg/rc_web.js';

const b64 = {
  enc: (u8) => btoa(String.fromCharCode(...u8)),
  dec: (s) => Uint8Array.from(atob(s), c => c.charCodeAt(0)),
};
/** This browser's persistent static key (identity to every host). */
function staticKey() {
  let s = localStorage.rc_static;
  if (!s) { s = b64.enc(gen_static_key()); localStorage.rc_static = s; }
  return b64.dec(s);
}

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];

/**
 * A plain tap/click for a small on-screen button that sits over the video
 * (joystick toggle, controls toggle, ...). Deliberately doesn't rely on the
 * browser synthesizing a `click` from touch events — that synthesis can be
 * delayed or skipped depending on what else is listening on ancestors with
 * `touch-action: none` in play, which is exactly what made these feel like
 * they needed a long-press to register instead of a quick tap. Touch fires
 * `fn` straight from `touchend`; mouse still uses plain `click`. Also stops
 * the underlying mousedown/touchstart from reaching `#stage`, so pressing
 * the button doesn't also forward a click to the remote desktop.
 */
function bindTap(el, fn) {
  let touching = false;
  el.addEventListener('touchstart', e => {
    e.preventDefault(); e.stopPropagation();
    touching = true;
  }, { passive: false });
  el.addEventListener('touchend', e => {
    e.preventDefault(); e.stopPropagation();
    if (touching) { touching = false; fn(); }
  }, { passive: false });
  el.addEventListener('touchcancel', () => { touching = false; }, { passive: true });
  el.addEventListener('mousedown', e => e.stopPropagation());
  el.addEventListener('click', fn);
}
const CH_VIDEO = 1;

/* ─────────────────────────  persistent state  ───────────────────────── */
const store = {
  get devices() { try { return JSON.parse(localStorage.rc_devices || '[]'); } catch { return []; } },
  set devices(v) { localStorage.rc_devices = JSON.stringify(v); },
  get settings() {
    return Object.assign(
      { relay: '', relayKey: '', sens: 1.4, clip: false, invertScroll: false, mode: 'trackpad', joystick: true },
      (() => { try { return JSON.parse(localStorage.rc_settings || '{}'); } catch { return {}; } })());
  },
  set settings(v) { localStorage.rc_settings = JSON.stringify(v); },
};
const uid = () => Math.random().toString(36).slice(2, 10);

/* ─────────────────────────  screen switching  ──────────────────────── */
const screens = {
  list: $('#screen-list'), connecting: $('#screen-connecting'), viewer: $('#screen-viewer'),
};
function show(name) {
  for (const [k, el] of Object.entries(screens)) el.hidden = k !== name;
}
function toast(msg, ms = 1400) {
  const t = $('#toast');
  t.textContent = msg; t.hidden = false;
  clearTimeout(toast._t); toast._t = setTimeout(() => (t.hidden = true), ms);
}

/* ─────────────────────────  relay URL helper  ─────────────────────── */
function wsUrl(relay) {
  relay = (relay || store.settings.relay || '').trim();
  if (!relay) {
    // No relay configured — default to this same site's own /relay endpoint.
    // Works out of the box when the PWA and the relay are one Cloudflare
    // Worker deployment (the common case); a self-hosted rc-relay still
    // needs its host:port entered explicitly in Settings.
    const scheme = location.protocol === 'https:' ? 'wss://' : 'ws://';
    return `${scheme}${location.host}/relay`;
  }
  if (/^wss?:\/\//i.test(relay)) return relay;
  if (!relay.includes(':')) relay += ':9878';
  return (location.protocol === 'https:' ? 'wss://' : 'ws://') + relay;
}

/* ═════════════════════════  device list UI  ═══════════════════════════ */
function renderList() {
  const list = $('#device-list'); list.innerHTML = '';
  const devs = store.devices;
  $('#empty').hidden = devs.length > 0;
  list.hidden = devs.length === 0;
  for (const d of devs) {
    const card = document.createElement('div');
    card.className = 'card';
    card.innerHTML = `
      <span class="dot" data-dot></span>
      <div class="meta">
        <div class="name"></div>
        <div class="sub"></div>
      </div>
      <button class="go">Connect</button>
      <button class="kebab" aria-label="Edit">⋮</button>`;
    card.querySelector('.name').textContent = d.name;
    card.querySelector('.sub').textContent = d.deviceId;
    card.querySelector('.go').onclick = () => connect(d);
    card.querySelector('.kebab').onclick = () => openDeviceDialog(d);
    list.appendChild(card);
    checkOnline(d).then(on => {
      const dot = card.querySelector('[data-dot]');
      dot.classList.toggle('online', on === true);
      dot.classList.toggle('offline', on === false);
      const go = card.querySelector('.go');
      if (on === false) { go.textContent = 'Offline'; }
    });
  }
}

/** Ask the relay whether a host is parked, without disturbing it. */
async function checkOnline(d) {
  const url = wsUrl(d.relay);
  if (!url) return null;
  return new Promise(res => {
    let done = false;
    const ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';
    const finish = v => { if (!done) { done = true; try { ws.close(); } catch {} res(v); } };
    const timer = setTimeout(() => finish(null), 4000);
    ws.onopen = () => ws.send(encode_query_hello(d.deviceId, store.settings.relayKey));
    ws.onmessage = ev => {
      clearTimeout(timer);
      const b = new Uint8Array(ev.data);
      finish(b[0] === ack_ok());
    };
    ws.onerror = () => { clearTimeout(timer); finish(null); };
  });
}

/* ─────────────────────────  add / edit dialog  ───────────────────── */
const dlg = $('#dlg-device');
function openDeviceDialog(existing) {
  const f = dlg.querySelector('form');
  $('#dlg-title').textContent = existing ? 'Edit computer' : 'Add a computer';
  f.name.value = existing?.name || '';
  f.deviceId.value = existing?.deviceId || '';
  f.relay.value = existing?.relay || '';
  dlg._editing = existing?.id || null;
  // add a delete affordance when editing
  let del = f.querySelector('.btn.danger');
  if (existing && !del) {
    del = document.createElement('button');
    del.className = 'btn danger'; del.value = 'delete'; del.textContent = 'Delete';
    f.querySelector('menu').prepend(del);
  } else if (!existing && del) del.remove();
  dlg.showModal();
}
dlg.addEventListener('close', () => {
  const f = dlg.querySelector('form');
  if (dlg.returnValue === 'save') {
    const prev = store.devices.find(d => d.id === dlg._editing);
    const deviceId = canonicalId(f.deviceId.value);
    const rec = {
      id: dlg._editing || uid(),
      name: f.name.value.trim(),
      deviceId,
      relay: f.relay.value.trim(),
      // Keep the pinned host key unless the PC ID changed.
      hostKey: prev && prev.deviceId === deviceId ? prev.hostKey : undefined,
    };
    if (!rec.name || !rec.deviceId) return;
    const devs = store.devices.filter(d => d.id !== rec.id);
    devs.push(rec); store.devices = devs;
  } else if (dlg.returnValue === 'delete') {
    store.devices = store.devices.filter(d => d.id !== dlg._editing);
  }
  renderList();
});
$('#btn-add').onclick = () => openDeviceDialog(null);

/* ─────────────────────────  settings dialog  ─────────────────────── */
const sdlg = $('#dlg-settings');
$('#btn-settings').onclick = () => {
  const s = store.settings, f = sdlg.querySelector('form');
  f.relay.value = s.relay; f.relayKey.value = s.relayKey;
  f.sens.value = s.sens; f.clip.checked = s.clip; f.invertScroll.checked = s.invertScroll;
  sdlg.showModal();
};
sdlg.addEventListener('close', () => {
  const f = sdlg.querySelector('form');
  store.settings = {
    ...store.settings,
    relay: f.relay.value.trim(), relayKey: f.relayKey.value.trim(),
    sens: parseFloat(f.sens.value), clip: f.clip.checked, invertScroll: f.invertScroll.checked,
  };
  renderList();
});

/* ═════════════════════════  connection  ═══════════════════════════════ */
/** Any-case, dashes-optional PC ID → the host's `XXXXX-XXXXX-XXXXX-X` form. */
function canonicalId(s) {
  const raw = String(s || '').replace(/[^A-Za-z0-9]/g, '').toUpperCase();
  return (raw.match(/.{1,5}/g) || []).join('-');
}

class Conn {
  constructor(device) {
    this.device = device;
    this.ws = null; this.sess = null; this.phase = 'idle'; this.pairing = false;
    this.onvideo = () => {}; this.onhost = () => {}; this.onclose = () => {}; this.onpaired = () => {};
  }
  async open() {
    const url = wsUrl(this.device.relay);
    if (!url) throw new Error('No relay configured — set one in Settings.');
    await init();
    const sk = staticKey();
    // The PC ID is all that's needed — it's the handshake secret.
    this.sess = Session.new_pair(sk, canonicalId(this.device.deviceId));
    this.pairing = true;
    this.ws = new WebSocket(url);
    this.ws.binaryType = 'arraybuffer';
    this.phase = 'hello';

    this.ws.onopen = () => {
      this.ws.send(encode_client_hello(this.device.deviceId, store.settings.relayKey));
    };
    this.ws.onmessage = ev => this._rx(new Uint8Array(ev.data));
    this.ws.onerror = () => this._fail('Network error reaching the relay.');
    this.ws.onclose = () => { if (this.phase !== 'done') this._fail('Connection closed.'); };
  }
  _fail(reason) {
    if (this.phase === 'done') return;
    this.phase = 'done';
    try { this.ws && this.ws.close(); } catch {}
    this.onclose(reason);
  }
  close() {
    if (this.sess && this.phase === 'live') {
      try { this.ws.send(this.sess.seal_control(enc_disconnect())); } catch {}
    }
    this.phase = 'done';
    try { this.ws && this.ws.close(); } catch {}
  }
  send(controlBytes) {
    if (this.phase !== 'live') return;
    try { this.ws.send(this.sess.seal_control(controlBytes)); }
    catch (e) { this._fail(String(e)); }
  }
  /** Ask the host to send a fresh keyframe (recovery from a decode error). */
  requestKeyframe() {
    if (this.phase !== 'live') return;
    try { this.ws.send(this.sess.seal_keyframe_request()); } catch {}
  }
  _rx(bytes) {
    try {
      if (this.phase === 'hello') {
        const ack = bytes[0];
        if (ack === ack_host_offline()) return this._fail('That PC is offline.');
        if (ack === ack_bad_key()) return this._fail('The relay rejected the access key.');
        if (ack !== ack_ok()) return this._fail('Relay error.');
        this.ws.send(this.sess.first_message());
        this.phase = 'handshake';
        return;
      }
      if (this.phase === 'handshake') {
        const send = this.sess.read_handshake(bytes);
        if (send) this.ws.send(send);
        if (this.sess.ready()) {
          this.phase = 'live';
          // Pin the host key on first connect; refuse a PC whose key changed.
          const pk = this.sess.peer_key();
          if (pk) {
            const pkB64 = b64.enc(pk);
            if (this.device.hostKey && this.device.hostKey !== pkB64)
              return this._fail('The PC presented a different identity key — not connecting.');
            if (!this.device.hostKey) this.onpaired && this.onpaired(pkB64);
          }
          this._drain(this.sess.feed(new Uint8Array()));
          this.onopen && this.onopen();
        }
        return;
      }
      if (this.phase === 'live') {
        this._drain(this.sess.feed(bytes));
        for (const reply of this.sess.take_keepalive_replies()) this.ws.send(reply);
      }
    } catch (e) { this._fail(String(e && e.message || e)); }
  }
  _drain(frames) {
    for (const [ch, payload] of frames) {
      if (ch === CH_VIDEO) {
        const v = parse_video_payload(payload);
        this.onvideo(new Uint8Array(v.data), v.key_frame, v.timestamp_us);
      } else {
        try { this.onhost(decode_host_message(payload)); } catch {}
      }
    }
  }
  get onopen() { return this._onopen; }
  set onopen(f) { this._onopen = f; }
}

/* ═════════════════════════  video decode  ════════════════════════════ */
class Decoder {
  constructor(canvas) {
    this.canvas = canvas;
    // Low-latency ("desynchronized") canvas lets the display scan the buffer
    // out while it's still being drawn. On ChromeOS that shows up as tearing
    // and black bands across the picture, so it's off there; elsewhere it's
    // kept for the latency it saves.
    const cros = /\bCrOS\b/.test(navigator.userAgent);
    this.ctx = canvas.getContext('2d', { alpha: false, desynchronized: !cros });
    this._placed = null;
    this.dec = null; this.configured = false; this.vw = 0; this.vh = 0;
    this.view = { scale: 1, tx: 0, ty: 0 };
    this.frameCount = 0;
    // Decode health, surfaced in the HUD — the difference between "the
    // network is slow" and "this device is decoding badly" is otherwise
    // invisible, and they need completely different fixes.
    this.errors = 0;
    this.software = false;
  }
  _make() {
    this.dec = new VideoDecoder({
      output: f => this._draw(f),
      error: () => {
        this.errors++;
        // Some hardware H.264 decoders (cheap Chromebooks especially) decode
        // this stream badly rather than cleanly failing — torn or banded
        // output, on and on. Retrying on the same path forever just repeats
        // it, so after a few errors switch to software decoding and stay
        // there for the session. Slower, but it actually decodes.
        if (this.errors >= 3 && !this.software) {
          this.software = true;
          toast('switching to software video decoding', 2600);
        }
        this.reset();
        if (!this.software) toast('video hiccup — recovering');
      },
    });
  }
  reset() {
    try { this.dec && this.dec.close(); } catch {}
    this.dec = null; this.configured = false;
    this._placed = null; // repaint the surround for whatever comes next
    // Without this, the picture just stays broken: `push()` won't decode
    // anything more until a keyframe shows up, and nothing was asking the
    // host to send one — it would only happen to arrive on its own GOP
    // schedule, which can be many seconds away (or effectively never, on a
    // low-motion desktop the encoder isn't re-keying often).
    this.onNeedKeyframe && this.onNeedKeyframe();
  }
  push(annexb, key, tsUs) {
    if (!this.dec) this._make();
    if (!this.configured) {
      if (!key) return;                       // wait for a keyframe
      const codec = avc_codec_string(annexb) || 'avc1.4d002a';
      const accel = this.software ? 'prefer-software' : 'prefer-hardware';
      try { this.dec.configure({ codec, optimizeForLatency: true, hardwareAcceleration: accel }); }
      catch { try { this.dec.configure({ codec }); } catch (e) { toast('decoder: ' + e); return; } }
      this.configured = true;
    }
    try {
      this.dec.decode(new EncodedVideoChunk({
        type: key ? 'key' : 'delta',
        timestamp: Number(tsUs) || 0,
        data: annexb,
      }));
    } catch { this.reset(); }
  }
  _draw(frame) {
    this.frameCount++;
    this.vw = frame.displayWidth; this.vh = frame.displayHeight;
    const c = this.canvas, dpr = devicePixelRatio || 1;
    // Whole pixels. `clientWidth × dpr` is fractional at the scale factors
    // Chromebooks default to (1.25, 1.6, …), and canvas.width truncates what
    // it's given — so comparing against the unrounded value never matched,
    // and the canvas was reallocated (wiped to black) on every single frame.
    const cw = Math.round(c.clientWidth * dpr), ch = Math.round(c.clientHeight * dpr);
    let resized = false;
    if (c.width !== cw || c.height !== ch) { c.width = cw; c.height = ch; resized = true; }
    const base = Math.min(cw / this.vw, ch / this.vh);
    const s = base * this.view.scale;
    // Whole-pixel placement too, so the picture's edges never blend with
    // whatever was drawn there before.
    const w = Math.round(this.vw * s), h = Math.round(this.vh * s);
    const x = Math.round((cw - w) / 2 + this.view.tx * dpr);
    const y = Math.round((ch - h) / 2 + this.view.ty * dpr);
    // Paint the black surround only when the picture moves or resizes. Filling
    // the whole canvas black before every frame hands any display that catches
    // the buffer mid-draw a black band to show; the frame itself overwrites
    // everything it covers anyway.
    const placed = `${cw}x${ch}:${x},${y},${w},${h}`;
    const moved = resized || placed !== this._placed;
    if (moved) {
      this.ctx.fillStyle = '#000'; this.ctx.fillRect(0, 0, cw, ch);
      this._placed = placed;
    }
    this.ctx.drawImage(frame, x, y, w, h);
    frame.close();
    this._layout = { x: x / dpr, y: y / dpr, w: w / dpr, h: h / dpr };
    // Overlays pinned to the picture (a game's hotbar targets) follow it.
    if (moved && this.onPlaced) this.onPlaced();
  }
  /** video-normalized (0..1) → CSS px within the stage */
  normToScreen(nx, ny) {
    const L = this._layout; if (!L) return { x: 0, y: 0 };
    return { x: L.x + nx * L.w, y: L.y + ny * L.h };
  }
  /** CSS px within the stage → video-normalized (0..1) */
  screenToNorm(px, py) {
    const L = this._layout; if (!L) return { x: .5, y: .5 };
    return { x: clamp((px - L.x) / L.w), y: clamp((py - L.y) / L.h) };
  }
}
const clamp = (v, a = 0, b = 1) => Math.max(a, Math.min(b, v));

/* ═════════════════════════  viewer / gestures  ══════════════════════ */
const KEYMAP = {
  Backspace: 8, Tab: 9, Enter: 13, Escape: 27, ' ': 32,
  PageUp: 33, PageDown: 34, End: 35, Home: 36,
  ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40,
  Insert: 45, Delete: 46,
};

/**
 * `KeyboardEvent.code` (the physical key) → Windows virtual-key code.
 *
 * Keyed on `code`, not `key`: `key` is the *character* the layout produces,
 * so on anything but US QWERTY it names the wrong key — and a game reading
 * WASD wants the physical position regardless. Letters and digits have to be
 * here at all, because sending them as typed text (which is all this client
 * used to do) gives a game no key to hold down: text arrives as a Unicode
 * character, not a press and a release.
 */
const CODE_VK = (() => {
  const m = {
    Space: 32, Enter: 13, NumpadEnter: 13, Tab: 9, Escape: 27, Backspace: 8,
    Delete: 46, Insert: 45, Home: 36, End: 35, PageUp: 33, PageDown: 34,
    ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, CapsLock: 20,
    ShiftLeft: 160, ShiftRight: 161, ControlLeft: 162, ControlRight: 163,
    AltLeft: 164, AltRight: 165, MetaLeft: 91, MetaRight: 92,
    Minus: 189, Equal: 187, BracketLeft: 219, BracketRight: 221, Backslash: 220,
    Semicolon: 186, Quote: 222, Backquote: 192, Comma: 188, Period: 190, Slash: 191,
    NumpadMultiply: 106, NumpadAdd: 107, NumpadSubtract: 109,
    NumpadDecimal: 110, NumpadDivide: 111,
  };
  for (let i = 0; i < 26; i++) m['Key' + String.fromCharCode(65 + i)] = 65 + i;
  for (let i = 0; i <= 9; i++) { m['Digit' + i] = 48 + i; m['Numpad' + i] = 96 + i; }
  for (let i = 1; i <= 12; i++) m['F' + i] = 111 + i;
  return m;
})();

/* ═════════════════════════  game modes  ══════════════════════════════ */
/**
 * On-screen controls per game. `topLeft` and `right` are button groups pinned
 * to the stage's corners; `hotbar` lays invisible slot targets over the game's
 * own hotbar in the video, with `left` / `right` buttons docked beside it.
 * Button fields: `vk` (a key), `btn` (mouse button), `toggle` (latches on and
 * off), `act` (an app action). Only Minecraft so far — add the next game here.
 */
const GAME_MODES = {
  minecraft: {
    name: 'Minecraft',
    topLeft: [
      { label: 'Esc', vk: 27 },
      { label: 'Chat', vk: 84 },  // T
      { label: 'View', vk: 116 }, // F5
      { label: 'Swap', vk: 70 },  // F — offhand
      { label: '?', act: 'help' },
    ],
    right: [
      // As the user has them bound: sprint on Shift, crouch on Ctrl.
      { label: 'Sprint', vk: 160, toggle: true }, // left Shift
      { label: 'Crouch', vk: 162, toggle: true }, // left Ctrl
      { label: 'Use', btn: 1 },                   // right mouse, held
      { label: 'Jump', vk: 32 },
    ],
    hotbar: {
      slots: 9,
      vkBase: 49, // keys 1–9
      left: { label: 'Inv', vk: 69 },   // E
      right: { label: 'Drop', vk: 81 }, // Q
      /** Where Java Edition draws its hotbar, in video pixels: a 182×22 GUI-unit
       * bar at bottom centre (slots 20 units wide after a 1-unit border),
       * multiplied by the GUI scale — `gui` from settings, or 0 for Auto. */
      rect(vw, vh, gui) {
        const s = gui || mcAutoGuiScale(vw, vh);
        const w = 182 * s, h = 22 * s;
        return { x: (vw - w) / 2, y: vh - h, w, h, slotX0: s, slotW: 20 * s };
      },
    },
  },
};

/** Minecraft's "Auto" GUI scale: the largest that still leaves the window at
 * least 320×240 GUI units (1920×1080 → 4). */
function mcAutoGuiScale(w, h) {
  let s = 1;
  while (s < 16 && w / (s + 1) >= 320 && h / (s + 1) >= 240) s++;
  return s;
}

let V = null; // active viewer

class Viewer {
  constructor(device) {
    this.device = device;
    this.conn = new Conn(device);
    this.dec = new Decoder($('#screen'));
    this.dec.onNeedKeyframe = () => this.conn.requestKeyframe();
    this.dec.onPlaced = () => this._layoutPad();
    this.padOn = false;
    // Where the game that has the mouse is drawn, as fractions of the picture
    // (from the host). null = assume it fills the whole screen.
    this.gameArea = null;
    this.cursor = { x: .5, y: .5 };
    this.dragging = false;
    this.held = new Set();          // sticky modifier VKs
    this.mode = store.settings.mode; // 'trackpad' | 'direct'
    this.controlsTimer = null;
    // A laptop/desktop with a real mouse and keyboard has no use for the
    // touch-only chrome (movement joystick, jump button, the on-screen
    // "Keys"/trackpad-mode buttons) — mouse already positions the cursor
    // directly and the keyboard already auto-focuses. Detected once, not
    // tied to screen size: a touchscreen laptop still has touch.
    this.isTouch = ('ontouchstart' in window) || navigator.maxTouchPoints > 0;
    // Separate from isTouch: a touchscreen laptop is isTouch too, but it
    // still has a trackpad/mouse and that's what mouse-look cares about —
    // gating the toggle on !isTouch hid it on any touch-capable laptop.
    this.hasMouse = matchMedia('(pointer: fine)').matches;
    // Mouse-look motion accumulated between animation frames — see
    // `_scheduleLookFlush`.
    this._lookDx = 0;
    this._lookDy = 0;
    this._lookPending = 0;
    // Right button held → camera-drag; see the mousemove handler.
    this._relativeDrag = false;
    // The host reports a game has captured its mouse — see `_onCapture`.
    this.hostCaptured = false;
    // Pointer Lock we took on our own for a captured game (vs. the user's
    // "Mouse look" button), and one we're releasing ourselves.
    this._autoLocked = false;
    this._releasingLock = false;
    this._lastEscAt = 0;
  }

  async start() {
    show('connecting');
    $('#connecting-text').textContent = `Connecting to ${this.device.name}…`;
    this.conn.onopen = () => { show('viewer'); this._afterConnect(); };
    this.conn.onclose = (reason) => this._end(reason);
    this.conn.onvideo = (au, key, ts) => this.dec.push(au, key, ts);
    this.conn.onhost = (m) => this._host(m);
    this.conn.onpaired = (hostKeyB64) => {
      const devs = store.devices;
      const d = devs.find(x => x.id === this.device.id);
      if (d) { d.hostKey = hostKeyB64; store.devices = devs; }
    };
    try { await this.conn.open(); }
    catch (e) { this._end(String(e.message || e)); }
  }

  _afterConnect() {
    this._bindGestures();
    this._bindMouse();
    this._bindControls();
    this._bindKeyboard();
    this._bindJoystick();
    if (!this.isTouch) {
      // "Keys" only exists to summon a phone's soft keyboard — meaningless
      // with a real keyboard already attached.
      $('[data-act="keyboard"]').hidden = true;
    }
    if (this.hasMouse || this.isTouch) {
      // On a mouse this button is a Pointer Lock toggle; on touch it cycles
      // trackpad / direct / look. Both end in the same place — relative
      // motion for a game that has captured the cursor — because Pointer
      // Lock simply doesn't exist on a phone. Keyed off hasMouse rather than
      // !isTouch so a touchscreen laptop gets the mouse behaviour.
      $('#mode-btn').hidden = false;
      this._syncModeButton();
    } else {
      // Neither a fine pointer nor touch shouldn't really happen, but don't
      // show a mode picker that controls nothing.
      $('[data-act="mode"]').hidden = true;
    }
    // Controls start hidden — only the small ⋯ button in the corner is
    // ever on screen by default; see _bindControls().
    // Grab keyboard focus immediately — no-op on a phone (mobile browsers
    // only summon the soft keyboard from a focus that happens inside a
    // direct tap), but on a laptop it means typing works right away.
    $('#kbd').focus({ preventScroll: true });
    // RTT ping loop
    this._ping = setInterval(() => this.conn.send(enc_ping(performance.now())), 3000);
    // ping + fps HUD
    this._rtt = null;
    this._lastFrameCount = 0;
    this._hud = setInterval(() => {
      const fps = Math.round(this.dec.frameCount - this._lastFrameCount);
      this._lastFrameCount = this.dec.frameCount;
      const bits = [];
      if (this._rtt != null) bits.push(`${this._rtt} ms`);
      bits.push(`${fps} fps`);
      // Only shown when they mean something, so the pill stays unobtrusive.
      const q = this.dec.dec?.decodeQueueSize ?? 0;
      if (q > 2) bits.push(`q${q}`);            // decoder falling behind
      if (this.dec.software) bits.push('sw');    // fell back to software decode
      if (this.dec.errors) bits.push(`${this.dec.errors} err`);
      $('#hud').textContent = bits.join(' · ');
    }, 1000);
    // keep screen awake
    if (navigator.wakeLock) navigator.wakeLock.request('screen').then(w => (this._wake = w)).catch(() => {});
    toast('connected');
  }

  _host(m) {
    if (m.kind === 'disconnect') this._end(m.reason || 'Host ended the session.');
    else if (m.kind === 'notice') toast(m.text);
    else if (m.kind === 'clipboard' && store.settings.clip && navigator.clipboard)
      navigator.clipboard.writeText(m.text).catch(() => {});
    else if (m.kind === 'pong')
      this._rtt = Math.max(0, Math.round(performance.now() - m.nonce));
    else if (m.kind === 'cursorcaptured') this._onCapture(!!m.captured);
    else if (m.kind === 'gamearea') {
      this.gameArea = { x: m.x, y: m.y, w: m.w, h: m.h };
      this._layoutPad();
    }
  }

  /** A game on the host grabbed (or let go of) the mouse. While it holds it,
   * every pointer path here sends relative motion and in-place clicks with no
   * mode to find and switch on, and a real mouse takes Pointer Lock on its
   * next click so aiming isn't stopped by the edge of the video. */
  _onCapture(on) {
    if (on === this.hostCaptured) return;
    this.hostCaptured = on;
    this._layoutPad();
    const stage = $('#stage');
    if (on) {
      $('#cursor').hidden = true;
      if (this.hasMouse) {
        if (document.pointerLockElement !== stage) toast('Game has the mouse — click to aim', 1600);
      } else if (this.isTouch && !this.padOn && this.mode !== 'look') {
        // (Not with the game pad up: opening and closing the inventory flips
        // capture every time, and the pad already handles both states.)
        toast('Game has the mouse — drag to aim, tap to hit, hold to mine', 2200);
      }
    } else if (this._autoLocked && document.pointerLockElement === stage) {
      // Menu/inventory opened: hand the cursor back.
      this._releasingLock = true;
      document.exitPointerLock();
    }
  }

  _end(reason) {
    clearInterval(this._ping);
    clearInterval(this._hud);
    $('#hud').textContent = '';
    this._releaseMoveKeys && this._releaseMoveKeys();
    this._releasePad && this._releasePad();
    // Release anything still held, then unhook — these live on `window`, so
    // without this a second session would stack another set of listeners on
    // top and send every keystroke twice.
    this._releaseKeys && this._releaseKeys();
    if (this._onKeyDown) window.removeEventListener('keydown', this._onKeyDown);
    if (this._onKeyUp) window.removeEventListener('keyup', this._onKeyUp);
    if (this._releaseKeys) window.removeEventListener('blur', this._releaseKeys);
    if (this._lookPending) { cancelAnimationFrame(this._lookPending); this._lookPending = 0; }
    if (document.pointerLockElement === $('#stage')) document.exitPointerLock();
    try { this._wake && this._wake.release(); } catch {}
    this.dec.reset();
    this.conn.close();
    if (V === this) V = null;
    show('list');
    if (reason) toast(reason, 2600);
    renderList();
  }

  /** How a one-finger drag or tap on the video behaves right now. With the
   * game pad up, the game decides: aim while it has the mouse captured,
   * point-and-tap in its menus and inventory. Otherwise the chosen mode,
   * unless a game has captured the mouse anyway. */
  get touchStyle() {
    if (this.padOn) return this.hostCaptured ? 'look' : 'direct';
    return this.hostCaptured ? 'look' : this.mode;
  }

  /* ── input senders ── */
  moveTo(nx, ny) {
    this.cursor.x = clamp(nx); this.cursor.y = clamp(ny);
    const p = this.dec.normToScreen(this.cursor.x, this.cursor.y);
    const c = $('#cursor'); c.hidden = false; c.style.left = p.x + 'px'; c.style.top = p.y + 'px';
    this.conn.send(enc_pointer_move(this.cursor.x, this.cursor.y));
  }
  moveBy(dxPx, dyPx) {
    const L = this.dec._layout; if (!L) return;
    const s = store.settings.sens;
    this.moveTo(this.cursor.x + (dxPx / L.w) * s, this.cursor.y + (dyPx / L.h) * s);
  }
  /** True while the remote pointer is captured — touch look mode, or Pointer
   * Lock on a desktop. Clicks must not carry a position in that state. */
  get looking() {
    return this.touchStyle === 'look'
      || document.pointerLockElement === $('#stage')
      || this._relativeDrag;
  }
  button(button, pressed) {
    this.conn.send(this.looking
      ? enc_pointer_button_in_place(button, pressed)
      : enc_pointer_button(button, pressed, this.cursor.x, this.cursor.y));
  }
  click(button = 0) {
    this.button(button, true);
    setTimeout(() => this.button(button, false), 15);
  }
  key(vk, down) { this.conn.send(enc_key(vk, down)); }
  tap(vk) { this.key(vk, true); setTimeout(() => this.key(vk, false), 15); }

  /* ── movement joystick (WASD, thumb-held) ── */
  _bindJoystick() {
    const zone = $('#joystick');
    const pad = $('#pad');
    const toggle = $('#btn-joystick-toggle');
    if (!this.isTouch) {
      // Real mouse + keyboard: nothing here applies, and no toggle for a
      // feature that isn't there.
      zone.hidden = true; pad.hidden = true; toggle.hidden = true;
      return;
    }
    const knob = zone.querySelector('.knob');
    const W = 87, A = 65, S = 83, D = 68;
    const R = 44, DEAD = 12;
    let touchId = null, center = null;
    const active = new Set();

    const setKey = (vk, on) => {
      if (on && !active.has(vk)) { active.add(vk); this.key(vk, true); }
      else if (!on && active.has(vk)) { active.delete(vk); this.key(vk, false); }
    };
    this._releaseMoveKeys = () => { for (const vk of [...active]) setKey(vk, false); };

    const update = (clientX, clientY) => {
      const dx = clientX - center.x, dy = clientY - center.y;
      const dist = Math.min(Math.hypot(dx, dy), R) || 0;
      const angle = Math.atan2(dy, dx);
      knob.style.transform = dist
        ? `translate(${Math.cos(angle) * dist}px, ${Math.sin(angle) * dist}px)`
        : '';
      setKey(D, dx > DEAD);
      setKey(A, dx < -DEAD);
      setKey(S, dy > DEAD);
      setKey(W, dy < -DEAD);
    };

    const grab = (clientX, clientY) => {
      const r = zone.getBoundingClientRect();
      center = { x: r.left + r.width / 2, y: r.top + r.height / 2 };
      zone.classList.add('active');
      update(clientX, clientY);
    };
    const drop = () => {
      touchId = null; center = null; mouseDown = false;
      knob.style.transform = '';
      zone.classList.remove('active');
      this._releaseMoveKeys();
    };

    zone.addEventListener('touchstart', e => {
      e.preventDefault(); e.stopPropagation();
      if (touchId !== null) return;
      const t = e.changedTouches[0];
      touchId = t.identifier;
      grab(t.clientX, t.clientY);
    }, { passive: false });

    zone.addEventListener('touchmove', e => {
      e.preventDefault(); e.stopPropagation();
      for (const t of e.changedTouches) {
        if (t.identifier === touchId) update(t.clientX, t.clientY);
      }
    }, { passive: false });

    const release = e => {
      e.preventDefault(); e.stopPropagation();
      for (const t of e.changedTouches) {
        if (t.identifier === touchId) drop();
      }
    };
    zone.addEventListener('touchend', release, { passive: false });
    zone.addEventListener('touchcancel', release, { passive: false });

    // Mouse (laptop): the pad's just a click-and-drag joystick — one
    // "pointer" instead of tracking touch identifiers, and mousemove/up on
    // `window` rather than `zone` so dragging past the small circle (easy
    // to do with a mouse) doesn't freeze the last direction.
    let mouseDown = false;
    zone.addEventListener('mousedown', e => {
      e.preventDefault(); e.stopPropagation();
      mouseDown = true;
      grab(e.clientX, e.clientY);
    });
    window.addEventListener('mousemove', e => {
      if (mouseDown) update(e.clientX, e.clientY);
    });
    window.addEventListener('mouseup', () => { if (mouseDown) drop(); });

    this._buildPad(pad);


    // Movement pad is opt-in visible per session — some sessions are just
    // "poke around the desktop", not a game that needs WASD on-screen.
    const setVisible = on => {
      zone.hidden = !on; pad.hidden = !on;
      this.padOn = on;
      $('#stage').classList.toggle('pad-on', on);
      if (!on) this._releasePad();
      toggle.classList.toggle('on', on);
      store.settings = { ...store.settings, joystick: on };
      this._layoutPad();
    };
    bindTap(toggle, () => {
      const on = zone.hidden;
      setVisible(on);
      if (on && !store.settings.padHelpSeen) {
        store.settings = { ...store.settings, padHelpSeen: true };
        this._showHelp();
      } else if (on) {
        toast('Game controls on — tap ? for help', 1600);
      }
    });
    setVisible(!!store.settings.joystick);
  }

  /* ── gesture recogniser ── */
  _bindGestures() {
    const stage = $('#stage');
    let pts = new Map();               // id → {x,y,x0,y0,t0}
    let lp = null;                     // long-press timer
    let g = null;                      // 'move' | 'drag' | 'two' | null
    let twoStart = null;

    const pos = t => {
      const r = stage.getBoundingClientRect();
      return { x: t.clientX - r.left, y: t.clientY - r.top };
    };

    stage.addEventListener('touchstart', e => {
      e.preventDefault();
      for (const t of e.changedTouches) {
        const p = pos(t);
        pts.set(t.identifier, { ...p, x0: p.x, y0: p.y, t0: performance.now() });
      }
      if (pts.size === 1) {
        g = 'pending';
        if (this.touchStyle === 'direct') {
          // Put the pointer under the finger straight away, so a long-press
          // drag (moving an item in an inventory) starts where it should.
          const [first] = pts.values();
          const n = this.dec.screenToNorm(first.x, first.y); this.moveTo(n.x, n.y);
        }
        lp = setTimeout(() => {
          g = 'drag'; this.dragging = true;
          this.button(0, true);
          navigator.vibrate && navigator.vibrate(15);
          if (!this.padOn) toast(this.touchStyle === 'look' ? 'firing' : 'drag');
        }, 480);
      } else if (pts.size === 2) {
        clearTimeout(lp);
        g = 'two';
        const [a, b] = [...pts.values()];
        twoStart = {
          mid: { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 },
          dist: Math.hypot(a.x - b.x, a.y - b.y),
          scale: this.dec.view.scale, tx: this.dec.view.tx, ty: this.dec.view.ty,
          t0: performance.now(), moved: 0,
          kind: null, // decided on first real movement: 'pinch' | 'scroll'
        };
      }
    }, { passive: false });

    stage.addEventListener('touchmove', e => {
      e.preventDefault();
      for (const t of e.changedTouches) {
        const cur = pts.get(t.identifier); if (!cur) continue;
        Object.assign(cur, pos(t));
      }
      if (g === 'pending' || g === 'move' || g === 'drag') {
        const t = [...pts.values()][0];
        const moved = Math.hypot(t.x - t.x0, t.y - t.y0);
        if (g === 'pending' && moved > 8) { g = 'move'; clearTimeout(lp); }
        if (g === 'move' || g === 'drag') {
          const px = t._px ?? t.x0, py = t._py ?? t.y0;
          const style = this.touchStyle;
          if (style === 'look') {
            // Camera look: the game owns the cursor and reads raw relative
            // motion, so send the drag as a delta rather than steering an
            // absolute pointer that would fight the game's own recentring.
            // Same path the desktop's Pointer Lock mode uses.
            const s = store.settings.sens;
            this._lookDx += (t.x - px) * s;
            this._lookDy += (t.y - py) * s;
            this._scheduleLookFlush();
          } else if (style === 'direct') {
            const n = this.dec.screenToNorm(t.x, t.y); this.moveTo(n.x, n.y);
          } else {
            this.moveBy(t.x - px, t.y - py);
          }
          t._px = t.x; t._py = t.y;
        }
      } else if (g === 'two' && pts.size >= 2) {
        const [a, b] = [...pts.values()];
        const mid = { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
        const dist = Math.hypot(a.x - b.x, a.y - b.y);
        const distChange = Math.abs(dist - twoStart.dist);
        const midTravel = Math.hypot(mid.x - twoStart.mid.x, mid.y - twoStart.mid.y);
        twoStart.moved += Math.abs(mid.y - twoStart.mid.y) + Math.abs(mid.x - twoStart.mid.x);

        // Decide once, on the first real movement, and stick with it — a
        // scroll that briefly reads as "sort of a pinch" (or vice versa) is
        // what made two-finger scroll feel broken before: it kept flip-
        // flopping every single move event instead of committing early.
        if (!twoStart.kind && (distChange > 6 || midTravel > 6)) {
          twoStart.kind = distChange > midTravel ? 'pinch' : 'scroll';
        }

        if (twoStart.kind === 'pinch') {
          const sc = clamp((dist / twoStart.dist) * twoStart.scale, 1, 4);
          this.dec.view.scale = sc;
          this.dec.view.tx = twoStart.tx + (mid.x - twoStart.mid.x);
          this.dec.view.ty = twoStart.ty + (mid.y - twoStart.mid.y);
        } else if (twoStart.kind === 'scroll') {
          const dy = mid.y - (twoStart._my ?? twoStart.mid.y);
          const dx = mid.x - (twoStart._mx ?? twoStart.mid.x);
          const inv = store.settings.invertScroll ? -1 : 1;
          this.conn.send(enc_scroll(dx / 40, (-dy / 24) * inv, this.cursor.x, this.cursor.y));
        }
        twoStart._my = mid.y; twoStart._mx = mid.x;
      }
    }, { passive: false });

    const clearOne = id => { pts.delete(id); };
    stage.addEventListener('touchend', e => {
      e.preventDefault();
      const now = performance.now();
      for (const t of e.changedTouches) {
        const cur = pts.get(t.identifier);
        if (g === 'pending' && cur && now - cur.t0 < 250 &&
            Math.hypot(cur.x - cur.x0, cur.y - cur.y0) < 8) {
          if (this.touchStyle === 'direct') {
            const n = this.dec.screenToNorm(cur.x, cur.y); this.moveTo(n.x, n.y);
          }
          this.click(0);
        }
        if (g === 'drag') {
          this.button(0, false);
          this.dragging = false;
        }
        clearOne(t.identifier);
      }
      if (g === 'two' && twoStart && pts.size <= 1) {
        // quick two-finger tap with little travel → right click
        if (now - twoStart.t0 < 260 && twoStart.moved < 16) this.click(1);
      }
      clearTimeout(lp);
      if (pts.size === 0) { g = null; twoStart = null; }
      else if (pts.size === 1) { g = 'pending'; }
    }, { passive: false });

    stage.addEventListener('touchcancel', () => {
      clearTimeout(lp); pts.clear(); g = null; twoStart = null;
      if (this.dragging) { this.button(0, false); this.dragging = false; }
    });
  }

  /* ── real mouse (laptop/desktop browsers) ──
   * The gesture recogniser above is touch-only (touchstart/move/end never
   * fire for an actual mouse), so a laptop opening this site had a keyboard
   * but no pointer input at all. A real mouse doesn't need any of the
   * tap-vs-drag-vs-two-finger heuristics touch needs — it already has
   * buttons — so this is most of a normal remote-desktop client's input
   * loop: cursor tracks the mouse 1:1, buttons map straight through, wheel
   * is scroll. `touchstart`'s `preventDefault()` suppresses the synthetic
   * mouse events browsers fire after a real touch, so this coexists fine
   * with phones/tablets using the same page.
   */
  _bindMouse() {
    const stage = $('#stage');
    // DOM MouseEvent.button (0 left,1 middle,2 right,3 back,4 fwd) → this
    // app's PointerButton order (0 left,1 right,2 middle,3 X1,4 X2).
    const mapButton = b => ({ 0: 0, 1: 2, 2: 1, 3: 3, 4: 4 })[b] ?? 0;
    const held = new Set();

    const posToNorm = (clientX, clientY) => {
      const r = stage.getBoundingClientRect();
      return this.dec.screenToNorm(clientX - r.left, clientY - r.top);
    };

    stage.addEventListener('mousemove', e => {
      // Locked (mouse-look): the OS cursor is hidden and pinned in place,
      // so clientX/Y stop being meaningful — accumulate the raw relative
      // delta instead, same as a real mouse feeding a game's captured-cursor
      // look path. See `_toggleMouseLook` and `_flushLook`.
      if (document.pointerLockElement === stage) {
        const s = store.settings.sens;
        this._lookDx += e.movementX * s;
        this._lookDy += e.movementY * s;
        this._scheduleLookFlush();
        return;
      }
      if (this.hostCaptured) {
        // Game has the mouse but Pointer Lock isn't on yet (it engages on the
        // next click): still send relative motion, never an absolute jump.
        const s = store.settings.sens;
        this._lookDx += e.movementX * s;
        this._lookDy += e.movementY * s;
        this._scheduleLookFlush();
        return;
      }
      if (this._relativeDrag) {
        // Right button held. Games that keep an ordinary free cursor — Roblox,
        // most third-person games — still capture and recentre it for as long
        // as you right-drag to swing the camera, so absolute positioning
        // fights them for that stretch exactly like a fully captured game
        // does all the time. Send those deltas relative and it behaves like a
        // real mouse, with no mode for anyone to find and switch on.
        const k = this._remotePerClientPx();
        this._lookDx += e.movementX * k;
        this._lookDy += e.movementY * k;
        this._scheduleLookFlush();
        return;
      }
      const n = posToNorm(e.clientX, e.clientY);
      this.moveTo(n.x, n.y);
    });

    stage.addEventListener('mousedown', e => {
      e.preventDefault();
      // A laptop has a real keyboard — typing should just work while
      // looking at the screen, not require hunting for the "Keys" button
      // first (that's there for phones, which need it to summon the soft
      // keyboard). Keep it invisible; it's only a text-capture target.
      $('#kbd').focus({ preventScroll: true });
      const btn = mapButton(e.button);
      // A captured game: take Pointer Lock now (a click is the user gesture
      // the browser requires) so aim is unbounded. The click itself still
      // goes through below.
      if (this.hostCaptured && this.hasMouse && document.pointerLockElement !== stage) {
        this._autoLocked = true;
        const req = stage.requestPointerLock({ unadjustedMovement: true });
        Promise.resolve(req).catch(() => { try { stage.requestPointerLock(); } catch {} });
      }
      // Under Pointer Lock there is no meaningful click position — aiming is
      // the camera, not a cursor — and repositioning first would jerk it.
      if (!this.looking) {
        const n = posToNorm(e.clientX, e.clientY);
        this.moveTo(n.x, n.y);
      }
      held.add(btn);
      this.button(btn, true);
      // Only *after* the press has gone out at a real position — a right
      // click still has to land where you aimed it — does motion switch to
      // relative for the duration of the drag.
      if (btn === 1) this._relativeDrag = true;
    });

    // On window, not just the stage — releasing outside the video (having
    // dragged past its edge) must still lift the button on the host.
    window.addEventListener('mouseup', e => {
      const btn = mapButton(e.button);
      if (!held.has(btn)) return;
      held.delete(btn);
      // Release in place: `looking` is still true here, so this doesn't drag
      // the pointer back to wherever the tracked cursor was left when the
      // drag started.
      this.button(btn, false);
      if (btn === 1) this._relativeDrag = false;
    });

    stage.addEventListener('contextmenu', e => e.preventDefault());

    stage.addEventListener('wheel', e => {
      e.preventDefault();
      const inv = store.settings.invertScroll ? -1 : 1;
      this.conn.send(enc_scroll(e.deltaX / 100, (-e.deltaY / 100) * inv, this.cursor.x, this.cursor.y));
    }, { passive: false });

    // The browser can drop pointer lock on its own (Esc, alt-tab, losing
    // focus) as well as from our own exitPointerLock() call — one handler
    // for both keeps the button/toast in sync with reality either way.
    document.addEventListener('pointerlockchange', () => {
      if (V !== this) return;
      const locked = document.pointerLockElement === stage;
      $('#mode-btn')?.classList.toggle('on', locked);
      if (!locked) {
        const ours = this._releasingLock, auto = this._autoLocked;
        this._releasingLock = false; this._autoLocked = false;
        // Esc releases Pointer Lock in the browser, which usually swallows
        // the key — so the game never sees it and its pause menu won't open.
        // Pass it on, unless the key did reach us after all.
        if (!ours && this.hostCaptured) {
          setTimeout(() => {
            if (this.hostCaptured && performance.now() - this._lastEscAt > 300) this.tap(27);
          }, 60);
        }
        if (ours || auto) return;
      } else if (this._autoLocked) {
        return;
      }
      toast(locked ? 'Mouse look on — Esc to release' : 'Mouse look off', 1400);
    });
    document.addEventListener('pointerlockerror', () => toast('Mouse look unavailable here', 1800));
  }

  /* ── game pad, built from the active game mode ── */
  _buildPad(pad) {
    const mode = GAME_MODES[store.settings.game] || GAME_MODES.minecraft;
    pad.textContent = '';
    const latched = new Set();
    this._releasePad = () => {
      for (const vk of latched) this.key(vk, false);
      latched.clear();
      pad.querySelectorAll('.latched').forEach(b => b.classList.remove('latched'));
    };

    // Every control is a real key or mouse button held for exactly as long as
    // its finger is down (mining, eating and charging a bow all need holding);
    // toggles latch instead, since no thumb can hold Sprint while also
    // walking and aiming. Each element tracks its own touch, so several can
    // be held at once.
    const bindTouch = (el, onDown, onUp) => {
      let id = null;
      el.addEventListener('touchstart', e => {
        e.preventDefault(); e.stopPropagation();
        if (id !== null) return;
        const t = e.changedTouches[0];
        id = t.identifier;
        onDown(t);
      }, { passive: false });
      const end = e => {
        e.preventDefault(); e.stopPropagation();
        if (id === null || ![...e.changedTouches].some(t => t.identifier === id)) return;
        id = null;
        onUp();
      };
      el.addEventListener('touchend', end, { passive: false });
      el.addEventListener('touchcancel', end, { passive: false });
    };
    const act = (spec, el, down) => {
      if (spec.act === 'help') { if (down) this._showHelp(); return; }
      if (spec.toggle) {
        if (!down) return;
        const on = !latched.has(spec.vk);
        on ? latched.add(spec.vk) : latched.delete(spec.vk);
        el.classList.toggle('latched', on);
        this.key(spec.vk, on);
      } else {
        if (spec.btn != null) this.button(spec.btn, down);
        else if (spec.vk) this.key(spec.vk, down);
        el.classList.toggle('active', down);
      }
      if (down && navigator.vibrate) navigator.vibrate(8);
    };
    const make = (spec, cls = '') => {
      const el = document.createElement('button');
      el.className = cls;
      el.textContent = spec.label;
      bindTouch(el, () => act(spec, el, true), () => act(spec, el, false));
      return el;
    };
    for (const [cls, specs] of [['pad-top-left', mode.topLeft], ['pad-main', mode.right]]) {
      const g = document.createElement('div');
      g.className = cls;
      for (const s of specs || []) g.appendChild(make(s));
      pad.appendChild(g);
    }

    this._hotbar = null;
    if (mode.hotbar) {
      const hb = mode.hotbar;
      const strip = document.createElement('div');
      strip.className = 'pad-hotbar';
      const flash = document.createElement('div');
      flash.className = 'pad-slot-flash';
      strip.appendChild(flash);
      let heldVk = 0;
      bindTouch(strip, t => {
        const geom = this._hotbar && this._hotbar.geom;
        if (!geom) return;
        const r = strip.getBoundingClientRect();
        const slot = Math.max(0, Math.min(hb.slots - 1,
          Math.floor((t.clientX - r.left - geom.slotX0) / geom.slotW)));
        heldVk = hb.vkBase + slot;
        this.key(heldVk, true);
        flash.style.left = (geom.slotX0 + slot * geom.slotW) + 'px';
        flash.style.width = geom.slotW + 'px';
        flash.classList.remove('on'); void flash.offsetWidth; flash.classList.add('on');
        navigator.vibrate && navigator.vibrate(8);
      }, () => {
        if (heldVk) this.key(heldVk, false);
        heldVk = 0;
      });
      pad.appendChild(strip);
      const left = hb.left ? make(hb.left, 'pad-beside') : null;
      const right = hb.right ? make(hb.right, 'pad-beside') : null;
      for (const el of [left, right]) if (el) pad.appendChild(el);
      this._hotbar = { spec: hb, strip, left, right, geom: null };
    }
    this._layoutPad();
  }

  /** Lay the hotbar targets over the game's own hotbar, wherever the picture
   * is drawn right now (letterboxing, pinch-zoom). The invisible strip only
   * takes touches while the game has the mouse captured — in play. With the
   * inventory or a menu open, taps there have to reach the game's own UI. */
  _layoutPad() {
    const H = this._hotbar;
    if (!H) return;
    const L = this.dec._layout, vw = this.dec.vw, vh = this.dec.vh;
    const beside = [H.left, H.right].filter(Boolean);
    if (!L || !vw || !vh || !this.padOn) {
      H.strip.hidden = true;
      beside.forEach(el => { el.hidden = true; });
      return;
    }
    // The game's own drawing area within the picture: a maximized window
    // leaves its title bar above and the taskbar below, and the hotbar sits
    // at the bottom of the game, not of the screen.
    const A = this.gameArea || { x: 0, y: 0, w: 1, h: 1 };
    const gx = A.x * vw, gy = A.y * vh, gw = A.w * vw, gh = A.h * vh;
    const r = H.spec.rect(gw, gh, +store.settings.mcGuiScale || 0);
    const k = L.w / vw; // CSS px per video px
    const x = L.x + (gx + r.x) * k, y = L.y + (gy + r.y) * k, w = r.w * k, h = r.h * k;
    H.geom = { slotX0: r.slotX0 * k, slotW: r.slotW * k };
    Object.assign(H.strip.style, { left: x + 'px', top: y + 'px', width: w + 'px', height: h + 'px' });
    H.strip.hidden = !this.hostCaptured;
    // Docked either side of the hotbar, bottom-aligned with it.
    const bw = 46, bh = Math.max(h, 38), gap = 6;
    // …but never hanging off the bottom of the stage.
    const top = Math.min(y + h - bh, $('#stage').clientHeight - bh - 6);
    const place = (el, left) => {
      if (!el) return;
      Object.assign(el.style, { left: left + 'px', top: top + 'px', width: bw + 'px', height: bh + 'px' });
      el.hidden = false;
    };
    place(H.left, x - gap - bw);
    place(H.right, x + w + gap);
  }

  /** Cheat sheet for the game pad, plus the hotbar-size setting. */
  _showHelp() {
    const d = $('#dlg-help');
    if (d.open) return;
    const sel = d.querySelector('select[name="gui"]');
    sel.value = String(+store.settings.mcGuiScale || 0);
    d.addEventListener('close', () => {
      store.settings = { ...store.settings, mcGuiScale: +sel.value || 0 };
      this._layoutPad();
    }, { once: true });
    d.showModal();
  }

  /** Keep the mode button showing what mode is actually active — it persists
   * across sessions, so a stale label is genuinely misleading. In look mode
   * the on-screen pointer means nothing, so hide it. */
  _syncModeButton() {
    const btn = $('#mode-btn');
    if (!btn) return;
    const [icon, label] = this.hasMouse
      ? ['🎯', 'Mouse look']
      : ({ trackpad: ['🖱', 'Trackpad'], direct: ['👆', 'Direct'], look: ['🎯', 'Look'] }[this.mode]
          ?? ['🖱', 'Trackpad']);
    btn.firstChild.textContent = icon;
    btn.lastElementChild.textContent = label;
    btn.classList.toggle('on', !this.hasMouse && this.mode === 'look');
    if (this.mode === 'look') $('#cursor').hidden = true;
  }

  /** Coalesce mouse-look motion to one message per animation frame.
   * A trackpad or gaming mouse reports motion far faster than the screen
   * refreshes — up to 1000Hz — and one encrypted message per report floods
   * the link with hundreds of tiny packets a second, which reads as
   * stuttery aim rather than fast aim. Summing them loses nothing: the host
   * applies relative motion, so one delta of 10 moves exactly as far as ten
   * deltas of 1. */
  _scheduleLookFlush() {
    if (this._lookPending) return;
    this._lookPending = requestAnimationFrame(() => {
      this._lookPending = 0;
      const dx = this._lookDx, dy = this._lookDy;
      this._lookDx = 0; this._lookDy = 0;
      if (dx || dy) this.conn.send(enc_pointer_delta(dx, dy));
    });
  }

  /** Remote pixels per client pixel: the video is drawn scaled to fit, so raw
   * client-space motion would move the remote pointer by the wrong distance. */
  _remotePerClientPx() {
    const L = this.dec._layout;
    return L && L.w && this.dec.vw ? this.dec.vw / L.w : 1;
  }

  /** Toggle Pointer Lock on the stage — see the mousemove handler above for
   * where the resulting relative deltas get sent. Games with their own
   * captured cursor (most FPS/third-person camera look) need this instead
   * of the normal absolute cursor positioning, which otherwise fights the
   * game's own recentring and reads as the camera drifting on its own. */
  _toggleMouseLook() {
    const stage = $('#stage');
    this._autoLocked = false;
    if (document.pointerLockElement === stage) {
      this._releasingLock = true;
      document.exitPointerLock();
      return;
    }
    const req = stage.requestPointerLock({ unadjustedMovement: true });
    // Older browsers return undefined instead of a Promise.
    Promise.resolve(req).catch(() => stage.requestPointerLock());
  }

  _showControls() {
    $('#controls').classList.remove('hidden');
    $('#btn-controls-toggle').classList.add('on');
    clearTimeout(this.controlsTimer);
    this.controlsTimer = setTimeout(() => this._hideControls(), 4000);
  }

  _hideControls() {
    clearTimeout(this.controlsTimer);
    $('#controls').classList.add('hidden');
    $('#specials').hidden = true;
    $('#btn-controls-toggle').classList.remove('on');
  }

  _bindControls() {
    const menuToggle = $('#btn-controls-toggle');
    bindTap(menuToggle, () => {
      $('#controls').classList.contains('hidden') ? this._showControls() : this._hideControls();
    });

    $('#controls').onclick = e => {
      const act = e.target.closest('.ctl')?.dataset.act; if (!act) return;
      this._showControls();
      if (act === 'keyboard') $('#kbd').focus();
      else if (act === 'specials') $('#specials').hidden = !$('#specials').hidden;
      else if (act === 'disconnect') this._end();
      else if (act === 'quality') $('#quality-sheet').hidden = false;
      else if (act === 'mode') {
        if (this.hasMouse) {
          this._toggleMouseLook();
        } else if (this.isTouch) {
          const order = ['trackpad', 'direct', 'look'];
          this.mode = order[(order.indexOf(this.mode) + 1) % order.length];
          store.settings = { ...store.settings, mode: this.mode };
          this._syncModeButton();
          toast({
            trackpad: 'Trackpad — drag moves the pointer',
            direct: 'Direct touch — tap where you want to click',
            look: 'Look — drag to aim, tap to click',
          }[this.mode], 2200);
        }
      }
    };
    $('#quality-sheet').onclick = e => {
      const q = e.target.dataset.q; if (!q) return;
      this.conn.send(enc_quality(q)); $('#quality-sheet').hidden = true; toast('quality: ' + q);
    };
    $('#specials').onclick = e => {
      const b = e.target.closest('button'); if (!b) return;
      this._showControls();
      if (b.dataset.key) this._withHeld(() => this.tap(+b.dataset.key));
      else if (b.dataset.combo) {
        const vks = b.dataset.combo.split(',').map(Number);
        vks.forEach(v => this.key(v, true));
        setTimeout(() => [...vks].reverse().forEach(v => this.key(v, false)), 25);
      } else if (b.dataset.mod) {
        const vk = +b.dataset.mod;
        if (this.held.has(vk)) { this.held.delete(vk); this.key(vk, false); b.classList.remove('held'); }
        else { this.held.add(vk); this.key(vk, true); b.classList.add('held'); }
      }
    };
  }
  _withHeld(fn) {
    // held modifiers are already "down"; just fire the key
    fn();
  }

  _bindKeyboard() {
    const k = $('#kbd');
    k.addEventListener('input', e => {
      if (e.inputType === 'insertText' && e.data) this.conn.send(enc_text(e.data));
      else if (e.inputType === 'insertLineBreak') this.tap(13);
      else if (e.inputType === 'deleteContentBackward') this.tap(8);
      k.value = '';
    });
    const down = new Set();
    const vkFor = e => {
      if (e.isComposing) return 0;        // mid-IME: leave it to the text path
      return CODE_VK[e.code] ?? KEYMAP[e.key] ?? 0;
    };
    // On `window`, not the hidden input: a physical keyboard should keep
    // working even when focus has drifted to one of the on-screen control
    // buttons, which is easy to do mid-session and looked exactly like "the
    // keyboard stopped working".
    const onKey = (e, pressed) => {
      const t = e.target;
      if (t && t !== k && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) {
        return; // genuinely typing into a dialog
      }
      const vk = vkFor(e);
      if (!vk) return;
      if (vk === 27 && pressed) this._lastEscAt = performance.now();
      // Swallowing it also keeps the character out of the hidden input, so
      // the text path below doesn't send the same keystroke a second time.
      e.preventDefault();
      if (pressed) {
        if (e.repeat) return;   // held on the host already; repeat is its job
        down.add(vk);
      } else {
        down.delete(vk);
      }
      this.key(vk, pressed);
    };
    this._onKeyDown = e => onKey(e, true);
    this._onKeyUp = e => onKey(e, false);
    window.addEventListener('keydown', this._onKeyDown);
    window.addEventListener('keyup', this._onKeyUp);

    // Losing focus mid-hold (alt-tab out, screen lock) would otherwise leave
    // the key held down on the host with nothing left to release it.
    this._releaseKeys = () => {
      for (const vk of down) this.key(vk, false);
      down.clear();
    };
    window.addEventListener('blur', this._releaseKeys);
    // clipboard → host on focus
    if (store.settings.clip && navigator.clipboard) {
      window.addEventListener('focus', async () => {
        try {
          const t = await navigator.clipboard.readText();
          if (t && t !== this._lastClip) { this._lastClip = t; this.conn.send(enc_clipboard(t)); }
        } catch {}
      });
    }
  }
}

async function connect(device) {
  if (V) V._end();
  V = new Viewer(device);
  await V.start();
}
$('#btn-cancel').onclick = () => { if (V) V._end(); };

/* ─────────────────────────  deep link  #add=id[,relay]  ───────────── */
function handleDeepLink() {
  // Older links carried a 6-digit code after the id; it's skipped if present.
  const m = location.hash.match(/add=([^,]+)(?:,\d{6})?(?:,([^,]+))?/);
  if (!m) return;
  history.replaceState(null, '', location.pathname);
  openDeviceDialog(null);
  const f = dlg.querySelector('form');
  f.deviceId.value = canonicalId(decodeURIComponent(m[1]));
  if (m[2]) f.relay.value = decodeURIComponent(m[2]);
  f.name.focus();
}

/* ─────────────────────────  boot  ────────────────────────────────── */
(async function boot() {
  if (!('VideoDecoder' in window)) {
    document.body.innerHTML =
      `<div class="empty" style="margin:auto;text-align:center">
        <h1>Unsupported browser</h1>
        <p class="dim">This needs the WebCodecs video decoder — use Chrome/Edge,
        Android Chrome, or Safari / iOS 16.4 or newer.</p></div>`;
    return;
  }
  await init();
  renderList();
  handleDeepLink();
  window.addEventListener('hashchange', handleDeepLink);
  if ('serviceWorker' in navigator)
    navigator.serviceWorker.register('sw.js').catch(() => {});
})();
