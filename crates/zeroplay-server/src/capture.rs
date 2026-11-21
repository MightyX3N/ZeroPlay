//! Picture (and optionally sound) from a capture card instead of the Xbox's
//! remote play video. Remote play stays connected for the controllers.
//!
//! Capture cards that act like a webcam (UVC) show up as /dev/videoN. ffmpeg
//! reads the card, encodes H.264 (on the GPU when there is one, else on the
//! processor) and Opus, and sends RTP to two local ports. Those packets go into
//! the same channel as the console's, so every player path works unchanged.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rtc::shared::marshal::Unmarshal;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::net::UdpSocket;
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use zeroplay_xbox::session::MediaPacket;

use crate::access::Quality;

/// The admin's choice, saved with the other settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureSettings {
    /// /dev/videoN; `None` = the Xbox's own remote play picture.
    pub device: Option<String>,
    /// ALSA card id for sound; `None` = the Xbox's remote play sound.
    pub audio: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct VideoDevice {
    pub path: String,
    pub name: String,
    /// The container can open it.
    pub reachable: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

/// Capture cards (and webcams) the container can see. Each card has one
/// capture node (index 0); the others carry metadata.
pub fn video_devices() -> Vec<VideoDevice> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let node = entry.file_name().to_string_lossy().to_string();
        let index = std::fs::read_to_string(dir.join("index")).unwrap_or_default();
        if index.trim() != "0" && !index.trim().is_empty() {
            continue;
        }
        let name = std::fs::read_to_string(dir.join("name"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let path = format!("/dev/{node}");
        ensure_node(&path, &dir.join("dev").to_string_lossy());
        let reachable = std::fs::File::open(&path).is_ok();
        out.push(VideoDevice {
            reachable,
            name: if name.is_empty() { node } else { name },
            path,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Sound cards from /proc/asound/cards (" 1 [S4K ]: USB-Audio - Cam Link 4K").
pub fn audio_devices() -> Vec<AudioDevice> {
    parse_asound_cards(&std::fs::read_to_string("/proc/asound/cards").unwrap_or_default())
}

fn parse_asound_cards(text: &str) -> Vec<AudioDevice> {
    text.lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once('[')?;
            let (id, rest) = rest.split_once(']')?;
            let name = rest.split_once(" - ").map(|(_, n)| n).unwrap_or(rest);
            let name = name.trim_start_matches(':').trim();
            Some(AudioDevice {
                id: id.trim().to_owned(),
                name: name.to_owned(),
            })
        })
        .collect()
}

/// The sound card that belongs to a capture card, by a shared word in the names.
pub fn matching_audio(video_name: &str) -> Option<AudioDevice> {
    let words: Vec<String> = video_name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(str::to_lowercase)
        .collect();
    audio_devices().into_iter().find(|a| {
        let name = a.name.to_lowercase();
        words.iter().any(|w| name.contains(w.as_str()))
    })
}

/// A GPU ffmpeg can encode on.
pub fn gpu() -> Option<&'static str> {
    let path = "/dev/dri/renderD128";
    ensure_node(path, "/sys/class/drm/renderD128/dev");
    (Path::new(path).exists() && std::fs::File::open(path).is_ok()).then_some(path)
}

/// Makes a device node the container lacks, from the kernel's numbers in
/// /sys (the NAS shows the card in /sys even when /dev was not shared).
fn ensure_node(path: &str, sys_dev: &str) {
    if Path::new(path).exists() {
        return;
    }
    let Ok(numbers) = std::fs::read_to_string(sys_dev) else {
        return;
    };
    let Some((major, minor)) = numbers.trim().split_once(':') else {
        return;
    };
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let made = std::process::Command::new("mknod")
        .args([path, "c", major, minor])
        .output()
        .is_ok_and(|o| o.status.success());
    if made {
        println!("[capture] made {path} ({major}:{minor})");
    }
}

/// The nodes for a capture card and the sound cards.
fn ensure_nodes(device: &str) {
    if let Some(name) = device.strip_prefix("/dev/") {
        ensure_node(device, &format!("/sys/class/video4linux/{name}/dev"));
    }
    if let Ok(entries) = std::fs::read_dir("/sys/class/sound") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("control") || name.starts_with("pcm") {
                ensure_node(
                    &format!("/dev/snd/{name}"),
                    &format!("/sys/class/sound/{name}/dev"),
                );
            }
        }
    }
}

/// Checks the container may open the card, with a message saying what to do.
fn check_access(device: &str) -> Result<()> {
    ensure_nodes(device);
    match std::fs::File::open(device) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "The NAS sees the capture card, but ZeroPlay's container cannot reach {device}. Add the capture card lines from the compose file (/dev:/dev and device_cgroup_rules) to the container and recreate it."
        ),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(1) => bail!(
            "The container is not allowed to use {device}. Add the device_cgroup_rules lines from the compose file to the container and recreate it."
        ),
        Err(e) => bail!("Could not open {device}: {e}"),
    }
}

