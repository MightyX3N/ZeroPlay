//! The home-streaming session API ("xHome").
//!
//! Ported from Greenlight's `xcloudapi.ts` and `streammanager.ts` (MIT, UnknownSKL).
//! This is plain HTTPS: start a session on a console, wait until it is ready,
//! swap SDP and ICE candidates, and keep the session alive.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::StreamingToken;

/// Where a session is in its start-up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Provisioning,
    /// The console wants the transfer token before it continues.
    ReadyToConnect,
    /// Ready for the SDP exchange.
    Provisioned,
    Failed(String),
    Other(String),
}

/// One ICE candidate in the JSON shape the session API uses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(rename = "sdpMLineIndex", default)]
    pub sdp_mline_index: Value,
    #[serde(rename = "sdpMid", default)]
    pub sdp_mid: Value,
}

#[derive(Deserialize)]
struct PlayResponse {
    #[serde(rename = "sessionPath")]
    session_path: String,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    #[serde(rename = "exchangeResponse")]
    exchange_response: Option<String>,
    #[serde(rename = "errorDetails", default)]
    error_details: Option<Value>,
}

#[derive(Deserialize)]
struct SdpAnswer {
    sdp: String,
}

/// Talks to the streaming API for one account.
#[derive(Clone)]
pub struct HomeApi {
    http: reqwest::Client,
    base_uri: String,
    gs_token: String,
    os_name: &'static str,
}

fn device_info() -> String {
    json!({
        "appInfo": {
            "env": {
                "clientAppId": "Microsoft.GamingApp",
                "clientAppType": "native",
                "clientAppVersion": "2203.1001.4.0",
                "clientSdkVersion": "8.5.2",
                "httpEnvironment": "prod",
                "sdkInstallId": "",
            },
        },
        "dev": {
            "hw": { "make": "Microsoft", "model": "Surface Pro", "sdktype": "native" },
            "os": { "name": "Windows 11", "ver": "22631.2715", "platform": "desktop" },
            "displayInfo": {
                "dimensions": { "widthInPixels": 1920, "heightInPixels": 1080 },
                "pixelDensity": { "dpiX": 1, "dpiY": 1 },
            },
        },
    })
    .to_string()
}

/// The body that starts a session on `console_id`.
///
/// `os_name` picks the picture size the console sends: "android" gives 720p,
/// "windows" 1080p.
fn play_body(console_id: &str, os_name: &str) -> Value {
    json!({
        "titleId": "",
        "systemUpdateGroup": "",
        "clientSessionId": "",
        "settings": {
            "nanoVersion": "V3;WebrtcTransport.dll",
            "enableTextToSpeech": false,
            "highContrast": 0,
            "locale": "en-US",
            "useIceConnection": false,
            "timezoneOffsetMinutes": 120,
            "sdkType": "web",
            "osName": os_name,
        },
        "serverId": console_id,
        "fallbackRegionNames": [],
    })
}

/// The body that carries our SDP offer and the channel versions we speak.
fn sdp_body(sdp: &str) -> Value {
    json!({
        "messageType": "offer",
        "sdp": sdp,
        "configuration": {
            "chatConfiguration": {
                "bytesPerSample": 2,
                "expectedClipDurationMs": 20,
                "format": { "codec": "opus", "container": "webm" },
                "numChannels": 1,
                "sampleFrequencyHz": 24000,
            },
            "chat": { "minVersion": 1, "maxVersion": 1 },
            "control": { "minVersion": 1, "maxVersion": 3 },
            "input": { "minVersion": 1, "maxVersion": 8 },
            "message": { "minVersion": 1, "maxVersion": 1 },
        },
    })
}

/// "v5/sessions/home/<id>" gives "<id>".
fn session_id_from_path(path: &str) -> Result<String> {
    path.trim_start_matches('/')
        .split('/')
        .nth(3)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("unexpected session path from the server: {path}"))
}

