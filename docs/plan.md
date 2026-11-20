# ZeroPlay plan

ZeroPlay runs as a Docker container on a NAS at home. It holds one Remote Play
session to the console and lets several people play it from anywhere, from a
browser, a desktop app or an Android app, with any controller or with mouse and
keyboard.

UI mockups: https://claude.ai/artifact/JNLiGwMrmWNbaWX5w8nEmF

## How it fits together

```
Players (browser / desktop app / Android app)
        │  WebRTC: video + audio down, input up
        ▼
playit.gg tunnel  ──►  NAS container (ZeroPlay server)
                        ├─ Web portal, accounts, lobby
                        ├─ Relay: one stream sent on to every player, no re-encode
                        ├─ Input mixer: each player → their own gamepad slot
                        └─ Source addons ──► Xbox / PlayStation / PC
```

The server opens one session to the console. Every player connects to the
server, not to Microsoft. Video and audio are passed through as they arrive
(H.264/HEVC + Opus), so the NAS does no encoding.

## Core (built once)

- **Relay:** WebRTC to every player (webrtc-rs, already used by `zeroplay-xbox`).
- **Accounts and lobby:** ZeroPlay's own user accounts, separate from the
  Microsoft account. Player slots P1–P4, spectators, invite links with QR code,
  permissions (who may launch games, open Xbox Home, join without asking).
- **Input layer:** every device becomes a virtual Xbox gamepad.
  - Rebinding for any input on any controller, including back paddles
    (Elite Series 2, DualSense Edge, Steam Controller, 8BitDo).
  - Gyro to a stick: always / hold / toggle, aim (velocity) or tilt (absolute),
    left or right stick, sensitivity per axis, smoothing, invert, recenter,
    mix with the real stick.
  - Keyboard and mouse mapped to a controller, or sent as real keyboard and
    mouse where the game supports it.
  - Profiles per player and per controller, with per-game overrides, stored on
    the NAS.
- **Game library:** installed games from the source, art cached on the NAS.
- **Tunnel:** playit.gg agent inside the container. TCP for the portal and
  signalling, UDP for media, TCP/TURN fallback.

## Source addons

The core knows nothing about a specific console. Each platform is an addon:
a folder in `/addons` with a manifest and its own program. The server starts it
as a separate process and talks to it over a local socket with a fixed,
versioned API. Addons can be written in any language. Adding a source never
needs a new server build.

An addon provides:

- sign-in or pairing
- power: wake, sleep
- library: list games, art, launch (optional)
- stream: video and audio as RTP (H.264/HEVC + Opus)
- input sink: takes the core's virtual gamepads (and keyboard/mouse) and sends
  them in the platform's own format

The manifest declares capabilities, and the UI adapts to them:
`maxPlayers`, `library`, `launch`, `home`, `wake`, `nativeKeyboardMouse`,
`gyroPassthrough`.

| Source | Stream | Players | Game list + launch | Wake |
| --- | --- | --- | --- | --- |
| Xbox (`zeroplay-xbox`) | Yes | Up to 4 | Yes | Yes |
| PlayStation (chiaki-ng) | Yes, PS4/PS5 | Likely 1 | Probably not | Yes |
| PC (Sunshine / Moonlight protocol) | Yes | Several | Yes (app list) | Wake-on-LAN |
| PC (Steam Link client, fallback) | Captured + re-encoded | Several | No (Big Picture) | No |

Steam Link's protocol is closed, so a Steam Link addon would have to run Valve's
client on a virtual display and capture it. Sunshine/Moonlight is the main PC
source.

## Xbox specifics

- **Sign-in:** device code, done once from a phone; tokens kept on the NAS.
- **Wake:** cloud wake command plus a local SmartGlass power-on packet. Needs
  the console's power mode set to *Sleep*, not *Shutdown (energy saving)*.
- **Installed games:** console control API (`xccs.xboxlive.com`) lists what is
  installed and launches a title by product ID.
- **Art:** TitleHub and the Store catalog by title/product ID: box art and
  poster for tiles, super hero art for the large banner, logo, screenshots.
- **Several players:** gamepad index 1–4 on the one stream.

## App flow

1. Connect. The server wakes the console if needed and starts the stream in
   the background.
2. The game library shows, with the stream already running behind it.
3. Pick a game: it launches on the console and the library fades into the
   stream. Players who join while a game runs go straight into it.
4. **Jump into Xbox** shows the full stream at the console's Home screen.
5. In game, hold View + Menu (or F1) for the overlay: resume, game library,
   Xbox Home, controls and gyro, players and invite, stream quality,
   disconnect.

Layout is handheld-style (like a PS Portal), in Xbox colours. The Android app
also has an **On this device** tab listing local Android games and emulators.

## Clients

| Client | Built with | Back paddles | Gyro |
| --- | --- | --- | --- |
| Browser | Web client served by the server | Partial | DualSense / Switch Pro via WebHID (Chrome, Edge) |
| Desktop (Windows + Linux) | Tauri + SDL3 | Yes | Yes (DualSense/DS4, Switch Pro, Joy-Cons, more) |
| Android | Kotlin wrapper around the web client | Yes on most | DualSense/DS4, Switch Pro (Android 12+), phone gyro |

## Build order

1. Server skeleton in Docker: wraps `zeroplay-xbox`, streams to one browser.
2. Relay to several players, lobby, slots, invites.
3. Input layer: rebinding, keyboard/mouse mapping, gyro, profiles.
4. Game library, art cache, launch, wake flow.
5. playit.gg tunnel.
6. Addon API: move the Xbox code behind it as the first addon.
7. Desktop app.
8. Android app with the On this device tab.
9. More addons: PlayStation, PC.

## Limits

- One stream per console: everyone sees the same screen, so local co-op and
  split-screen only.
- Unofficial protocol: Microsoft changes can break it.
- Latency is console → Microsoft relay → NAS → player. Fine for co-op, not
  ideal for competitive shooters.
