//! Microsoft sign-in for Xbox streaming.
//!
//! Uses the device-code flow: the app shows a short code, the person types it
//! in at microsoft.com/link on any device, and we get a refresh token. No
//! browser window and no password ever touch this app.
//!
//! Ported from xal-node's `msal.ts` (MIT, UnknownSKL), the same flow
//! Greenlight uses.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The public client id xal-node and Greenlight sign in with.
const CLIENT_ID: &str = "1f907974-e22b-4810-a9de-d9647380c97e";
const SCOPE: &str = "xboxlive.signin openid profile offline_access";
const TRANSFER_SCOPE: &str =
    "service::http://Passport.NET/purpose::PURPOSE_XBOX_CLOUD_CONSOLE_TRANSFER_TOKEN";

const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const LIVE_TOKEN_URL: &str = "https://login.live.com/oauth20_token.srf";
const USER_AUTH_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";

/// What to show the person so they can sign in.
#[derive(Clone, Debug, Deserialize)]
pub struct DeviceCode {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub message: String,
}

/// The sign-in we keep on disk.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoredTokens {
    pub refresh_token: String,
    #[serde(default)]
    pub access_token: String,
    /// Unix time in seconds when `access_token` stops working.
    #[serde(default)]
    pub access_expires_at: u64,
}

#[derive(Deserialize)]
struct OauthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct OauthError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

/// A token from Xbox Live's auth servers.
#[derive(Clone, Debug, Deserialize)]
pub struct XboxToken {
    #[serde(rename = "Token")]
    pub token: String,
    #[serde(rename = "DisplayClaims")]
    display_claims: DisplayClaims,
}

#[derive(Clone, Debug, Deserialize)]
struct DisplayClaims {
    xui: Vec<Xui>,
}

#[derive(Clone, Debug, Deserialize)]
struct Xui {
    uhs: String,
}

impl XboxToken {
    /// The "user hash" that goes into the `XBL3.0` authorization header.
    pub fn user_hash(&self) -> &str {
        self.display_claims
            .xui
            .first()
            .map(|x| x.uhs.as_str())
            .unwrap_or("")
    }

    /// The `Authorization` header value for Xbox Live web APIs.
    pub fn authorization_header(&self) -> String {
        format!("XBL3.0 x={};{}", self.user_hash(), self.token)
    }
}

/// The token and server address for home streaming.
#[derive(Clone, Debug)]
pub struct StreamingToken {
    pub gs_token: String,
    /// Base address of the streaming API, e.g. `https://weu.core.gssv-play-prodxhome.xboxlive.com`.
    pub base_uri: String,
    pub region: String,
}

#[derive(Deserialize)]
struct StreamingTokenResponse {
    #[serde(rename = "gsToken")]
    gs_token: String,
    #[serde(rename = "offeringSettings")]
    offering_settings: OfferingSettings,
}

#[derive(Deserialize)]
struct OfferingSettings {
    regions: Vec<Region>,
}

#[derive(Deserialize)]
struct Region {
    name: String,
    #[serde(rename = "baseUri")]
    base_uri: String,
    #[serde(rename = "isDefault", default)]
    is_default: bool,
}