/// One input format the card offers.
#[derive(Debug, PartialEq)]
struct Format {
    name: String,
    raw: bool,
    sizes: Vec<(u32, u32)>,
}

/// Parses `ffmpeg -f v4l2 -list_formats all` output.
fn parse_formats(text: &str) -> Vec<Format> {
    let mut out = Vec::new();
    for line in text.lines() {
        let raw = line.contains("Raw");
        if !raw && !line.contains("Compressed") {
            continue;
        }
        let parts: Vec<&str> = line.split(" : ").collect();
        if parts.len() < 3 {
            continue;
        }
        // The format name is the first word after "Raw :" / "Compressed:".
        let keyword = if raw { "Raw" } else { "Compressed" };
        let after = line.split_once(keyword).map(|(_, rest)| rest).unwrap_or("");
        let name = after
            .trim_start_matches(|c: char| c == ':' || c.is_whitespace())
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_owned();
        let sizes = parts[parts.len() - 1]
            .split_whitespace()
            .filter_map(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .collect();
        out.push(Format { name, raw, sizes });
    }
    out
}

/// The input to ask the card for: 1080p, uncompressed if offered (the
/// cleanest), else MJPEG; else the largest size it has.
fn choose_format(formats: &[Format]) -> Option<(String, (u32, u32))> {
    let usable = |f: &&Format| {
        matches!(
            f.name.as_str(),
            "nv12" | "yuyv422" | "uyvy422" | "yuv420p" | "mjpeg"
        )
    };
    let full = (1920, 1080);
    let with_full = |raw: bool| {
        formats
            .iter()
            .filter(usable)
            .find(|f| f.raw == raw && f.sizes.contains(&full))
            .map(|f| (f.name.clone(), full))
    };
    with_full(true).or_else(|| with_full(false)).or_else(|| {
        formats
            .iter()
            .filter(usable)
            .flat_map(|f| f.sizes.iter().map(move |s| (f.name.clone(), *s)))
            .max_by_key(|(_, (w, h))| w * h)
    })
}

async fn card_formats(device: &str) -> Vec<Format> {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-f",
            "v4l2",
            "-list_formats",
            "all",
            "-i",
            device,
        ])
        .stdin(Stdio::null())
        .output()
        .await;
    match output {
        Ok(output) => parse_formats(&String::from_utf8_lossy(&output.stderr)),
        Err(_) => Vec::new(),
    }
}

