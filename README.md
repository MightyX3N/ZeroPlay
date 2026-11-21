# ZeroPlay

Play your consoles from any device, with any controller or with mouse and keyboard.

ZeroPlay runs as a Docker container on a NAS at home. It keeps one Remote Play
session open to your console and lets several people play it from anywhere,
in a browser, a desktop app (Windows and Linux) or an Android app, reached
through a playit.gg tunnel. Each streaming service is an addon; the first one
is Xbox Remote Play.

The full plan is in [docs/plan.md](docs/plan.md).

**Status: step 1 of the plan.** The server runs in Docker, signs in to
Microsoft, wakes the console, holds the stream and passes it on to every
player's browser, with each player's controllers in their own console slot.
No game library, rebinding or apps yet.

## Run the server

On the NAS (or any Linux machine on the same network as the Xbox), use
[`docker-compose.yml`](docker-compose.yml): paste it into the NAS's container
manager (Portainer, Synology Container Manager, Unraid, TrueNAS) or save it and
run `docker compose up -d`. It pulls the ready-made image
`ghcr.io/mightyx3n/zeroplay:latest` (amd64 and arm64), so nothing is built on
the NAS.

Open `http://<nas-address>:8080` from your home network:

1. Press **Get a sign-in code** and enter the code at microsoft.com/link.
2. Create the admin profile (name and password). This can only be done from
   the home network. The admin adds more profiles under the people icon, and
   can switch on *Allow access requests* so others can ask for a profile from
   the login page. Per profile the admin sets whether it may open Xbox Home
   and its highest stream quality.
3. For use outside your home, press the globe icon and open the playit.gg
   claim link it shows. Once approved, ZeroPlay creates its playit tunnels
   itself and shows the address to use.

Everything is kept in the `/data` folder. To start the profiles over, delete
`access.json` there and create the admin profile again from home.

While playing:

- **Hand-off:** pressing *Back to home*, or no input for 5 minutes (with a
  warning 30 seconds before), passes your controller to whoever has waited
  longest. With nobody else there, the stream stops; it also stops when
  nobody has been connected for 30 seconds.
- **Lost connections:** if a player's connection drops, their controller is
  unplugged on the console (most games pause) and their slot is kept for a
  minute. If the stream to the Xbox drops, ZeroPlay presses the Xbox button
  through Xbox Live (the guide pauses most games) and reconnects.
- **Timed play** (admin setting, under the people icon): players who opt in
  from the in-game menu take turns, with warnings and a 10 second countdown
  before switching. More time can be asked for; an admin approves it.
- **Volume** and **recording** are in the in-game menu, per device. Recordings
  are made in the browser from the video it already receives, and saved on
  that device.

**As an app:** on iPhone or iPad open the page in Safari, then *Share → Add to
Home Screen*. ZeroPlay then opens full screen like an app (log in once more
there; it keeps its own login). Android and desktop Chrome/Edge offer
*Install* as well.

From outside the home everything goes through one playit tunnel: the video
and sound travel inside the page's own connection and the browser decodes
them (Safari 16.4+, Chrome, Edge, Firefox 130+). At home the video goes
straight over the home network.

In the browser: controllers work as soon as they are plugged in, as one
console slot. All controllers, the keyboard and the mouse in one browser are
one player, so you can switch between them; for couch co-op on one device,
set *Controllers here* to *separate players* in the in-game menu. Keyboard and
mouse work as a controller (switch them off in the in-game menu: F1, or hold View + Menu). Click the
picture to capture the mouse, Esc lets it go.

| Keyboard / mouse | Controller |
| --- | --- |
| Mouse movement | Right stick (sensitivity in the menu) |
| Left click / right click | RT / LT |
| Middle click, wheel up / down | RS click, RB / LB |
| Mouse back / forward buttons | B / Y |
| WASD, IJKL, arrows | Left stick, right stick, d-pad |
| Space, B or Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | LB / RB, LT / RT |
| F / G | LS / RS click |
| Tab, Enter, H | View, Menu, Xbox button |

## What works

