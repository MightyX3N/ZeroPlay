//! Picture (and optionally sound) from a capture card instead of the Xbox's
//! remote play video. Remote play stays connected for the controllers.
//!
//! Capture cards that act like a webcam (UVC) show up as /dev/videoN. ffmpeg
//! reads the card, encodes H.264 (on the GPU when there is one, else on the
//! processor) and Opus, and sends RTP to two local ports. Those packets go into
//! the same channel as the console's, so every player path works unchanged.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
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
    /// Prefer the sharpest picture (e.g. 4K at 30 fps) over 60 fps.
    #[serde(default)]
    pub sharp: bool,
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
/// Docker hides /proc/asound, so the cards are read from /sys (with
/// /proc/asound as a fallback).
pub fn audio_devices() -> Vec<AudioDevice> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/sound") {
        for entry in entries.flatten() {
            let node = entry.file_name().to_string_lossy().to_string();
            if !node.starts_with("card") {
                continue;
            }
            let dir = entry.path();
            let Ok(id) = std::fs::read_to_string(dir.join("id")) else {
                continue;
            };
            let id = id.trim().to_owned();
            // The USB product name, if it is a USB card.
            let name = ["device/../product", "device/product"]
                .iter()
                .find_map(|p| std::fs::read_to_string(dir.join(p)).ok())
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| id.clone());
            out.push(AudioDevice { id, name });
        }
    }
    if out.is_empty() {
        out =
            parse_asound_cards(&std::fs::read_to_string("/proc/asound/cards").unwrap_or_default());
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The USB device a /sys class entry belongs to (the parent of its interface).
fn usb_device(class_dir: &str) -> Option<std::path::PathBuf> {
    let interface = std::fs::canonicalize(format!("{class_dir}/device")).ok()?;
    interface.parent().map(|p| p.to_path_buf())
}

/// The sound card on the same USB device as a capture card.
pub fn sound_of_card(video_path: &str) -> Option<AudioDevice> {
    let node = video_path.strip_prefix("/dev/")?;
    let usb = usb_device(&format!("/sys/class/video4linux/{node}"))?;
    let entries = std::fs::read_dir("/sys/class/sound").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("card") {
            continue;
        }
        if usb_device(&entry.path().to_string_lossy()) == Some(usb.clone()) {
            let id = std::fs::read_to_string(entry.path().join("id"))
                .ok()?
                .trim()
                .to_owned();
            return audio_devices().into_iter().find(|a| a.id == id);
        }
    }
    None
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

/// Which graphics chip the NAS has, e.g. "Intel 0x46d1".
pub fn gpu_model() -> String {
    let read = |f: &str| {
        std::fs::read_to_string(format!("/sys/class/drm/renderD128/device/{f}"))
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    };
    let vendor = match read("vendor").as_str() {
        "0x8086" => "Intel".to_owned(),
        "0x1002" => "AMD".to_owned(),
        "0x10de" => "NVIDIA".to_owned(),
        other => other.to_owned(),
    };
    let driver = std::fs::read_link("/sys/class/drm/renderD128/device/driver")
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default();
    format!("{vendor} {} (kernel driver {driver})", read("device"))
}

/// Finds a VA-API driver that starts on this graphics chip: the automatic
/// choice first, then each one by name. `Ok(None)` = the automatic choice.
async fn vaapi_driver() -> std::result::Result<Option<&'static str>, String> {
    let mut said = String::new();
    for name in [None, Some("iHD"), Some("i965"), Some("radeonsi")] {
        let mut command = Command::new("ffmpeg");
        if let Some(name) = name {
            command.env("LIBVA_DRIVER_NAME", name);
        }
        let output = command
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-init_hw_device",
                "vaapi=va:/dev/dri/renderD128",
                "-f",
                "lavfi",
                "-i",
                "nullsrc=s=64x64",
                "-frames:v",
                "1",
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::null())
            .output()
            .await;
        match output {
            Ok(o) if o.status.success() => {
                println!(
                    "[capture] graphics driver works: {}",
                    name.unwrap_or("automatic")
                );
                return Ok(name);
            }
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stderr);
                if let Some(line) = text.lines().find(|l| !l.trim().is_empty()) {
                    said = line.to_owned();
                }
            }
            Err(e) => said = e.to_string(),
        }
    }
    let why = format!(
        "no graphics driver starts on this chip: {} — {said}",
        gpu_model()
    );
    println!("[capture] {why}");
    Err(why)
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

