# Architecture

## Goals, in priority order

1. **Reliability** — never break the host PC; degrade gracefully.
2. **Security** — this app grants complete control of a computer.
3. **Low latency** — responsiveness beats image quality.
4. **Low host overhead** — near-zero when idle; modest when streaming.
5. **Easy controls** — natural touch gestures; physical kbd/mouse on PC↔PC.
6. **Good visual quality** — after the above are satisfied.

## Technology choices

| Area | Choice | Rationale |
|---|---|---|
| Host, desktop client, server | **Rust** | Lowest idle footprint with memory safety for a full-control app; one language across host/client/server/protocol. Proven at this exact job by RustDesk. |
| Transport | **WebRTC** (ICE + DTLS-SRTP + SCTP data channels) | NAT traversal, encryption, congestion control and adaptive bitrate are built in. Hidden behind a trait so the relay/signaling can be swapped later. |
| Video | **H.264** baseline; HW encoders (NVENC / AMD AMF / Intel QSV), software x264 fallback. H.265/AV1 optional. | Universal hardware **decode** on phones; hardware **encode** keeps host CPU near zero. |
| Screen capture | **DXGI Desktop Duplication** + dirty rectangles | Fastest supported path; reports changed regions; no admin, no driver. |
| Input injection | **SendInput** | Standard, no admin, no driver. |
| Desktop client UI | **Rust + WebView2 (`wry`)** | Modern UI, reuses the core crates. |
| Mobile | **Flutter + `flutter_webrtc`** | One codebase for Android + iOS, consumer-grade UI, mature WebRTC. |
| iOS without a Mac | **Installable PWA** (web manifest + service worker) | Safari → Add to Home Screen gives an app-like, full-screen client with no App Store or Mac build. Same PWA serves Android/desktop browsers. |
| Signaling / registry | **Rust (axum) + SQLite/Postgres** | Shares the protocol + crypto crates; tiny resident footprint. |
| Relay | **coturn** (TURN) | Battle-tested; only ever forwards DTLS/SRTP ciphertext. |
| Pairing / crypto | Ed25519 identity + Noise (`snow`) session + optional SPAKE2 PIN; DPAPI at rest | Standard, auditable, end-to-end authenticated even through a relay. |
| Installer | **Inno Setup**, per-user, `HKCU\...\Run` for startup | Clean, no service, doesn't touch the system. |
| Updates | Signed version manifest + staged installer swap | Safe; no self-modifying code. |
| Logging | `tracing` + daily-rotating file; "Copy diagnostics" button | Structured and redacted. |

### Why not…

* **Electron / a webview shell for the host** — idle RAM and background CPU are
  the whole point; a Chromium runtime resident 24/7 contradicts the brief.
* **A Windows service** — needs admin to install, and the spec explicitly rules
  out unnecessary services. A per-user `Run` entry is the documented mechanism
  for user apps and is trivial to inspect/remove.
* **C++ for the host** — achievable, but Rust gives the same performance with
  memory safety and a shared codebase with the server and desktop client.
* **Raw custom UDP + hand-rolled crypto** — reinventing DTLS, ICE and
  congestion control is a security and reliability liability. WebRTC is behind a
  trait, so a custom transport can be added later without touching the pipeline.

## Components

```
┌───────────────────────── Windows Host (rc-host) ─────────────────────────┐
│  tray + message loop (UI thread)        async core (rc-core thread)      │
│  ├─ tray icon / context menu            ├─ device identity (sealed)      │
│  ├─ settings window (WebView2, M6)      ├─ idle heartbeat  ◄── M1        │
│  └─ autostart (HKCU Run)                ├─ listener + pairing      (M3/4)│
│                                         ├─ capture → encode → sink  (M2/3)│
│                                         ├─ input injection          (M3) │
│                                         └─ clipboard sync           (M3) │
└───────────────┬─────────────────────────────────────────────────────────┘
                │ WebRTC (DTLS-SRTP + data channels)
   ┌────────────┴───────────┐        signaling / presence / pairing broker
   │                        │        + TURN relay (ciphertext only)   (M4)
┌──┴───────────┐   ┌────────┴─────────┐        ▲
│ Desktop      │   │ Mobile (Flutter) │        │ WebSocket signaling
│ client (M3)  │   │ + PWA (M5)       │────────┘
└──────────────┘   └──────────────────┘
```

### Shared crates

* **`rc-common`** — `AppPaths` (per-user config/data/log dirs), `init_logging`,
  cross-crate constants (mutex/window-class names, custom window messages).