/// Video bitrate per quality, in kbit/s, and whether to scale to 720p.
fn video_settings(quality: Quality) -> (u32, bool) {
    match quality {
        Quality::Max => (20_000, false),
        Quality::High => (12_000, false),
        Quality::Medium => (6_000, true),
        Quality::Low => (2_500, true),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Encoder {
    Gpu,
    Cpu,
}

/// The ffmpeg arguments for one encoder.
fn ffmpeg_args(
    input: &[String],
    audio: Option<&str>,
    encoder: Encoder,
    quality: Quality,
    video_port: u16,
    audio_port: u16,
) -> Vec<String> {
    let (kbps, small) = video_settings(quality);
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-nostdin"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if encoder == Encoder::Gpu {
        args.extend([
            "-vaapi_device".into(),
            gpu().unwrap_or("/dev/dri/renderD128").into(),
        ]);
    }
    args.extend(input.iter().cloned());
    if let Some(card) = audio {
        args.extend(
            [
                "-f",
                "alsa",
                "-thread_queue_size",
                "1024",
                "-ac",
                "2",
                "-ar",
                "48000",
                "-i",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        args.push(format!("plughw:CARD={card}"));
    }
    args.extend(["-map".into(), "0:v:0".into()]);
    let rate = format!("{kbps}k");
    let buffer = format!("{}k", kbps / 2);
    match encoder {
        Encoder::Gpu => {
            let filter = if small {
                "format=nv12,hwupload,scale_vaapi=w=1280:h=720"
            } else {
                "format=nv12,hwupload"
            };
            args.extend(
                [
                    "-vf",
                    filter,
                    "-c:v",
                    "h264_vaapi",
                    "-profile:v",
                    "main",
                    "-bf",
                    "0",
                    "-g",
                    "60",
                    "-rc_mode",
                    "CBR",
                    "-async_depth",
                    "1",
                ]
                .iter()
                .map(|s| s.to_string()),
            );
        }
        Encoder::Cpu => {
            if small {
                args.extend(["-vf".into(), "scale=1280:720".into()]);
            }
            args.extend(
                [
                    "-c:v",
                    "libx264",
                    "-preset",
                    "superfast",
                    "-tune",
                    "zerolatency",
                    "-profile:v",
                    "main",
                    "-pix_fmt",
                    "yuv420p",
                    "-bf",
                    "0",
                    "-g",
                    "60",
                ]
                .iter()
                .map(|s| s.to_string()),
            );
        }
    }
    args.extend([
        "-b:v".into(),
        rate.clone(),
        "-maxrate".into(),
        rate,
        "-bufsize".into(),
        buffer,
        // The picture settings (SPS/PPS) with every key frame, inside the
        // stream: players never see ffmpeg's SDP.
        "-bsf:v".into(),
        "dump_extra=freq=keyframe".into(),
        "-f".into(),
        "rtp".into(),
        "-payload_type".into(),
        "102".into(),
        format!("rtp://127.0.0.1:{video_port}?pkt_size=1200"),
    ]);
    if audio.is_some() {
        args.extend(
            [
                "-map",
                "1:a:0",
                "-c:a",
                "libopus",
                "-b:a",
                "128k",
                "-ar",
                "48000",
                "-ac",
                "2",
                "-application",
                "lowdelay",
                "-frame_duration",
                "10",
                "-f",
                "rtp",
                "-payload_type",
                "111",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        args.push(format!("rtp://127.0.0.1:{audio_port}?pkt_size=1200"));
    }
    args
}

/// A running capture. Stops ffmpeg when dropped.
pub struct Capture {
    children: Vec<Child>,
    tasks: Vec<JoinHandle<()>>,
    pub encoder: &'static str,
    /// Sound comes from the card (else the Xbox's sound is used).
    pub sound: bool,
}

impl Drop for Capture {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.start_kill();
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// The card's sound as Opus RTP, in its own ffmpeg.
async fn start_sound(
    card: &str,
    socket: UdpSocket,
    media: broadcast::Sender<MediaPacket>,
) -> Result<(Child, JoinHandle<()>)> {
    let port = socket.local_addr()?.port();
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostdin",
        "-f",
        "alsa",
        "-thread_queue_size",
        "1024",
        "-ac",
        "2",
        "-ar",
        "48000",
        "-i",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("plughw:CARD={card}"));
    args.extend(
        [
            "-c:a",
            "libopus",
            "-b:a",
            "128k",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-application",
            "lowdelay",
            "-frame_duration",
            "10",
            "-f",
            "rtp",
            "-payload_type",
            "111",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    args.push(format!("rtp://127.0.0.1:{port}?pkt_size=1200"));
    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("ffmpeg is missing")?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("[capture sound] {line}");
            }
        });
    }
    let mut probe = [0u8; 2048];
    match tokio::time::timeout(Duration::from_secs(5), socket.peek_from(&mut probe)).await {
        Ok(Ok(_)) => Ok((child, pump(socket, media, false))),
        _ => {
            let _ = child.start_kill();
            bail!("no sound packets from {card}")
        }
    }
}

/// Reads RTP from a local port into the media channel.
fn pump(socket: UdpSocket, media: broadcast::Sender<MediaPacket>, video: bool) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            let Ok(n) = socket.recv(&mut buf).await else {
                return;
            };
            let mut bytes = &buf[..n];
            if let Ok(packet) = rtc::rtp::packet::Packet::unmarshal(&mut bytes) {
                let _ = media.send(if video {
                    MediaPacket::Video(packet)
                } else {
                    MediaPacket::Audio(packet)
                });
            }
        }
    })
}

