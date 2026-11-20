//! Who may use the page.
//!
//! The server listens on two ports. The home port (8080) is for the home
//! network. The remote port (8081, only on 127.0.0.1) is where the playit
//! tunnel delivers visitors from the internet. Everything arriving there looks
//! like it comes from the NAS itself, so it is always treated as outside.
//!
//! Until a password is set, only the home network can use the page. Setting
//! the password is also only possible from the home network. After that,
//! everyone logs in with it and gets a session cookie.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::{header, HeaderMap};
use serde::{Deserialize, Serialize};

/// How long a login lasts.
const SESSION_DAYS: u64 = 30;
pub const COOKIE: &str = "zp_session";

/// Where a request came in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Door {
    Home,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Verdict {
    /// Let in.
    Ok,
    /// A password exists; this visitor has not logged in.
    LoginNeeded,
    /// No password yet and this visitor is not at home.
    NotSetUp,
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    password_hash: Option<String>,
    /// Session token → expiry (seconds since 1970).
    sessions: HashMap<String, u64>,
}

pub struct Access {
    path: PathBuf,
    stored: Mutex<Stored>,
    failures: Mutex<Vec<Instant>>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Private, loopback and link-local addresses: the home network.
pub fn is_home_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_private() || v4.is_loopback() || v4.is_link_local();
            }
            v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80
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

impl Access {
    pub async fn load(data_dir: &std::path::Path) -> Result<Access> {
        let path = data_dir.join("access.json");
        let stored = match tokio::fs::read_to_string(&path).await {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Stored::default(),
        };
        Ok(Access { path, stored: Mutex::new(stored), failures: Mutex::new(Vec::new()) })
    }