/// Where the sign-in is saved: `<config dir>/zeroplay/tokens.json`.
pub fn default_token_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow!("could not find a config folder on this system"))?;
    Ok(dir.join("zeroplay").join("tokens.json"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn read_error(response: reqwest::Response, what: &str) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let body: String = body.chars().take(600).collect();
    anyhow!("{what} failed with HTTP {status}: {body}")
}

/// Signs in and hands out the tokens the rest of the crate needs.
pub struct Auth {
    http: reqwest::Client,
    path: PathBuf,
    tokens: Option<StoredTokens>,
}

impl Auth {
    /// Loads a saved sign-in from `path` if there is one.
    pub async fn load(path: PathBuf) -> Result<Self> {
        let tokens = match tokio::fs::read(&path).await {
            Ok(bytes) => Some(
                serde_json::from_slice::<StoredTokens>(&bytes)
                    .with_context(|| format!("{} is not a valid token file", path.display()))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()))
            }
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, path, tokens })
    }

    pub fn is_signed_in(&self) -> bool {
        self.tokens.is_some()
    }

    pub fn token_path(&self) -> &std::path::Path {
        &self.path
    }

    async fn save(&self) -> Result<()> {
        let Some(tokens) = &self.tokens else {
            return Ok(());
        };
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = serde_json::to_vec_pretty(tokens)?;
        tokio::fs::write(&self.path, bytes)
            .await
            .with_context(|| format!("could not write {}", self.path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Forgets the saved sign-in.
    pub async fn sign_out(&mut self) -> Result<()> {
        self.tokens = None;
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Step 1 of signing in: ask Microsoft for a code to show the person.
    pub async fn start_device_code(&self) -> Result<DeviceCode> {
        let response = self
            .http
            .post(DEVICE_CODE_URL)
            .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(read_error(response, "asking for a sign-in code").await);
        }
        Ok(response.json().await?)
    }

    /// Step 2 of signing in: wait until the person has entered the code.
    pub async fn finish_device_code(&mut self, code: &DeviceCode) -> Result<()> {
        let deadline = unix_now() + code.expires_in;
        let interval = Duration::from_secs(code.interval.max(1));
        loop {
            tokio::time::sleep(interval).await;
            if unix_now() > deadline {
                bail!("the sign-in code expired before it was used");
            }
            let response = self
                .http
                .post(TOKEN_URL)
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", CLIENT_ID),
                    ("device_code", code.device_code.as_str()),
                ])
                .send()
                .await?;
            if response.status().is_success() {
                let token: OauthTokenResponse = response.json().await?;
                self.store(token)?;
                self.save().await?;
                return Ok(());
            }
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let error: OauthError = serde_json::from_str(&body).unwrap_or(OauthError {
                error: String::new(),
                error_description: body.chars().take(300).collect(),
            });
            match error.error.as_str() {
                "authorization_pending" | "slow_down" => continue,
                "authorization_declined" => bail!("the sign-in was declined"),
                "expired_token" => bail!("the sign-in code expired before it was used"),
                _ => bail!(
                    "sign-in failed with HTTP {status}: {} {}",
                    error.error,
                    error.error_description
                ),
            }
        }
    }

    fn store(&mut self, token: OauthTokenResponse) -> Result<()> {
        let refresh_token = token
            .refresh_token
            .or_else(|| self.tokens.as_ref().map(|t| t.refresh_token.clone()))
            .ok_or_else(|| anyhow!("Microsoft did not return a refresh token"))?;
        self.tokens = Some(StoredTokens {
            refresh_token,
            access_token: token.access_token,
            access_expires_at: unix_now() + token.expires_in.unwrap_or(0),
        });
        Ok(())
    }

    fn tokens(&self) -> Result<&StoredTokens> {
        self.tokens
            .as_ref()
            .ok_or_else(|| anyhow!("not signed in yet: run the login command first"))
    }

    /// A Microsoft access token that is good for at least another minute.
    async fn access_token(&mut self) -> Result<String> {
        let tokens = self.tokens()?;
        if !tokens.access_token.is_empty() && tokens.access_expires_at > unix_now() + 60 {
            return Ok(tokens.access_token.clone());
        }
        let refresh_token = tokens.refresh_token.clone();
        let response = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
                ("scope", SCOPE),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let error = read_error(response, "refreshing the sign-in").await;
            if status == reqwest::StatusCode::BAD_REQUEST {
                bail!(
                    "the saved sign-in is no longer valid, run the login command again ({error})"
                );
            }
            return Err(error);
        }
        let token: OauthTokenResponse = response.json().await?;
        self.store(token)?;
        self.save().await?;
        Ok(self.tokens()?.access_token.clone())
    }

    /// Turns the Microsoft token into an Xbox Live user token.
    async fn user_token(&mut self) -> Result<XboxToken> {
        let access_token = self.access_token().await?;
        let body = json!({
            "Properties": {
                "AuthMethod": "RPS",
                "RpsTicket": format!("d={access_token}"),
                "SiteName": "user.auth.xboxlive.com",
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        });
        let response = self
            .http
            .post(USER_AUTH_URL)
            .header("x-xbl-contract-version", "1")
            .header("Cache-Control", "no-cache")
            .header("Origin", "https://www.xbox.com")
            .header("Referer", "https://www.xbox.com/")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(read_error(response, "Xbox Live sign-in").await);
        }
        Ok(response.json().await?)
    }

    async fn xsts_token(&mut self, relying_party: &str) -> Result<XboxToken> {
        let user_token = self.user_token().await?;
        let body = json!({
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [user_token.token],
            },
            "RelyingParty": relying_party,
            "TokenType": "JWT",
        });
        let response = self
            .http
            .post(XSTS_URL)
            .header("x-xbl-contract-version", "1")
            .header("Cache-Control", "no-cache")
            .header("Origin", "https://www.xbox.com")
            .header("Referer", "https://www.xbox.com/")
            .header("Accept", "*/*")
            .header("ms-cv", "0")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(read_error(
                response,
                &format!("Xbox Live authorization for {relying_party}"),
            )
            .await);
        }
        Ok(response.json().await?)
    }

    /// The token for Xbox Live web APIs such as the console list.
    pub async fn web_token(&mut self) -> Result<XboxToken> {
        self.xsts_token("http://xboxlive.com").await
    }

    /// The token and server address for streaming from your own console.
    pub async fn home_streaming_token(&mut self) -> Result<StreamingToken> {
        let gssv = self.xsts_token("http://gssv.xboxlive.com/").await?;
        let body = json!({ "token": gssv.token, "offeringId": "xhome" });
        let response = self
            .http
            .post("https://xhome.gssv-play-prod.xboxlive.com/v2/login/user")
            .header("Cache-Control", "no-store, must-revalidate, no-cache")
            .header("x-gssv-client", "XboxComBrowser")
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(read_error(response, "getting the home streaming token").await);
        }
        let parsed: StreamingTokenResponse = response.json().await?;
        pick_region(parsed)
    }

    /// The one-time token the console asks for before it lets a stream start.
    pub async fn console_transfer_token(&mut self) -> Result<String> {
        // Make sure the refresh token is current first, like Greenlight does.
        self.access_token().await?;
        let refresh_token = self.tokens()?.refresh_token.clone();
        let response = self
            .http
            .post(LIVE_TOKEN_URL)
            .form(&[
                ("client_id", CLIENT_ID),
                ("scope", TRANSFER_SCOPE),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(read_error(response, "getting the console transfer token").await);
        }
        let token: OauthTokenResponse = response.json().await?;
        Ok(token.access_token)
    }
}