/// The input to ask the card for: 1080p MJPEG when the card offers it, else
/// 1080p uncompressed; else the largest size it has. Cards that offer both
/// are usually on USB 2, where uncompressed 1080p runs at about 5 frames a
/// second; cards that only send uncompressed (USB 3) do 60.
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
    with_full(false).or_else(|| with_full(true)).or_else(|| {
        formats
            .iter()
            .filter(usable)
            .flat_map(|f| f.sizes.iter().map(move |s| (f.name.clone(), *s)))
            .max_by_key(|(_, (w, h))| w * h)
    })
}

/// One mode a card can send: format, size and frame rate.
#[derive(Clone, Debug, PartialEq)]
struct Mode {
    /// ffmpeg's name for the format (mjpeg, yuyv422, nv12, ...).
    format: String,
    width: u32,
    height: u32,
    fps: f64,
}

impl Mode {
    fn key(&self) -> String {
        format!(
            "{}x{} {} {}",
            self.width, self.height, self.format, self.fps
        )
    }

    fn raw(&self) -> bool {
        self.format != "mjpeg"
    }
}

/// Parses `v4l2-ctl --list-formats-ext`.
fn parse_modes(text: &str) -> Vec<Mode> {
    let mut out = Vec::new();
    let mut format: Option<String> = None;
    let mut size: Option<(u32, u32)> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.contains("]: '") {
            let fourcc = line.split('\'').nth(1).unwrap_or("");
            format = match fourcc {
                "MJPG" => Some("mjpeg"),
                "YUYV" => Some("yuyv422"),
                "UYVY" => Some("uyvy422"),
                "NV12" => Some("nv12"),
                "YU12" => Some("yuv420p"),
                _ => None,
            }
            .map(str::to_owned);
            size = None;
        } else if let Some(rest) = line.strip_prefix("Size: Discrete ") {
            size = rest
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse().ok()?, h.trim().parse().ok()?)));
        } else if line.starts_with("Interval:") {
            let fps = line
                .split('(')
                .nth(1)
                .and_then(|r| r.split_whitespace().next())
                .and_then(|n| n.parse::<f64>().ok());
            if let (Some(format), Some((width, height)), Some(fps)) = (&format, size, fps) {
                out.push(Mode {
                    format: format.clone(),
                    width,
                    height,
                    fps,
                });
            }
        }
    }
    out
}

/// The best real mode for games: smooth first (50+ fps, then 25+), then the
/// biggest picture up to 1080p, then uncompressed over MJPEG.
/// With `sharp`, the biggest picture wins as long as it has at least 24 fps
/// (4K30 over 1440p60).
fn choose_mode(modes: &[Mode], sharp: bool) -> Option<Mode> {
    let bucket = |fps: f64| {
        if fps >= 49.0 {
            2
        } else if fps >= 24.0 {
            1
        } else {
            0
        }
    };
    // HD first (an SD mode like 720x576 looks blurry however smooth it is),
    // then smooth (50+ fps), then the biggest picture up to 4K, then
    // uncompressed, then the frame rate nearest 60 (120 doubles the work and
    // halves each frame's share of the bitrate, for screens that show 60).
    let key = |m: &Mode| {
        (
            m.height >= 720,
            if sharp {
                bucket(m.fps).min(1)
            } else {
                bucket(m.fps)
            },
            m.width * m.height,
            m.raw(),
            10_000 - ((m.fps - 60.0).abs() * 100.0).min(10_000.0) as u32,
        )
    };
    modes
        .iter()
        .filter(|m| m.height <= 2160)
        .max_by(|a, b| key(a).cmp(&key(b)))
        .cloned()
}

