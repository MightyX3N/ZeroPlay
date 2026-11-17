//! Input packets for the Xbox streaming "input" data channel.
//!
//! Ported from Greenlight's `channel/input/packet.ts` (MIT, UnknownSKL).
//! One packet has a 14-byte header followed by one section per kind of input
//! that is present, always in this order: frame metadata, gamepads, mouse,
//! keyboard.

/// Report type bits in the packet header.
pub mod report {
    pub const METADATA: u16 = 1;
    pub const GAMEPAD: u16 = 2;
    pub const CLIENT_METADATA: u16 = 8;
    pub const SERVER_METADATA: u16 = 16;
    pub const MOUSE: u16 = 32;
    pub const KEYBOARD: u16 = 64;
    pub const VIBRATION: u16 = 128;
}

/// Gamepad button bits, as the console expects them.
pub mod button {
    pub const NEXUS: u16 = 2;
    pub const MENU: u16 = 4;
    pub const VIEW: u16 = 8;
    pub const A: u16 = 16;
    pub const B: u16 = 32;
    pub const X: u16 = 64;
    pub const Y: u16 = 128;
    pub const DPAD_UP: u16 = 256;
    pub const DPAD_DOWN: u16 = 512;
    pub const DPAD_LEFT: u16 = 1024;
    pub const DPAD_RIGHT: u16 = 2048;
    pub const LEFT_SHOULDER: u16 = 4096;
    pub const RIGHT_SHOULDER: u16 = 8192;
    pub const LEFT_THUMB: u16 = 16384;
    pub const RIGHT_THUMB: u16 = 32768;
}

/// Mouse button bits (same as the browser's `MouseEvent.buttons`).
pub mod mouse_button {
    pub const LEFT: u8 = 1;
    pub const RIGHT: u8 = 2;
    pub const MIDDLE: u8 = 4;
}

/// The standard gamepad every input device is turned into.
///
/// Sticks are -1.0..=1.0 with **up and right positive**. Triggers are 0.0..=1.0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GamepadState {
    /// Player slot on the console, 0..=3.
    pub index: u8,
    /// Bits from [`button`].
    pub buttons: u16,
    pub left_x: f64,
    pub left_y: f64,
    pub right_x: f64,
    pub right_y: f64,
    pub left_trigger: f64,
    pub right_trigger: f64,
}

impl GamepadState {
    pub fn new(index: u8) -> Self {
        Self { index, ..Self::default() }
    }
}

/// One mouse report for games that take a real mouse.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MouseFrame {
    pub x: i32,
    pub y: i32,
    pub wheel_x: i32,
    pub wheel_y: i32,
    /// Bits from [`mouse_button`].
    pub buttons: u8,
    /// `false` = relative movement, `true` = absolute position.
    pub absolute: bool,
}

/// One key going down or up. `key_code` is a Windows virtual-key code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyboardFrame {
    pub pressed: bool,
    pub key_code: u8,
}

/// Frame timing the client reports back to the console.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MetadataFrame {
    pub server_data_key: u32,
    pub first_frame_packet_arrival_time_ms: u32,
    pub frame_submitted_time_ms: u32,
    pub frame_decoded_time_ms: u32,
    pub frame_rendered_time_ms: u32,
}

const HEADER_LEN: usize = 14;

fn header(report_type: u16, sequence: u32, now_ms: f64, capacity: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(&report_type.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&now_ms.to_le_bytes());
    out
}

/// The first packet on the input channel: tells the console how many touch points we have.
pub fn client_metadata_packet(sequence: u32, now_ms: f64, max_touch_points: u8) -> Vec<u8> {
    let mut out = header(report::CLIENT_METADATA, sequence, now_ms, HEADER_LEN + 1);
    out.push(max_touch_points);
    out
}

