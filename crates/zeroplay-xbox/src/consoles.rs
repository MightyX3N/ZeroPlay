//! The consoles on a Microsoft account.
//!
//! Ported from xbox-webapi-node's SmartGlass provider (MIT, UnknownSKL).

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;

use crate::auth::XboxToken;

const HOST: &str = "https://xccs.xboxlive.com";

/// One console as Xbox Live describes it.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Console {
    /// The id used to start a stream on this console.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub console_type: String,
    /// "On", "ConnectedStandby", "Off", ...
    #[serde(default)]
    pub power_state: String,
    /// Whether remote play is switched on in the console's settings.
    #[serde(default)]
    pub console_streaming_enabled: bool,
    #[serde(default)]
    pub remote_management_enabled: bool,
}

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    result: Vec<Console>,
}

fn request(
    http: &reqwest::Client,
    method: reqwest::Method,
    path: &str,
    token: &XboxToken,
) -> reqwest::RequestBuilder {
    http.request(method, format!("{HOST}{path}"))
        .header("Authorization", token.authorization_header())
        .header("Accept-Language", "en-US")
        .header("Accept", "application/json")
        .header("x-xbl-contract-version", "4")
        .header("skillplatform", "RemoteManagement")
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

/// Lists every console on the account.
pub async fn list(http: &reqwest::Client, token: &XboxToken) -> Result<Vec<Console>> {
    let response = request(
        http,
        reqwest::Method::GET,
        "/lists/devices?queryCurrentDevice=false&includeStorageDevices=true",
        token,
    )
    .send()
    .await?;
    let parsed: ListResponse = check(response, "listing consoles").await?.json().await?;
    Ok(parsed.result)
}

/// Asks a console in standby to wake up.
pub async fn wake(http: &reqwest::Client, token: &XboxToken, console_id: &str) -> Result<()> {
    let body = json!({
        "destination": "Xbox",
        "type": "Power",
        "command": "WakeUp",
        "sessionId": uuid::Uuid::new_v4().to_string(),
        "sourceId": "com.microsoft.smartglass",
        "parameters": [],
        "linkedXboxId": console_id,
    });
    let response = request(http, reqwest::Method::POST, "/commands", token)
        .json(&body)
        .send()
        .await?;
    check(response, "waking the console").await?;
    Ok(())
}

/// Presses a button on a console through Xbox Live, without a stream.
/// `key` is one of Guide, Menu, View, A, B, X, Y, Up, Down, Left, Right, Nexus.
pub async fn press_button(
    http: &reqwest::Client,
    token: &XboxToken,
    console_id: &str,
    key: &str,
) -> Result<()> {
    let body = json!({
        "destination": "Xbox",
        "type": "Shell",
        "command": "InjectKey",
        "sessionId": uuid::Uuid::new_v4().to_string(),
        "sourceId": "com.microsoft.smartglass",
        "parameters": [{ "keyType": key }],
        "linkedXboxId": console_id,
    });
    let response = request(http, reqwest::Method::POST, "/commands", token)
        .json(&body)
        .send()
        .await?;
    check(response, "pressing a button on the console").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_console_list() {
        let parsed: ListResponse = serde_json::from_str(
            r#"{"status":{"errorCode":"OK","errorMessage":null},"result":[{
                "id":"F400000000000000","name":"Living room","locale":"en-US","region":"DK",
                "consoleType":"XboxSeriesX","powerState":"ConnectedStandby",
                "digitalAssistantRemoteControlEnabled":true,"remoteManagementEnabled":true,
                "consoleStreamingEnabled":true,"wirelessWarning":false,"outOfHomeWarning":false,
                "storageDevices":[]}],"agentUserId":null}"#,
        )
        .unwrap();
        assert_eq!(parsed.result.len(), 1);
        let console = &parsed.result[0];
        assert_eq!(console.id, "F400000000000000");
        assert_eq!(console.name, "Living room");
        assert_eq!(console.console_type, "XboxSeriesX");
        assert_eq!(console.power_state, "ConnectedStandby");
        assert!(console.console_streaming_enabled);
    }

    #[test]
    fn an_empty_account_gives_an_empty_list() {
        let parsed: ListResponse =
            serde_json::from_str(r#"{"status":{"errorCode":"OK"}}"#).unwrap();
        assert!(parsed.result.is_empty());
    }
}
