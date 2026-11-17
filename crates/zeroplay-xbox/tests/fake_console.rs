//! Runs a whole stream session against a fake console in the same process.
//!
//! The fake console is a second peer connection that behaves the way
//! Greenlight's code expects a real Xbox to behave: it answers the offer,
//! writes its candidates with the `a=` prefix, acknowledges the handshake,
//! and sends a rumble command, the video size and a dialog. Everything the
//! session sends is recorded so the test can check it byte for byte.
//!
//! This proves the session code end to end over real UDP sockets. It cannot
//! prove that a real Xbox accepts it; that needs a real console.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use rtc::interceptor::Registry;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::transport::RTCIceCandidateInit;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use serde_json::{json, Value};
use tokio::sync::Notify;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionIceEvent,
};
use zeroplay_xbox::api::IceCandidate;
use zeroplay_xbox::input::{button, GamepadState, KeyboardFrame, MouseFrame, Vibration};
use zeroplay_xbox::session::{Session, SessionEvent, Signaling, StreamOptions};

const VIDEO_SSRC: u32 = 0x5a_70_1a_79;
/// The payload type the session offers for H.264 baseline (42e01f).
const VIDEO_PAYLOAD_TYPE: u8 = 106;

#[derive(Default)]
struct Recorded {
    /// (channel label, channel protocol, payload)
    messages: Mutex<Vec<(String, String, Vec<u8>)>>,
}

impl Recorded {
    fn on(&self, label: &str) -> Vec<Vec<u8>> {
        self.messages.lock().unwrap().iter().filter(|m| m.0 == label).map(|m| m.2.clone()).collect()
    }
    fn json_on(&self, label: &str) -> Vec<Value> {
        self.on(label).iter().filter_map(|bytes| serde_json::from_slice(bytes).ok()).collect()
    }
    fn protocol_of(&self, label: &str) -> Option<String> {
        self.messages.lock().unwrap().iter().find(|m| m.0 == label).map(|m| m.1.clone())
    }
}

struct ConsoleHandler {
    recorded: Arc<Recorded>,
    candidates: Mutex<Vec<String>>,
    gathering_done: Notify,
    input_channel: Mutex<Option<Arc<dyn DataChannel>>>,
    message_channel: Mutex<Option<Arc<dyn DataChannel>>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for ConsoleHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            self.candidates.lock().unwrap().push(init.candidate);
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathering_done.notify_one();
        }
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let label = channel.label().await.unwrap_or_default();
        let protocol = channel.protocol().await.unwrap_or_default();
        match label.as_str() {
            "input" => *self.input_channel.lock().unwrap() = Some(channel.clone()),
            "message" => *self.message_channel.lock().unwrap() = Some(channel.clone()),
            _ => {}
        }
        let recorded = self.recorded.clone();
        tokio::spawn(async move {
            while let Some(event) = channel.poll().await {
                match event {
                    DataChannelEvent::OnMessage(message) => {
                        let payload = message.data.to_vec();
                        recorded.messages.lock().unwrap().push((label.clone(), protocol.clone(), payload.clone()));
                        if label == "message" {
                            if let Ok(value) = serde_json::from_slice::<Value>(&payload) {
                                if value["type"] == "Handshake" {
                                    // A real console answers with text, which is what Greenlight parses.
                                    let ack = json!({ "type": "HandshakeAck", "version": "messageV1", "id": value["id"], "cv": "" });
                                    let _ = channel.send_text(&ack.to_string()).await;
                                }
                            }
                        }
                    }
                    DataChannelEvent::OnClose => break,
                    _ => {}
                }
            }
        });
    }
}

struct FakeConsole {
    peer: Arc<dyn PeerConnection>,
    handler: Arc<ConsoleHandler>,
    recorded: Arc<Recorded>,
    offer_seen: Mutex<Option<String>>,
    video_track: Arc<TrackLocalStaticRTP>,
    keepalives: AtomicBool,
    stopped: AtomicBool,
}