/// A normal input packet. Returns `None` when there is nothing to send.
pub fn input_packet(
    sequence: u32,
    now_ms: f64,
    metadata: &[MetadataFrame],
    gamepads: &[GamepadState],
    mouse: &[MouseFrame],
    keyboard: &[KeyboardFrame],
) -> Option<Vec<u8>> {
    let mut report_type = 0u16;
    let mut size = HEADER_LEN;
    if !metadata.is_empty() {
        report_type |= report::METADATA;
        size += 1 + 28 * metadata.len();
    }
    if !gamepads.is_empty() {
        report_type |= report::GAMEPAD;
        size += 1 + 23 * gamepads.len();
    }
    if !mouse.is_empty() {
        report_type |= report::MOUSE;
        size += 1 + 18 * mouse.len();
    }
    if !keyboard.is_empty() {
        report_type |= report::KEYBOARD;
        size += 1 + 3 * keyboard.len();
    }
    if report_type == 0 {
        return None;
    }

    let mut out = header(report_type, sequence, now_ms, size);

    if !metadata.is_empty() {
        out.push(metadata.len() as u8);
        for frame in metadata {
            out.extend_from_slice(&frame.server_data_key.to_le_bytes());
            out.extend_from_slice(&frame.first_frame_packet_arrival_time_ms.to_le_bytes());
            out.extend_from_slice(&frame.frame_submitted_time_ms.to_le_bytes());
            out.extend_from_slice(&frame.frame_decoded_time_ms.to_le_bytes());
            out.extend_from_slice(&frame.frame_rendered_time_ms.to_le_bytes());
            // Two "now" stamps, truncated to whole milliseconds like the original.
            let now = now_ms as u32;
            out.extend_from_slice(&now.to_le_bytes());
            out.extend_from_slice(&now.to_le_bytes());
        }
    }

    if !gamepads.is_empty() {
        out.push(gamepads.len() as u8);
        for pad in gamepads {
            out.push(pad.index);
            out.extend_from_slice(&pad.buttons.to_le_bytes());
            out.extend_from_slice(&normalize_axis(pad.left_x).to_le_bytes());
            out.extend_from_slice(&normalize_axis(pad.left_y).to_le_bytes());
            out.extend_from_slice(&normalize_axis(pad.right_x).to_le_bytes());
            out.extend_from_slice(&normalize_axis(pad.right_y).to_le_bytes());
            out.extend_from_slice(&normalize_trigger(pad.left_trigger).to_le_bytes());
            out.extend_from_slice(&normalize_trigger(pad.right_trigger).to_le_bytes());
            // "Physicality" fields. The second one really is big-endian in the original.
            out.extend_from_slice(&1u32.to_le_bytes());
            out.extend_from_slice(&1u32.to_be_bytes());
        }
    }

    if !mouse.is_empty() {
        out.push(mouse.len() as u8);
        for frame in mouse {
            out.extend_from_slice(&frame.x.to_le_bytes());
            out.extend_from_slice(&frame.y.to_le_bytes());
            out.extend_from_slice(&frame.wheel_x.to_le_bytes());
            out.extend_from_slice(&frame.wheel_y.to_le_bytes());
            out.push(frame.buttons);
            out.push(frame.absolute as u8);
        }
    }

    if !keyboard.is_empty() {
        out.push(keyboard.len() as u8);
        for frame in keyboard {
            out.push(2); // 2 = virtual-key code
            out.push(frame.pressed as u8);
            out.push(frame.key_code);
        }
    }

    debug_assert_eq!(out.len(), size);
    Some(out)
}

/// -1.0..=1.0 to -32767..=32767, rounding like JavaScript's `Math.round`.
pub fn normalize_axis(value: f64) -> i16 {
    if value.is_nan() {
        return 0;
    }
    let scaled = (value * 32767.0 + 0.5).floor();
    scaled.clamp(-32767.0, 32767.0) as i16
}

/// 0.0..=1.0 to 0..=65535, truncating like the original.
pub fn normalize_trigger(value: f64) -> u16 {
    if value.is_nan() || value <= 0.0 {
        0
    } else if value >= 1.0 {
        65535
    } else {
        (value * 65535.0) as u16
    }
}

/// A rumble command from the console.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vibration {
    pub gamepad_index: u8,
    /// Motor strengths, 0.0..=1.0.
    pub left_motor: f32,
    pub right_motor: f32,
    pub left_trigger_motor: f32,
    pub right_trigger_motor: f32,
    pub duration_ms: u16,
    pub delay_ms: u16,
    pub repeat: u8,
}

/// What the console can send us on the input channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ServerReport {
    Vibration(Vibration),
    /// The size of the video the console is sending.
    VideoSize { width: u32, height: u32 },
}

