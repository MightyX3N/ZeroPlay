//! A live stream from a console over WebRTC.
//!
//! Ported from Greenlight's player (MIT, UnknownSKL): one peer connection with
//! an audio and a video transceiver and four data channels (`chat`, `control`,
//! `input`, `message`). Once the `message` channel has shaken hands, the
//! console starts sending video and accepts input.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bytes::BytesMut;
use rtc::data_channel::RTCDataChannelInit;
use rtc::interceptor::Registry;
use rtc::media::io::h26x_writer::H26xWriter;
use rtc::media::io::ogg_writer::OggWriter;
use rtc::media::io::Writer;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceCandidateInit;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};

use crate::api::{HomeApi, IceCandidate, SessionState};
use crate::auth::Auth;
use crate::input::{self, GamepadState, KeyboardFrame, MouseFrame, ServerReport, Vibration};
use crate::teredo;

/// How SDP and ICE candidates get to the console and back.
///
/// The real one is [`HomeSignaling`]. Tests use a fake console instead.
#[async_trait::async_trait]
pub trait Signaling: Send + Sync + 'static {
    async fn exchange_sdp(&self, offer_sdp: &str) -> Result<String>;
    async fn exchange_ice(&self, candidates: &[IceCandidate]) -> Result<Vec<IceCandidate>>;
    async fn keepalive(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
}

/// Signaling through Microsoft's home-streaming API.
pub struct HomeSignaling {
    api: HomeApi,
    session_id: String,
}

impl HomeSignaling {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

#[async_trait::async_trait]
impl Signaling for HomeSignaling {
    async fn exchange_sdp(&self, offer_sdp: &str) -> Result<String> {
        self.api.exchange_sdp(&self.session_id, offer_sdp).await
    }
    async fn exchange_ice(&self, candidates: &[IceCandidate]) -> Result<Vec<IceCandidate>> {
        self.api.exchange_ice(&self.session_id, candidates).await
    }
    async fn keepalive(&self) -> Result<()> {
        self.api.keepalive(&self.session_id).await
    }
    async fn stop(&self) -> Result<()> {
        self.api.stop(&self.session_id).await
    }
}

/// Starts a session on a console and waits until it is ready for WebRTC.
///
/// `status` is called with a short line each time something happens.
pub async fn start_home_session(
    api: &HomeApi,
    auth: &mut Auth,
    console_id: &str,
    status: &(dyn Fn(&str) + Send + Sync),
) -> Result<HomeSignaling> {
    let session_id = api.start(console_id).await?;
    status(&format!("session {session_id} created, waiting for the console"));
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut authorized = false;
    loop {
        if Instant::now() > deadline {
            let _ = api.stop(&session_id).await;
            bail!("the console did not get ready within 90 seconds");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        match api.state(&session_id).await? {
            SessionState::Provisioning => {}
            SessionState::ReadyToConnect => {
                if !authorized {
                    status("console is asking for authorization");
                    let token = auth.console_transfer_token().await?;
                    api.connect(&session_id, &token).await?;
                    authorized = true;
                }
            }
            SessionState::Provisioned => {
                status("console is ready");
                return Ok(HomeSignaling { api: api.clone(), session_id });
            }
            SessionState::Failed(details) => {
                let _ = api.stop(&session_id).await;
                bail!("the console refused the stream: {details}");
            }
            SessionState::Other(state) => status(&format!("session state: {state}")),
        }
    }
}

/// One RTP packet as it came from the console.
#[derive(Clone, Debug)]
pub enum MediaPacket {
    /// H.264 video.
    Video(rtc::rtp::Packet),
    /// Opus audio, 48 kHz stereo.
    Audio(rtc::rtp::Packet),
}

/// Where to send what the console sends. Leave a field out to skip it.
#[derive(Clone, Debug, Default)]
pub struct StreamOptions {
    /// Raw H.264 video (plays in VLC or ffplay).
    pub video_path: Option<PathBuf>,
    /// Opus audio in an Ogg file.
    pub audio_path: Option<PathBuf>,
    /// Every video and audio packet, unchanged, for passing on to other peers.
    pub media: Option<tokio::sync::broadcast::Sender<MediaPacket>>,
}

/// Things that happen during a stream.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    /// The peer connection changed state ("connecting", "connected", "failed", ...).
    Connection(String),
    /// The console accepted our handshake. Input works from here on.
    Ready,
    VideoSize { width: u32, height: u32 },
    Vibration(Vibration),
    /// The console told us which game or app is in front.
    TitleInfo(String),
    /// A message from the console we do not act on.
    ServerMessage { target: String, content: String },
    /// The console ended the stream.
    Disconnected(String),
}

/// Running totals for a stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub video_packets: u64,
    pub video_bytes: u64,
    pub audio_packets: u64,
    pub audio_bytes: u64,
    pub input_packets_sent: u64,
}

