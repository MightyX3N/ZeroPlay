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
    pub content_type: Option<String>,
    #[serde(default)]
    pub last_active_time: Option<String>,
}

impl InstalledApp {
    pub fn looks_like_a_game(&self) -> bool {
        self.is_game
            || self
                .content_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("game"))
    }
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

/// A title's name (game or app) from Xbox Live, by title id.
pub async fn title_name(
    http: &reqwest::Client,
    token: &XboxToken,
    title_id: u64,
) -> Result<Option<String>> {
    let response = http
        .post("https://titlehub.xboxlive.com/titles/batch/decoration/image")
        .header("Authorization", token.authorization_header())
        .header("x-xbl-contract-version", "2")
        .header("Accept-Language", "en-US")
        .header("Accept", "application/json")
        .json(&json!({ "pfns": null, "titleIds": [title_id.to_string()] }))
        .send()
        .await?;
    let body: serde_json::Value = check(response, "getting the title's name")
        .await?
        .json()
        .await?;
    Ok(body
        .pointer("/titles/0/name")
        .and_then(|n| n.as_str())
        .map(str::to_owned))
}

/// What the console says is on screen ("/streaming/properties/titleinfo"):
/// the title id, read from whichever key holds it (decimal, hex, or a number).
pub fn title_info_id(raw: &str) -> Option<u64> {
    fn find(v: &serde_json::Value) -> Option<u64> {
        match v {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if key.to_ascii_lowercase().replace('_', "") == "titleid" {
                        let id = match value {
                            serde_json::Value::Number(n) => n.as_u64(),
                            serde_json::Value::String(s) => {
                                let s = s.trim();
                                match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                                    Some(hex) => u64::from_str_radix(hex, 16).ok(),
                                    None => s.parse().ok(),
                                }
                            }
                            _ => None,
                        };
                        if id.is_some_and(|id| id != 0) {
                            return id;
                        }
                    }
                }
                map.values().find_map(find)
            }
            serde_json::Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    find(&serde_json::from_str(raw).ok()?)
}

/// A game's pictures from the Microsoft Store.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoreArt {
    /// Portrait cover (poster, box art or tile).
    pub poster: Option<String>,
    /// Wide picture for a featured banner (hero art).
    pub hero: Option<String>,
}

/// Pictures from the Microsoft Store, by Store id: wide hero art for every
/// game, and covers for games Xbox Live has none for (often Xbox 360 games
/// played on newer consoles).
pub async fn store_art(
    http: &reqwest::Client,
    product_ids: &[String],
) -> Result<std::collections::HashMap<String, StoreArt>> {
    let mut art = std::collections::HashMap::new();
    for chunk in product_ids.chunks(20) {
        let response = http
            .get("https://displaycatalog.mp.microsoft.com/v7.0/products")
            .query(&[
                ("bigIds", chunk.join(",")),
                ("market", "US".to_owned()),
                ("languages", "en-US".to_owned()),
            ])
            .send()
            .await?;
        let body: serde_json::Value = check(response, "getting Store art").await?.json().await?;
        art.extend(parse_store_art(&body));
    }
    Ok(art)
}

fn https(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        url.replace("http://", "https://")
    }
}

