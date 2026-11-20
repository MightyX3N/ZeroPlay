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

/// A game or app installed on a console.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledApp {
    /// The Store id; launching uses it.
    #[serde(default)]
    pub one_store_product_id: Option<String>,
    #[serde(default)]
    pub title_id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub is_game: bool,
    #[serde(default)]
    pub last_active_time: Option<String>,
}

#[derive(Deserialize)]
struct InstalledResponse {
    #[serde(default)]
    result: Vec<InstalledApp>,
}

/// The games and apps installed on a console (it has to be on or in standby
/// with remote features enabled).
pub async fn installed_apps(
    http: &reqwest::Client,
    token: &XboxToken,
    console_id: &str,
) -> Result<Vec<InstalledApp>> {
    let response = request(
        http,
        reqwest::Method::GET,
        &format!("/lists/installedApps?deviceId={console_id}"),
        token,
    )
    .send()
    .await?;
    let parsed: InstalledResponse = check(response, "listing the installed games")
        .await?
        .json()
        .await?;
    Ok(parsed.result)
}

/// Starts a game or app on the console by its Store id.
pub async fn launch(
    http: &reqwest::Client,
    token: &XboxToken,
    console_id: &str,
    one_store_product_id: &str,
) -> Result<()> {
    let body = json!({
        "destination": "Xbox",
        "type": "Shell",
        "command": "ActivateApplicationWithOneStoreProductId",
        "sessionId": uuid::Uuid::new_v4().to_string(),
        "sourceId": "com.microsoft.smartglass",
        "parameters": [{ "oneStoreProductId": one_store_product_id }],
        "linkedXboxId": console_id,
    });
    let response = request(http, reqwest::Method::POST, "/commands", token)
        .json(&body)
        .send()
        .await?;
    check(response, "starting the game").await?;
    Ok(())
}

/// Cover art for titles, by title id (best effort: titles without art are
/// left out).
pub async fn title_art(
    http: &reqwest::Client,
    token: &XboxToken,
    title_ids: &[u64],
) -> Result<std::collections::HashMap<u64, String>> {
    let mut art = std::collections::HashMap::new();
    for chunk in title_ids.chunks(50) {
        let ids: Vec<String> = chunk.iter().map(u64::to_string).collect();
        let response = http
            .post("https://titlehub.xboxlive.com/titles/batch/decoration/image")
            .header("Authorization", token.authorization_header())
            .header("x-xbl-contract-version", "2")
            .header("Accept-Language", "en-US")
            .header("Accept", "application/json")
            .json(&json!({ "pfns": null, "titleIds": ids }))
            .send()
            .await?;
        let body: serde_json::Value = check(response, "getting cover art").await?.json().await?;
        art.extend(parse_art(&body));
    }
    Ok(art)
}

/// Picks a portrait cover (poster or box art) for each title, else its display image.
fn parse_art(body: &serde_json::Value) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for title in body
        .get("titles")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
    {
        let Some(id) = title.get("titleId").and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| v.as_u64())
        }) else {
            continue;
        };
        let images = title.get("images").and_then(|i| i.as_array());
        let of_type = |wanted: &str| {
            images.into_iter().flatten().find_map(|image| {
                (image.get("type").and_then(|t| t.as_str()) == Some(wanted))
                    .then(|| image.get("url").and_then(|u| u.as_str()))
                    .flatten()
            })
        };
        let url = of_type("Poster")
            .or_else(|| of_type("BoxArt"))
            .or_else(|| title.get("displayImage").and_then(|d| d.as_str()));
        if let Some(url) = url {
            out.push((id, url.replace("http://", "https://")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_installed_apps() {
        let parsed: InstalledResponse = serde_json::from_str(
            r#"{"status":{"errorCode":"OK"},"result":[
                {"oneStoreProductId":"9NBLGGH4R315","titleId":219630713,"aumid":"x","lastActiveTime":"2026-11-01T10:00:00Z","isGame":true,"name":"Forza Horizon 5","contentType":"Game","instanceId":"1","storageDeviceId":"2","uniqueId":"3","version":1,"sizeInBytes":1,"installTime":"2026-01-01T00:00:00Z"},
                {"titleId":1,"isGame":false,"name":"Settings","contentType":"App","instanceId":"1","storageDeviceId":"2","uniqueId":"3","version":1,"sizeInBytes":1,"installTime":"2026-01-01T00:00:00Z"}]}"#,
        )
        .unwrap();
        assert_eq!(parsed.result.len(), 2);
        assert_eq!(
            parsed.result[0].one_store_product_id.as_deref(),
            Some("9NBLGGH4R315")
        );
        assert!(parsed.result[0].is_game);
        assert_eq!(parsed.result[1].one_store_product_id, None);
    }

    #[test]
    fn picks_a_poster_for_cover_art() {
        let body = serde_json::json!({ "titles": [
            { "titleId": "219630713", "displayImage": "http://a/display.png",
              "images": [ { "type": "Logo", "url": "http://a/logo.png" }, { "type": "Poster", "url": "http://a/poster.png" } ] },
            { "titleId": "5", "displayImage": "http://a/only.png" },
            { "titleId": "6" }
        ]});
        assert_eq!(
            parse_art(&body),
            vec![
                (219630713, "https://a/poster.png".to_owned()),
                (5, "https://a/only.png".to_owned())
            ]
        );
    }

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