#[derive(Default)]
struct Counters {
    video_packets: AtomicU64,
    video_bytes: AtomicU64,
    audio_packets: AtomicU64,
    audio_bytes: AtomicU64,
    input_packets_sent: AtomicU64,
}

struct Channels {
    control: Arc<dyn DataChannel>,
    input: Arc<dyn DataChannel>,
    message: Arc<dyn DataChannel>,
}

struct Shared {
    started: Instant,
    pads: Mutex<[Option<GamepadState>; 4]>,
    mouse: Mutex<Vec<MouseFrame>>,
    keys: Mutex<Vec<KeyboardFrame>>,
    ready: AtomicBool,
    closed: AtomicBool,
    sequence: AtomicU32,
    counters: Counters,
    events: mpsc::UnboundedSender<SessionEvent>,
}

impl Shared {
    fn now_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }
    fn emit(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }
}

async fn send_json(channel: &Arc<dyn DataChannel>, text: String) -> Result<()> {
    // Greenlight sends JSON as binary messages, so we do too.
    channel.send(BytesMut::from(text.as_bytes())).await.map_err(|e| anyhow!("{e}"))
}

// ── Protocol messages (pure functions, see the tests) ───────────────────────

fn handshake_message() -> String {
    json!({
        "type": "Handshake",
        "version": "messageV1",
        "id": "be0bfc6d-1e83-4c8a-90ed-fa8601c5a179",
        "cv": "0",
    })
    .to_string()
}

fn authorization_message() -> String {
    json!({
        "message": "authorizationRequest",
        "accessKey": "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E",
    })
    .to_string()
}

fn gamepad_changed_message(index: u8, was_added: bool) -> String {
    json!({ "message": "gamepadChanged", "gamepadIndex": index, "wasAdded": was_added }).to_string()
}

fn keyframe_request_message() -> String {
    json!({ "message": "videoKeyframeRequested", "ifrRequested": true }).to_string()
}

fn wrap_message(target: &str, content: Value) -> String {
    json!({
        "type": "Message",
        "content": content.to_string(),
        "id": uuid::Uuid::new_v4().to_string(),
        "target": target,
        "cv": "",
    })
    .to_string()
}

/// What the client tells the console about itself right after the handshake.
fn config_messages() -> Vec<String> {
    vec![
        wrap_message("/streaming/systemUi/configuration", json!({ "version": [0, 2, 0], "systemUis": [] })),
        wrap_message(
            "/streaming/properties/clientappinstallidchanged",
            json!({ "clientAppInstallId": "c97d7ee0-73b2-4239-bf1d-9d805a338429" }),
        ),
        wrap_message("/streaming/characteristics/orientationchanged", json!({ "orientation": 0 })),
        wrap_message("/streaming/characteristics/touchinputenabledchanged", json!({ "touchInputEnabled": false })),
        wrap_message("/streaming/characteristics/clientdevicecapabilities", json!({})),
        wrap_message(
            "/streaming/characteristics/dimensionschanged",
            json!({
                "horizontal": 1920,
                "vertical": 1080,
                "preferredWidth": 1920,
                "preferredHeight": 1080,
                "safeAreaLeft": 0,
                "safeAreaTop": 0,
                "safeAreaRight": 1920,
                "safeAreaBottom": 1080,
                "supportsCustomResolution": true,
            }),
        ),
    ]
}

fn transaction_complete_message(id: &Value, content: Value) -> String {
    json!({ "type": "TransactionComplete", "content": content.to_string(), "id": id, "cv": "" }).to_string()
}

/// A remote candidate ready to hand to the peer connection.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteCandidate {
    candidate: String,
    sdp_mid: Option<String>,
    sdp_mline_index: Option<u16>,
}