impl FakeConsole {
    async fn new() -> Result<Arc<Self>> {
        let recorded = Arc::new(Recorded::default());
        let handler = Arc::new(ConsoleHandler {
            recorded: recorded.clone(),
            candidates: Mutex::new(Vec::new()),
            gathering_done: Notify::new(),
            input_channel: Mutex::new(None),
            message_channel: Mutex::new(None),
        });
        let mut engine = MediaEngine::default();
        engine.register_default_codecs().map_err(|e| anyhow!("{e}"))?;
        let registry = register_default_interceptors(Registry::new(), &mut engine).map_err(|e| anyhow!("{e}"))?;
        let peer = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .with_handler(handler.clone() as Arc<dyn PeerConnectionEventHandler>)
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let video_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            "console-stream".to_owned(),
            "console-video".to_owned(),
            "console-video".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(VIDEO_SSRC), ..Default::default() },
                codec: RTCRtpCodec {
                    mime_type: "video/H264".to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        )));
        Ok(Arc::new(Self {
            video_track,
            peer: Arc::new(peer),
            handler,
            recorded,
            offer_seen: Mutex::new(None),
            keepalives: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }))
    }

    fn channel(&self, slot: &Mutex<Option<Arc<dyn DataChannel>>>) -> Arc<dyn DataChannel> {
        slot.lock().unwrap().clone().expect("the session should have opened this channel")
    }

    /// Sends one tiny H.264 picture (SPS, PPS and a keyframe slice), each in its own RTP packet.
    async fn send_video_picture(&self, first_sequence: u16, timestamp: u32) {
        let nals: [&[u8]; 3] = [
            &[0x67, 0x42, 0xe0, 0x1f, 0x96, 0x54, 0x05, 0x01, 0xed, 0x00],
            &[0x68, 0xce, 0x3c, 0x80],
            &[0x65, 0x88, 0x84, 0x00, 0x10, 0xff, 0xfe, 0xf6, 0xf0, 0xfe],
        ];
        for (i, nal) in nals.iter().enumerate() {
            let packet = rtc::rtp::Packet {
                header: rtc::rtp::header::Header {
                    version: 2,
                    marker: i == nals.len() - 1,
                    payload_type: VIDEO_PAYLOAD_TYPE,
                    sequence_number: first_sequence + i as u16,
                    timestamp,
                    ssrc: VIDEO_SSRC,
                    ..Default::default()
                },
                payload: bytes::Bytes::copy_from_slice(nal),
            };
            self.video_track.write_rtp(packet).await.expect("the fake console should be able to send video");
        }
    }

    async fn send_on_input(&self, bytes: &[u8]) {
        self.channel(&self.handler.input_channel).send(BytesMut::from(bytes)).await.unwrap();
    }

    async fn send_on_message(&self, value: Value) {
        self.channel(&self.handler.message_channel).send_text(&value.to_string()).await.unwrap();
    }
}

#[async_trait::async_trait]
impl Signaling for FakeConsole {
    async fn exchange_sdp(&self, offer_sdp: &str) -> Result<String> {
        *self.offer_seen.lock().unwrap() = Some(offer_sdp.to_owned());
        let offer = RTCSessionDescription::offer(offer_sdp.to_owned()).map_err(|e| anyhow!("{e}"))?;
        self.peer.set_remote_description(offer).await.map_err(|e| anyhow!("{e}"))?;
        self.peer.add_track(self.video_track.clone() as Arc<dyn TrackLocal>).await.map_err(|e| anyhow!("{e}"))?;
        let answer = self.peer.create_answer(None).await.map_err(|e| anyhow!("{e}"))?;
        let sdp = answer.sdp.clone();
        self.peer.set_local_description(answer).await.map_err(|e| anyhow!("{e}"))?;
        Ok(sdp)
    }

    async fn exchange_ice(&self, candidates: &[IceCandidate]) -> Result<Vec<IceCandidate>> {
        for candidate in candidates {
            assert!(candidate.candidate.starts_with("candidate:"), "local candidates use the browser form");
            self.peer
                .add_ice_candidate(RTCIceCandidateInit {
                    candidate: candidate.candidate.clone(),
                    sdp_mid: Some("0".to_owned()),
                    sdp_mline_index: Some(0),
                    ..Default::default()
                })
                .await
                .map_err(|e| anyhow!("{e}"))?;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.handler.gathering_done.notified()).await;
        // Write candidates the way the console does: "a=" in front, a space at
        // the end, string indexes, and an end-of-candidates marker.
        let mut out: Vec<IceCandidate> = self
            .handler
            .candidates
            .lock()
            .unwrap()
            .iter()
            .map(|candidate| IceCandidate {
                candidate: format!("a={candidate} "),
                sdp_mline_index: json!("0"),
                sdp_mid: json!("0"),
            })
            .collect();
        out.push(IceCandidate { candidate: "a=end-of-candidates".to_owned(), sdp_mline_index: json!("0"), sdp_mid: json!("0") });
        Ok(out)
    }

