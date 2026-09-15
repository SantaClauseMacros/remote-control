# Developing Remote Control

## Repository layout

| Path | What it is |
|---|---|
| `crates/common` | Paths, logging, shared constants |
| `crates/protocol` | Wire message types (input, clipboard, control) + codec |
| `crates/crypto` | Ed25519 device identity, static X25519 identity, pairing KDF |
| `crates/transport` | `Session` trait + `lan` (TCP + Noise `XXpsk0`/`XX`) + `relay` + WebSocket adapter |
| `crates/web` | `rc-web` — transport + codec compiled to WASM for the browser client |
| `crates/capture` | DXGI Desktop Duplication |
| `crates/encode` | H.264 via Media Foundation (hardware MFT, software fallback) |
| `crates/decode` | H.264 → BGRA (Media Foundation) |
| `crates/input` | `SendInput` injection, per-monitor-V2 DPI, cursor-capture detection |
| `crates/clipboard` | Clipboard text get/set + change watcher |
| `crates/discovery` | mDNS advertise/browse (`_remotecontrol._tcp`) |
| `host` | Windows tray host and app window (`host/src/dashboard`): session loop, settings, autostart, relay parker, update checker |
| `desktop-client` | Windows viewer/controller (LAN + relay) |
| `server` | `rc-relay` — self-hostable rendezvous relay (ciphertext only) |
| `cf-worker` | Cloudflare Worker: the same relay protocol over WebSocket, and it serves `web/` |
| `web` | Installable PWA client (phone, Chromebook, desktop browser) |
| `deploy` | `docker-compose.yml` (relay + Caddy `wss://`) for self-hosting |
| `installer` | Inno Setup script — per-user, no-admin installer |
| `.github/workflows` | CI build check, and the release pipeline |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how the pieces fit together.

## Building the Windows host

Prerequisites:

* Rust (stable, MSVC toolchain) — <https://rustup.rs>
* Visual Studio Build Tools 2022 with **Desktop development with C++**

```bash
cargo build -p rc-host             # debug build (keeps a console for logs)
cargo build -p rc-host --release   # optimised, no console window
cargo test -p rc-host -p rc-crypto -p rc-input
```

Run with `--console` to force a log console in any build; set `RC_LOG=rc_host=trace` for verbose logging.

### The app window

The tray host serves a small dashboard on `127.0.0.1` (random port, per-launch token) and opens it as a Microsoft Edge app window. The page is a single file, `host/src/dashboard/ui.html`, compiled into the binary; the JSON API behind it is in `host/src/dashboard/mod.rs`.

```bash
cargo run -p rc-host -- --dashboard-preview
```

serves the window with made-up data and a throwaway config folder, and prints its URL. It runs alongside an installed host without touching it, the registry, or real settings.

### Build-time settings

| Environment variable | Effect |
|---|---|
| `RC_DEFAULT_RELAY` | Relay a fresh install parks at. Defaults to the project's Cloudflare Worker. |
| `RC_UPDATE_URL` | Update manifest URL baked into the build. Unset means no update checks. The release workflow points it at this repo's `latest.json`. |

A `network.signaling_url` or `update.check_url` already in a user's `config.toml` always overrides these.

### Where the host keeps its data

| | Path |
|---|---|
| Config (editable) | `%APPDATA%\RemoteControl\RemoteControl\config\config.toml` |
| Device identity (DPAPI-sealed) | `%LOCALAPPDATA%\RemoteControl\RemoteControl\data\identity.bin` |
| Logs (daily rotation) | `%LOCALAPPDATA%\RemoteControl\RemoteControl\data\logs\` |

Nothing is written under `Program Files`; the host never needs administrator rights (except the optional "Restart as Administrator").

## The web client (`web/`)

Plain static files — no bundler. The WASM half (`crates/web`) is prebuilt into `web/pkg` and committed, so the site deploys with no build step. Rebuild it after touching `crates/web` or `crates/protocol`:

```bash
wasm-pack build crates/web --target web --out-dir ../../web/pkg --release
```

Bump `CACHE` in `web/sw.js` whenever the app shell changes, so installed copies pick up the new version.

Game control layouts live in `GAME_MODES` in `web/app.js` — add a new entry there for another game.

## The relay (`cf-worker/`)

```bash
cd cf-worker
npx wrangler deploy
```

One Worker serves both the web app and the relay (`/relay`). See [cf-worker/README.md](../cf-worker/README.md). A deploy restarts the relay's Durable Object; parked hosts reconnect on their own within a few seconds. To self-host instead, see [deploy/README.md](../deploy/README.md).

## Releasing

1. Bump `version` in the workspace `Cargo.toml`, run `cargo build` so `Cargo.lock` updates, and commit both.
2. Tag and push:

   ```bash
   git tag v0.3.0
   git push origin v0.3.0
   ```

3. The [Release workflow](../.github/workflows/release.yml) builds the host with `RC_UPDATE_URL` set, packages `installer/RemoteControl.iss` with Inno Setup, and publishes two assets to the GitHub release:
   * `RemoteControlSetup.exe` — what the README's download button links to
   * `latest.json` — `{"version","url","notes"}`, polled by installed hosts, which then show **Update available** in the tray

To build the installer locally instead (Inno Setup 6 installed):

```powershell
$env:RC_UPDATE_URL = "https://github.com/SantaClauseMacros/remote-control/releases/latest/download/latest.json"
cargo build --release -p rc-host -p rc-desktop-client
iscc /DMyAppVersion=0.3.0 /DMyAppURL=https://github.com/SantaClauseMacros/remote-control installer\RemoteControl.iss
# → installer\output\RemoteControlSetup.exe
```