fn parse_state(body: &Value) -> SessionState {
    match body.get("state").and_then(Value::as_str).unwrap_or("") {
        "Provisioning" => SessionState::Provisioning,
        "ReadyToConnect" => SessionState::ReadyToConnect,
        "Provisioned" => SessionState::Provisioned,
        "Failed" => SessionState::Failed(
            body.get("errorDetails")
                .map(|d| d.to_string())
                .unwrap_or_else(|| "no details".to_owned()),
        ),
        other => SessionState::Other(other.to_owned()),
    }
}

impl HomeApi {
    pub fn new(token: &StreamingToken) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base_uri: token.base_uri.clone(),
            gs_token: token.gs_token.clone(),
            os_name: "windows",
        })
    }

    /// 1080p (the default) or 720p.
    pub fn with_720p(mut self, yes: bool) -> Self {
        self.os_name = if yes { "android" } else { "windows" };
        self
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base_uri, path))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.gs_token))
    }

    async fn check(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body: String = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(600)
            .collect();
        Err(anyhow!("{what} failed with HTTP {status}: {body}"))
    }

    /// GET that retries while the server answers "204, not ready yet".
    async fn get_when_ready(&self, path: &str, what: &str) -> Result<Value> {
        for _ in 0..80 {
            let response =
                Self::check(self.request(reqwest::Method::GET, path).send().await?, what).await?;
            if response.status() == reqwest::StatusCode::NO_CONTENT {
                tokio::time::sleep(Duration::from_millis(750)).await;
                continue;
            }
            return Ok(response.json().await?);
        }
        bail!("{what}: the console did not answer in time")
    }

    /// Starts a session on a console and returns the session id.
    pub async fn start(&self, console_id: &str) -> Result<String> {
        let response = self
            .request(reqwest::Method::POST, "/v5/sessions/home/play")
            .header("X-MS-Device-Info", device_info())
            .json(&play_body(console_id, self.os_name))
            .send()
            .await?;
        let play: PlayResponse = Self::check(response, "starting the stream")
            .await?
            .json()
            .await?;
        session_id_from_path(&play.session_path)
    }

    pub async fn state(&self, session_id: &str) -> Result<SessionState> {
        let body = self
            .get_when_ready(
                &format!("/v5/sessions/home/{session_id}/state"),
                "reading the session state",
            )
            .await?;
        Ok(parse_state(&body))
    }

    /// Hands the console the transfer token it asks for in `ReadyToConnect`.
    pub async fn connect(&self, session_id: &str, transfer_token: &str) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v5/sessions/home/{session_id}/connect"),
            )
            .json(&json!({ "userToken": transfer_token }))
            .send()
            .await?;
        Self::check(response, "authorizing the session").await?;
        Ok(())
    }

    async fn exchange(&self, path: &str, body: &Value, what: &str) -> Result<String> {
        let response = self
            .request(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await?;
        Self::check(response, what).await?;
        let answer: ExchangeResponse =
            serde_json::from_value(self.get_when_ready(path, what).await?)
                .with_context(|| format!("{what}: unexpected answer"))?;
        match answer.exchange_response {
            Some(text) => Ok(text),
            None => bail!(
                "{what}: the console sent no answer ({})",
                answer
                    .error_details
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "no details".to_owned())
            ),
        }
    }

    /// Sends our SDP offer and returns the console's SDP answer.
    pub async fn exchange_sdp(&self, session_id: &str, offer_sdp: &str) -> Result<String> {
        let text = self
            .exchange(
                &format!("/v5/sessions/home/{session_id}/sdp"),
                &sdp_body(offer_sdp),
                "exchanging SDP",
            )
            .await?;
        let answer: SdpAnswer =
            serde_json::from_str(&text).context("the console's SDP answer was not valid JSON")?;
        Ok(answer.sdp)
    }

    /// Sends our ICE candidates and returns the console's.
    pub async fn exchange_ice(
        &self,
        session_id: &str,
        candidates: &[IceCandidate],
    ) -> Result<Vec<IceCandidate>> {
        let body = json!({ "messageType": "iceCandidate", "candidate": candidates });
        let text = self
            .exchange(
                &format!("/v5/sessions/home/{session_id}/ice"),
                &body,
                "exchanging ICE candidates",
            )
            .await?;
        serde_json::from_str(&text).context("the console's ICE candidates were not valid JSON")
    }

    /// Must be sent about every 30 seconds while streaming.
    pub async fn keepalive(&self, session_id: &str) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v5/sessions/home/{session_id}/keepalive"),
            )
            .json(&json!({}))
            .send()
            .await?;
        Self::check(response, "keeping the session alive").await?;
        Ok(())
    }

    /// Ends the session so the console is free again.
    pub async fn stop(&self, session_id: &str) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("/v5/sessions/home/{session_id}"),
            )
            .send()
            .await?;
        Self::check(response, "stopping the stream").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_comes_from_the_path() {
        assert_eq!(
            session_id_from_path("v5/sessions/home/ABC-123").unwrap(),
            "ABC-123"
        );
        assert_eq!(
            session_id_from_path("/v5/sessions/home/ABC-123").unwrap(),
            "ABC-123"
        );
        assert!(session_id_from_path("v5/sessions/home").is_err());
        assert!(session_id_from_path("").is_err());
    }

    #[test]
    fn states_are_recognised() {
        assert_eq!(
            parse_state(&json!({"state": "Provisioning"})),
            SessionState::Provisioning
        );
        assert_eq!(
            parse_state(&json!({"state": "ReadyToConnect"})),
            SessionState::ReadyToConnect
        );
        assert_eq!(
            parse_state(&json!({"state": "Provisioned"})),
            SessionState::Provisioned
        );
        assert!(matches!(
            parse_state(&json!({"state": "Failed", "errorDetails": {"code": "X", "message": "nope"}})),
            SessionState::Failed(details) if details.contains("nope")
        ));
        assert_eq!(
            parse_state(&json!({"state": "Surprise"})),
            SessionState::Other("Surprise".to_owned())
        );
        assert_eq!(parse_state(&json!({})), SessionState::Other(String::new()));
    }

    #[test]
    fn play_body_targets_the_console() {
        let body = play_body("F400000000000000", "windows");
        assert_eq!(body["serverId"], "F400000000000000");
        assert_eq!(body["settings"]["osName"], "windows");
        assert_eq!(play_body("F4", "android")["settings"]["osName"], "android");
        assert_eq!(body["titleId"], "");
        assert_eq!(body["settings"]["nanoVersion"], "V3;WebrtcTransport.dll");
        assert_eq!(body["settings"]["useIceConnection"], false);
    }

    #[test]
    fn sdp_body_carries_the_offer_and_channel_versions() {
        let body = sdp_body("v=0\r\n");
        assert_eq!(body["messageType"], "offer");
        assert_eq!(body["sdp"], "v=0\r\n");
        assert_eq!(body["configuration"]["input"]["maxVersion"], 8);
        assert_eq!(body["configuration"]["control"]["maxVersion"], 3);
    }

    #[test]
    fn ice_candidates_keep_the_servers_field_names() {
        let parsed: Vec<IceCandidate> = serde_json::from_str(
            r#"[{"candidate":"a=candidate:1 1 UDP 100 192.168.1.20 9002 typ host ","messageType":"iceCandidate","sdpMLineIndex":"0","sdpMid":"0"},
                {"candidate":"a=end-of-candidates","messageType":"iceCandidate","sdpMLineIndex":"0","sdpMid":"0"}]"#,
        )
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].sdp_mid, json!("0"));
        let back = serde_json::to_value(&parsed[0]).unwrap();
        assert!(back.get("sdpMLineIndex").is_some());
        assert!(back.get("sdpMid").is_some());
    }

    #[test]
    fn device_info_is_json() {
        let parsed: Value = serde_json::from_str(&device_info()).unwrap();
        assert_eq!(parsed["dev"]["os"]["platform"], "desktop");
    }
}
