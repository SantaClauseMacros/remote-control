<div align="center">

# Remote Control

**Control your Windows PC from your phone, Chromebook or another computer — games included.**

Low-latency screen streaming · full mouse & keyboard · touch game controls · works from any network · end-to-end encrypted

<a href="https://github.com/SantaClauseMacros/remote-control/releases/latest/download/RemoteControlSetup.exe"><img alt="Download for Windows" src="https://img.shields.io/badge/Download-Windows%20installer-2ea44f?style=for-the-badge&logo=windows&logoColor=white"></a>
&nbsp;
<a href="https://remote-control.bloxvault8436200.workers.dev"><img alt="Open the phone app" src="https://img.shields.io/badge/Open-phone%20app-4c8dff?style=for-the-badge&logo=googlechrome&logoColor=white"></a>

<a href="https://github.com/SantaClauseMacros/remote-control/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/SantaClauseMacros/remote-control?label=latest&style=flat-square"></a>
<a href="https://github.com/SantaClauseMacros/remote-control/releases"><img alt="Downloads" src="https://img.shields.io/github/downloads/SantaClauseMacros/remote-control/total?style=flat-square"></a>

[All releases](https://github.com/SantaClauseMacros/remote-control/releases) · [How it works](ARCHITECTURE.md) · [Build from source](docs/DEVELOPING.md)

</div>

---

## Set it up in 3 minutes

### 1. On the PC you want to control

1. **[Download RemoteControlSetup.exe](https://github.com/SantaClauseMacros/remote-control/releases/latest/download/RemoteControlSetup.exe)** and run it.
   - Windows may say **"Windows protected your PC"** because the app isn't code-signed yet. Click **More info → Run anyway**.
   - No administrator rights needed. Leave **Start automatically when I sign in** ticked, so you can connect while you're away.
2. The **Remote Control app** opens, showing your **PC ID** (like `ABCDE-FGHIJ-KLMNO-P`) and a **QR code**.
3. It keeps running in the system tray, next to the clock. Click that icon any time to open the app again (check the **^** arrow if you don't see it).

### 2. On your phone, Chromebook or other computer

1. **Scan the QR code** in the Remote Control app with your phone's camera. (Or click **Copy phone link** and send it to yourself, or go to **https://remote-control.bloxvault8436200.workers.dev**, tap **+**, and type the PC ID.)
2. Give the PC a name, tap **Save**, then **Connect**.
3. Optional: **Add to Home Screen** (the Share menu on iPhone, the ⋮ menu on Android) so it opens like a normal app.

That's it — no accounts, no port forwarding, no router settings. Works best in **Chrome** or **Edge**, or **Safari** on iPhone and iPad.

---

## The PC app

Click the tray icon to open it.

- **Home** — your PC ID and QR code, who's connected right now (with live FPS and ping), quick switches, and a health check that tells you if anything needs fixing
- **Devices** — phones and computers that have connected
- **Activity** — recent connections, drops and updates
- **Settings** — PC name, start with Windows, video quality and frame rate, clipboard sync, microphone, keeping the PC awake, multi-monitor handling, updates, and tools like *Restart as Administrator*
- **Help** — how to connect, touch controls, Minecraft mode and common fixes

## Using it

| Do this | To |
|---|---|
| Drag one finger | Move the pointer |
| Tap | Click |
| Hold, then drag | Click and drag |
| Two-finger tap | Right-click |
| Two-finger drag | Scroll |
| Pinch | Zoom in on the screen |
| **⋯** (bottom-left) | Keyboard, special keys (Ctrl, Alt, Esc, F-keys…), quality, disconnect |

**On a laptop or Chromebook,** your mouse and keyboard just work. When a game grabs the mouse, your first click locks it for aiming — press **Esc** to get it back.

**Sound:** you hear the PC on your phone automatically. Tap **⋯ → Sound** to mute. (On iPhone, turn the silent switch off.)

**Microphone:** tap **⋯ → Mic** to send your phone's mic to the PC. Turn on **Microphone** in the PC app's Settings first. Without a virtual audio cable installed on the PC, it just plays out loud through its speakers rather than being usable as a mic in Discord or a game — see [Using it as an actual PC microphone](#using-your-phones-mic-as-a-pc-microphone) below.

**Send a file:** tap **⋯ → Send file** to upload something from your phone straight to the PC's `Downloads\RemoteControl` folder. The PC app can send files back the same way (Home, while connected → **Send a file to this device**).

**Controller:** pair a real controller (Bluetooth or USB-OTG) to your phone and it shows up on the PC as a virtual Xbox controller — works in Steam games, Fortnite, Rocket League, and anything else that takes a controller. Needs the free [ViGEmBus driver](https://github.com/ViGEm/ViGEmBus/releases) installed on the PC once; if it's missing, the app tells you the first time you use a controller.

### 🎮 Game modes (phone)

Tap **🕹** and pick your game — **Minecraft**, **Roblox**, **Fortnite** or **Any other game**. Each has its own buttons and a **?** help sheet; switch any time with **Game**.

- **Roblox:** drag to turn the camera, tap to click, Shift / E / Click / Jump, and 1–6 for tools
- **Fortnite:** drag to aim, hold **Fire**, tap **Aim**, Reload, Use, Crouch, Jump, and 1–6 for weapons
- **Any other game:** joystick for W A S D, Jump, Shift, Ctrl, Q E R F and 1–6

#### Minecraft

Tap **🕹** in the top-left corner to show game controls:

- **Tap anywhere** to hit, **hold** to mine, **drag** to look around
- **Tap a slot right on Minecraft's hotbar** to switch items
- **Inv** is left of the hotbar, **Drop** is right of it
- **Joystick** to walk · **Jump** · **Use** to place or eat
- **Sprint** (Shift) and **Crouch** (Ctrl) — tap to toggle on and off
- **Esc**, **Chat**, **View** (F5) and **Swap** (F) are in the top-left
- In your inventory and menus, tap exactly where you want to click; hold to drag an item

Tap **?** for the full list. If the hotbar targets don't line up, set **Hotbar size** in there to match Minecraft's *GUI Scale* setting. More games are coming.

---

## Using your phone's mic as a PC microphone

Windows has no built-in way to turn incoming audio into a microphone other apps can select — it needs one free, one-time driver:

1. Install [VB-CABLE](https://vb-audio.com/Cable/) (free) on the PC and reboot.
2. In the PC app's **Settings → Features**, turn on **Microphone**.
3. In whatever app you want to talk in (Discord, a game), set its microphone to **CABLE Output**.
4. On your phone, tap **⋯ → Mic**.

Without VB-CABLE installed, the mic toggle still works — your phone's mic just plays out loud through the PC's speakers instead, which is fine for testing but not for voice chat (everyone would hear an echo).

---

## Tips

- **Can't click on Task Manager or admin windows?** In the app, go to **Settings → Tools → Restart as Administrator**.
- **Same Wi-Fi as your PC?** Remote Control connects straight across your home network (you'll see **⚡** at the top) for the lowest lag. On many home routers it can also connect directly from a *different* network (mobile data, a friend's house) the same way — if it can't, it falls back to the relay automatically either way. You can turn direct connections off in the phone app's **⚙** settings.
- **Laggy or choppy?** Tap **⋯ → Quality → Low**. A wired connection (or 5 GHz Wi-Fi) on the PC helps most. On a direct (⚡) connection, turning on **60 FPS boost on direct connections** in the PC app's Settings can make fast games noticeably smoother.
- **Running Parsec, OBS or GeForce ShadowPlay too?** They can use up your graphics card's video encoder, which pushes Remote Control onto slower CPU encoding. Close them if things feel slow.
- **Clipboard** text syncs both ways automatically. You can turn that off in the phone app's **⚙** settings.
- **Second monitor?** Only your primary display is streamed by default. In **Settings → Multiple monitors**, choose to duplicate everything onto the primary display, or move windows there, for the length of a session — it's undone automatically when you disconnect.
- **PC going to sleep while you're out?** Turn on **Keep this PC awake** in Settings so it stays reachable.
- **Updates:** the tray icon and app both tell you when a new version is out, with a one-click **Install now** that downloads and installs it, then restarts the app — your PC ID and settings are kept either way.

---

## Security & privacy

- **Your PC ID works like a password.** Anyone who has it can control your PC, so only use it on your own devices. If it ever leaks: right-click the tray icon → **Exit**, delete the folder `%LOCALAPPDATA%\RemoteControl`, and start Remote Control again to get a brand-new ID.
- Everything between your device and your PC is **end-to-end encrypted** (the Noise protocol). The relay in the middle only passes encrypted data along — it can't see your screen or what you type. Direct (⚡) connections use the same encryption.
- Your PC shows a notification whenever a device connects. **Disconnect all sessions** in the tray menu kicks everyone off.
- To stop remote access, right-click the tray icon → **Exit**, or uninstall it.

---

## Troubleshooting

<details>
<summary><b>The phone app says the PC is offline</b></summary>

Check that the PC is switched on, signed in, and the Remote Control tray icon is there. If it isn't, open **Remote Control** from the Start menu. A sleeping PC can't be reached — turn sleep off in **Settings → System → Power** if you want to connect while you're away.
</details>

<details>
<summary><b>Black screen, or the picture freezes</b></summary>

Tap **⋯ → Leave** and connect again. If it keeps happening, try **⋯ → Quality → Low**. Windows doesn't allow screen capture on the lock screen or on admin prompts, so sign in on the PC first.
</details>

<details>
<summary><b>My antivirus flagged it</b></summary>

Remote-control tools sometimes get flagged because of what they do (see your screen, move your mouse). The installer is built from the source code in this repository, so you can check exactly what's in it.
</details>

<details>
<summary><b>How do I uninstall?</b></summary>

**Settings → Apps → Installed apps → Remote Control → Uninstall.** To also remove your PC ID and settings, delete `%APPDATA%\RemoteControl` and `%LOCALAPPDATA%\RemoteControl`.
</details>

---

## For developers

A Rust tray host (DXGI capture, Media Foundation H.264, SendInput), a browser client (WebCodecs + WebAssembly), and a Cloudflare Worker relay. **[docs/DEVELOPING.md](docs/DEVELOPING.md)** covers building, the web client, the relay and cutting releases; **[ARCHITECTURE.md](ARCHITECTURE.md)** explains the design.

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
