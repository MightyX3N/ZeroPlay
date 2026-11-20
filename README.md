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

Open `http://<nas-address>:8080`, press **Get a sign-in code** and enter the
code at microsoft.com/link. The sign-in is kept in the `/data` folder.

The image is rebuilt and published automatically on every push to `main`
(about 5 minutes; see the *Docker image* workflow under *Actions*).

To build the image yourself instead: `docker build -t zeroplay .` and change
`image:` in the compose file to `zeroplay`.

Settings (environment variables in the compose file):

| Setting | What it does |
| --- | --- |
| `ZEROPLAY_PASSWORD` | Password for the web page (any user name). Set it before using playit. |
| `PLAYIT_SECRET` | playit.gg secret key. When set, the playit agent runs inside the container. |
| `ZEROPLAY_RTC_PORTS` | UDP ports for video, one per connected player or watcher. Default `50000-50009`. |
| `ZEROPLAY_ANNOUNCE` | Extra addresses players reach the video ports on, comma separated. `host` keeps the same ports; `host:12000` means the first video port is public port 12000 and the rest follow in order. |
| `ZEROPLAY_HTTP` | Address of the web page. Default `0.0.0.0:8080`. |

From outside the home network, two playit.gg tunnels are needed: TCP for the
web page (port 8080) and UDP for the video ports (50000-50009). Put the UDP
tunnel's address in `ZEROPLAY_ANNOUNCE`.

In the browser: controllers work as soon as they are plugged in, each in its
own console slot (up to four players). Keyboard controls are switched on in the
in-game menu (F1, Esc, or hold View + Menu): WASD left stick, IJKL right stick,
arrows d-pad, Space A, B / Backspace B, X, Y, Q / E bumpers, Z / C triggers,
Tab View, Enter Menu, H Xbox button.

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
