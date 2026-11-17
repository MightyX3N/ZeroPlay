//! The typed commands of the test tool.

use zeroplay_xbox::input::{button, mouse_button};

pub const HELP: &str = "\
Gamepad (acts on the current pad):
  a b x y lb rb ls rs up down left right menu view nexus   tap a button
  hold <button> / release <button>                          keep a button down / let go
  lt / rt                                                   tap a trigger
  stick l|r <x> <y>                                         tilt a stick, -1 to 1 (up and right are positive)
  center                                                    let go of everything
  pad <1-4>                                                 plug in and control another pad
  unplug <1-4>                                              unplug a pad
Real mouse and keyboard (only games that support them react):
  key <letter, digit or code>                               tap a key
  mouse <dx> <dy>                                           move the mouse
  click [left|right|middle]                                 click
Other:
  keyframe   stats   help   quit";

#[derive(Debug, PartialEq)]
pub enum Command {
    Nothing,
    Help,
    Quit,
    Tap(u16),
    Hold(u16),
    Release(u16),
    Trigger { right: bool },
    Stick { right: bool, x: f64, y: f64 },
    Center,
    /// Zero-based pad slot.
    Pad(u8),
    Unplug(u8),
    /// Windows virtual-key code.
    Key(u8),
    MouseMove { x: i32, y: i32 },
    Click(u8),
    Keyframe,
    Stats,
}

fn button_named(name: &str) -> Option<u16> {
    Some(match name {
        "a" => button::A,
        "b" => button::B,
        "x" => button::X,
        "y" => button::Y,
        "lb" => button::LEFT_SHOULDER,
        "rb" => button::RIGHT_SHOULDER,
        "ls" => button::LEFT_THUMB,
        "rs" => button::RIGHT_THUMB,
        "up" => button::DPAD_UP,
        "down" => button::DPAD_DOWN,
        "left" => button::DPAD_LEFT,
        "right" => button::DPAD_RIGHT,
        "menu" | "start" => button::MENU,
        "view" | "back" | "select" => button::VIEW,
        "nexus" | "guide" | "xbox" | "home" => button::NEXUS,
        _ => return None,
    })
}

fn pad_slot(text: Option<&str>) -> Result<u8, String> {
    match text.and_then(|t| t.parse::<u8>().ok()) {
        Some(n @ 1..=4) => Ok(n - 1),
        _ => Err("give a pad number from 1 to 4".to_owned()),
    }
}

fn axis(text: Option<&str>) -> Result<f64, String> {
    match text.and_then(|t| t.parse::<f64>().ok()) {
        Some(v) if (-1.0..=1.0).contains(&v) => Ok(v),
        _ => Err("stick values go from -1 to 1, for example: stick l 0.5 -1".to_owned()),
    }
}

/// A letter or digit is its own virtual-key code; anything else must be a number.
fn key_code(text: &str) -> Option<u8> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_uppercase() as u8),
        _ => match text {
            "space" => Some(0x20),
            "enter" => Some(0x0d),
            "esc" | "escape" => Some(0x1b),
            "tab" => Some(0x09),
            "shift" => Some(0x10),
            "ctrl" => Some(0x11),
            _ => text.strip_prefix("0x").map_or_else(|| text.parse().ok(), |hex| u8::from_str_radix(hex, 16).ok()),
        },
    }
}

