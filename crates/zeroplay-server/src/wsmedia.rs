//! Video and sound inside the page's own connection (the WebSocket), for
//! players outside the home: one tunnel carries everything.
//!
//! The console's H.264 RTP packets are put back together into whole frames
//! (RFC 6184: single NAL units, STAP-A and FU-A), in Annex B form, which the
//! browser's VideoDecoder takes as is. Opus packets go as they are.
//!
//! Each message is binary: kind (1 video, 2 audio), flags (1 = key frame),
//! RTP timestamp (4 bytes, big endian), then the data.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use zeroplay_xbox::session::MediaPacket;

use crate::app::App;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// One whole video frame.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub data: Vec<u8>,
    pub key: bool,
    pub timestamp: u32,
}

/// Turns H.264 RTP packets back into frames.
#[derive(Default)]
pub struct Depacketizer {
    timestamp: Option<u32>,
    frame: Vec<u8>,
    fragment: Option<Vec<u8>>,
    last_sequence: Option<u16>,
    /// A packet of this frame went missing: it is dropped.
    broken: bool,
}

fn is_key(annex_b: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < annex_b.len() {
        if annex_b[i..i + 4] == START_CODE {
            let kind = annex_b[i + 4] & 0x1f;
            if kind == 5 || kind == 7 {
                return true;
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    false
}

impl Depacketizer {
    fn finish(&mut self) -> Option<Frame> {
        let data = std::mem::take(&mut self.frame);
        let timestamp = self.timestamp?;
        let broken = std::mem::replace(&mut self.broken, false);
        self.fragment = None;
        if broken || data.is_empty() {
            return None;
        }
        Some(Frame {
            key: is_key(&data),
            data,
            timestamp,
        })
    }

    /// Adds one packet. Returns the frames it completes (at most two: the
    /// previous one, when a new timestamp starts without a marker, and this one).
    pub fn push(
        &mut self,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Vec<Frame> {
        let mut done = Vec::new();
        if let Some(last) = self.last_sequence {
            if sequence != last.wrapping_add(1) {
                self.broken = true;
                self.fragment = None;
            }
        }
        self.last_sequence = Some(sequence);
        if self.timestamp.is_some_and(|t| t != timestamp) && !self.frame.is_empty() {
            if let Some(frame) = self.finish() {
                done.push(frame);
            }
        }
        self.timestamp = Some(timestamp);

        if let Some(&first) = payload.first() {
            match first & 0x1f {
                1..=23 => {
                    self.frame.extend_from_slice(&START_CODE);
                    self.frame.extend_from_slice(payload);
                }
                24 => {
                    // STAP-A: several NAL units, each after a 2-byte size.
                    let mut rest = &payload[1..];
                    while rest.len() >= 2 {
                        let size = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                        rest = &rest[2..];
                        if size == 0 || size > rest.len() {
                            self.broken = true;
                            break;
                        }
                        self.frame.extend_from_slice(&START_CODE);
                        self.frame.extend_from_slice(&rest[..size]);
                        rest = &rest[size..];
                    }
                }
                28 if payload.len() >= 2 => {
                    // FU-A: one NAL unit in pieces.
                    let header = payload[1];
                    if header & 0x80 != 0 {
                        let mut nal = vec![(first & 0xe0) | (header & 0x1f)];
                        nal.extend_from_slice(&payload[2..]);
                        self.fragment = Some(nal);
                    } else if let Some(nal) = self.fragment.as_mut() {
                        nal.extend_from_slice(&payload[2..]);
                    } else {
                        self.broken = true;
                    }
                    if header & 0x40 != 0 {
                        if let Some(nal) = self.fragment.take() {
                            self.frame.extend_from_slice(&START_CODE);
                            self.frame.extend_from_slice(&nal);
                        }
                    }
                }
                _ => {}
            }
        }
        if marker {
            if let Some(frame) = self.finish() {
                done.push(frame);
            }
        }
        done
    }
}

fn message(kind: u8, key: bool, timestamp: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + data.len());
    out.push(kind);
    out.push(key as u8);
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// Sends the console's video and sound to one player through `to_page`.
/// Starts with a key frame, and when the player's connection cannot keep up,
/// drops frames until the next key frame instead of falling behind.
pub fn spawn(
    app: Arc<App>,
    mut media: broadcast::Receiver<MediaPacket>,
    to_page: mpsc::Sender<Vec<u8>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut depacketizer = Depacketizer::default();
        let mut need_key = true;
        app.request_keyframe();
        loop {
            match media.recv().await {
                Ok(MediaPacket::Video(packet)) => {
                    let h = &packet.header;
                    for frame in
                        depacketizer.push(h.sequence_number, h.timestamp, h.marker, &packet.payload)
                    {
                        if need_key && !frame.key {
                            continue;
                        }
                        match to_page.try_send(message(1, frame.key, frame.timestamp, &frame.data))
                        {
                            Ok(()) => need_key = false,
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                need_key = true;
                                app.request_keyframe();
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => return,
                        }
                    }
                }
                Ok(MediaPacket::Audio(packet)) => {
                    if let Err(mpsc::error::TrySendError::Closed(_)) = to_page.try_send(message(
                        2,
                        false,
                        packet.header.timestamp,
                        &packet.payload,
                    )) {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    need_key = true;
                    depacketizer = Depacketizer::default();
                    app.request_keyframe();
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x4d, 0x00, 0x2a, 0x95];
    const PPS: &[u8] = &[0x68, 0xee, 0x3c, 0x80];

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| START_CODE.iter().chain(n.iter()).copied())
            .collect()
    }

    fn stap_a(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![0x18];
        for nal in nals {
            out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            out.extend_from_slice(nal);
        }
        out
    }

    #[test]
    fn single_nal_frames() {
        let mut d = Depacketizer::default();
        let frames = d.push(1, 3000, true, &[0x41, 1, 2, 3]);
        assert_eq!(
            frames,
            vec![Frame {
                data: annex_b(&[&[0x41, 1, 2, 3]]),
                key: false,
                timestamp: 3000
            }]
        );
    }

    #[test]
    fn a_key_frame_from_stap_a_and_fu_a() {
        let mut d = Depacketizer::default();
        assert!(d.push(10, 9000, false, &stap_a(&[SPS, PPS])).is_empty());
        // IDR slice (type 5) split in three FU-A pieces: 0x65 = nri 3, type 5.
        assert!(d
            .push(11, 9000, false, &[0x7c, 0x85, 0xaa, 0xbb])
            .is_empty());
        assert!(d.push(12, 9000, false, &[0x7c, 0x05, 0xcc]).is_empty());
        let frames = d.push(13, 9000, true, &[0x7c, 0x45, 0xdd]);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].key);
        assert_eq!(
            frames[0].data,
            annex_b(&[SPS, PPS, &[0x65, 0xaa, 0xbb, 0xcc, 0xdd]])
        );
    }

    #[test]
    fn a_lost_packet_drops_only_that_frame() {
        let mut d = Depacketizer::default();
        assert!(d.push(1, 100, false, &[0x7c, 0x85, 1]).is_empty());
        // Packet 2 is lost.
        assert!(d.push(3, 100, true, &[0x7c, 0x45, 3]).is_empty());
        let next = d.push(4, 200, true, &[0x41, 9]);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].timestamp, 200);
    }

    #[test]
    fn a_new_timestamp_finishes_a_frame_without_a_marker() {
        let mut d = Depacketizer::default();
        assert!(d.push(1, 100, false, &[0x41, 1]).is_empty());
        let frames = d.push(2, 200, true, &[0x41, 2]);
        assert_eq!(
            frames.iter().map(|f| f.timestamp).collect::<Vec<_>>(),
            vec![100, 200]
        );
    }

    #[test]
    fn messages_have_a_six_byte_header() {
        assert_eq!(
            message(1, true, 0x01020304, &[9, 9]),
            vec![1, 1, 1, 2, 3, 4, 9, 9]
        );
    }
}