fn value_to_mid(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn value_to_mline(value: &Value) -> Option<u16> {
    match value {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
        _ => None,
    }
}

/// Cleans up the console's candidates and adds the ones hidden in Teredo addresses.
///
/// The console writes candidates as `a=candidate:...`; the peer connection wants
/// `candidate:...`. For a Teredo address we add its real IPv4 address twice, once
/// on port 9002 and once on the port stored in the address, as Greenlight does.
fn expand_remote_candidates(remote: &[IceCandidate]) -> Vec<RemoteCandidate> {
    let mut out = Vec::new();
    for item in remote {
        let raw = item.candidate.trim();
        if raw.is_empty() || raw.contains("end-of-candidates") {
            continue;
        }
        let sdp_mid = value_to_mid(&item.sdp_mid);
        let sdp_mline_index = value_to_mline(&item.sdp_mline_index);
        let parts: Vec<&str> = raw.split_whitespace().collect();
        if let Some(client) = parts.get(4).and_then(|address| teredo::parse(address)) {
            for (foundation, port) in [(10, 9002), (11, client.port)] {
                out.push(RemoteCandidate {
                    candidate: format!("candidate:{foundation} 1 UDP 1 {} {port} typ host", client.ipv4),
                    sdp_mid: sdp_mid.clone(),
                    sdp_mline_index,
                });
            }
        }
        out.push(RemoteCandidate {
            candidate: raw.strip_prefix("a=").unwrap_or(raw).to_owned(),
            sdp_mid,
            sdp_mline_index,
        });
    }
    out
}

/// Asks the console for stereo sound, as Greenlight does.
fn request_stereo(sdp: &str) -> String {
    sdp.replacen("useinbandfec=1", "useinbandfec=1; stereo=1", 1)
}

fn local_candidate_json(init: &RTCIceCandidateInit) -> IceCandidate {
    let candidate = if init.candidate.starts_with("candidate:") {
        init.candidate.clone()
    } else {
        format!("candidate:{}", init.candidate)
    };
    IceCandidate {
        candidate,
        sdp_mline_index: json!(init.sdp_mline_index.unwrap_or(0)),
        sdp_mid: json!(init.sdp_mid.clone().unwrap_or_else(|| "0".to_owned())),
    }
}

// ── Peer connection plumbing ────────────────────────────────────────────────

struct Handler {
    shared: Arc<Shared>,
    options: StreamOptions,
    local_candidates: Mutex<Vec<IceCandidate>>,
    gathering_done: Notify,
    connection_changed: Notify,
    connection_state: Mutex<RTCPeerConnectionState>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            self.local_candidates.lock().unwrap().push(local_candidate_json(&init));
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathering_done.notify_one();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        *self.connection_state.lock().unwrap() = state;
        self.shared.emit(SessionEvent::Connection(state.to_string()));
        self.connection_changed.notify_one();
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let kind = track.kind().await;
        let shared = self.shared.clone();
        match kind {
            RtpCodecKind::Video => {
                let mut writer = match &self.options.video_path {
                    Some(path) => match std::fs::File::create(path) {
                        Ok(file) => Some(H26xWriter::new(file, false)),
                        Err(error) => {
                            eprintln!("could not create {}: {error}", path.display());
                            None
                        }
                    },
                    None => None,
                };
                let media = self.options.media.clone();
                tokio::spawn(async move {
                    while let Some(event) = track.poll().await {
                        if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                            shared.counters.video_packets.fetch_add(1, Ordering::Relaxed);
                            shared.counters.video_bytes.fetch_add(packet.payload.len() as u64, Ordering::Relaxed);
                            if let Some(w) = writer.as_mut() {
                                let _ = w.write_rtp(&packet);
                            }
                            if let Some(media) = &media {
                                let _ = media.send(MediaPacket::Video(packet));
                            }
                        }
                    }
                    if let Some(w) = writer.as_mut() {
                        let _ = w.close();
                    }
                });
            }
            RtpCodecKind::Audio => {
                let mut writer = match &self.options.audio_path {
                    Some(path) => match std::fs::File::create(path).map_err(anyhow::Error::from).and_then(|file| {
                        OggWriter::new(file, 48000, 2).map_err(|e| anyhow!("{e}"))
                    }) {
                        Ok(writer) => Some(writer),
                        Err(error) => {
                            eprintln!("could not create {}: {error}", path.display());
                            None
                        }
                    },
                    None => None,
                };
                let media = self.options.media.clone();
                tokio::spawn(async move {
                    while let Some(event) = track.poll().await {
                        if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                            shared.counters.audio_packets.fetch_add(1, Ordering::Relaxed);
                            shared.counters.audio_bytes.fetch_add(packet.payload.len() as u64, Ordering::Relaxed);
                            if let Some(w) = writer.as_mut() {
                                let _ = w.write_rtp(&packet);
                            }
                            if let Some(media) = &media {
                                let _ = media.send(MediaPacket::Audio(packet));
                            }
                        }
                    }
                    if let Some(w) = writer.as_mut() {
                        let _ = w.close();
                    }
                });
            }
            _ => {}
        }
    }
}

fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

fn media_engine() -> Result<MediaEngine> {
    let mut engine = MediaEngine::default();
    engine
        .register_codec(
            RTCRtpCodecParameters { rtp_codec: opus_codec(), payload_type: 111 },
            RtpCodecKind::Audio,
        )
        .map_err(|e| anyhow!("{e}"))?;

    let feedback = vec![
        RTCPFeedback { typ: "goog-remb".to_owned(), parameter: String::new() },
        RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
        RTCPFeedback { typ: "nack".to_owned(), parameter: String::new() },
        RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
    ];
    // Same preference order as Greenlight: main profile, then the two baseline ones.
    for (payload_type, profile) in [(102u8, "4d001f"), (106, "42e01f"), (108, "42001f")] {
        engine
            .register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: RTCRtpCodec {
                        mime_type: MIME_TYPE_H264.to_owned(),
                        clock_rate: 90000,
                        channels: 0,
                        sdp_fmtp_line: format!(
                            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile}"
                        ),
                        rtcp_feedback: feedback.clone(),
                    },
                    payload_type,
                },
                RtpCodecKind::Video,
            )
            .map_err(|e| anyhow!("{e}"))?;
    }
    Ok(engine)
}

fn channel_init(protocol: &str) -> Option<RTCDataChannelInit> {
    Some(RTCDataChannelInit { ordered: true, protocol: protocol.to_owned(), ..Default::default() })
}

