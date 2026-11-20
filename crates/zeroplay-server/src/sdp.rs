//! Small SDP helpers for the browser side.

use std::collections::HashMap;
use std::net::IpAddr;

use anyhow::{bail, Result};

/// The codecs we will send to a browser, with the payload types the browser chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserCodecs {
    pub video_pt: u8,
    /// The browser's own fmtp line for that H.264 payload type.
    pub video_fmtp: String,
    pub audio_pt: u8,
}

/// Picks the H.264 and Opus payload types from a browser's offer.
///
/// The console sends H.264 with packetization mode 1, mostly main profile, so
/// main profile is preferred, then constrained baseline, then any H.264 with
/// packetization mode 1.
pub fn pick_codecs(offer: &str) -> Result<BrowserCodecs> {
    let mut rtpmap: Vec<(u8, String)> = Vec::new();
    let mut fmtp: HashMap<u8, String> = HashMap::new();
    for line in offer.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some((pt, name)) = rest.split_once(' ') {
                if let Ok(pt) = pt.parse() {
                    rtpmap.push((pt, name.to_ascii_lowercase()));
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            if let Some((pt, params)) = rest.split_once(' ') {
                if let Ok(pt) = pt.parse() {
                    fmtp.insert(pt, params.to_owned());
                }
            }
        }
    }

    let audio_pt = rtpmap
        .iter()
        .find(|(_, name)| name.starts_with("opus/48000"))
        .map(|(pt, _)| *pt);

    let h264: Vec<(u8, String)> = rtpmap
        .iter()
        .filter(|(_, name)| name.starts_with("h264/90000"))
        .map(|(pt, _)| (*pt, fmtp.get(pt).cloned().unwrap_or_default()))
        .filter(|(_, params)| params.to_ascii_lowercase().contains("packetization-mode=1"))
        .collect();
    let profile = |params: &str| -> String {
        params
            .split(';')
            .find_map(|p| p.trim().to_ascii_lowercase().strip_prefix("profile-level-id=").map(str::to_owned))
            .unwrap_or_default()
    };
    let video = ["4d", "42e0", "42"]
        .iter()
        .find_map(|prefix| h264.iter().find(|(_, params)| profile(params).starts_with(prefix)))
        .or_else(|| h264.first());

    match (video, audio_pt) {
        (Some((video_pt, video_fmtp)), Some(audio_pt)) => Ok(BrowserCodecs {
            video_pt: *video_pt,
            video_fmtp: video_fmtp.clone(),
            audio_pt,
        }),
        (None, _) => bail!("this browser offered no H.264 video it can play"),
        (_, None) => bail!("this browser offered no Opus audio"),
    }
}

/// Adds extra host candidates (for example the NAS's LAN address or a tunnel
/// address) to every media section of an answer.
pub fn add_candidates(sdp: &str, extra: &[(IpAddr, u16)]) -> String {
    if extra.is_empty() {
        return sdp.to_owned();
    }
    let lines: Vec<&str> = sdp.split("\r\n").filter(|l| !l.is_empty()).collect();
    let candidate_lines: Vec<String> = extra
        .iter()
        .enumerate()
        .map(|(i, (ip, port))| {
            // High priority so the browser tries these first.
            let priority = 2_130_706_431u32 - i as u32;
            format!("a=candidate:zp{i} 1 udp {priority} {ip} {port} typ host")
        })
        .collect();

    // Split into the session part and one part per m= section.
    let mut sections: Vec<Vec<&str>> = vec![Vec::new()];
    for line in lines {
        if line.starts_with("m=") {
            sections.push(Vec::new());
        }
        sections.last_mut().unwrap().push(line);
    }

    let mut out: Vec<String> = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        let is_media = index > 0;
        let insert_at = if !is_media {
            None
        } else if let Some(pos) = section.iter().position(|l| *l == "a=end-of-candidates") {
            Some(pos)
        } else if let Some(pos) = section.iter().rposition(|l| l.starts_with("a=candidate:")) {
            Some(pos + 1)
        } else {
            Some(section.len())
        };
        for (i, line) in section.iter().enumerate() {
            if Some(i) == insert_at {
                out.extend(candidate_lines.iter().cloned());
            }
            out.push((*line).to_owned());
        }
        if insert_at == Some(section.len()) {
            out.extend(candidate_lines.iter().cloned());
        }
    }
    let mut text = out.join("\r\n");
    text.push_str("\r\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME_OFFER: &str = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111 63\r\n\
a=mid:0\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 minptime=10;useinbandfec=1\r\n\
a=rtpmap:63 red/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 102 104 106 108\r\n\
a=mid:1\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:102 H264/90000\r\n\
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
a=rtpmap:104 H264/90000\r\n\
a=fmtp:104 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=4d001f\r\n\
a=rtpmap:106 H264/90000\r\n\
a=fmtp:106 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
a=rtpmap:108 H264/90000\r\n\
a=fmtp:108 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f\r\n";

    #[test]
    fn main_profile_with_mode_1_wins() {
        let codecs = pick_codecs(CHROME_OFFER).unwrap();
        assert_eq!(codecs.video_pt, 108);
        assert_eq!(codecs.video_fmtp, "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f");
        assert_eq!(codecs.audio_pt, 111);
    }

    #[test]
    fn baseline_is_used_when_main_is_missing() {
        let offer = CHROME_OFFER.replace("a=rtpmap:108 H264/90000\r\n", "");
        assert_eq!(pick_codecs(&offer).unwrap().video_pt, 106);
    }

    #[test]
    fn no_h264_is_an_error() {
        let offer: String = CHROME_OFFER.lines().filter(|l| !l.contains("H264")).map(|l| format!("{l}\r\n")).collect();
        assert!(pick_codecs(&offer).unwrap_err().to_string().contains("H.264"));
    }

    #[test]
    fn candidates_go_into_every_media_section() {
        let answer = "v=0\r\ns=-\r\nm=audio 9 X 111\r\na=mid:0\r\na=candidate:1 1 udp 1 10.0.0.2 50000 typ host\r\na=end-of-candidates\r\nm=video 9 X 108\r\na=mid:1\r\n";
        let out = add_candidates(answer, &[("192.168.1.10".parse().unwrap(), 50000)]);
        let lines: Vec<&str> = out.lines().collect();
        let first = lines.iter().position(|l| l.contains("192.168.1.10")).unwrap();
        assert_eq!(lines[first - 1], "a=candidate:1 1 udp 1 10.0.0.2 50000 typ host");
        assert_eq!(lines[first + 1], "a=end-of-candidates");
        assert_eq!(out.matches("192.168.1.10 50000 typ host").count(), 2);
        assert!(lines.last().unwrap().contains("192.168.1.10"));
        assert!(out.ends_with("\r\n"));
    }

    #[test]
    fn no_extra_candidates_leaves_the_answer_alone() {
        assert_eq!(add_candidates("v=0\r\n", &[]), "v=0\r\n");
    }
}
