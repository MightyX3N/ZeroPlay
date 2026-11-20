# ZeroPlay

Play your consoles from any device, with any controller or with mouse and keyboard.

ZeroPlay runs as a Docker container on a NAS at home. It keeps one Remote Play
session open to your console and lets several people play it from anywhere,
in a browser, a desktop app (Windows and Linux) or an Android app, reached
through a playit.gg tunnel. Each streaming service is an addon; the first one
is Xbox Remote Play.

The full plan is in [docs/plan.md](docs/plan.md).

**Status: milestone 0.** There is no server or app yet. This repo holds the
Xbox Remote Play protocol in Rust and a small command-line test tool that
proves it works against a real console.

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

The tests include a full session against a fake console in the same process:
SDP and ICE exchange, the data channel handshake, gamepad, mouse and keyboard
packets checked byte for byte, rumble, and video saved to a file. A fake console
cannot prove that a real Xbox accepts the same bytes; the checklist above does that.

## Layout

| Path | What it is |
| --- | --- |
| `crates/zeroplay-xbox` | Xbox Remote Play: sign-in, console list, session API, WebRTC stream, input packets |
| `crates/zeroplay-spike` | The command-line test tool |
| `docs/plan.md` | The plan: NAS server, relay, input layer, addons, apps |

## Credits and licence

ZeroPlay is MIT licensed. The Xbox protocol code is a Rust port of work by
UnknownSKL (Jim Kroon) in Greenlight, xbox-xcloud-player, xal-node and
xbox-webapi-node, all MIT. See [CREDITS.md](CREDITS.md).

ZeroPlay is not made or endorsed by Microsoft.
