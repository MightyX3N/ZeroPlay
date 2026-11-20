//! Profiles: who may use ZeroPlay, and what each of them may do.
//!
//! The server listens on two ports. The home port (8080) is for the home
//! network. The remote port (8081, only on 127.0.0.1) is where the playit
//! tunnel delivers visitors from the internet. Everything arriving there looks
//! like it comes from the NAS itself, so it is always treated as outside.
//!
//! Until the admin profile exists, only the home network can use the page, and
//! the admin profile can only be made from there. After that everyone logs in
//! with a profile name and password and gets a session cookie. The admin makes
//! more profiles, and can let people ask for one ("request access").

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::{header, HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How long a login lasts.
const SESSION_DAYS: u64 = 30;
pub const COOKIE: &str = "zp_session";
/// Most access requests waiting at once.
const MAX_REQUESTS: usize = 20;

/// Where a request came in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Door {
    Home,
    Remote,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    #[default]
    Max,
    High,
    Medium,
    Low,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    password_hash: String,
    #[serde(default)]
    pub admin: bool,
    /// May open the Xbox Home screen (and press the Xbox button).
    #[serde(default = "yes")]
    pub xbox_home: bool,
    /// Games this profile may launch. `None` = all.
    #[serde(default)]
    pub games: Option<Vec<String>>,
    /// Highest stream quality this profile may pick.
    #[serde(default)]
    pub max_quality: Quality,
}

fn yes() -> bool {
    true
}

impl Profile {
    /// Everything but the password hash, for the page.
    pub fn public(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "admin": self.admin,
            "xboxHome": self.xbox_home,
            "games": self.games,
            "maxQuality": self.max_quality,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AccessRequest {
    id: String,
    name: String,
    password_hash: String,
    created: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Session {
    profile: String,
    expires: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    /// The login page offers "Request access".
    #[serde(default)]
    pub allow_requests: bool,
    /// Players who opt in take turns.
    #[serde(default)]
    pub timed_play: bool,
    #[serde(default = "thirty")]
    pub turn_minutes: u32,
}

fn thirty() -> u32 {
    30
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            allow_requests: false,
            timed_play: false,
            turn_minutes: 30,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    profiles: Vec<Profile>,
    #[serde(default, deserialize_with = "lenient_sessions")]
    sessions: HashMap<String, Session>,
    #[serde(default)]
    requests: Vec<AccessRequest>,
    #[serde(default)]
    settings: Settings,
    /// From the single-password version; turned into an admin profile.
    #[serde(default, skip_serializing)]
    password_hash: Option<String>,
}

/// Sessions in a shape we no longer use (the single-password version kept only
/// an expiry) are dropped instead of failing the whole file.
fn lenient_sessions<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<HashMap<String, Session>, D::Error> {
    let raw: HashMap<String, Value> = HashMap::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(k, v)| serde_json::from_value(v).ok().map(|s| (k, s)))
        .collect())
}

/// Changes the admin can make to a profile. Missing fields stay as they are.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileChanges {
    pub name: Option<String>,
    pub password: Option<String>,
    pub admin: Option<bool>,
    pub xbox_home: Option<bool>,
    /// `Some(None)` = all games.
    #[serde(default, deserialize_with = "some_option")]
    pub games: Option<Option<Vec<String>>>,
    pub max_quality: Option<Quality>,
}

fn some_option<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<Vec<String>>>, D::Error> {
    Ok(Some(Option::deserialize(d)?))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Logged in as this profile, or (before any profile exists) at home.
    Ok(Option<Profile>),
    /// Profiles exist; this visitor has not logged in.
    LoginNeeded,
    /// No profiles yet and this visitor is not at home.
    NotSetUp,
}

impl Verdict {
    pub fn name(&self) -> &'static str {
        match self {
            Verdict::Ok(_) => "ok",
            Verdict::LoginNeeded => "loginNeeded",
            Verdict::NotSetUp => "notSetUp",
        }
    }
}

pub struct Access {
    path: PathBuf,
    stored: Mutex<Stored>,
    failures: Mutex<Vec<Instant>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Private, loopback and link-local addresses: the home network.
pub fn is_home_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_private() || v4.is_loopback() || v4.is_link_local();
            }
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// True when the visitor is on the home network and came in through the home port.
pub fn at_home(door: Door, peer: SocketAddr) -> bool {
    door == Door::Home && is_home_ip(peer.ip())
}

pub fn session_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value.to_owned())
}