/// Turns one typed line into a command, or a message saying what was wrong.
pub fn parse(line: &str) -> Result<Command, String> {
    let lower = line.trim().to_ascii_lowercase();
    let mut words = lower.split_whitespace();
    let Some(first) = words.next() else { return Ok(Command::Nothing) };
    let command = match first {
        "help" | "?" => Command::Help,
        "quit" | "exit" | "q" => Command::Quit,
        "hold" | "release" => {
            let name = words.next().ok_or("which button? for example: hold a")?;
            let bits = button_named(name).ok_or_else(|| format!("unknown button \"{name}\""))?;
            if first == "hold" { Command::Hold(bits) } else { Command::Release(bits) }
        }
        "lt" => Command::Trigger { right: false },
        "rt" => Command::Trigger { right: true },
        "stick" => {
            let right = match words.next() {
                Some("l") | Some("left") => false,
                Some("r") | Some("right") => true,
                _ => return Err("which stick? for example: stick l 0.5 -1".to_owned()),
            };
            Command::Stick { right, x: axis(words.next())?, y: axis(words.next())? }
        }
        "center" | "centre" => Command::Center,
        "pad" => Command::Pad(pad_slot(words.next())?),
        "unplug" => Command::Unplug(pad_slot(words.next())?),
        "key" => {
            let text = words.next().ok_or("which key? for example: key w")?;
            Command::Key(key_code(text).ok_or_else(|| format!("unknown key \"{text}\""))?)
        }
        "mouse" => {
            let x = words.next().and_then(|t| t.parse().ok());
            let y = words.next().and_then(|t| t.parse().ok());
            match (x, y) {
                (Some(x), Some(y)) => Command::MouseMove { x, y },
                _ => return Err("give two whole numbers, for example: mouse 40 -10".to_owned()),
            }
        }
        "click" => match words.next() {
            None | Some("left") => Command::Click(mouse_button::LEFT),
            Some("right") => Command::Click(mouse_button::RIGHT),
            Some("middle") => Command::Click(mouse_button::MIDDLE),
            Some(other) => return Err(format!("unknown mouse button \"{other}\"")),
        },
        "keyframe" => Command::Keyframe,
        "stats" => Command::Stats,
        name => match button_named(name) {
            Some(bits) => Command::Tap(bits),
            None => return Err(format!("unknown command \"{name}\", type help for the list")),
        },
    };
    if let Some(extra) = words.next() {
        return Err(format!("did not expect \"{extra}\" there, type help for the list"));
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_tap() {
        assert_eq!(parse("a"), Ok(Command::Tap(button::A)));
        assert_eq!(parse("  NEXUS "), Ok(Command::Tap(button::NEXUS)));
        assert_eq!(parse("guide"), Ok(Command::Tap(button::NEXUS)));
        assert_eq!(parse("up"), Ok(Command::Tap(button::DPAD_UP)));
    }

    #[test]
    fn hold_and_release() {
        assert_eq!(parse("hold rb"), Ok(Command::Hold(button::RIGHT_SHOULDER)));
        assert_eq!(parse("release rb"), Ok(Command::Release(button::RIGHT_SHOULDER)));
        assert!(parse("hold").is_err());
        assert!(parse("hold banana").is_err());
    }

    #[test]
    fn sticks_and_triggers() {
        assert_eq!(parse("stick l 0.5 -1"), Ok(Command::Stick { right: false, x: 0.5, y: -1.0 }));
        assert_eq!(parse("stick right 0 1"), Ok(Command::Stick { right: true, x: 0.0, y: 1.0 }));
        assert!(parse("stick l 2 0").is_err());
        assert!(parse("stick l 0").is_err());
        assert!(parse("stick up 0 0").is_err());
        assert_eq!(parse("rt"), Ok(Command::Trigger { right: true }));
        assert_eq!(parse("center"), Ok(Command::Center));
    }

    #[test]
    fn pads_are_numbered_from_one() {
        assert_eq!(parse("pad 1"), Ok(Command::Pad(0)));
        assert_eq!(parse("pad 4"), Ok(Command::Pad(3)));
        assert_eq!(parse("unplug 2"), Ok(Command::Unplug(1)));
        assert!(parse("pad 0").is_err());
        assert!(parse("pad 5").is_err());
        assert!(parse("pad").is_err());
    }

    #[test]
    fn keys_and_mouse() {
        assert_eq!(parse("key w"), Ok(Command::Key(0x57)));
        assert_eq!(parse("key 5"), Ok(Command::Key(0x35)));
        assert_eq!(parse("key space"), Ok(Command::Key(0x20)));
        assert_eq!(parse("key 0x70"), Ok(Command::Key(0x70)));
        assert_eq!(parse("key 112"), Ok(Command::Key(112)));
        assert!(parse("key nope").is_err());
        assert_eq!(parse("mouse 40 -10"), Ok(Command::MouseMove { x: 40, y: -10 }));
        assert!(parse("mouse 40").is_err());
        assert_eq!(parse("click"), Ok(Command::Click(mouse_button::LEFT)));
        assert_eq!(parse("click right"), Ok(Command::Click(mouse_button::RIGHT)));
        assert!(parse("click sideways").is_err());
    }

    #[test]
    fn other_lines() {
        assert_eq!(parse(""), Ok(Command::Nothing));
        assert_eq!(parse("   "), Ok(Command::Nothing));
        assert_eq!(parse("quit"), Ok(Command::Quit));
        assert_eq!(parse("help"), Ok(Command::Help));
        assert_eq!(parse("stats"), Ok(Command::Stats));
        assert_eq!(parse("keyframe"), Ok(Command::Keyframe));
        assert!(parse("dance").is_err());
        assert!(parse("a b").is_err());
    }
}