fn pick_region(parsed: StreamingTokenResponse) -> Result<StreamingToken> {
    let regions = parsed.offering_settings.regions;
    let region = regions
        .iter()
        .find(|r| r.is_default)
        .or_else(|| regions.first())
        .ok_or_else(|| anyhow!("the streaming service returned no regions"))?;
    Ok(StreamingToken {
        gs_token: parsed.gs_token,
        base_uri: region.base_uri.trim_end_matches('/').to_owned(),
        region: region.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xbox_token_builds_the_authorization_header() {
        let token: XboxToken = serde_json::from_str(
            r#"{"IssueInstant":"2026-01-01T00:00:00Z","NotAfter":"2026-01-02T00:00:00Z","Token":"abc","DisplayClaims":{"xui":[{"uhs":"12345"}]}}"#,
        )
        .unwrap();
        assert_eq!(token.authorization_header(), "XBL3.0 x=12345;abc");
    }

    #[test]
    fn picks_the_default_region() {
        let parsed: StreamingTokenResponse = serde_json::from_str(
            r#"{"offeringSettings":{"allowRegionSelection":false,"regions":[
                {"name":"EastUS","baseUri":"https://eus.example/","isDefault":false},
                {"name":"WestEurope","baseUri":"https://weu.example","isDefault":true}]},
                "market":"DK","gsToken":"gs","tokenType":"bearer","durationInSeconds":14400}"#,
        )
        .unwrap();
        let token = pick_region(parsed).unwrap();
        assert_eq!(token.region, "WestEurope");
        assert_eq!(token.base_uri, "https://weu.example");
        assert_eq!(token.gs_token, "gs");
    }

    #[test]
    fn falls_back_to_the_first_region() {
        let parsed: StreamingTokenResponse = serde_json::from_str(
            r#"{"offeringSettings":{"regions":[{"name":"A","baseUri":"https://a.example/"}]},"gsToken":"gs"}"#,
        )
        .unwrap();
        let token = pick_region(parsed).unwrap();
        assert_eq!(token.base_uri, "https://a.example");
    }

    #[test]
    fn stored_tokens_round_trip() {
        let tokens = StoredTokens {
            refresh_token: "r".into(),
            access_token: "a".into(),
            access_expires_at: 5,
        };
        let text = serde_json::to_string(&tokens).unwrap();
        let back: StoredTokens = serde_json::from_str(&text).unwrap();
        assert_eq!(back.refresh_token, "r");
        // Older files with only a refresh token still load.
        let old: StoredTokens = serde_json::from_str(r#"{"refresh_token":"x"}"#).unwrap();
        assert_eq!(old.access_expires_at, 0);
    }
}
