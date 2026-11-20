//! One player's WebRTC connection: the console's video and sound are passed
//! on to it unchanged. Only the SSRC and payload type are rewritten to what
//! this browser agreed to.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS,
};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::{broadcast, Notify};
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use zeroplay_xbox::session::MediaPacket;

use crate::app::App;
use crate::sdp::{pick_codecs, BrowserCodecs};

pub struct ViewerPeer {
    peer: Arc<dyn PeerConnection>,
    forwarder: tokio::task::JoinHandle<()>,
    port: u16,
    app: Arc<App>,
}

impl ViewerPeer {
    pub async fn close(self) {
        self.forwarder.abort();
        let _ = self.peer.close().await;
        self.app.return_port(self.port);
    }
}

struct Handler {
    app: Arc<App>,
    gathered: Notify,
    state: Mutex<RTCPeerConnectionState>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathered.notify_one();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        *self.state.lock().unwrap() = state;
        if state == RTCPeerConnectionState::Connected {
            // A new picture so this player does not wait for the next full frame.
            self.app.request_keyframe();
        }
    }
}

fn h264_codec(fmtp: &str) -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: fmtp.to_owned(),
        rtcp_feedback: vec![
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: String::new(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "pli".to_owned(),
            },
            RTCPFeedback {
                typ: "ccm".to_owned(),
                parameter: "fir".to_owned(),
            },
        ],
    }
}

fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

fn track(kind: RtpCodecKind, ssrc: u32, codec: RTCRtpCodec) -> Arc<TrackLocalStaticRTP> {
    Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
        "zeroplay".to_owned(),
        format!("zeroplay-{kind}"),
        format!("zeroplay-{kind}"),
        kind,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec,
            ..Default::default()
        }],
    )))
}

/// Answers a browser's offer and starts passing the stream on to it.
pub async fn connect(app: &Arc<App>, offer_sdp: String) -> Result<(ViewerPeer, String)> {
    let codecs = pick_codecs(&offer_sdp)?;
    let port = app
        .take_port()
        .ok_or_else(|| anyhow!("too many people are connected; free media ports: none"))?;
    match build(app, offer_sdp, &codecs, port).await {
        Ok(result) => Ok(result),
        Err(error) => {
            app.return_port(port);
            Err(error)
        }
    }
}

async fn build(
    app: &Arc<App>,
    offer_sdp: String,
    codecs: &BrowserCodecs,
    port: u16,
) -> Result<(ViewerPeer, String)> {
    let mut engine = MediaEngine::default();
    engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: h264_codec(&codecs.video_fmtp),
                payload_type: codecs.video_pt,
            },
            RtpCodecKind::Video,
        )
        .map_err(|e| anyhow!("{e}"))?;
    engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: opus_codec(),
                payload_type: codecs.audio_pt,
            },
            RtpCodecKind::Audio,
        )
        .map_err(|e| anyhow!("{e}"))?;
    let registry =
        register_default_interceptors(Registry::new(), &mut engine).map_err(|e| anyhow!("{e}"))?;

    let handler = Arc::new(Handler {
        app: app.clone(),
        gathered: Notify::new(),
        state: Mutex::new(RTCPeerConnectionState::New),
    });
    let peer = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .with_handler(handler.clone() as Arc<dyn PeerConnectionEventHandler>)
        .with_udp_addrs(vec![format!("0.0.0.0:{port}")])
        .build()
        .await
        .map_err(|e| anyhow!("could not create the player connection: {e}"))?;
    let peer: Arc<dyn PeerConnection> = Arc::new(peer);

    let video_ssrc = uuid::Uuid::new_v4().as_u128() as u32;
    let audio_ssrc = uuid::Uuid::new_v4().as_u128() as u32;
    let video = track(
        RtpCodecKind::Video,
        video_ssrc,
        h264_codec(&codecs.video_fmtp),
    );
    let audio = track(RtpCodecKind::Audio, audio_ssrc, opus_codec());
    let setup = async {
        peer.add_track(video.clone() as Arc<dyn TrackLocal>)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        peer.add_track(audio.clone() as Arc<dyn TrackLocal>)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let offer = RTCSessionDescription::offer(offer_sdp)
            .map_err(|e| anyhow!("the browser's offer is invalid: {e}"))?;
        peer.set_remote_description(offer)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let answer = peer.create_answer(None).await.map_err(|e| anyhow!("{e}"))?;
        peer.set_local_description(answer)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let _ = tokio::time::timeout(Duration::from_secs(3), handler.gathered.notified()).await;
        let local = peer
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no answer was made"))?;
        anyhow::Ok(local.sdp)
    };
    let answer = match setup.await {
        Ok(answer) => answer,
        Err(error) => {
            let _ = peer.close().await;
            return Err(error);
        }
    };

    let forwarder = tokio::spawn(forward(
        app.clone(),
        app.media.subscribe(),
        video,
        audio,
        (video_ssrc, codecs.video_pt),
        (audio_ssrc, codecs.audio_pt),
    ));

    Ok((
        ViewerPeer {
            peer,
            forwarder,
            port,
            app: app.clone(),
        },
        answer,
    ))
}