fn clean_name(name: &str) -> Result<String> {
    let name: String = name.trim().chars().filter(|c| !c.is_control()).collect();
    if name.is_empty() {
        bail!("Give the profile a name.");
    }
    if name.chars().count() > 24 {
        bail!("Use a name of at most 24 characters.");
    }
    Ok(name)
}

fn check_strength(password: &str) -> Result<()> {
    if password.chars().count() < 8 {
        bail!("Use a password of at least 8 characters.");
    }
    Ok(())
}

fn hash(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("could not hash the password: {e}"))
}

fn verify(password: &str, stored_hash: &str) -> bool {
    PasswordHash::new(stored_hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn new_id() -> String {
    new_token()[..12].to_owned()
}

pub fn session_cookie(token: &str) -> String {
    format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        SESSION_DAYS * 86400
    )
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

impl Stored {
    fn name_taken(&self, name: &str, except: Option<&str>) -> bool {
        self.profiles
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name) && Some(p.id.as_str()) != except)
            || self
                .requests
                .iter()
                .any(|r| r.name.eq_ignore_ascii_case(name))
    }

    fn new_session(&mut self, profile: &str) -> String {
        let token = new_token();
        self.sessions.insert(
            token.clone(),
            Session {
                profile: profile.to_owned(),
                expires: now_secs() + SESSION_DAYS * 86400,
            },
        );
        token
    }

    fn admins(&self) -> usize {
        self.profiles.iter().filter(|p| p.admin).count()
    }
}

impl Access {
    pub async fn load(data_dir: &std::path::Path) -> Result<Access> {
        let path = data_dir.join("access.json");
        let mut stored: Stored = match tokio::fs::read_to_string(&path).await {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Stored::default(),
        };
        // The single-password version becomes an admin profile called Admin.
        let migrated = match stored.password_hash.take() {
            Some(password_hash) if stored.profiles.is_empty() => {
                stored.profiles.push(Profile {
                    id: new_id(),
                    name: "Admin".to_owned(),
                    password_hash,
                    admin: true,
                    xbox_home: true,
                    games: None,
                    max_quality: Quality::Max,
                });
                // Old sessions had no profile; they log in again.
                stored.sessions.clear();
                true
            }
            _ => false,
        };
        let access = Access {
            path,
            stored: Mutex::new(stored),
            failures: Mutex::new(Vec::new()),
        };
        if migrated {
            access.save().await?;
        }
        Ok(access)
    }

    async fn save(&self) -> Result<()> {
        let text = {
            let mut stored = self.stored.lock().unwrap();
            let now = now_secs();
            stored.sessions.retain(|_, s| s.expires > now);
            serde_json::to_string_pretty(&*stored)?
        };
        tokio::fs::write(&self.path, text).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        Ok(())
    }

    pub fn has_profiles(&self) -> bool {
        !self.stored.lock().unwrap().profiles.is_empty()
    }

    pub fn settings(&self) -> Settings {
        self.stored.lock().unwrap().settings.clone()
    }

    pub async fn set_timed_play(&self, enabled: bool, minutes: u32) -> Result<()> {
        if !(1..=240).contains(&minutes) {
            bail!("Pick a turn length between 1 and 240 minutes.");
        }
        {
            let mut stored = self.stored.lock().unwrap();
            stored.settings.timed_play = enabled;
            stored.settings.turn_minutes = minutes;
        }
        self.save().await
    }

    pub fn requests_open(&self) -> bool {
        self.stored.lock().unwrap().settings.allow_requests
    }