* **`rc-protocol`** — `ClientMessage` / `HostMessage`, `InputEvent`,
  `DisplayInfo`, `QualityMode`. `serde`, no platform types. `PROTOCOL_VERSION`
  is exchanged in the handshake.
* **`rc-crypto`** — `DeviceIdentity` (Ed25519): generate, seed round-trip,
  public-key fingerprint, short human `device_id` (`XXXXX-XXXXX-XXXXX-X`).
  Platform-neutral; the host seals the seed with DPAPI before writing it.
* **`rc-transport`** — `Session` / `ControlChannel` / `VideoSink` traits +
  `EncodedFrame`, `LinkFeedback`. The WebRTC implementation is added in M3.

### Windows host threading model

* **Main thread** — Win32 message loop + tray icon (`platform::tray`). Never
  does slow work.
* **`rc-core` thread** — a single-threaded Tokio runtime running
  `engine::run`. Talks to the UI thread only over an `mpsc` command channel and
  a `watch` status channel. A stalled core task can't freeze the tray, and vice
  versa.

## Security model

* **Identity** — on first run the host generates an Ed25519 keypair, seals the
  seed with **DPAPI** (user scope) and stores it under `%LOCALAPPDATA%`. The
  device ID is the public-key fingerprint. Clients generate their own identity
  the same way.
* **Pairing** *(M4)* — a QR code / 6-digit code carries `{signaling URL, host
  fingerprint, one-time token}`. Both ends run an authenticated Noise handshake
  (the host's static key is known from the fingerprint), optionally mixing a
  SPAKE2 PIN so a leaked code alone is insufficient. Each side pins the other's
  key (trust on first use).
* **Session** *(M3/M4)* — signaling is authenticated with a short-TTL session
  token. The WebRTC DTLS fingerprints are bound into the Noise transcript, so
  the peer is authenticated end-to-end even when traffic flows through the
  relay. Input, clipboard and control travel on encrypted data channels.
* **Control** — per-device revoke, "disconnect all", optional on-connect
  confirmation prompt on the host, a persistent on-screen "connected"
  indicator, and a session log that never records keystrokes, screen contents
  or clipboard data.
* **At rest** — no plaintext secrets on disk, ever. PINs are stored only as an
  Argon2 hash.

## Performance model

* **Idle** (nobody connected): no capture, no encoder, no video sockets. The
  entire cost is one timer wake-up per minute in the core thread. Target: low
  single-digit % CPU spikes at most; ~15–40 MB RAM (the WebView2 settings
  window is spawned only when opened).
* **Active**: capture only changed regions → hardware encode → the adaptive
  controller sets bitrate / FPS / resolution scale each frame from WebRTC's RTT
  and target-bitrate feedback. Frames are dropped under load rather than queued.
* **Modes**: `Low` / `Balanced` / `High` / `Auto`, each capping resolution
  scale, FPS and bitrate. `Auto` lets congestion control drive.
* **Teardown**: capture, encoder and transport resources are released
  deterministically on disconnect; no reliance on GC or process exit.

## Networking

* **Same LAN** — the host advertises via mDNS; the client connects directly. No
  server anywhere.
* **Across networks** — a small always-on signaling server (a $5 VPS or a
  Raspberry Pi; ~10 MB RAM) brokers presence, pairing and ICE. Direct P2P is
  attempted first; a **coturn** relay is the fallback and only sees ciphertext.
* **Netlify** hosts the download/landing page and the PWA client's static
  assets — it can't hold the persistent WebSocket connections signaling needs,
  so it is deliberately not in the data path.
* The signaling endpoint is a single setting (`network.signaling_url`); swapping
  or self-hosting it requires no code changes.

## Milestones

Each is built and measured before the next begins.

| # | Deliverable | State |
|---|---|---|
| **1** | Rust workspace; host skeleton — tray, settings file, autostart, single-instance, sealed identity, idle supervisor, logging. Installs and starts with Windows. | **done** |
| **2** | DXGI capture + hardware encode + loopback MP4 (no network). Measured idle vs. streaming CPU/RAM, latency, FPS. | **done** |
| **3a** | `rc-input`: `SendInput` injection for the full `InputEvent` set; per-monitor-V2 DPI awareness (verified pixel-accurate). | **done** |
| **3b** | `rc-transport::lan`: direct TCP + Noise (`NNpsk0`, PSK from pairing code); multiplexed control/video/keepalive; RTT feedback; wrong-code rejection. Loopback-verified. | **done** |
| **3c** | Streaming H.264 encoder (`StreamEncoder`, raw Annex-B, hardware MFT) + decoder (`rc-decode`); host session loop (capture→encode→send, recv→inject); `rc-desktop-client` viewer window. First usable PC↔PC control. Fixed a Noise nonce-ordering race in the LAN transport. | **done** |
| **3d** | `rc-clipboard` text sync (loop-safe, settings-gated); pairing UX — tray shows the code + "copy", connect/disconnect balloons, `require_confirmation` dialog; `rc-discovery` mDNS advertise/browse, connect by name. | **done** |
| **4a** | `rc-relay` rendezvous server (ciphertext-only); stream-generic transport (`LanSession::over_stream`); host dials out & parks with backoff; client connects by device id via `--relay`; `docker-compose.yml`. Control across any network, no inbound port. | **done** |
| **5** | Installable PWA client (`web/` + `rc-web` WASM): "My Computers" device list, connecting/viewer screens, WebCodecs H.264 decode, touch trackpad + tap/right-click/scroll/drag gestures, special-keys bar with sticky modifiers, IME typing, clipboard sync, quality control, add-to-home-screen, service-worker shell cache, Netlify static deploy + Caddy `wss://` termination. The browser wire is byte-identical to native (same `snow` + framing + `rc-protocol` codec compiled to WASM). | **done** |
| **5.5** | Persistent pairing: Noise `XXpsk0` on first pair (code as PSK) → `XX` on every reconnect (no code, mutual static-key auth). Static X25519 identities on all three clients; host keeps a `paired.json` allowlist; "Forget paired devices" in the tray; client pins the host key and rejects a mismatch. Fixed a relay-parker race where a stream buffered during an active session went stale. Verified end-to-end PAIR→RESUME on LAN, desktop-client, and web (direct + relay). | **done** |
| **6** | Inno Setup installer (per-user, no admin, embeds the app icon); optional update checker (opt-in manifest URL, notify-and-link-out only — never self-modifies); "Copy Diagnostic Logs" tray item; desktop-client identity DPAPI-sealed (matching the host); security/log audit (pairing codes no longer land in the log file). | **done** |
| **4b** | WebRTC/ICE direct-P2P behind the `Session` trait with the relay as fallback; coturn; reconnection on network change. | **deferred** — see note below |

### Why M4b (WebRTC direct-P2P) is deferred

The relay path (M4a) already gives every cross-network connection full
end-to-end Noise encryption, works through NAT/CGNAT with zero inbound ports,
and is what's shipped and tested. WebRTC would remove the relay as a hop for
most connections (lower latency, less load on whoever self-hosts the relay),
but it's a genuinely separate transport stack — ICE candidate gathering,
STUN/TURN, DTLS — layered *underneath* a data channel, which would then need
the same Noise session run on top of it to keep the "relay/TURN never sees
plaintext" guarantee. That's substantial new surface area (a coturn
deployment, NAT-traversal edge cases across carrier-grade NAT, a second code
path in `rc-transport` to keep at parity with `lan.rs`) for a latency/cost
optimization on top of a path that already works end-to-end. It's the right
next milestone for someone continuing this project, not a blocker for a
production-usable v1.0 — nothing in the non-negotiables below depends on it.