/// Takes the console's packets and writes them to this player's tracks.
async fn forward(
    app: Arc<App>,
    mut media: broadcast::Receiver<MediaPacket>,
    video: Arc<TrackLocalStaticRTP>,
    audio: Arc<TrackLocalStaticRTP>,
    (video_ssrc, video_pt): (u32, u8),
    (audio_ssrc, audio_pt): (u32, u8),
) {
    loop {
        let (mut packet, track, ssrc, pt) = match media.recv().await {
            Ok(MediaPacket::Video(packet)) => (packet, &video, video_ssrc, video_pt),
            Ok(MediaPacket::Audio(packet)) => (packet, &audio, audio_ssrc, audio_pt),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // This player fell behind and lost packets: ask for a fresh picture.
                app.request_keyframe();
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        packet.header.ssrc = ssrc;
        packet.header.payload_type = pt;
        // The console's header extensions were not agreed with this browser.
        packet.header.extension = false;
        packet.header.extension_profile = 0;
        packet.header.extensions.clear();
        // Fails until the browser has connected; those packets are simply dropped.
        let _ = track.write_rtp(packet).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

    use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
    use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

    use crate::config::Config;

    /// Plays the browser: receives video and audio and counts the packets.
    struct Browser {
        gathered: Notify,
        video_packets: Arc<AtomicU64>,
        audio_packets: Arc<AtomicU64>,
        video_pt: Arc<AtomicU8>,
    }

    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for Browser {
        async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
            if state == RTCIceGatheringState::Complete {
                self.gathered.notify_one();
            }
        }
        async fn on_track(&self, track: Arc<dyn TrackRemote>) {
            let kind = track.kind().await;
            let counter = if kind == RtpCodecKind::Video {
                self.video_packets.clone()
            } else {
                self.audio_packets.clone()
            };
            let video_pt = self.video_pt.clone();
            tokio::spawn(async move {
                while let Some(event) = track.poll().await {
                    if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                        if kind == RtpCodecKind::Video {
                            video_pt.store(packet.header.payload_type, Ordering::Relaxed);
                        }
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    }

    fn console_packet(video: bool, sequence: u16) -> MediaPacket {
        let mut packet = rtc::rtp::Packet::default();
        packet.header.version = 2;
        packet.header.sequence_number = sequence;
        packet.header.timestamp = sequence as u32 * 1500;
        packet.header.ssrc = 1234;
        packet.header.marker = true;
        if video {
            // Payload type the console uses, different from the browser's.
            packet.header.payload_type = 102;
            // A tiny H.264 NAL unit (an SPS header byte and some data).
            packet.payload = bytes::Bytes::from_static(&[0x67, 0x4d, 0x00, 0x1f, 0x95, 0xa8]);
            MediaPacket::Video(packet)
        } else {
            packet.header.payload_type = 111;
            packet.payload = bytes::Bytes::from_static(&[0xfc, 0xff, 0xfe]);
            MediaPacket::Audio(packet)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn console_packets_reach_the_browser_with_its_payload_type() {
        let data_dir = std::env::temp_dir().join(format!("zeroplay-test-{}", uuid::Uuid::new_v4()));
        let config = Config {
            http_addr: "127.0.0.1:0".parse().unwrap(),
            remote_addr: "127.0.0.1:0".parse().unwrap(),
            data_dir: data_dir.clone(),
            rtc_ports: vec![0],
        };
        let app = App::new(config).await.unwrap();

        // The browser side, offering main-profile H.264 on payload type 108.
        let mut engine = MediaEngine::default();
        engine
            .register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: h264_codec(
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f",
                    ),
                    payload_type: 108,
                },
                RtpCodecKind::Video,
            )
            .unwrap();
        engine
            .register_codec(
                RTCRtpCodecParameters {
                    rtp_codec: opus_codec(),
                    payload_type: 111,
                },
                RtpCodecKind::Audio,
            )
            .unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut engine).unwrap();
        let handler = Arc::new(Browser {
            gathered: Notify::new(),
            video_packets: Arc::new(AtomicU64::new(0)),
            audio_packets: Arc::new(AtomicU64::new(0)),
            video_pt: Arc::new(AtomicU8::new(0)),
        });
        let browser = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .with_handler(handler.clone() as Arc<dyn PeerConnectionEventHandler>)
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await
            .unwrap();
        for kind in [RtpCodecKind::Video, RtpCodecKind::Audio] {
            browser
                .add_transceiver_from_kind(
                    kind,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        streams: vec![],
                        send_encodings: vec![],
                    }),
                )
                .await
                .unwrap();
        }
        let offer = browser.create_offer(None).await.unwrap();
        browser.set_local_description(offer).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), handler.gathered.notified()).await;
        let offer_sdp = browser.local_description().await.unwrap().sdp;

        let (viewer, answer) = connect(&app, offer_sdp).await.unwrap();
        browser
            .set_remote_description(RTCSessionDescription::answer(answer).unwrap())
            .await
            .unwrap();

        // Feed console packets until the browser has received some of each.
        let mut sequence = 0u16;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while (handler.video_packets.load(Ordering::Relaxed) < 5
            || handler.audio_packets.load(Ordering::Relaxed) < 5)
            && std::time::Instant::now() < deadline
        {
            sequence = sequence.wrapping_add(1);
            let _ = app.media.send(console_packet(true, sequence));
            let _ = app.media.send(console_packet(false, sequence));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            handler.video_packets.load(Ordering::Relaxed) >= 5,
            "no video reached the browser"
        );
        assert!(
            handler.audio_packets.load(Ordering::Relaxed) >= 5,
            "no audio reached the browser"
        );
        assert_eq!(handler.video_pt.load(Ordering::Relaxed), 108);

        viewer.close().await;
        let _ = browser.close().await;
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