/// A connected stream. Dropping it does not end the session; call [`Session::close`].
pub struct Session {
    shared: Arc<Shared>,
    channels: Arc<Channels>,
    peer: Arc<dyn PeerConnection>,
    signaling: Arc<dyn Signaling>,
    events: Option<mpsc::UnboundedReceiver<SessionEvent>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Session {
    /// Builds the peer connection, swaps SDP and ICE through `signaling`, and
    /// waits until the connection is up.
    pub async fn connect(signaling: Arc<dyn Signaling>, options: StreamOptions) -> Result<Session> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            started: Instant::now(),
            pads: Mutex::new([None; 4]),
            mouse: Mutex::new(Vec::new()),
            keys: Mutex::new(Vec::new()),
            ready: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            sequence: AtomicU32::new(0),
            counters: Counters::default(),
            events: events_tx,
        });
        let handler = Arc::new(Handler {
            shared: shared.clone(),
            options,
            local_candidates: Mutex::new(Vec::new()),
            gathering_done: Notify::new(),
            connection_changed: Notify::new(),
            connection_state: Mutex::new(RTCPeerConnectionState::New),
        });

        let mut engine = media_engine()?;
        let registry = register_default_interceptors(Registry::new(), &mut engine).map_err(|e| anyhow!("{e}"))?;
        let peer = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .with_handler(handler.clone() as Arc<dyn PeerConnectionEventHandler>)
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await
            .map_err(|e| anyhow!("could not create the peer connection: {e}"))?;
        let peer: Arc<dyn PeerConnection> = Arc::new(peer);

        // Audio goes both ways (the console expects a microphone line, even
        // though we send nothing on it yet); video only comes to us.
        let microphone = RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(uuid::Uuid::new_v4().as_u128() as u32),
                ..Default::default()
            },
            codec: opus_codec(),
            ..Default::default()
        };
        peer.add_transceiver_from_kind(
            RtpCodecKind::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Sendrecv,
                streams: vec![],
                send_encodings: vec![microphone],
            }),
        )
        .await
        .map_err(|e| anyhow!("could not add the audio line: {e}"))?;
        peer.add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                streams: vec![],
                send_encodings: vec![],
            }),
        )
        .await
        .map_err(|e| anyhow!("could not add the video line: {e}"))?;

        // Same four channels, in the same order, as Greenlight.
        let chat = peer.create_data_channel("chat", channel_init("chatV1")).await.map_err(|e| anyhow!("{e}"))?;
        let control = peer.create_data_channel("control", channel_init("controlV1")).await.map_err(|e| anyhow!("{e}"))?;
        let input = peer.create_data_channel("input", channel_init("1.0")).await.map_err(|e| anyhow!("{e}"))?;
        let message = peer.create_data_channel("message", channel_init("messageV1")).await.map_err(|e| anyhow!("{e}"))?;
        let channels = Arc::new(Channels { control, input, message });

        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(async move { while chat.poll().await.is_some() {} }));
        tasks.push(tokio::spawn(run_control_channel(channels.clone())));
        tasks.push(tokio::spawn(run_input_channel(shared.clone(), channels.clone())));
        tasks.push(tokio::spawn(run_message_channel(shared.clone(), channels.clone())));

        let offer = peer.create_offer(None).await.map_err(|e| anyhow!("could not create the offer: {e}"))?;
        let offer_sdp = request_stereo(&offer.sdp);
        peer.set_local_description(offer).await.map_err(|e| anyhow!("could not apply the offer: {e}"))?;

        let answer_sdp = signaling.exchange_sdp(&offer_sdp).await?;
        let answer = RTCSessionDescription::answer(answer_sdp).map_err(|e| anyhow!("the console's SDP answer is invalid: {e}"))?;
        peer.set_remote_description(answer).await.map_err(|e| anyhow!("could not apply the console's answer: {e}"))?;

        // Give candidate gathering a moment, then send whatever we have.
        let _ = tokio::time::timeout(Duration::from_secs(5), handler.gathering_done.notified()).await;
        let local = handler.local_candidates.lock().unwrap().clone();
        if local.is_empty() {
            bail!("no local network candidates were found; is this device online?");
        }
        let remote = signaling.exchange_ice(&local).await?;
        let remote = expand_remote_candidates(&remote);
        if remote.is_empty() {
            bail!("the console sent no network candidates");
        }
        let mut accepted = 0;
        for candidate in &remote {
            let init = RTCIceCandidateInit {
                candidate: candidate.candidate.clone(),
                sdp_mid: candidate.sdp_mid.clone(),
                sdp_mline_index: candidate.sdp_mline_index,
                ..Default::default()
            };
            match peer.add_ice_candidate(init).await {
                Ok(()) => accepted += 1,
                Err(error) => eprintln!("skipped a candidate from the console ({}): {error}", candidate.candidate),
            }
        }
        if accepted == 0 {
            bail!("none of the console's network candidates could be used");
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let state = *handler.connection_state.lock().unwrap();
            match state {
                RTCPeerConnectionState::Connected => break,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                    let _ = peer.close().await;
                    bail!("could not reach the console (connection {state}). On another network than the console, its router must allow the connection.");
                }
                _ => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let _ = peer.close().await;
                bail!("could not reach the console within 30 seconds");
            }
            let _ = tokio::time::timeout(left, handler.connection_changed.notified()).await;
        }

        tasks.push(tokio::spawn(run_input_loop(shared.clone(), channels.clone())));
        tasks.push(tokio::spawn(run_keepalive(shared.clone(), signaling.clone())));

        Ok(Session { shared, channels, peer, signaling, events: Some(events), tasks })
    }

    /// The next thing that happened, or `None` once the session is gone.
    pub async fn next_event(&mut self) -> Option<SessionEvent> {
        match self.events.as_mut() {
            Some(events) => events.recv().await,
            None => None,
        }
    }

    /// Takes the event stream out, so the session itself can be shared
    /// (for example in an `Arc`) while one task reads the events.
    /// [`Session::next_event`] returns `None` after this.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SessionEvent>> {
        self.events.take()
    }

    /// True once the console has accepted the handshake and input works.
    pub fn is_ready(&self) -> bool {
        self.shared.ready.load(Ordering::Relaxed)
    }

    /// Sets the current state of one gamepad (slot 0..=3). The state is sent
    /// to the console about 60 times a second until it is changed or removed.
    pub async fn set_gamepad(&self, state: GamepadState) -> Result<()> {
        let index = state.index as usize;
        if index > 3 {
            bail!("gamepad slots go from 0 to 3");
        }
        let is_new = {
            let mut pads = self.shared.pads.lock().unwrap();
            let is_new = pads[index].is_none();
            pads[index] = Some(state);
            is_new
        };
        if is_new && self.is_ready() {
            send_json(&self.channels.control, gamepad_changed_message(state.index, true)).await?;
        }
        Ok(())
    }

    /// Unplugs a gamepad slot.
    pub async fn remove_gamepad(&self, index: u8) -> Result<()> {
        let was_there = match self.shared.pads.lock().unwrap().get_mut(index as usize) {
            Some(slot) => slot.take().is_some(),
            None => false,
        };
        if was_there && self.is_ready() {
            send_json(&self.channels.control, gamepad_changed_message(index, false)).await?;
        }
        Ok(())
    }

    /// The gamepad slots that are plugged in right now.
    pub fn gamepads(&self) -> Vec<GamepadState> {
        self.shared.pads.lock().unwrap().iter().flatten().copied().collect()
    }

    /// Queues a real mouse report (for games that support mouse and keyboard).
    pub fn send_mouse(&self, frame: MouseFrame) {
        self.shared.mouse.lock().unwrap().push(frame);
    }

    /// Queues a real key press or release.
    pub fn send_key(&self, frame: KeyboardFrame) {
        self.shared.keys.lock().unwrap().push(frame);
    }

    /// Asks the console for a fresh full video frame.
    pub async fn request_keyframe(&self) -> Result<()> {
        send_json(&self.channels.control, keyframe_request_message()).await
    }

    pub fn stats(&self) -> StreamStats {
        let c = &self.shared.counters;
        StreamStats {
            video_packets: c.video_packets.load(Ordering::Relaxed),
            video_bytes: c.video_bytes.load(Ordering::Relaxed),
            audio_packets: c.audio_packets.load(Ordering::Relaxed),
            audio_bytes: c.audio_bytes.load(Ordering::Relaxed),
            input_packets_sent: c.input_packets_sent.load(Ordering::Relaxed),
        }
    }

    /// Ends the stream and frees the console.
    pub async fn close(self) -> Result<()> {
        self.shutdown().await
    }

    /// Same as [`Session::close`], for a session that is shared. Calling it
    /// more than once does nothing the second time.
    pub async fn shutdown(&self) -> Result<()> {
        if self.shared.closed.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        for task in &self.tasks {
            task.abort();
        }
        let _ = self.peer.close().await;
        self.signaling.stop().await.context("the stream is closed here, but telling the console failed")
    }
}

