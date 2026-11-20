//! ZeroPlay milestone 0 test tool.
//!
//! Signs in, lists your consoles, streams from one of them and lets you send
//! input by typing commands. Video and sound are saved to files so you can
//! check them in a player; nothing is shown on screen yet.

mod commands;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use zeroplay_xbox::api::HomeApi;
use zeroplay_xbox::auth::{self, Auth};
use zeroplay_xbox::consoles::{self, Console};
use zeroplay_xbox::input::{GamepadState, KeyboardFrame, MouseFrame};
use zeroplay_xbox::session::{start_home_session, Session, SessionEvent, StreamOptions};

use commands::Command;

#[derive(Parser)]
#[command(
    name = "zeroplay-spike",
    version,
    about = "ZeroPlay milestone 0: test Xbox Remote Play from Rust"
)]
struct Cli {
    /// Where the sign-in is saved. Defaults to the system config folder.
    #[arg(long, global = true)]
    tokens: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Sign in to your Microsoft account with a code.
    Login,
    /// Forget the saved sign-in.
    Logout,
    /// List the consoles on your account.
    Consoles,
    /// Wake a console that is in standby.
    Wake {
        /// Console name or id. Not needed if you have only one console.
        console: Option<String>,
    },
    /// Stream from a console and send input by typing commands.
    Stream {
        /// Console name or id. Not needed if you have only one console.
        console: Option<String>,
        /// Save the video here (raw H.264, plays in VLC or ffplay).
        #[arg(long, default_value = "zeroplay-video.h264")]
        video: PathBuf,
        /// Save the sound here (Opus in Ogg).
        #[arg(long, default_value = "zeroplay-audio.ogg")]
        audio: PathBuf,
        /// Stop by itself after this many seconds.
        #[arg(long)]
        seconds: Option<u64>,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let path = match cli.tokens {
        Some(path) => path,
        None => auth::default_token_path()?,
    };
    let mut auth = Auth::load(path).await?;

    match cli.command {
        Action::Login => login(&mut auth).await,
        Action::Logout => {
            auth.sign_out().await?;
            println!("Signed out.");
            Ok(())
        }
        Action::Consoles => {
            let consoles = list_consoles(&mut auth).await?;
            print_consoles(&consoles);
            Ok(())
        }
        Action::Wake { console } => {
            let http = reqwest_client()?;
            let token = auth.web_token().await?;
            let consoles = consoles::list(&http, &token).await?;
            let console = pick_console(&consoles, console.as_deref())?;
            consoles::wake(&http, &token, &console.id).await?;
            println!("Asked {} to wake up.", console.name);
            Ok(())
        }
        Action::Stream {
            console,
            video,
            audio,
            seconds,
        } => stream(&mut auth, console, video, audio, seconds).await,
    }
}

fn reqwest_client() -> Result<zeroplay_xbox::reqwest::Client> {
    Ok(zeroplay_xbox::reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?)
}

async fn login(auth: &mut Auth) -> Result<()> {
    let code = auth.start_device_code().await?;
    println!();
    println!("To sign in, open  {}  on any device", code.verification_uri);
    println!("and enter the code  {}", code.user_code);
    println!();
    println!("Waiting for you to finish signing in...");
    auth.finish_device_code(&code).await?;
    println!(
        "Signed in. The sign-in is saved in {}",
        auth.token_path().display()
    );
    Ok(())
}

async fn list_consoles(auth: &mut Auth) -> Result<Vec<Console>> {
    let http = reqwest_client()?;
    let token = auth.web_token().await?;
    consoles::list(&http, &token).await
}

fn print_consoles(consoles: &[Console]) {
    if consoles.is_empty() {
        println!("No consoles found on this account.");
        return;
    }
    for console in consoles {
        println!("{}", console.name);
        println!("    id:          {}", console.id);
        println!("    type:        {}", console.console_type);
        println!("    power:       {}", console.power_state);
        println!(
            "    remote play: {}",
            if console.console_streaming_enabled {
                "on"
            } else {
                "OFF (turn on remote features in the console's settings)"
            }
        );
    }
}

fn pick_console<'a>(consoles: &'a [Console], wanted: Option<&str>) -> Result<&'a Console> {
    match wanted {
        Some(wanted) => consoles
            .iter()
            .find(|c| c.id.eq_ignore_ascii_case(wanted) || c.name.eq_ignore_ascii_case(wanted))
            .ok_or_else(|| {
                let names: Vec<&str> = consoles.iter().map(|c| c.name.as_str()).collect();
                anyhow!(
                    "no console called \"{wanted}\". Your consoles: {}",
                    names.join(", ")
                )
            }),
        None => match consoles {
            [] => bail!("no consoles found on this account"),
            [only] => Ok(only),
            many => {
                let names: Vec<&str> = many.iter().map(|c| c.name.as_str()).collect();
                bail!(
                    "you have several consoles, name one of them: {}",
                    names.join(", ")
                )
            }
        },
    }
}