    pub fn profile(&self, id: &str) -> Option<Profile> {
        self.stored
            .lock()
            .unwrap()
            .profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    pub fn verdict(&self, door: Door, peer: SocketAddr, headers: &HeaderMap) -> Verdict {
        let stored = self.stored.lock().unwrap();
        if stored.profiles.is_empty() {
            return if at_home(door, peer) {
                Verdict::Ok(None)
            } else {
                Verdict::NotSetUp
            };
        }
        let profile = session_from(headers)
            .and_then(|token| stored.sessions.get(&token).cloned())
            .filter(|s| s.expires > now_secs())
            .and_then(|s| stored.profiles.iter().find(|p| p.id == s.profile).cloned());
        match profile {
            Some(profile) => Verdict::Ok(Some(profile)),
            None => Verdict::LoginNeeded,
        }
    }

    /// Makes the first profile, the admin. Only from home, and only once.
    pub async fn create_admin(
        &self,
        door: Door,
        peer: SocketAddr,
        name: &str,
        password: &str,
    ) -> Result<String> {
        if !at_home(door, peer) {
            bail!("The admin profile can only be made from your home network.");
        }
        let name = clean_name(name)?;
        check_strength(password)?;
        let password_hash = hash(password)?;
        let token = {
            let mut stored = self.stored.lock().unwrap();
            if !stored.profiles.is_empty() {
                bail!("The admin profile already exists.");
            }
            let id = new_id();
            stored.profiles.push(Profile {
                id: id.clone(),
                name,
                password_hash,
                admin: true,
                xbox_home: true,
                games: None,
                max_quality: Quality::Max,
            });
            stored.new_session(&id)
        };
        self.save().await?;
        Ok(token)
    }

    /// Checks a profile's name and password and starts a session.
    pub async fn login(&self, name: &str, password: &str) -> Result<String> {
        {
            let mut failures = self.failures.lock().unwrap();
            failures.retain(|t| t.elapsed() < Duration::from_secs(60));
            if failures.len() >= 5 {
                bail!("Too many wrong passwords. Wait a minute and try again.");
            }
        }
        let found = {
            let stored = self.stored.lock().unwrap();
            stored
                .profiles
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
                .map(|p| (p.id.clone(), p.password_hash.clone()))
        };
        let ok = found.as_ref().is_some_and(|(_, h)| verify(password, h));
        let Some((id, _)) = found.filter(|_| ok) else {
            self.failures.lock().unwrap().push(Instant::now());
            tokio::time::sleep(Duration::from_millis(800)).await;
            bail!("Wrong name or password.");
        };
        let token = self.stored.lock().unwrap().new_session(&id);
        self.save().await?;
        Ok(token)
    }

    pub async fn logout(&self, headers: &HeaderMap) {
        if let Some(token) = session_from(headers) {
            self.stored.lock().unwrap().sessions.remove(&token);
            let _ = self.save().await;
        }
    }

    /// Someone asks for a profile. The admin approves or rejects it.
    pub async fn request_access(&self, name: &str, password: &str) -> Result<()> {
        let name = clean_name(name)?;
        check_strength(password)?;
        let password_hash = hash(password)?;
        {
            let mut stored = self.stored.lock().unwrap();
            if !stored.settings.allow_requests || stored.profiles.is_empty() {
                bail!("Access requests are switched off.");
            }
            if stored.name_taken(&name, None) {
                bail!("That name is taken. Pick another.");
            }
            if stored.requests.len() >= MAX_REQUESTS {
                bail!("Too many requests are waiting. Try again later.");
            }
            stored.requests.push(AccessRequest {
                id: new_id(),
                name,
                password_hash,
                created: now_secs(),
            });
        }
        self.save().await
    }

    // ── For the admin ──────────────────────────────────────────────────────

    /// Profiles, waiting requests and settings, for the admin's page.
    pub fn admin_view(&self) -> Value {
        let stored = self.stored.lock().unwrap();
        json!({
            "profiles": stored.profiles.iter().map(Profile::public).collect::<Vec<_>>(),
            "requests": stored.requests.iter().map(|r| json!({ "id": r.id, "name": r.name, "created": r.created })).collect::<Vec<_>>(),
            "settings": {
                "allowRequests": stored.settings.allow_requests,
                "timedPlay": stored.settings.timed_play,
                "turnMinutes": stored.settings.turn_minutes,
            },
        })
    }

    pub async fn create_profile(&self, name: &str, password: &str, admin: bool) -> Result<()> {
        let name = clean_name(name)?;
        check_strength(password)?;
        let password_hash = hash(password)?;
        {
            let mut stored = self.stored.lock().unwrap();
            if stored.name_taken(&name, None) {
                bail!("That name is taken.");
            }
            stored.profiles.push(Profile {
                id: new_id(),
                name,
                password_hash,
                admin,
                xbox_home: true,
                games: None,
                max_quality: Quality::Max,
            });
        }
        self.save().await
    }

    pub async fn update_profile(&self, id: &str, changes: ProfileChanges) -> Result<()> {
        let new_hash = match &changes.password {
            Some(p) if !p.is_empty() => {
                check_strength(p)?;
                Some(hash(p)?)
            }
            _ => None,
        };
        let new_name = changes.name.as_deref().map(clean_name).transpose()?;
        {
            let mut stored = self.stored.lock().unwrap();
            if let Some(name) = &new_name {
                if stored.name_taken(name, Some(id)) {
                    bail!("That name is taken.");
                }
            }
            let losing_admin = changes.admin == Some(false)
                && stored.profiles.iter().any(|p| p.id == id && p.admin);
            if losing_admin && stored.admins() <= 1 {
                bail!("There has to be at least one admin.");
            }
            let profile = stored
                .profiles
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or_else(|| anyhow!("No such profile."))?;
            if let Some(name) = new_name {
                profile.name = name;
            }
            if let Some(hash) = new_hash {
                profile.password_hash = hash;
            }
            if let Some(admin) = changes.admin {
                profile.admin = admin;
            }
            if let Some(xbox_home) = changes.xbox_home {
                profile.xbox_home = xbox_home;
            }
            if let Some(games) = changes.games {
                profile.games = games;
            }
            if let Some(quality) = changes.max_quality {
                profile.max_quality = quality;
            }
        }
        self.save().await
    }

    pub async fn delete_profile(&self, id: &str) -> Result<()> {
        {
            let mut stored = self.stored.lock().unwrap();
            let Some(index) = stored.profiles.iter().position(|p| p.id == id) else {
                bail!("No such profile.")
            };
            if stored.profiles[index].admin && stored.admins() <= 1 {
                bail!("There has to be at least one admin.");
            }
            stored.profiles.remove(index);
            stored.sessions.retain(|_, s| s.profile != id);
        }
        self.save().await
    }

    pub async fn answer_request(&self, id: &str, approve: bool) -> Result<()> {
        {
            let mut stored = self.stored.lock().unwrap();
            let Some(index) = stored.requests.iter().position(|r| r.id == id) else {
                bail!("That request is gone.")
            };
            let request = stored.requests.remove(index);
            if approve {
                stored.profiles.push(Profile {
                    id: new_id(),
                    name: request.name,
                    password_hash: request.password_hash,
                    admin: false,
                    xbox_home: true,
                    games: None,
                    max_quality: Quality::Max,
                });
            }
        }
        self.save().await
    }

    pub async fn set_allow_requests(&self, allow: bool) -> Result<()> {
        self.stored.lock().unwrap().settings.allow_requests = allow;
        if !allow {
            self.stored.lock().unwrap().requests.clear();
        }
        self.save().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn peer(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 50000)
    }

    fn with_cookie(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("other=1; {COOKIE}={token}")).unwrap(),
        );
        headers
    }

    async fn fresh() -> (Access, PathBuf) {
        let dir = std::env::temp_dir().join(format!("zp-access-{}", new_token()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        (Access::load(&dir).await.unwrap(), dir)
    }

    fn logged_in_as(verdict: Verdict) -> Option<String> {
        match verdict {
            Verdict::Ok(Some(p)) => Some(p.name),
            _ => None,
        }
    }

    #[test]
    fn home_addresses() {
        for ip in [
            "192.168.1.20",
            "10.0.0.5",
            "172.16.3.4",
            "127.0.0.1",
            "fd00::1",
            "::ffff:192.168.1.2",
        ] {
            assert!(is_home_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2a00:1450::1", "::ffff:8.8.8.8"] {
            assert!(!is_home_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn before_the_admin_only_home_gets_in() {
        let (access, dir) = fresh().await;
        let none = HeaderMap::new();
        assert_eq!(
            access.verdict(Door::Home, peer("192.168.1.20"), &none),
            Verdict::Ok(None)
        );
        assert_eq!(
            access.verdict(Door::Home, peer("8.8.8.8"), &none),
            Verdict::NotSetUp
        );
        assert_eq!(
            access.verdict(Door::Remote, peer("127.0.0.1"), &none),
            Verdict::NotSetUp
        );
        assert!(access
            .create_admin(Door::Remote, peer("127.0.0.1"), "Patrick", "longenough")
            .await
            .is_err());
        assert!(access
            .create_admin(Door::Home, peer("8.8.8.8"), "Patrick", "longenough")
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn profiles_log_in_by_name() {
        let (access, dir) = fresh().await;
        assert!(access
            .create_admin(Door::Home, peer("192.168.1.20"), "Patrick", "short")
            .await
            .is_err());
        let admin = access
            .create_admin(Door::Home, peer("192.168.1.20"), "Patrick", "longenough")
            .await
            .unwrap();
        assert!(access
            .create_admin(Door::Home, peer("192.168.1.20"), "Other", "longenough")
            .await
            .is_err());
        assert_eq!(
            logged_in_as(access.verdict(Door::Remote, peer("127.0.0.1"), &with_cookie(&admin)))
                .as_deref(),
            Some("Patrick")
        );
        assert_eq!(
            access.verdict(Door::Home, peer("192.168.1.20"), &HeaderMap::new()),
            Verdict::LoginNeeded
        );

        access
            .create_profile("Guest", "guestpass1", false)
            .await
            .unwrap();
        assert!(
            access
                .create_profile("guest", "guestpass1", false)
                .await
                .is_err(),
            "names are unique"
        );
        assert!(access.login("Guest", "wrong-pass").await.is_err());
        let guest = access.login("guest", "guestpass1").await.unwrap();
        let verdict = access.verdict(Door::Remote, peer("127.0.0.1"), &with_cookie(&guest));
        assert!(matches!(verdict, Verdict::Ok(Some(ref p)) if p.name == "Guest" && !p.admin));

        // Survives a restart.
        let again = Access::load(&dir).await.unwrap();
        assert_eq!(
            logged_in_as(again.verdict(Door::Remote, peer("127.0.0.1"), &with_cookie(&guest)))
                .as_deref(),
            Some("Guest")
        );

        // Deleting a profile ends its sessions; the last admin stays.
        let guest_id = access.admin_view()["profiles"][1]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        access.delete_profile(&guest_id).await.unwrap();
        assert_eq!(
            access.verdict(Door::Remote, peer("127.0.0.1"), &with_cookie(&guest)),
            Verdict::LoginNeeded
        );
        let admin_id = access.admin_view()["profiles"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(access.delete_profile(&admin_id).await.is_err());
        assert!(access
            .update_profile(
                &admin_id,
                ProfileChanges {
                    admin: Some(false),
                    ..Default::default()
                }
            )
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn access_requests_wait_for_the_admin() {
        let (access, dir) = fresh().await;
        access
            .create_admin(Door::Home, peer("192.168.1.20"), "Patrick", "longenough")
            .await
            .unwrap();
        assert!(
            access.request_access("Friend", "friendpass").await.is_err(),
            "off by default"
        );
        access.set_allow_requests(true).await.unwrap();
        access.request_access("Friend", "friendpass").await.unwrap();
        assert!(
            access.request_access("friend", "friendpass").await.is_err(),
            "name already asked for"
        );
        assert!(
            access.login("Friend", "friendpass").await.is_err(),
            "not approved yet"
        );
        let id = access.admin_view()["requests"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        access.answer_request(&id, true).await.unwrap();
        assert!(access.login("Friend", "friendpass").await.is_ok());
        assert_eq!(access.admin_view()["requests"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn profile_changes_apply() {
        let (access, dir) = fresh().await;
        access
            .create_admin(Door::Home, peer("192.168.1.20"), "Patrick", "longenough")
            .await
            .unwrap();
        access
            .create_profile("Kid", "kidpass12", false)
            .await
            .unwrap();
        let id = access.admin_view()["profiles"][1]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let changes: ProfileChanges = serde_json::from_str(
            r#"{"xboxHome":false,"games":["9NBLGGH4R315"],"maxQuality":"medium"}"#,
        )
        .unwrap();
        access.update_profile(&id, changes).await.unwrap();
        let p = access.profile(&id).unwrap();
        assert!(!p.xbox_home);
        assert_eq!(p.games.as_deref(), Some(&["9NBLGGH4R315".to_owned()][..]));
        assert_eq!(p.max_quality, Quality::Medium);
        // "games": null means all games again; leaving it out changes nothing.
        access
            .update_profile(
                &id,
                serde_json::from_str(r#"{"maxQuality":"low"}"#).unwrap(),
            )
            .await
            .unwrap();
        assert!(access.profile(&id).unwrap().games.is_some());
        access
            .update_profile(&id, serde_json::from_str(r#"{"games":null}"#).unwrap())
            .await
            .unwrap();
        assert!(access.profile(&id).unwrap().games.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_old_single_password_becomes_an_admin_profile() {
        let dir = std::env::temp_dir().join(format!("zp-access-{}", new_token()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let old = json!({ "password_hash": hash("oldpassword").unwrap(), "sessions": { "abc": now_secs() + 100 } });
        std::fs::write(dir.join("access.json"), old.to_string()).unwrap();
        let access = Access::load(&dir).await.unwrap();
        assert!(access.login("Admin", "oldpassword").await.is_ok());
        assert!(access.admin_view()["profiles"][0]["admin"]
            .as_bool()
            .unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn too_many_wrong_passwords_wait() {
        let (access, dir) = fresh().await;
        access
            .create_admin(Door::Home, peer("192.168.1.20"), "Patrick", "longenough")
            .await
            .unwrap();
        for _ in 0..5 {
            let _ = access.login("Patrick", "bad").await;
        }
        let error = access
            .login("Patrick", "longenough")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Too many"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