/// Parse a message received on the input channel. Unknown reports give `None`.
pub fn parse_server_report(data: &[u8]) -> Option<ServerReport> {
    let report_type = *data.first()? as u16;
    if report_type == report::VIBRATION {
        if data.len() < 13 {
            return None;
        }
        Some(ServerReport::Vibration(Vibration {
            gamepad_index: data[3],
            left_motor: data[4] as f32 / 100.0,
            right_motor: data[5] as f32 / 100.0,
            left_trigger_motor: data[6] as f32 / 100.0,
            right_trigger_motor: data[7] as f32 / 100.0,
            duration_ms: u16::from_le_bytes([data[8], data[9]]),
            delay_ms: u16::from_le_bytes([data[10], data[11]]),
            repeat: data[12],
        }))
    } else if report_type == report::SERVER_METADATA {
        if data.len() < 10 {
            return None;
        }
        let height = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);
        let width = u32::from_le_bytes([data[6], data[7], data[8], data[9]]);
        Some(ServerReport::VideoSize { width, height })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected bytes below are worked out by hand from Greenlight's packet.ts.
    // 1.5 as a little-endian f64 is 00 00 00 00 00 00 f8 3f.
    const NOW: f64 = 1.5;
    const NOW_BYTES: [u8; 8] = [0, 0, 0, 0, 0, 0, 0xf8, 0x3f];

    fn expect_header(report_type: u16, sequence: u32) -> Vec<u8> {
        let mut v = vec![];
        v.extend_from_slice(&report_type.to_le_bytes());
        v.extend_from_slice(&sequence.to_le_bytes());
        v.extend_from_slice(&NOW_BYTES);
        v
    }

    #[test]
    fn client_metadata_is_15_bytes() {
        let packet = client_metadata_packet(0, NOW, 1);
        let mut expected = expect_header(8, 0);
        expected.push(1);
        assert_eq!(packet, expected);
        assert_eq!(packet.len(), 15);
    }

    #[test]
    fn empty_input_sends_nothing() {
        assert_eq!(input_packet(1, NOW, &[], &[], &[], &[]), None);
    }

    #[test]
    fn idle_gamepad() {
        let packet = input_packet(1, NOW, &[], &[GamepadState::new(0)], &[], &[]).unwrap();
        let mut expected = expect_header(2, 1);
        expected.push(1); // one gamepad
        expected.push(0); // index
        expected.extend_from_slice(&[0, 0]); // buttons
        expected.extend_from_slice(&[0; 8]); // four axes
        expected.extend_from_slice(&[0; 4]); // two triggers
        expected.extend_from_slice(&[1, 0, 0, 0]); // little-endian 1
        expected.extend_from_slice(&[0, 0, 0, 1]); // big-endian 1
        assert_eq!(packet, expected);
        assert_eq!(packet.len(), 14 + 1 + 23);
    }

    #[test]
    fn gamepad_buttons_sticks_and_triggers() {
        let pad = GamepadState {
            index: 2,
            buttons: button::A | button::NEXUS | button::DPAD_LEFT | button::RIGHT_THUMB,
            left_x: 0.5,
            left_y: 0.25,
            right_x: -1.0,
            right_y: -1.0,
            left_trigger: 0.3,
            right_trigger: 1.0,
        };
        let packet = input_packet(7, NOW, &[], &[pad], &[], &[]).unwrap();
        let mut expected = expect_header(2, 7);
        expected.push(1);
        expected.push(2);
        // 16 + 2 + 1024 + 32768 = 33810 = 0x8412
        expected.extend_from_slice(&[0x12, 0x84]);
        // round(0.5 * 32767) = round(16383.5) = 16384 = 0x4000
        expected.extend_from_slice(&[0x00, 0x40]);
        // round(0.25 * 32767) = round(8191.75) = 8192 = 0x2000
        expected.extend_from_slice(&[0x00, 0x20]);
        // -32767 = 0x8001
        expected.extend_from_slice(&[0x01, 0x80]);
        expected.extend_from_slice(&[0x01, 0x80]);
        // trunc(0.3 * 65535) = trunc(19660.5) = 19660 = 0x4ccc
        expected.extend_from_slice(&[0xcc, 0x4c]);
        expected.extend_from_slice(&[0xff, 0xff]);
        expected.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(packet, expected);
    }

    #[test]
    fn all_buttons_mask() {
        let all = button::NEXUS
            | button::MENU
            | button::VIEW
            | button::A
            | button::B
            | button::X
            | button::Y
            | button::DPAD_UP
            | button::DPAD_DOWN
            | button::DPAD_LEFT
            | button::DPAD_RIGHT
            | button::LEFT_SHOULDER
            | button::RIGHT_SHOULDER
            | button::LEFT_THUMB
            | button::RIGHT_THUMB;
        assert_eq!(all, 0xfffe);
    }

    #[test]
    fn several_gamepads_in_one_packet() {
        let pads = [GamepadState::new(0), GamepadState::new(1), GamepadState::new(3)];
        let packet = input_packet(300, NOW, &[], &pads, &[], &[]).unwrap();
        assert_eq!(packet.len(), 14 + 1 + 23 * 3);
        assert_eq!(&packet[0..2], &[2, 0]);
        assert_eq!(&packet[2..6], &300u32.to_le_bytes());
        assert_eq!(packet[14], 3);
        assert_eq!(packet[15], 0);
        assert_eq!(packet[15 + 23], 1);
        assert_eq!(packet[15 + 46], 3);
    }

    #[test]
    fn mouse_and_keyboard_sections() {
        let mouse = MouseFrame { x: 5, y: -3, wheel_x: 0, wheel_y: -120, buttons: 1, absolute: false };
        let keys = [
            KeyboardFrame { pressed: true, key_code: 87 },
            KeyboardFrame { pressed: false, key_code: 32 },
        ];
        let packet = input_packet(11, NOW, &[], &[], &[mouse], &keys).unwrap();
        let mut expected = expect_header(32 | 64, 11);
        expected.push(1);
        expected.extend_from_slice(&[5, 0, 0, 0]);
        expected.extend_from_slice(&[0xfd, 0xff, 0xff, 0xff]); // -3
        expected.extend_from_slice(&[0, 0, 0, 0]);
        expected.extend_from_slice(&[0x88, 0xff, 0xff, 0xff]); // -120
        expected.extend_from_slice(&[1, 0]);
        expected.push(2);
        expected.extend_from_slice(&[2, 1, 87]);
        expected.extend_from_slice(&[2, 0, 32]);
        assert_eq!(packet, expected);
    }

    #[test]
    fn sections_come_in_fixed_order() {
        let meta = MetadataFrame { server_data_key: 42, ..Default::default() };
        let packet = input_packet(
            13,
            NOW,
            &[meta],
            &[GamepadState::new(0)],
            &[MouseFrame::default()],
            &[KeyboardFrame { pressed: true, key_code: 13 }],
        )
        .unwrap();
        assert_eq!(u16::from_le_bytes([packet[0], packet[1]]), 1 | 2 | 32 | 64);
        assert_eq!(packet.len(), 14 + (1 + 28) + (1 + 23) + (1 + 18) + (1 + 3));
        assert_eq!(packet[14], 1); // metadata count
        assert_eq!(&packet[15..19], &42u32.to_le_bytes());
        assert_eq!(packet[14 + 29], 1); // gamepad count
        assert_eq!(packet[14 + 29 + 24], 1); // mouse count
        assert_eq!(packet[14 + 29 + 24 + 19], 1); // keyboard count
    }

    #[test]
    fn axis_rounding_and_clamping() {
        assert_eq!(normalize_axis(0.0), 0);
        assert_eq!(normalize_axis(1.0), 32767);
        assert_eq!(normalize_axis(-1.0), -32767);
        assert_eq!(normalize_axis(1.5), 32767);
        assert_eq!(normalize_axis(-1.5), -32767);
        assert_eq!(normalize_axis(0.5), 16384); // 16383.5 rounds up
        assert_eq!(normalize_axis(-0.5), -16383); // Math.round(-16383.5) is -16383
        assert_eq!(normalize_axis(f64::NAN), 0);
    }

    #[test]
    fn trigger_truncation() {
        assert_eq!(normalize_trigger(-1.0), 0);
        assert_eq!(normalize_trigger(0.0), 0);
        assert_eq!(normalize_trigger(0.5), 32767);
        assert_eq!(normalize_trigger(1.0), 65535);
        assert_eq!(normalize_trigger(2.0), 65535);
        assert_eq!(normalize_trigger(f64::NAN), 0);
    }

    #[test]
    fn parses_vibration() {
        let data = [128, 0, 0, 1, 50, 100, 0, 25, 0xf4, 0x01, 0x0a, 0x00, 3];
        assert_eq!(
            parse_server_report(&data),
            Some(ServerReport::Vibration(Vibration {
                gamepad_index: 1,
                left_motor: 0.5,
                right_motor: 1.0,
                left_trigger_motor: 0.0,
                right_trigger_motor: 0.25,
                duration_ms: 500,
                delay_ms: 10,
                repeat: 3,
            }))
        );
    }

    #[test]
    fn parses_video_size() {
        let mut data = vec![16, 0];
        data.extend_from_slice(&1080u32.to_le_bytes());
        data.extend_from_slice(&1920u32.to_le_bytes());
        assert_eq!(parse_server_report(&data), Some(ServerReport::VideoSize { width: 1920, height: 1080 }));
    }

    #[test]
    fn short_or_unknown_reports_are_ignored() {
        assert_eq!(parse_server_report(&[]), None);
        assert_eq!(parse_server_report(&[128, 0, 0]), None);
        assert_eq!(parse_server_report(&[99, 0, 0, 0]), None);
    }
}