async fn card_modes(device: &str) -> Vec<Mode> {
    let output = Command::new("v4l2-ctl")
        .args(["--list-formats-ext", "-d", device])
        .stdin(Stdio::null())
        .output()
        .await;
    match output {
        Ok(output) => parse_modes(&String::from_utf8_lossy(&output.stdout)),
        Err(_) => Vec::new(),
    }
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
/// Video bitrate in kbit/s, and the size to scale down to (if the card's
/// picture is bigger): Max keeps the card's picture (up to 4K), High is at
/// most 1080p, Medium and Low 720p.
fn video_settings(quality: Quality, source_height: u32) -> (u32, Option<(u32, u32)>) {
    let cap = match quality {
        Quality::Max => None,
        Quality::High => Some((1920, 1080)),
        Quality::Medium | Quality::Low => Some((1280, 720)),
    };
    let scale = cap.filter(|(_, h)| source_height > *h);
    let height = scale.map(|(_, h)| h).unwrap_or(source_height);
    let kbps = match quality {
        Quality::Max if height >= 2160 => 40_000,
        Quality::Max if height >= 1440 => 28_000,
        Quality::Max => 20_000,
        Quality::High => 12_000,
        Quality::Medium => 6_000,
        Quality::Low => 2_500,
    };
    (kbps, scale)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Encoder {
    Gpu,
    /// The graphics chip's low-power encoder (VDENC): newer Intel chips only
    /// have this one for H.264.
    GpuLowPower,
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
    preset: &str,
    source_height: u32,
) -> Vec<String> {
    let (kbps, scale) = video_settings(quality, source_height);
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if encoder != Encoder::Cpu {
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
    // Real frames only: no repeats to fill a frame rate (they hid that the
    // card sent few pictures, and add delay).
    args.extend([
        "-map".into(),
        "0:v:0".into(),
        "-fps_mode".into(),
        "passthrough".into(),
    ]);
    let rate = format!("{kbps}k");
    let buffer = format!("{}k", kbps / 2);
    match encoder {
        Encoder::Gpu | Encoder::GpuLowPower => {
            if encoder == Encoder::GpuLowPower {
                args.extend(["-low_power".into(), "1".into()]);
            }
            let filter = match scale {
                Some((w, h)) => format!("format=nv12,hwupload,scale_vaapi=w={w}:h={h}"),
                None => "format=nv12,hwupload".to_owned(),
            };
            args.extend(["-vf".into(), filter]);
            args.extend(
                [
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
            if let Some((w, h)) = scale {
                args.extend(["-vf".into(), format!("scale={w}:{h}")]);
            }
            args.extend(
                [
                    "-c:v",
                    "libx264",
                    "-preset",
                    preset,
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
    /// Frames a second leaving the encoder right now.
    pub fps: Arc<AtomicU32>,
    /// What the card is asked to send, e.g. "1920x1080 mjpeg".
    pub input: String,
    /// Why the graphics chip was not used, when it was tried.
    pub gpu_error: Option<String>,
    /// Why the card's sound is not used, when it was asked for.
    pub sound_error: Option<String>,
    /// The card mode in use ("1920x1080 mjpeg 60") and its promised frame rate.
    pub mode_key: Option<String>,
    /// Every mode the card lists, for the admin.
    pub offers: String,
    pub expected_fps: f64,
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
            // The card's sound timing jumps; resampling smooths it out
            // instead of clicks and gaps.
            "-af",
            "aresample=async=1000:first_pts=0",
            "-c:a",
            "libopus",
            "-b:a",
            "160k",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-application",
            "audio",
            "-frame_duration",
            "20",
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
        Ok(Ok(_)) => Ok((
            child,
            pump(socket, media, false, Arc::new(AtomicU32::new(0))),
        )),
        _ => {
            let _ = child.start_kill();
            bail!("no sound packets from {card}")
        }
    }
}

/// Reads RTP from a local port into the media channel.
/// Counts whole frames (RTP marker bits) into `fps`, once a second.
fn pump(
    socket: UdpSocket,
    media: broadcast::Sender<MediaPacket>,
    video: bool,
    fps: Arc<AtomicU32>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut frames = 0u32;
        let mut since = std::time::Instant::now();
        loop {
            let Ok(n) = socket.recv(&mut buf).await else {
                return;
            };
            let mut bytes = &buf[..n];
            if let Ok(packet) = rtc::rtp::packet::Packet::unmarshal(&mut bytes) {
                if packet.header.marker {
                    frames += 1;
                }
                if since.elapsed() >= Duration::from_secs(1) {
                    fps.store(frames, Ordering::Relaxed);
                    frames = 0;
                    since = std::time::Instant::now();
                }
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
    skip: &[String],
    sharp: bool,
) -> Result<Capture> {
    check_access(device)?;
    let mut input: Vec<String> = [
        "-f",
        "v4l2",
        // A short queue: a late frame is dropped instead of every later
        // frame waiting behind it (that delay only grew).
        "-thread_queue_size",
        "4",
        "-fflags",
        "nobuffer",
        "-flags",
        "low_delay",
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // The modes the card really has (with frame rates); else a guess from
    // ffmpeg's list of formats.
    let all_modes = card_modes(device).await;
    // Modes that turned out not to deliver their frame rate are left out.
    let modes: Vec<Mode> = all_modes
        .iter()
        .filter(|m| !skip.contains(&m.key()))
        .cloned()
        .collect();
    let chosen = choose_mode(&modes, sharp);
    // The processor's encoder gets a better (slower) setting when there are
    // fewer pixels a second to encode.
    let preset = match &chosen {
        Some(m) => {
            let rate = m.width as f64 * m.height as f64 * m.fps;
            if rate <= 1280.0 * 720.0 * 31.0 {
                "veryfast"
            } else if rate <= 1920.0 * 1080.0 * 31.0 {
                "superfast"
            } else {
                "ultrafast"
            }
        }
        None => "superfast",
    };
    let described = if let Some(mode) = chosen.clone() {
        println!("[capture] card modes: {modes:?}");
        input.extend([
            "-input_format".into(),
            mode.format.clone(),
            "-video_size".into(),
            format!("{}x{}", mode.width, mode.height),
            "-framerate".into(),
            format!("{}", mode.fps),
        ]);
        format!(
            "{}x{} {} at {:.0} fps",
            mode.width, mode.height, mode.format, mode.fps
        )
    } else {
        let formats = card_formats(device).await;
        input.extend(["-framerate".into(), "60".into()]);
        match choose_format(&formats) {
            Some((format, (w, h))) => {
                input.extend([
                    "-input_format".into(),
                    format.clone(),
                    "-video_size".into(),
                    format!("{w}x{h}"),
                ]);
                format!("{w}x{h} {format}")
            }
            None => "the card's default format".to_owned(),
        }
    };
    input.extend(["-i".into(), device.to_owned()]);
    let source_height = chosen.as_ref().map(|m| m.height).unwrap_or(1080);
    let mut capture =
        start_with_input(&input, audio, quality, media, preset, source_height).await?;
    capture.input = described;
    capture.offers = all_modes
        .iter()
        .map(|m| format!("{}x{} {} {:.0}", m.width, m.height, m.format, m.fps))
        .collect::<Vec<_>>()
        .join(", ");
    capture.mode_key = chosen.as_ref().map(Mode::key);
    capture.expected_fps = chosen.map(|m| m.fps).unwrap_or(0.0);
    Ok(capture)
}

async fn start_with_input(
    input: &[String],
    audio: Option<&str>,
    quality: Quality,
    media: broadcast::Sender<MediaPacket>,
    preset: &str,
    source_height: u32,
) -> Result<Capture> {
    let mut gpu_error: Option<String> = None;
    let mut driver: Option<&'static str> = None;
    let encoders: Vec<Encoder> = if gpu().is_some() {
        match vaapi_driver().await {
            Ok(found) => {
                driver = found;
                vec![Encoder::Gpu, Encoder::GpuLowPower, Encoder::Cpu]
            }
            Err(why) => {
                gpu_error = Some(why);
                vec![Encoder::Cpu]
            }
        }
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
            preset,
            source_height,
        );
        let mut command = Command::new("ffmpeg");
        if let (Some(name), true) = (driver, encoder != Encoder::Cpu) {
            command.env("LIBVA_DRIVER_NAME", name);
        }
        let mut child = command
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
        // A picture packet, or ffmpeg giving up (then the next encoder is tried at once).
        let first = tokio::select! {
            got = tokio::time::timeout(Duration::from_secs(8), video_socket.peek_from(&mut probe)) => got,
            _ = child.wait() => Ok(Err(std::io::Error::other("ffmpeg stopped"))),
        };
        // Let the last lines of a stopped ffmpeg arrive.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut said = Vec::new();
        while let Ok(line) = lines_rx.try_recv() {
            said.push(line);
        }
        match (first, child.try_wait()) {
            (Ok(Ok(_)), Ok(None)) => {
                let name = match encoder {
                    Encoder::Gpu | Encoder::GpuLowPower => "graphics chip",
                    Encoder::Cpu => "processor",
                };
                println!("[capture] running, encoding on the {name}");
                let fps = Arc::new(AtomicU32::new(0));
                let mut capture = Capture {
                    children: vec![child],
                    tasks: vec![pump(video_socket, media.clone(), true, fps.clone())],
                    encoder: name,
                    sound: false,
                    fps,
                    input: String::new(),
                    gpu_error: gpu_error.clone(),
                    sound_error: None,
                    mode_key: None,
                    offers: String::new(),
                    expected_fps: 0.0,
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
                        Err(error) => {
                            println!("[capture] no sound from the card: {error:#}");
                            capture.sound_error = Some(format!("{error:#}"));
                        }
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
                if encoder != Encoder::Cpu {
                    gpu_error = Some(last_error.clone());
                }
            }
        }
    }
    bail!("The capture card did not work: {last_error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_4k60_and_60_over_120() {
        let m = |w: u32, h: u32, fps: f64| Mode {
            format: "nv12".into(),
            width: w,
            height: h,
            fps,
        };
        let modes = vec![
            m(1920, 1080, 120.0),
            m(1920, 1080, 60.0),
            m(3840, 2160, 30.0),
        ];
        assert_eq!(choose_mode(&modes, false), Some(m(1920, 1080, 60.0)));
        let with_4k60 = vec![
            m(1920, 1080, 120.0),
            m(3840, 2160, 60.0),
            m(3840, 2160, 30.0),
        ];
        assert_eq!(choose_mode(&with_4k60, false), Some(m(3840, 2160, 60.0)));
    }

    #[test]
    fn sharp_picks_4k30() {
        let m = |w: u32, h: u32, fps: f64| Mode {
            format: "nv12".into(),
            width: w,
            height: h,
            fps,
        };
        let modes = vec![
            m(3840, 2160, 30.0),
            m(2560, 1440, 60.0),
            m(1920, 1080, 120.0),
        ];
        assert_eq!(choose_mode(&modes, true), Some(m(3840, 2160, 30.0)));
        assert_eq!(choose_mode(&modes, false), Some(m(2560, 1440, 60.0)));
    }

    #[test]
    fn quality_scales_a_4k_picture() {
        assert_eq!(video_settings(Quality::Max, 2160), (40_000, None));
        assert_eq!(
            video_settings(Quality::High, 2160),
            (12_000, Some((1920, 1080)))
        );
        assert_eq!(video_settings(Quality::High, 1080), (12_000, None));
        assert_eq!(
            video_settings(Quality::Low, 1080),
            (2_500, Some((1280, 720)))
        );
    }

    #[test]
    fn prefers_hd_over_smooth_sd() {
        let modes = vec![
            Mode {
                format: "yuyv422".into(),
                width: 720,
                height: 576,
                fps: 50.0,
            },
            Mode {
                format: "yuyv422".into(),
                width: 1920,
                height: 1080,
                fps: 30.0,
            },
            Mode {
                format: "mjpeg".into(),
                width: 1280,
                height: 720,
                fps: 60.0,
            },
        ];
        assert_eq!(choose_mode(&modes, false).map(|m| m.height), Some(720));
        assert_eq!(
            choose_mode(&modes[..2], false).map(|m| m.height),
            Some(1080)
        );
    }

    #[test]
    fn chooses_a_smooth_mode() {
        let text = "ioctl: VIDIOC_ENUM_FMT\n\tType: Video Capture\n\n\t[0]: 'MJPG' (Motion-JPEG, compressed)\n\t\tSize: Discrete 1920x1080\n\t\t\tInterval: Discrete 0.033s (30.000 fps)\n\t\tSize: Discrete 1280x720\n\t\t\tInterval: Discrete 0.017s (60.000 fps)\n\t[1]: 'YUYV' (YUYV 4:2:2)\n\t\tSize: Discrete 1920x1080\n\t\t\tInterval: Discrete 0.200s (5.000 fps)\n\t\tSize: Discrete 1280x720\n\t\t\tInterval: Discrete 0.100s (10.000 fps)\n";
        let modes = parse_modes(text);
        assert_eq!(modes.len(), 4);
        assert_eq!(
            choose_mode(&modes, false),
            Some(Mode {
                format: "mjpeg".into(),
                width: 1280,
                height: 720,
                fps: 60.0
            })
        );
        let usb3 = "\t[0]: 'MJPG' (Motion-JPEG, compressed)\n\t\tSize: Discrete 1920x1080\n\t\t\tInterval: Discrete 0.017s (60.000 fps)\n\t[1]: 'NV12' (Y/UV 4:2:0)\n\t\tSize: Discrete 1920x1080\n\t\t\tInterval: Discrete 0.017s (60.000 fps)\n\t\tSize: Discrete 3840x2160\n\t\t\tInterval: Discrete 0.033s (30.000 fps)\n";
        assert_eq!(
            choose_mode(&parse_modes(usb3), false),
            Some(Mode {
                format: "nv12".into(),
                width: 1920,
                height: 1080,
                fps: 60.0
            })
        );
    }

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
    fn picks_mjpeg_1080p_then_raw() {
        let text = "[video4linux2,v4l2 @ 0x1] Compressed:       mjpeg :          Motion-JPEG : 1920x1080 1280x720\n[video4linux2,v4l2 @ 0x1] Raw       :     yuyv422 :           YUYV 4:2:2 : 1920x1080 1280x720\n";
        let formats = parse_formats(text);
        assert_eq!(formats.len(), 2);
        assert_eq!(formats[0].name, "mjpeg");
        assert_eq!(
            choose_format(&formats),
            Some(("mjpeg".to_owned(), (1920, 1080)))
        );
        let raw_only =
            "[v4l2 @ 0x1] Raw       :        nv12 :         Y/CbCr 4:2:0 : 1920x1080 3840x2160\n";
        assert_eq!(
            choose_format(&parse_formats(raw_only)),
            Some(("nv12".to_owned(), (1920, 1080)))
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
        let capture = start_with_input(&input, None, Quality::Low, media, "ultrafast", 720)
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