/// Starts capturing from `device` (sound from ALSA card `audio` if given).
/// Tries the GPU encoder first when there is one, then the processor.
pub async fn start(
    device: &str,
    audio: Option<&str>,
    quality: Quality,
    media: broadcast::Sender<MediaPacket>,
) -> Result<Capture> {
    check_access(device)?;
    let formats = card_formats(device).await;
    let mut input: Vec<String> = [
        "-f",
        "v4l2",
        "-thread_queue_size",
        "64",
        "-fflags",
        "nobuffer",
        "-framerate",
        "60",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some((format, (w, h))) = choose_format(&formats) {
        input.extend([
            "-input_format".into(),
            format,
            "-video_size".into(),
            format!("{w}x{h}"),
        ]);
    }
    input.extend(["-i".into(), device.to_owned()]);
    start_with_input(&input, audio, quality, media).await
}

async fn start_with_input(
    input: &[String],
    audio: Option<&str>,
    quality: Quality,
    media: broadcast::Sender<MediaPacket>,
) -> Result<Capture> {
    let encoders: Vec<Encoder> = if gpu().is_some() {
        vec![Encoder::Gpu, Encoder::Cpu]
    } else {
        vec![Encoder::Cpu]
    };
    let mut last_error = String::new();
    for encoder in encoders {
        let video_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let audio_socket = UdpSocket::bind("127.0.0.1:0").await?;
        let args = ffmpeg_args(
            input,
            None,
            encoder,
            quality,
            video_socket.local_addr()?.port(),
            audio_socket.local_addr()?.port(),
        );
        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("ffmpeg is missing")?;
        // Keep the last lines ffmpeg prints, to explain a failure.
        let (lines_tx, mut lines_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    println!("[capture] {line}");
                    let _ = lines_tx.send(line);
                }
            });
        }
        // Working means video packets arrive within a few seconds.
        let mut probe = [0u8; 2048];
        let first =
            tokio::time::timeout(Duration::from_secs(8), video_socket.peek_from(&mut probe)).await;
        let mut said = Vec::new();
        while let Ok(line) = lines_rx.try_recv() {
            said.push(line);
        }
        match (first, child.try_wait()) {
            (Ok(Ok(_)), Ok(None)) => {
                let name = match encoder {
                    Encoder::Gpu => "graphics chip",
                    Encoder::Cpu => "processor",
                };
                println!("[capture] running, encoding on the {name}");
                let mut capture = Capture {
                    children: vec![child],
                    tasks: vec![pump(video_socket, media.clone(), true)],
                    encoder: name,
                    sound: false,
                };
                // Sound runs on its own: one ffmpeg reading both the card's
                // picture and its sound waits on the slower of the two, and
                // the picture froze after a second.
                if let Some(card) = audio {
                    match start_sound(card, audio_socket, media).await {
                        Ok((child, task)) => {
                            capture.children.push(child);
                            capture.tasks.push(task);
                            capture.sound = true;
                        }
                        Err(error) => println!("[capture] no sound from the card: {error:#}"),
                    }
                }
                return Ok(capture);
            }
            _ => {
                let _ = child.start_kill();
                last_error = said
                    .iter()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| "no picture from the capture card".to_owned());
                println!("[capture] {encoder:?} encoder did not work: {last_error}");
            }
        }
    }
    bail!("The capture card did not work: {last_error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sound_cards() {
        let cards = parse_asound_cards(
            " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0xf7f10000 irq 32\n 1 [S4K            ]: USB-Audio - Cam Link 4K\n                      Elgato Cam Link 4K at usb-0000:00:14.0-2, super speed\n",
        );
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[1].id, "S4K");
        assert_eq!(cards[1].name, "Cam Link 4K");
    }

    #[test]
    fn picks_raw_1080p_then_mjpeg() {
        let text = "[video4linux2,v4l2 @ 0x1] Compressed:       mjpeg :          Motion-JPEG : 1920x1080 1280x720\n[video4linux2,v4l2 @ 0x1] Raw       :     yuyv422 :           YUYV 4:2:2 : 1920x1080 1280x720\n";
        let formats = parse_formats(text);
        assert_eq!(formats.len(), 2);
        assert_eq!(formats[0].name, "mjpeg");
        assert_eq!(
            choose_format(&formats),
            Some(("yuyv422".to_owned(), (1920, 1080)))
        );
        let only_small_raw = "[v4l2 @ 0x1] Compressed:       mjpeg :          Motion-JPEG : 1920x1080\n[v4l2 @ 0x1] Raw       :     yuyv422 :           YUYV 4:2:2 : 1280x720\n";
        assert_eq!(
            choose_format(&parse_formats(only_small_raw)),
            Some(("mjpeg".to_owned(), (1920, 1080)))
        );
    }

    /// A test picture through ffmpeg on the processor, as RTP into the channel.
    #[tokio::test]
    async fn test_picture_reaches_the_channel() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            return;
        }
        let (media, mut rx) = broadcast::channel(256);
        let input: Vec<String> = ["-re", "-f", "lavfi", "-i", "testsrc=size=1280x720:rate=30"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let capture = start_with_input(&input, None, Quality::Low, media)
            .await
            .unwrap();
        // The first whole key frame carries its picture settings (SPS, NAL type 7).
        let mut depacketizer = crate::wsmedia::Depacketizer::default();
        let key = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Ok(MediaPacket::Video(p)) = rx.recv().await {
                    let h = &p.header;
                    for frame in
                        depacketizer.push(h.sequence_number, h.timestamp, h.marker, &p.payload)
                    {
                        if frame.key {
                            return frame;
                        }
                    }
                }
            }
        })
        .await
        .unwrap();
        assert!(key
            .data
            .windows(5)
            .any(|w| w[..4] == [0, 0, 0, 1] && w[4] & 0x1f == 7));
        drop(capture);
    }
}