async fn stream(
    auth: &mut Auth,
    wanted: Option<String>,
    video: PathBuf,
    audio: PathBuf,
    seconds: Option<u64>,
) -> Result<()> {
    if !auth.is_signed_in() {
        bail!("not signed in yet: run `zeroplay-spike login` first");
    }
    let consoles = list_consoles(auth).await?;
    let console = pick_console(&consoles, wanted.as_deref())?.clone();
    println!(
        "Console: {} ({}, power: {})",
        console.name, console.console_type, console.power_state
    );
    if !console.console_streaming_enabled {
        println!("Warning: remote features look switched off on this console. The stream will probably be refused.");
    }

    let token = auth.home_streaming_token().await?;
    println!("Streaming region: {}", token.region);
    let api = HomeApi::new(&token)?;
    let signaling =
        start_home_session(&api, auth, &console.id, &|line| println!("  {line}")).await?;
    println!("Connecting to the console...");
    let options = StreamOptions {
        video_path: Some(video.clone()),
        audio_path: Some(audio.clone()),
        ..Default::default()
    };
    let signaling = Arc::new(signaling);
    let mut session = match Session::connect(signaling.clone(), options).await {
        Ok(session) => session,
        Err(error) => {
            // Free the console again if we never got connected.
            use zeroplay_xbox::session::Signaling;
            let _ = signaling.stop().await;
            return Err(error);
        }
    };
    println!(
        "Connected. Video is saved to {} and sound to {}.",
        video.display(),
        audio.display()
    );
    println!("Type `help` for the input commands, `quit` to stop.");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    let mut stats_timer = tokio::time::interval(Duration::from_secs(5));
    stats_timer.tick().await;
    let started = Instant::now();
    let mut last = session.stats();
    let mut current_pad: u8 = 0;
    let stop_at = seconds.map(|s| started + Duration::from_secs(s));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        let timeout = async {
            match stop_at {
                Some(at) => tokio::time::sleep_until(at.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = &mut ctrl_c => {
                println!("Stopping.");
                break;
            }
            _ = timeout => {
                println!("Time is up, stopping.");
                break;
            }
            event = session.next_event() => match event {
                Some(SessionEvent::Ready) => println!("[console] handshake done, input works now"),
                Some(SessionEvent::Connection(state)) => {
                    println!("[connection] {state}");
                    if state == "failed" || state == "closed" || state == "disconnected" {
                        break;
                    }
                }
                Some(SessionEvent::VideoSize { width, height }) => println!("[console] video size {width}x{height}"),
                Some(SessionEvent::Vibration(v)) => println!(
                    "[rumble] pad {} left {:.0}% right {:.0}% triggers {:.0}%/{:.0}% for {} ms",
                    v.gamepad_index + 1,
                    v.left_motor * 100.0,
                    v.right_motor * 100.0,
                    v.left_trigger_motor * 100.0,
                    v.right_trigger_motor * 100.0,
                    v.duration_ms
                ),
                Some(SessionEvent::TitleInfo(info)) => println!("[console] now running: {info}"),
                Some(SessionEvent::ServerMessage { target, content }) => println!("[console] {target} {content}"),
                Some(SessionEvent::Disconnected(reason)) => {
                    println!("[console] ended the stream: {reason}");
                    break;
                }
                None => break,
            },
            _ = stats_timer.tick() => {
                let now = session.stats();
                let video_kbps = (now.video_bytes - last.video_bytes) * 8 / 5 / 1000;
                println!(
                    "[stats] {:>4}s  video {} kbit/s ({} packets)  audio {} packets  input {} packets sent",
                    started.elapsed().as_secs(),
                    video_kbps,
                    now.video_packets,
                    now.audio_packets,
                    now.input_packets_sent
                );
                last = now;
            }
            line = lines.next_line(), if stdin_open => match line {
                Ok(Some(line)) => {
                    match commands::parse(&line) {
                        Ok(Command::Nothing) => {}
                        Ok(Command::Quit) => break,
                        Ok(command) => {
                            if let Err(error) = apply(&session, &mut current_pad, command).await {
                                println!("that did not work: {error:#}");
                            }
                        }
                        Err(error) => println!("{error}"),
                    }
                }
                // No more input (for example when started from a script): keep streaming.
                _ => stdin_open = false,
            },
        }
    }

    let stats = session.stats();
    session.close().await?;
    println!(
        "Done. Received {} video packets ({} kB) and {} audio packets; sent {} input packets.",
        stats.video_packets,
        stats.video_bytes / 1000,
        stats.audio_packets,
        stats.input_packets_sent
    );
    Ok(())
}

fn pad_state(session: &Session, index: u8) -> GamepadState {
    session
        .gamepads()
        .into_iter()
        .find(|p| p.index == index)
        .unwrap_or_else(|| GamepadState::new(index))
}

const TAP: Duration = Duration::from_millis(150);

async fn apply(session: &Session, current_pad: &mut u8, command: Command) -> Result<()> {
    match command {
        Command::Nothing | Command::Quit => {}
        Command::Help => println!("{}", commands::HELP),
        Command::Tap(buttons) => {
            let mut state = pad_state(session, *current_pad);
            state.buttons |= buttons;
            session.set_gamepad(state).await?;
            tokio::time::sleep(TAP).await;
            let mut state = pad_state(session, *current_pad);
            state.buttons &= !buttons;
            session.set_gamepad(state).await?;
        }
        Command::Hold(buttons) => {
            let mut state = pad_state(session, *current_pad);
            state.buttons |= buttons;
            session.set_gamepad(state).await?;
        }
        Command::Release(buttons) => {
            let mut state = pad_state(session, *current_pad);
            state.buttons &= !buttons;
            session.set_gamepad(state).await?;
        }
        Command::Trigger { right } => {
            let mut state = pad_state(session, *current_pad);
            if right {
                state.right_trigger = 1.0
            } else {
                state.left_trigger = 1.0
            }
            session.set_gamepad(state).await?;
            tokio::time::sleep(TAP).await;
            let mut state = pad_state(session, *current_pad);
            if right {
                state.right_trigger = 0.0
            } else {
                state.left_trigger = 0.0
            }
            session.set_gamepad(state).await?;
        }
        Command::Stick { right, x, y } => {
            let mut state = pad_state(session, *current_pad);
            if right {
                state.right_x = x;
                state.right_y = y;
            } else {
                state.left_x = x;
                state.left_y = y;
            }
            session.set_gamepad(state).await?;
        }
        Command::Center => {
            let mut state = GamepadState::new(*current_pad);
            state.buttons = 0;
            session.set_gamepad(state).await?;
        }
        Command::Pad(index) => {
            *current_pad = index;
            session.set_gamepad(pad_state(session, index)).await?;
            println!(
                "Now controlling pad {} ({} plugged in)",
                index + 1,
                session.gamepads().len()
            );
        }
        Command::Unplug(index) => {
            session.remove_gamepad(index).await?;
            println!(
                "Pad {} unplugged ({} plugged in)",
                index + 1,
                session.gamepads().len()
            );
        }
        Command::Key(key_code) => {
            session.send_key(KeyboardFrame {
                pressed: true,
                key_code,
            });
            tokio::time::sleep(TAP).await;
            session.send_key(KeyboardFrame {
                pressed: false,
                key_code,
            });
        }
        Command::MouseMove { x, y } => {
            session.send_mouse(MouseFrame {
                x,
                y,
                ..Default::default()
            });
        }
        Command::Click(buttons) => {
            session.send_mouse(MouseFrame {
                buttons,
                ..Default::default()
            });
            tokio::time::sleep(TAP).await;
            session.send_mouse(MouseFrame::default());
        }
        Command::Keyframe => session.request_keyframe().await?,
        Command::Stats => println!("{:?}", session.stats()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(name: &str, id: &str) -> Console {
        Console {
            id: id.to_owned(),
            name: name.to_owned(),
            console_type: "XboxSeriesX".to_owned(),
            power_state: "On".to_owned(),
            console_streaming_enabled: true,
            remote_management_enabled: true,
        }
    }

    #[test]
    fn a_single_console_needs_no_name() {
        let consoles = [console("Living room", "F4001")];
        assert_eq!(pick_console(&consoles, None).unwrap().id, "F4001");
    }

    #[test]
    fn consoles_are_found_by_name_or_id() {
        let consoles = [console("Living room", "F4001"), console("Bedroom", "F4002")];
        assert_eq!(
            pick_console(&consoles, Some("bedroom")).unwrap().id,
            "F4002"
        );
        assert_eq!(
            pick_console(&consoles, Some("f4001")).unwrap().name,
            "Living room"
        );
    }

    #[test]
    fn several_consoles_need_a_name() {
        let consoles = [console("Living room", "F4001"), console("Bedroom", "F4002")];
        let error = pick_console(&consoles, None).unwrap_err().to_string();
        assert!(error.contains("Living room") && error.contains("Bedroom"));
        let error = pick_console(&consoles, Some("Kitchen"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Kitchen") && error.contains("Bedroom"));
        assert!(pick_console(&[], None).is_err());
    }
}