    async fn save(&self) -> Result<()> {
        let text = {
            let mut stored = self.stored.lock().unwrap();
            let now = now_secs();
            stored.sessions.retain(|_, expires| *expires > now);
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

    pub fn has_password(&self) -> bool {
        self.stored.lock().unwrap().password_hash.is_some()
    }

    fn session_valid(&self, token: &str) -> bool {
        self.stored.lock().unwrap().sessions.get(token).is_some_and(|expires| *expires > now_secs())
    }

    pub fn verdict(&self, door: Door, peer: SocketAddr, headers: &HeaderMap) -> Verdict {
        if !self.has_password() {
            return if at_home(door, peer) { Verdict::Ok } else { Verdict::NotSetUp };
        }
        match session_from(headers) {
            Some(token) if self.session_valid(&token) => Verdict::Ok,
            _ => Verdict::LoginNeeded,
        }
    }

    /// Sets the first password. Only from home, and only once.
    pub async fn set_password(&self, door: Door, peer: SocketAddr, password: &str) -> Result<String> {
        if !at_home(door, peer) {
            bail!("The password can only be set from your home network.");
        }
        if self.has_password() {
            bail!("A password is already set.");
        }
        check_strength(password)?;
        let hash = hash(password)?;
        let token = new_token();
        {
            let mut stored = self.stored.lock().unwrap();
            stored.password_hash = Some(hash);
            stored.sessions.insert(token.clone(), now_secs() + SESSION_DAYS * 86400);
        }
        self.save().await?;
        Ok(token)
    }

    /// Changes the password from a logged-in session at home. Logs out everyone else.
    pub async fn change_password(&self, door: Door, peer: SocketAddr, headers: &HeaderMap, password: &str) -> Result<String> {
        if !at_home(door, peer) {
            bail!("The password can only be changed from your home network.");
        }
        if self.verdict(door, peer, headers) != Verdict::Ok {
            bail!("Log in first.");
        }
        check_strength(password)?;
        let hash = hash(password)?;
        let token = new_token();
        {
            let mut stored = self.stored.lock().unwrap();
            stored.password_hash = Some(hash);
            stored.sessions.clear();
            stored.sessions.insert(token.clone(), now_secs() + SESSION_DAYS * 86400);
        }
        self.save().await?;
        Ok(token)
    }

    /// Checks a password and starts a session.
    pub async fn login(&self, password: &str) -> Result<String> {
        {
            let mut failures = self.failures.lock().unwrap();
            failures.retain(|t| t.elapsed() < Duration::from_secs(60));
            if failures.len() >= 5 {
                bail!("Too many wrong passwords. Wait a minute and try again.");
            }
        }
        let Some(stored_hash) = self.stored.lock().unwrap().password_hash.clone() else {
            bail!("No password is set yet.");
        };
        let ok = PasswordHash::new(&stored_hash)
            .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
            .unwrap_or(false);
        if !ok {
            self.failures.lock().unwrap().push(Instant::now());
            tokio::time::sleep(Duration::from_millis(800)).await;
            bail!("Wrong password.");
        }
        let token = new_token();
        self.stored.lock().unwrap().sessions.insert(token.clone(), now_secs() + SESSION_DAYS * 86400);
        self.save().await?;
        Ok(token)
    }

    pub async fn logout(&self, headers: &HeaderMap) {
        if let Some(token) = session_from(headers) {
            self.stored.lock().unwrap().sessions.remove(&token);
            let _ = self.save().await;
        }
    }
}

fn check_strength(password: &str) -> Result<()> {
    if password.chars().count() < 8 {
        bail!("Use at least 8 characters.");
    }
    Ok(())
}

fn hash(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("could not hash the password: {e}"))
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn session_cookie(token: &str) -> String {
    format!("{COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}", SESSION_DAYS * 86400)
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
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
        headers.insert(header::COOKIE, HeaderValue::from_str(&format!("other=1; {COOKIE}={token}")).unwrap());
        headers
    }

    async fn fresh() -> (Access, PathBuf) {
        let dir = std::env::temp_dir().join(format!("zp-access-{}", new_token()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        (Access::load(&dir).await.unwrap(), dir)
    }

    #[test]
    fn home_addresses() {
        for ip in ["192.168.1.20", "10.0.0.5", "172.16.3.4", "127.0.0.1", "fd00::1", "::ffff:192.168.1.2"] {
            assert!(is_home_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2a00:1450::1", "::ffff:8.8.8.8"] {
            assert!(!is_home_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn before_a_password_only_home_gets_in() {
        let (access, dir) = fresh().await;
        let none = HeaderMap::new();
        assert_eq!(access.verdict(Door::Home, peer("192.168.1.20"), &none), Verdict::Ok);
        assert_eq!(access.verdict(Door::Home, peer("8.8.8.8"), &none), Verdict::NotSetUp);
        // The tunnel delivers from the NAS itself, but through the remote door.
        assert_eq!(access.verdict(Door::Remote, peer("127.0.0.1"), &none), Verdict::NotSetUp);
        assert!(access.set_password(Door::Remote, peer("127.0.0.1"), "longenough").await.is_err());
        assert!(access.set_password(Door::Home, peer("8.8.8.8"), "longenough").await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn after_a_password_everyone_logs_in() {
        let (access, dir) = fresh().await;
        assert!(access.set_password(Door::Home, peer("192.168.1.20"), "short").await.is_err());
        let first = access.set_password(Door::Home, peer("192.168.1.20"), "longenough").await.unwrap();
        assert!(access.set_password(Door::Home, peer("192.168.1.20"), "another-one").await.is_err());

        let none = HeaderMap::new();
        assert_eq!(access.verdict(Door::Home, peer("192.168.1.20"), &none), Verdict::LoginNeeded);
        assert_eq!(access.verdict(Door::Home, peer("192.168.1.20"), &with_cookie(&first)), Verdict::Ok);
        assert_eq!(access.verdict(Door::Remote, peer("127.0.0.1"), &with_cookie("nope")), Verdict::LoginNeeded);

        assert!(access.login("wrong-password").await.is_err());
        let token = access.login("longenough").await.unwrap();
        assert_eq!(access.verdict(Door::Remote, peer("127.0.0.1"), &with_cookie(&token)), Verdict::Ok);

        // Survives a restart.
        let again = Access::load(&dir).await.unwrap();
        assert_eq!(again.verdict(Door::Remote, peer("127.0.0.1"), &with_cookie(&token)), Verdict::Ok);

        access.logout(&with_cookie(&token)).await;
        assert_eq!(access.verdict(Door::Remote, peer("127.0.0.1"), &with_cookie(&token)), Verdict::LoginNeeded);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn too_many_wrong_passwords_wait() {
        let (access, dir) = fresh().await;
        access.set_password(Door::Home, peer("192.168.1.20"), "longenough").await.unwrap();
        for _ in 0..5 {
            let _ = access.login("bad").await;
        }
        let error = access.login("longenough").await.unwrap_err().to_string();
        assert!(error.contains("Too many"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