fn parse_store_art(body: &serde_json::Value) -> Vec<(String, StoreArt)> {
    let mut out = Vec::new();
    for product in body
        .get("Products")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
    {
        let Some(id) = product.get("ProductId").and_then(|v| v.as_str()) else {
            continue;
        };
        let images = product
            .pointer("/LocalizedProperties/0/Images")
            .and_then(|i| i.as_array());
        let of_purpose = |wanted: &str| {
            images.into_iter().flatten().find_map(|image| {
                (image.get("ImagePurpose").and_then(|t| t.as_str()) == Some(wanted))
                    .then(|| image.get("Uri").and_then(|u| u.as_str()))
                    .flatten()
            })
        };
        let poster = of_purpose("Poster")
            .or_else(|| of_purpose("BoxArt"))
            .or_else(|| of_purpose("Tile"))
            .map(https);
        let hero = of_purpose("SuperHeroArt")
            .or_else(|| of_purpose("Hero"))
            .or_else(|| of_purpose("TitledHeroArt"))
            .or_else(|| of_purpose("BrandedKeyArt"))
            .map(https);
        if poster.is_some() || hero.is_some() {
            out.push((id.to_owned(), StoreArt { poster, hero }));
        }
    }
    out
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
    fn reads_the_title_id_from_titleinfo() {
        assert_eq!(
            super::title_info_id(r#"{"focused":true,"state":"Active","titleId":"219630713"}"#),
            Some(219630713)
        );
        assert_eq!(
            super::title_info_id(r#"{"focused":true,"title":{"TitleId":"0x2CB8A57F"}}"#),
            Some(0x2CB8A57F)
        );
        assert_eq!(
            super::title_info_id(r#"{"focused":true,"titleId":0}"#),
            None
        );
        assert_eq!(super::title_info_id("not json"), None);
    }

    #[test]
    fn picks_store_art() {
        let body = serde_json::json!({ "Products": [
            { "ProductId": "BRZ1", "LocalizedProperties": [ { "Images": [
                { "ImagePurpose": "Tile", "Uri": "//store-images.s-microsoft.com/tile" },
                { "ImagePurpose": "SuperHeroArt", "Uri": "//store-images.s-microsoft.com/hero" },
                { "ImagePurpose": "BoxArt", "Uri": "//store-images.s-microsoft.com/box" } ] } ] },
            { "ProductId": "NONE", "LocalizedProperties": [ { "Images": [] } ] }
        ]});
        assert_eq!(
            parse_store_art(&body),
            vec![(
                "BRZ1".to_owned(),
                StoreArt {
                    poster: Some("https://store-images.s-microsoft.com/box".to_owned()),
                    hero: Some("https://store-images.s-microsoft.com/hero".to_owned()),
                }
            )]
        );
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

/// What one of the account's consoles is running, from Xbox Live presence.
#[derive(Clone, Debug, PartialEq)]
pub struct Playing {
    /// The console family presence reports ("Scarlett" is Series X|S, "XboxOne").
    pub device: String,
    pub title_id: u64,
    pub name: String,
    /// What the game says the player is doing ("Racing in Mexico"), when it says.
    pub activity: Option<String>,
}

/// What the signed-in account is running on its consoles right now: the
/// title in the foreground on each console that is online.
pub async fn playing(http: &reqwest::Client, token: &XboxToken) -> Result<Vec<Playing>> {
    let response = http
        .get("https://userpresence.xboxlive.com/users/me?level=all")
        .header("Authorization", token.authorization_header())
        .header("x-xbl-contract-version", "3")
        .header("Accept-Language", "en-US")
        .header("Accept", "application/json")
        .send()
        .await?;
    let body: serde_json::Value = check(response, "reading what the console is running")
        .await?
        .json()
        .await?;
    Ok(parse_playing(&body))
}

fn parse_playing(body: &serde_json::Value) -> Vec<Playing> {
    const HOME: u64 = 750_323_071;
    let mut out = Vec::new();
    let devices = body.get("devices").and_then(|d| d.as_array());
    for device in devices.into_iter().flatten() {
        let kind = device.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if !(kind == "Scarlett" || kind.starts_with("Xbox")) {
            continue; // a PC or phone
        }
        let titles: Vec<&serde_json::Value> = device
            .get("titles")
            .and_then(|t| t.as_array())
            .into_iter()
            .flatten()
            .filter(|t| t.get("state").and_then(|s| s.as_str()).unwrap_or("Active") == "Active")
            .collect();
        let id_of = |t: &serde_json::Value| -> u64 {
            match t.get("id") {
                Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
                Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
                _ => 0,
            }
        };
        let placement = |t: &serde_json::Value| {
            t.get("placement").and_then(|p| p.as_str()).unwrap_or("").to_owned()
        };
        // The full-screen title; otherwise anything that is not the dashboard;
        // otherwise the dashboard.
        let chosen = titles
            .iter()
            .find(|t| placement(t) == "Full" && id_of(t) != HOME)
            .or_else(|| titles.iter().find(|t| id_of(t) != HOME && placement(t) != "Background"))
            .or_else(|| titles.iter().find(|t| id_of(t) == HOME));
        let Some(title) = chosen else { continue };
        let id = id_of(title);
        let name = if id == HOME {
            "Xbox Home".to_owned()
        } else {
            match title.get("name").and_then(|n| n.as_str()) {
                Some(n) if !n.is_empty() => n.to_owned(),
                _ => continue,
            }
        };
        let activity = title
            .pointer("/activity/richPresence")
            .and_then(|r| r.as_str())
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_owned);
        out.push(Playing {
            device: kind.to_owned(),
            title_id: id,
            name,
            activity,
        });
    }
    out
}

#[cfg(test)]
mod playing_tests {
    use super::*;

    #[test]
    fn picks_the_full_screen_game_with_its_activity() {
        let body = json!({
            "state": "Online",
            "devices": [
                { "type": "WindowsOneCore", "titles": [{ "id": "1", "name": "Xbox App", "placement": "Full", "state": "Active" }] },
                { "type": "Scarlett", "titles": [
                    { "id": "750323071", "name": "Home", "placement": "Background", "state": "Active" },
                    { "id": "2030093255", "name": "Forza Horizon 5", "placement": "Full", "state": "Active",
                      "activity": { "richPresence": "Racing in Mexico" } }
                ]}
            ]
        });
        assert_eq!(
            parse_playing(&body),
            vec![Playing {
                device: "Scarlett".into(),
                title_id: 2030093255,
                name: "Forza Horizon 5".into(),
                activity: Some("Racing in Mexico".into()),
            }]
        );
    }

    #[test]
    fn the_dashboard_when_nothing_else_runs() {
        let body = json!({ "devices": [{ "type": "Scarlett", "titles": [
            { "id": "750323071", "name": "Home", "placement": "Full", "state": "Active" }
        ]}]});
        let p = parse_playing(&body);
        assert_eq!(p[0].name, "Xbox Home");
        assert_eq!(p[0].activity, None);
    }

    #[test]
    fn offline_has_nothing() {
        assert!(parse_playing(&json!({ "state": "Offline" })).is_empty());
    }
}