**Also not attempted, for the same reason (real feature, not a fix, and the
current path already works):** the GPU-only capture→encode path (skipping
the CPU BGRA→NV12 copy by feeding the encoder its D3D11 texture directly).
The hardware H.264 encoder is already in use end-to-end; this would trim CPU
usage further but touches the capture/encode boundary in a way that deserves
its own dedicated measurement pass rather than being folded into a hardening
milestone.

### Serverless relay: a Cloudflare Worker as an alternative to `deploy/`

[`cf-worker/`](cf-worker/README.md) reimplements the exact same rendezvous
relay protocol (`Hello` → one ACK byte → opaque ciphertext) as a Cloudflare
Worker + Durable Object, and serves the PWA from the same URL. The host picks
raw TCP or WebSocket transport based on the `signaling_url` scheme
(`crates/transport/src/relay.rs`; `ws_io.rs` adapts a WebSocket into a plain
`AsyncRead + AsyncWrite` so `LanSession` doesn't need to know which it got),
so a self-hosted `rc-relay` and this Worker are drop-in equivalents from the
host's point of view. Verified end-to-end against a real deployment
(`crates/transport/examples/ws_relay_check.rs`): full Noise handshake and
encrypted data exchange through the live Durable Object.

## Non-negotiables (from the brief)

The app must never: modify unrelated Windows settings, touch security software,
change display resolution or mouse settings on its own, install services,
require admin except for a clearly-isolated feature (with an explanation), add
input lag while unconnected, or interfere with games. Crashes and disconnects
are handled gracefully. No hidden backdoors; remote access is always visible and
disableable by the user.
