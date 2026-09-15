# Building the installer

`RemoteControl.iss` is an [Inno Setup](https://jrsoftware.org/isinfo.php) 6
script that packages the host (and the desktop viewer/controller) into a
single per-user installer — no admin rights required, ever.

## Build

Releases are built by the [Release workflow](../.github/workflows/release.yml)
when a `v*` tag is pushed. To build one locally:

```powershell
cargo build --release -p rc-host -p rc-desktop-client
iscc /DMyAppVersion=0.2.0 /DMyAppURL=https://github.com/SantaClauseMacros/remote-control installer\RemoteControl.iss
```

Output: `installer/output/RemoteControlSetup.exe`. The name is deliberately
unversioned, so `releases/latest/download/RemoteControlSetup.exe` always
points at the newest installer. `MyAppVersion` and `MyAppURL` fall back to
defaults in the script when not passed.

Silent/unattended install (e.g. CI, remote deployment) — from a **native**
Windows shell (`cmd.exe` or PowerShell; MSYS/Git Bash rewrites a bare
`/VERYSILENT` into a path and breaks this):

```powershell
Start-Process .\output\RemoteControlSetup.exe -ArgumentList "/VERYSILENT","/SUPPRESSMSGBOXES","/NORESTART" -Wait
```

## What it does, and doesn't, do

* Installs to `%LOCALAPPDATA%\Programs\Remote Control` — the same privilege
  level the app runs at. `PrivilegesRequired=lowest` means Windows never
  shows a UAC prompt for this installer.
* Detects a running host via its single-instance mutex
  (`Local\RemoteControlHost.SingleInstance`) and offers to close it first, so
  upgrading over a running tray app doesn't fail on a locked file.
* Never touches `%APPDATA%\RemoteControl` (settings) or
  `%LOCALAPPDATA%\RemoteControl` (device identity, pairing, logs).
  Uninstalling removes the program, not the user's paired devices,
  identity, or preferences — reinstalling later picks up right where they
  left off.
* Does **not** write the `HKCU\...\Run` autostart entry itself. The
  "Start automatically when I sign in" checkbox (ticked by default — a
  remote-control host is only reachable while it's running) just passes
  `--enable-autostart` to the app on first launch, which persists the same
  `start_with_windows` preference the tray checkbox writes. There is exactly
  one code path that owns autostart ([`host/src/platform/autostart.rs`](../host/src/platform/autostart.rs)),
  regardless of whether it was turned on from the installer or from the tray
  later.

## Icon

`../host/assets/icon.ico` is also the installer's icon (`SetupIconFile`) and
is embedded into `rc-host.exe` itself at build time via `host/build.rs`
(`winresource`), so Explorer, the taskbar, and Alt+Tab show it too — not
just the tray, which loads the same resource explicitly at runtime.