async fn run_control_channel(channels: Arc<Channels>) {
    while let Some(event) = channels.control.poll().await {
        if matches!(event, DataChannelEvent::OnClose) {
            break;
        }
    }
}

async fn run_input_channel(shared: Arc<Shared>, channels: Arc<Channels>) {
    while let Some(event) = channels.input.poll().await {
        match event {
            DataChannelEvent::OnMessage(message) => match input::parse_server_report(&message.data) {
                Some(ServerReport::Vibration(vibration)) => shared.emit(SessionEvent::Vibration(vibration)),
                Some(ServerReport::VideoSize { width, height }) => shared.emit(SessionEvent::VideoSize { width, height }),
                None => {}
            },
            DataChannelEvent::OnClose => break,
            _ => {}
        }
    }
}

async fn run_message_channel(shared: Arc<Shared>, channels: Arc<Channels>) {
    while let Some(event) = channels.message.poll().await {
        match event {
            DataChannelEvent::OnOpen => {
                if let Err(error) = send_json(&channels.message, handshake_message()).await {
                    eprintln!("could not send the handshake: {error}");
                }
            }
            DataChannelEvent::OnMessage(message) => {
                let Ok(data) = serde_json::from_slice::<Value>(&message.data) else { continue };
                if let Err(error) = handle_message(&shared, &channels, &data).await {
                    eprintln!("could not answer the console: {error}");
                }
            }
            DataChannelEvent::OnClose => break,
            _ => {}
        }
    }
}