- Sign in to a Microsoft account with a code (no browser window, no password in the app)
- List the consoles on the account and wake one from standby
- Start a Remote Play session and connect to the console over WebRTC
- Receive video (H.264) and sound (Opus) and save them to files
- Send gamepad input for up to four gamepads at once
- Send real mouse and keyboard input
- Receive rumble commands and the name of the running game

## The test tool

Get `zeroplay-spike`:

- **Download:** open the latest run under the repo's *Actions* tab and download
  `zeroplay-spike-windows-latest` (or `ubuntu-latest`).
- **Or build it:** install [Rust](https://rustup.rs), then run
  `cargo build --release -p zeroplay-spike`. The program is in `target/release`.

Your console needs remote features switched on:
*Settings > Devices & connections > Remote features > Enable remote features*.

```
zeroplay-spike login       sign in (shows a code to enter at microsoft.com/link)
zeroplay-spike consoles    list your consoles
zeroplay-spike stream      stream from your console (name it if you have several)
```

While `stream` runs, type commands and press Enter:

```
a b x y lb rb ls rs up down left right menu view nexus   tap a button
hold <button> / release <button>                          keep a button down / let go
lt / rt                                                   tap a trigger
stick l|r <x> <y>                                         tilt a stick, -1 to 1
center                                                    let go of everything
pad <1-4>                                                 plug in and control another pad
unplug <1-4>                                              unplug a pad
key <letter, digit or code>                               tap a real keyboard key
mouse <dx> <dy>                                           move a real mouse
click [left|right|middle]                                 click a real mouse button
keyframe   stats   help   quit
```

Video is saved to `zeroplay-video.h264` and sound to `zeroplay-audio.ogg` in
the folder you started it from. Both play in VLC.

The sign-in is saved in your system's config folder under `zeroplay/tokens.json`.
`zeroplay-spike logout` removes it.

## Milestone 0 checklist

These need a real console, so they are run by hand:

1. **Video and input from Rust.** `stream`, then `nexus`. The guide should open
   on the console, and the saved video should show it.
2. **Several gamepads.** In a game with local multiplayer: `pad 2`, then `a`.
   A second player should join.
3. **Real mouse and keyboard.** In a game that supports them on Xbox: `key w`,
   `mouse 100 0`, `click`.
4. **Away from home.** Run `stream` from another network (for example a phone
   hotspot) and note whether it connects.
5. **Two consoles at once.** Run `stream` twice, once per console, at the same time.

## Tests

```
cargo test
```

The server tests include the relay: fake console packets go in, a WebRTC peer
playing the browser checks that video and sound come out with the payload
types it asked for.

The tests include a full session against a fake console in the same process:
SDP and ICE exchange, the data channel handshake, gamepad, mouse and keyboard
packets checked byte for byte, rumble, and video saved to a file. A fake console
cannot prove that a real Xbox accepts the same bytes; the checklist above does that.

## Layout

| Path | What it is |
| --- | --- |
| `crates/zeroplay-xbox` | Xbox Remote Play: sign-in, console list, session API, WebRTC stream, input packets |
| `crates/zeroplay-spike` | The command-line test tool |
| `crates/zeroplay-server` | The server: web page, sign-in, stream relay to players, controller slots |
| `Dockerfile`, `docker-compose.yml` | The Docker image, with the playit.gg agent |
| `docs/plan.md` | The plan: NAS server, relay, input layer, addons, apps |

## Credits and licence

ZeroPlay is MIT licensed. The Xbox protocol code is a Rust port of work by
UnknownSKL (Jim Kroon) in Greenlight, xbox-xcloud-player, xal-node and
xbox-webapi-node, all MIT. See [CREDITS.md](CREDITS.md).

ZeroPlay is not made or endorsed by Microsoft.

## Capture card (optional)

A capture card between the Xbox and the TV gives a cleaner picture than remote
play. Cards that work like a webcam (most current ones) are supported: plug it
into the NAS, then pick it under **Profiles → Picture source**. Remote play
stays connected for the controllers. The compose file above already gives the
container access to capture cards, sound and the graphics chip (used for
encoding when there is one). Turn off HDCP on the Xbox (Settings → General →
TV & display options → Video fidelity), or the card only gets a black picture.