    async fn keepalive(&self) -> Result<()> {
        self.keepalives.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.stopped.store(true, Ordering::Relaxed);
        Ok(())
    }
}

async fn wait_for_event(session: &mut Session, what: &str, matches: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
    let wait = async {
        loop {
            match session.next_event().await {
                Some(event) if matches(&event) => return event,
                Some(_) => {}
                None => panic!("the session ended while waiting for {what}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(15), wait).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn wait_until(what: &str, check: impl Fn() -> bool) {
    for _ in 0..300 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Splits an input packet into (report type, sequence, body).
fn split_packet(packet: &[u8]) -> (u16, u32, &[u8]) {
    (
        u16::from_le_bytes([packet[0], packet[1]]),
        u32::from_le_bytes([packet[2], packet[3], packet[4], packet[5]]),
        &packet[14..],
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_session_against_a_fake_console() {
    let console = FakeConsole::new().await.unwrap();
    let video_path = std::env::temp_dir().join(format!("zeroplay-test-{}.h264", std::process::id()));
    let options = StreamOptions { video_path: Some(video_path.clone()), audio_path: None };
    let mut session = Session::connect(console.clone(), options).await.expect("the session should connect");

    // ── The offer ────────────────────────────────────────────────────────
    let offer = console.offer_seen.lock().unwrap().clone().unwrap();
    assert!(offer.contains("useinbandfec=1; stereo=1"), "the offer should ask for stereo");
    assert!(offer.contains("H264/90000"), "the offer should carry H.264");
    assert!(offer.contains("profile-level-id=4d001f"));
    assert!(offer.contains("m=audio") && offer.contains("m=video") && offer.contains("m=application"));
    assert!(offer.contains("a=sendrecv"), "audio goes both ways");
    assert!(offer.contains("a=recvonly"), "video only comes to us");
    assert!(offer.contains(&format!("a=rtpmap:{VIDEO_PAYLOAD_TYPE} H264/90000")));

    // ── Handshake ────────────────────────────────────────────────────────
    wait_for_event(&mut session, "the handshake", |e| *e == SessionEvent::Ready).await;
    assert!(session.is_ready());

    let recorded = console.recorded.clone();
    wait_until("the six config messages", || recorded.json_on("message").len() >= 7).await;
    let on_message = recorded.json_on("message");
    assert_eq!(on_message[0]["type"], "Handshake");
    assert_eq!(on_message[0]["version"], "messageV1");
    let targets: Vec<&str> = on_message[1..7].iter().map(|m| m["target"].as_str().unwrap()).collect();
    assert_eq!(
        targets,
        [
            "/streaming/systemUi/configuration",
            "/streaming/properties/clientappinstallidchanged",
            "/streaming/characteristics/orientationchanged",
            "/streaming/characteristics/touchinputenabledchanged",
            "/streaming/characteristics/clientdevicecapabilities",
            "/streaming/characteristics/dimensionschanged",
        ]
    );

    let on_control = recorded.json_on("control");
    assert_eq!(on_control[0]["message"], "authorizationRequest");
    assert_eq!(on_control[1], json!({ "message": "gamepadChanged", "gamepadIndex": 0, "wasAdded": true }));
    assert_eq!(on_control[2], json!({ "message": "gamepadChanged", "gamepadIndex": 0, "wasAdded": false }));

    let on_input = recorded.on("input");
    assert_eq!(on_input[0].len(), 15, "the first input packet is the client metadata");
    assert_eq!(split_packet(&on_input[0]).0, 8);
    assert_eq!(on_input[0][14], 1);
    assert_eq!(on_input.len(), 1, "nothing else is sent while no gamepad is plugged in");

    // The channels carry the protocol names the console looks for.
    assert_eq!(recorded.protocol_of("control").as_deref(), Some("controlV1"));
    assert_eq!(recorded.protocol_of("input").as_deref(), Some("1.0"));
    assert_eq!(recorded.protocol_of("message").as_deref(), Some("messageV1"));

    // ── Two gamepads at once ─────────────────────────────────────────────
    let mut pad0 = GamepadState::new(0);
    pad0.buttons = button::A;
    pad0.left_x = 1.0;
    let mut pad1 = GamepadState::new(1);
    pad1.buttons = button::B | button::DPAD_UP;
    pad1.right_trigger = 1.0;
    session.set_gamepad(pad0).await.unwrap();
    session.set_gamepad(pad1).await.unwrap();

    wait_until("both gamepads in one packet", || {
        recorded.on("input").iter().any(|p| split_packet(p).0 == 2 && p.len() == 14 + 1 + 46)
    })
    .await;
    let on_control = recorded.json_on("control");
    assert!(on_control.contains(&json!({ "message": "gamepadChanged", "gamepadIndex": 0, "wasAdded": true })));
    assert!(on_control.contains(&json!({ "message": "gamepadChanged", "gamepadIndex": 1, "wasAdded": true })));

    let packets = recorded.on("input");
    let both = packets.iter().find(|p| split_packet(p).0 == 2 && p.len() == 14 + 1 + 46).unwrap();
    let (_, _, body) = split_packet(both);
    assert_eq!(body[0], 2, "two gamepads");
    assert_eq!(body[1], 0, "first pad is slot 0");
    assert_eq!(u16::from_le_bytes([body[2], body[3]]), button::A);
    assert_eq!(i16::from_le_bytes([body[4], body[5]]), 32767, "left stick fully right");
    assert_eq!(body[24], 1, "second pad is slot 1");
    assert_eq!(u16::from_le_bytes([body[25], body[26]]), button::B | button::DPAD_UP);
    assert_eq!(u16::from_le_bytes([body[37], body[38]]), 65535, "right trigger fully pressed");

    // Sequence numbers only go up.
    let sequences: Vec<u32> = recorded.on("input").iter().skip(1).map(|p| split_packet(p).1).collect();
    assert!(sequences.windows(2).all(|pair| pair[1] > pair[0]), "sequences should increase: {sequences:?}");
    assert_eq!(sequences[0], 1);

    // Unplugging tells the console and stops sending that pad.
    session.remove_gamepad(1).await.unwrap();
    wait_until("the unplug message", || {
        recorded.json_on("control").contains(&json!({ "message": "gamepadChanged", "gamepadIndex": 1, "wasAdded": false }))
    })
    .await;
    assert_eq!(session.gamepads().len(), 1);

    // ── Real mouse and keyboard ──────────────────────────────────────────
    session.send_mouse(MouseFrame { x: 12, y: -7, wheel_x: 0, wheel_y: 0, buttons: 1, absolute: false });
    session.send_key(KeyboardFrame { pressed: true, key_code: 0x57 });
    wait_until("a packet with mouse and keyboard", || {
        recorded.on("input").iter().any(|p| split_packet(p).0 == (2 | 32 | 64))
    })
    .await;
    let packets = recorded.on("input");
    let mixed = packets.iter().find(|p| split_packet(p).0 == (2 | 32 | 64)).unwrap();
    let (_, _, body) = split_packet(mixed);
    assert_eq!(body.len(), (1 + 23) + (1 + 18) + (1 + 3));
    let mouse = &body[24..];
    assert_eq!(mouse[0], 1);
    assert_eq!(i32::from_le_bytes([mouse[1], mouse[2], mouse[3], mouse[4]]), 12);
    assert_eq!(i32::from_le_bytes([mouse[5], mouse[6], mouse[7], mouse[8]]), -7);
    assert_eq!(mouse[17], 1, "left button held");
    assert_eq!(mouse[18], 0, "relative movement");
    assert_eq!(&body[24 + 19..], &[1, 2, 1, 0x57]);
    // Mouse and keys are sent once, not repeated.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(recorded.on("input").iter().filter(|p| split_packet(p).0 & 32 != 0).count(), 1);

    // ── Things the console sends ─────────────────────────────────────────
    console.send_on_input(&[128, 0, 0, 0, 50, 100, 0, 0, 0xf4, 0x01, 0, 0, 1]).await;
    let event = wait_for_event(&mut session, "the rumble command", |e| matches!(e, SessionEvent::Vibration(_))).await;
    assert_eq!(
        event,
        SessionEvent::Vibration(Vibration {
            gamepad_index: 0,
            left_motor: 0.5,
            right_motor: 1.0,
            left_trigger_motor: 0.0,
            right_trigger_motor: 0.0,
            duration_ms: 500,
            delay_ms: 0,
            repeat: 1,
        })
    );

    let mut size = vec![16, 0];
    size.extend_from_slice(&1080u32.to_le_bytes());
    size.extend_from_slice(&1920u32.to_le_bytes());
    console.send_on_input(&size).await;
    let event = wait_for_event(&mut session, "the video size", |e| matches!(e, SessionEvent::VideoSize { .. })).await;
    assert_eq!(event, SessionEvent::VideoSize { width: 1920, height: 1080 });

    console
        .send_on_message(json!({ "type": "Message", "target": "/streaming/properties/titleinfo", "id": "t1", "content": "{\"titleid\":\"123\"}" }))
        .await;
    let event = wait_for_event(&mut session, "the title info", |e| matches!(e, SessionEvent::TitleInfo(_))).await;
    assert_eq!(event, SessionEvent::TitleInfo("{\"titleid\":\"123\"}".to_owned()));

    console
        .send_on_message(json!({
            "type": "TransactionStart",
            "target": "/streaming/systemUi/messages/ShowMessageDialog",
            "id": "dialog-7",
            "content": "{\"TitleText\":\"Hello\",\"ContentText\":\"World\"}",
        }))
        .await;
    wait_until("the dialog to be answered", || {
        recorded.json_on("message").iter().any(|m| m["type"] == "TransactionComplete" && m["id"] == "dialog-7")
    })
    .await;
    let answers = recorded.json_on("message");
    let answer = answers.iter().find(|m| m["id"] == "dialog-7").unwrap();
    assert_eq!(answer["content"], "{\"Result\":0}");

    session.request_keyframe().await.unwrap();
    wait_until("the keyframe request", || {
        recorded.json_on("control").iter().any(|m| m["message"] == "videoKeyframeRequested")
    })
    .await;

    // ── Video ────────────────────────────────────────────────────────────
    for picture in 0..5u16 {
        console.send_video_picture(100 + picture * 3, 90000 + picture as u32 * 1500).await;
    }
    for _ in 0..300 {
        if session.stats().video_packets >= 15 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stats = session.stats();
    assert_eq!(stats.video_packets, 15, "every video packet should arrive");
    assert_eq!(stats.video_bytes, 5 * (10 + 4 + 10));

    // ── The console ends the stream ──────────────────────────────────────
    console
        .send_on_message(json!({
            "type": "TransactionStart",
            "target": "/streaming/sessionLifetimeManagement/serverInitiatedDisconnect",
            "id": "bye-1",
            "content": "{\"reason\":\"test\"}",
        }))
        .await;
    let event = wait_for_event(&mut session, "the disconnect", |e| matches!(e, SessionEvent::Disconnected(_))).await;
    assert_eq!(event, SessionEvent::Disconnected("{\"reason\":\"test\"}".to_owned()));
    wait_until("the disconnect to be acknowledged", || {
        recorded.json_on("message").iter().any(|m| m["type"] == "TransactionComplete" && m["id"] == "bye-1")
    })
    .await;

    assert!(session.stats().input_packets_sent > 0);
    session.close().await.unwrap();
    assert!(console.stopped.load(Ordering::Relaxed), "closing should tell the console");
    let _ = console.peer.close().await;

    // The saved file is a playable H.264 stream: start codes, then SPS, PPS and the keyframe.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let saved = std::fs::read(&video_path).expect("the video file should exist");
    let _ = std::fs::remove_file(&video_path);
    assert!(saved.starts_with(&[0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f]), "file should start with the SPS: {:02x?}", &saved[..saved.len().min(16)]);
    let start_codes = saved.windows(4).filter(|w| *w == [0, 0, 0, 1]).count();
    assert_eq!(start_codes, 15, "five pictures of three NAL units each");
}