async fn handle_message(shared: &Arc<Shared>, channels: &Arc<Channels>, data: &Value) -> Result<()> {
    let kind = data.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "HandshakeAck" => {
            // Same order as Greenlight: authorize, announce pad 0 and take it
            // away again, start the input channel, then describe ourselves.
            send_json(&channels.control, authorization_message()).await?;
            send_json(&channels.control, gamepad_changed_message(0, true)).await?;
            send_json(&channels.control, gamepad_changed_message(0, false)).await?;
            let first = input::client_metadata_packet(0, shared.now_ms(), 1);
            channels.input.send(BytesMut::from(&first[..])).await.map_err(|e| anyhow!("{e}"))?;
            for message in config_messages() {
                send_json(&channels.message, message).await?;
            }
            // Announce the pads that were plugged in before the handshake.
            let plugged: Vec<u8> = shared.pads.lock().unwrap().iter().flatten().map(|p| p.index).collect();
            for index in plugged {
                send_json(&channels.control, gamepad_changed_message(index, true)).await?;
            }
            shared.ready.store(true, Ordering::Relaxed);
            shared.emit(SessionEvent::Ready);
        }
        "TransactionStart" | "Message" => {
            let target = data.get("target").and_then(Value::as_str).unwrap_or("").to_owned();
            let content = data.get("content").and_then(Value::as_str).unwrap_or("").to_owned();
            let id = data.get("id").cloned().unwrap_or(Value::Null);
            match target.as_str() {
                "/streaming/sessionLifetimeManagement/serverInitiatedDisconnect" => {
                    send_json(&channels.message, transaction_complete_message(&id, json!(""))).await?;
                    shared.emit(SessionEvent::Disconnected(content));
                }
                "/streaming/properties/titleinfo" => shared.emit(SessionEvent::TitleInfo(content)),
                "/streaming/systemUi/messages/ShowMessageDialog" => {
                    // Answer "OK" so the console does not wait on a dialog nobody can see.
                    send_json(&channels.message, transaction_complete_message(&id, json!({ "Result": 0 }))).await?;
                    shared.emit(SessionEvent::ServerMessage { target, content });
                }
                _ => shared.emit(SessionEvent::ServerMessage { target, content }),
            }
        }
        _ => {}
    }
    Ok(())
}

/// Sends the current input about 60 times a second.
async fn run_input_loop(shared: Arc<Shared>, channels: Arc<Channels>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(16));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if shared.closed.load(Ordering::Relaxed) {
            break;
        }
        if !shared.ready.load(Ordering::Relaxed) {
            continue;
        }
        let pads: Vec<GamepadState> = shared.pads.lock().unwrap().iter().flatten().copied().collect();
        let mouse = std::mem::take(&mut *shared.mouse.lock().unwrap());
        let keys = std::mem::take(&mut *shared.keys.lock().unwrap());
        let sequence = shared.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let Some(packet) = input::input_packet(sequence, shared.now_ms(), &[], &pads, &mouse, &keys) else {
            shared.sequence.fetch_sub(1, Ordering::Relaxed);
            continue;
        };
        if channels.input.send(BytesMut::from(&packet[..])).await.is_ok() {
            shared.counters.input_packets_sent.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn run_keepalive(shared: Arc<Shared>, signaling: Arc<dyn Signaling>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if shared.closed.load(Ordering::Relaxed) {
            break;
        }
        if let Err(error) = signaling.keepalive().await {
            eprintln!("keepalive failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(text: &str) -> IceCandidate {
        IceCandidate { candidate: text.to_owned(), sdp_mline_index: json!("0"), sdp_mid: json!("0") }
    }

    #[test]
    fn remote_candidates_lose_the_a_prefix() {
        let out = expand_remote_candidates(&[candidate("a=candidate:1 1 UDP 100 192.168.1.20 9002 typ host ")]);
        assert_eq!(
            out,
            vec![RemoteCandidate {
                candidate: "candidate:1 1 UDP 100 192.168.1.20 9002 typ host".to_owned(),
                sdp_mid: Some("0".to_owned()),
                sdp_mline_index: Some(0),
            }]
        );
    }

    #[test]
    fn end_of_candidates_is_dropped() {
        let out = expand_remote_candidates(&[candidate("a=end-of-candidates"), candidate("")]);
        assert!(out.is_empty());
    }

    #[test]
    fn teredo_candidates_add_the_hidden_ipv4_address() {
        let out = expand_remote_candidates(&[candidate(
            "a=candidate:2 1 UDP 50 2001:0000:4136:e378:8000:63bf:3fff:fdd2 3074 typ host ",
        )]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].candidate, "candidate:10 1 UDP 1 192.0.2.45 9002 typ host");
        assert_eq!(out[1].candidate, "candidate:11 1 UDP 1 192.0.2.45 40000 typ host");
        assert!(out[2].candidate.starts_with("candidate:2 1 UDP 50 2001:"));
        assert!(out.iter().all(|c| c.sdp_mline_index == Some(0)));
    }

    #[test]
    fn mid_and_mline_accept_numbers_and_strings() {
        assert_eq!(value_to_mid(&json!(0)), Some("0".to_owned()));
        assert_eq!(value_to_mid(&json!("audio")), Some("audio".to_owned()));
        assert_eq!(value_to_mid(&Value::Null), None);
        assert_eq!(value_to_mline(&json!(1)), Some(1));
        assert_eq!(value_to_mline(&json!("2")), Some(2));
        assert_eq!(value_to_mline(&json!("x")), None);
    }

    #[test]
    fn stereo_is_requested_once() {
        let sdp = "a=fmtp:111 minptime=10;useinbandfec=1\r\na=fmtp:63 useinbandfec=1\r\n";
        assert_eq!(
            request_stereo(sdp),
            "a=fmtp:111 minptime=10;useinbandfec=1; stereo=1\r\na=fmtp:63 useinbandfec=1\r\n"
        );
        assert_eq!(request_stereo("v=0\r\n"), "v=0\r\n");
    }

    #[test]
    fn local_candidates_get_the_browser_shape() {
        let init = RTCIceCandidateInit {
            candidate: "candidate:1 1 udp 2130706431 192.168.1.5 50000 typ host".to_owned(),
            sdp_mid: Some("0".to_owned()),
            sdp_mline_index: Some(0),
            ..Default::default()
        };
        let json = serde_json::to_value(local_candidate_json(&init)).unwrap();
        assert_eq!(json["candidate"], "candidate:1 1 udp 2130706431 192.168.1.5 50000 typ host");
        assert_eq!(json["sdpMid"], "0");
        assert_eq!(json["sdpMLineIndex"], 0);
    }

    #[test]
    fn protocol_messages_match_greenlight() {
        let handshake: Value = serde_json::from_str(&handshake_message()).unwrap();
        assert_eq!(handshake["type"], "Handshake");
        assert_eq!(handshake["version"], "messageV1");

        let auth: Value = serde_json::from_str(&authorization_message()).unwrap();
        assert_eq!(auth["message"], "authorizationRequest");
        assert_eq!(auth["accessKey"], "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E");

        let pad: Value = serde_json::from_str(&gamepad_changed_message(2, true)).unwrap();
        assert_eq!(pad, json!({ "message": "gamepadChanged", "gamepadIndex": 2, "wasAdded": true }));

        let keyframe: Value = serde_json::from_str(&keyframe_request_message()).unwrap();
        assert_eq!(keyframe["message"], "videoKeyframeRequested");
    }

    #[test]
    fn config_messages_wrap_their_content_as_a_json_string() {
        let messages = config_messages();
        assert_eq!(messages.len(), 6);
        let last: Value = serde_json::from_str(&messages[5]).unwrap();
        assert_eq!(last["type"], "Message");
        assert_eq!(last["target"], "/streaming/characteristics/dimensionschanged");
        let content: Value = serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["preferredWidth"], 1920);
        assert!(uuid::Uuid::parse_str(last["id"].as_str().unwrap()).is_ok());
    }

    #[test]
    fn transactions_are_completed_with_the_same_id() {
        let done: Value = serde_json::from_str(&transaction_complete_message(&json!("abc"), json!({ "Result": 0 }))).unwrap();
        assert_eq!(done["type"], "TransactionComplete");
        assert_eq!(done["id"], "abc");
        assert_eq!(done["content"], "{\"Result\":0}");
        let empty: Value = serde_json::from_str(&transaction_complete_message(&json!("x"), json!(""))).unwrap();
        assert_eq!(empty["content"], "\"\"");
    }
}
