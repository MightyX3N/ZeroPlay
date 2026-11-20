//! playit.gg tunnel: linking the agent with a claim link, running it, and
//! creating its tunnels.
//!
//! The claim works like `playit setup`: we make a random code, the person
//! opens playit.gg/claim/<code> and approves it, and playit's API then hands
//! us the agent's secret key. The key is kept in the data folder and the
//! playit agent (`playitd`) is started with it. With the same key we create
//! two tunnels, one for the page (TCP) and one for the video (UDP), and read
//! back their public addresses.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;
use zeroplay_xbox::reqwest;

const API: &str = "https://api.playit.gg";

#[derive(Clone, Debug, PartialEq)]
enum State {
    Off,
    Claiming { url: String },
    Running,
    Failed { message: String },
}

/// The tunnels' public addresses, once playit has made them.
#[derive(Clone, Debug, Default, PartialEq)]
struct Tunnels {
    /// `host:port` of the page.
    web: Option<String>,
    /// Host and first port of the video ports, and how many ports.
    video: Option<(String, u16, u16)>,
    /// Something the person should know (for example a playit limit).
    note: Option<String>,
}

const WEB_TUNNEL: &str = "ZeroPlay web";
const VIDEO_TUNNEL: &str = "ZeroPlay video";

pub struct Playit {
    secret_path: PathBuf,
    web_port: u16,
    rtc_ports: Vec<u16>,
    tunnels: Mutex<Tunnels>,
    program: String,
    http: reqwest::Client,
    state: Mutex<State>,
    /// Bumped on every connect and disconnect so older tasks stop.
    generation: Mutex<u64>,
    updates: broadcast::Sender<()>,
}

/// playit's API answers `{"status": "success" | "fail" | "error", "data": ...}`.
#[derive(Deserialize)]
#[serde(tag = "status", content = "data", rename_all = "lowercase")]
enum ApiResult {
    Success(Value),
    Fail(Value),
    Error(Value),
}

impl Playit {
    pub fn new(
        data_dir: &std::path::Path,
        web_port: u16,
        rtc_ports: Vec<u16>,
        http: reqwest::Client,
        updates: broadcast::Sender<()>,
    ) -> Arc<Playit> {
        let program = std::env::var("ZEROPLAY_PLAYIT_BIN").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "playit".to_owned());
        Arc::new(Playit {
            secret_path: data_dir.join("playit-secret"),
            web_port,
            rtc_ports,
            tunnels: Mutex::new(Tunnels::default()),
            program,
            http,
            state: Mutex::new(State::Off),
            generation: Mutex::new(0),
            updates,
        })
    }

    fn set(&self, state: State) {
        *self.state.lock().unwrap() = state;
        let _ = self.updates.send(());
    }

    fn next_generation(&self) -> u64 {
        let mut generation = self.generation.lock().unwrap();
        *generation += 1;
        *generation
    }

    fn is_current(&self, generation: u64) -> bool {
        *self.generation.lock().unwrap() == generation
    }

    /// At startup: start the agent if it was linked before.
    pub async fn start_if_linked(self: &Arc<Self>) {
        if tokio::fs::try_exists(&self.secret_path).await.unwrap_or(false) {
            let generation = self.next_generation();
            self.clone().spawn_agent(generation);
        }
    }

    pub fn status(&self) -> Value {
        match &*self.state.lock().unwrap() {
            State::Off => json!({ "state": "off" }),
            State::Claiming { url } => json!({ "state": "claiming", "url": url }),
            State::Running => {
                let t = self.tunnels.lock().unwrap().clone();
                json!({
                    "state": "running",
                    "web": t.web,
                    "videoPorts": t.video.as_ref().map(|(_, _, count)| *count),
                    "note": t.note,
                })
            }
            State::Failed { message } => json!({ "state": "failed", "error": message }),
        }
    }

    /// Makes a claim link and waits in the background for it to be approved.
    pub fn connect(self: &Arc<Self>) {
        if matches!(*self.state.lock().unwrap(), State::Claiming { .. } | State::Running) {
            return;
        }
        let code = claim_code();
        let url = format!("https://playit.gg/claim/{code}");
        let generation = self.next_generation();
        self.set(State::Claiming { url });
        let playit = self.clone();
        tokio::spawn(async move {
            match playit.claim(&code, generation).await {
                Ok(secret) => {
                    if !playit.is_current(generation) {
                        return;
                    }
                    match playit.save_secret(&secret).await {
                        Ok(()) => playit.clone().spawn_agent(generation),
                        Err(error) => playit.set(State::Failed { message: format!("Could not save the key: {error:#}") }),
                    }
                }
                Err(error) => {
                    if playit.is_current(generation) {
                        playit.set(State::Failed { message: format!("{error:#}") });
                    }
                }
            }
        });
    }

    /// Stops the agent and forgets its key.
    pub async fn disconnect(&self) {
        self.next_generation();
        let _ = tokio::fs::remove_file(&self.secret_path).await;
        *self.tunnels.lock().unwrap() = Tunnels::default();
        self.set(State::Off);
    }

    async fn call(&self, path: &str, body: Value) -> Result<ApiResult> {
        let response = self.http.post(format!("{API}{path}")).json(&body).send().await?;
        let text = response.text().await?;
        serde_json::from_str(&text).map_err(|_| anyhow!("playit.gg answered something unexpected: {text}"))
    }

    async fn call_as_agent(&self, path: &str, body: Value) -> Result<ApiResult> {
        let secret = tokio::fs::read_to_string(&self.secret_path).await?;
        let response = self
            .http
            .post(format!("{API}{path}"))
            .header("Authorization", format!("Agent-Key {}", secret.trim()))
            .json(&body)
            .send()
            .await?;
        let text = response.text().await?;
        serde_json::from_str(&text).map_err(|_| anyhow!("playit.gg answered something unexpected: {text}"))
    }

    /// The public address for a player's video port, if the video tunnel covers it.
    pub fn video_address(&self, port_index: usize) -> Option<(String, u16)> {
        let tunnels = self.tunnels.lock().unwrap();
        let (host, first, count) = tunnels.video.as_ref()?;
        if port_index >= *count as usize {
            return None;
        }
        Some((host.clone(), first.saturating_add(port_index as u16)))
    }

    /// Keeps the two tunnels in place and their addresses up to date.
    async fn manage_tunnels(self: Arc<Self>, generation: u64) {
        let mut tried: Vec<&'static str> = Vec::new();
        tokio::time::sleep(Duration::from_secs(3)).await;
        while self.is_current(generation) {
            if let Err(error) = self.check_tunnels(&mut tried).await {
                println!("[playit] tunnels: {error:#}");
                self.tunnels.lock().unwrap().note = Some(format!("{error:#}"));
                let _ = self.updates.send(());
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    }

    async fn check_tunnels(&self, tried: &mut Vec<&'static str>) -> Result<()> {
        let data = match self.call_as_agent("/v1/agents/rundata", json!({})).await? {
            ApiResult::Success(data) => data,
            other => bail!("playit.gg would not list the tunnels: {}", describe(&other)),
        };
        let agent_id = data.get("agent_id").and_then(Value::as_str).ok_or_else(|| anyhow!("playit.gg sent no agent id"))?.to_owned();
        let listed = |key: &str| -> Vec<Value> { data.get(key).and_then(Value::as_array).cloned().unwrap_or_default() };
        let tunnels = listed("tunnels");
        let pending = listed("pending");
        let find = |name: &str| tunnels.iter().find(|t| t.get("name").and_then(Value::as_str) == Some(name)).cloned();
        let is_pending = |name: &str| pending.iter().any(|t| t.get("name").and_then(Value::as_str) == Some(name));

        let mut note = None;
        if find(WEB_TUNNEL).is_none() && !is_pending(WEB_TUNNEL) && !tried.contains(&WEB_TUNNEL) {
            tried.push(WEB_TUNNEL);
            if let Err(error) = self.create_tunnel(&agent_id, WEB_TUNNEL, "custom-tcp", 1, "127.0.0.1", self.web_port).await {
                note = Some(format!("Could not create the page tunnel: {error:#}"));
            }
        }
        if find(VIDEO_TUNNEL).is_none() && !is_pending(VIDEO_TUNNEL) && !tried.contains(&VIDEO_TUNNEL) {
            tried.push(VIDEO_TUNNEL);
            let first = self.rtc_ports.first().copied().unwrap_or(50000);
            let ip = lan_ip();
            let count = self.rtc_ports.len().min(u16::MAX as usize) as u16;
            if let Err(error) = self.create_tunnel(&agent_id, VIDEO_TUNNEL, "custom-udp", count, &ip, first).await {
                // Port ranges need playit premium; one port works on a free account.
                println!("[playit] {count} video ports refused ({error:#}), trying one port");
                match self.create_tunnel(&agent_id, VIDEO_TUNNEL, "custom-udp", 1, &ip, first).await {
                    Ok(()) => note = Some("Free playit account: one video port, so one player at a time from outside your home. playit premium allows more.".to_owned()),
                    Err(error) => note = Some(format!("Could not create the video tunnel: {error:#}")),
                }
            }
        }

        let address = |tunnel: &Value| tunnel.get("display_address").and_then(Value::as_str).map(str::to_owned);
        let web = find(WEB_TUNNEL).and_then(|t| address(&t));
        let video = find(VIDEO_TUNNEL).and_then(|t| {
            let (host, port) = address(&t)?.rsplit_once(':').map(|(h, p)| (h.to_owned(), p.parse::<u16>().ok()))?;
            let count = t.get("port_count").and_then(Value::as_u64).unwrap_or(1) as u16;
            Some((host, port?, count))
        });
        let mut current = self.tunnels.lock().unwrap();
        let new = Tunnels { web, video, note: note.or_else(|| current.note.clone().filter(|n| n.starts_with("Free playit"))) };
        if *current != new {
            *current = new;
            drop(current);
            let _ = self.updates.send(());
        }
        Ok(())
    }

    async fn create_tunnel(&self, agent_id: &str, name: &str, kind: &str, ports: u16, local_ip: &str, local_port: u16) -> Result<()> {
        let body = json!({
            "name": name,
            "ports": { "type": kind, "details": ports },
            "origin": {
                "type": "agent",
                "data": {
                    "agent_id": agent_id,
                    "config": { "fields": [
                        { "name": "local_ip", "value": local_ip },
                        { "name": "local_port", "value": local_port.to_string() },
                    ] },
                },
            },
            "enabled": true,
            "alloc": null,
            "firewall_id": null,
        });
        match self.call_as_agent("/v1/tunnels/create", body).await? {
            ApiResult::Success(_) => {
                println!("[playit] created tunnel \"{name}\" ({kind}, {ports} port(s) to {local_ip}:{local_port})");
                Ok(())
            }
            other => bail!("{}", describe(&other)),
        }
    }

    async fn claim(&self, code: &str, generation: u64) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(15 * 60);
        // Wait for the person to open the link and approve it.
        loop {
            if !self.is_current(generation) {
                bail!("cancelled");
            }
            if Instant::now() > deadline {
                bail!("The claim link was not approved within 15 minutes. Try again.");
            }
            let body = json!({ "code": code, "agent_type": "assignable", "version": concat!("ZeroPlay ", env!("CARGO_PKG_VERSION")) });
            match self.call("/claim/setup", body).await {
                Ok(ApiResult::Success(Value::String(step))) => match step.as_str() {
                    "UserAccepted" => break,
                    "UserRejected" => bail!("The claim was rejected on playit.gg."),
                    _ => {}
                },
                Ok(ApiResult::Fail(reason)) => bail!("playit.gg refused the claim: {reason}"),
                Ok(other) => println!("[playit] claim setup: {}", describe(&other)),
                Err(error) => println!("[playit] claim setup failed, retrying: {error:#}"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        // Collect the key.
        loop {
            if !self.is_current(generation) {
                bail!("cancelled");
            }
            if Instant::now() > deadline {
                bail!("playit.gg did not hand over the key in time. Try again.");
            }
            match self.call("/claim/exchange", json!({ "code": code })).await {
                Ok(ApiResult::Success(data)) => {
                    if let Some(key) = data.get("secret_key").and_then(Value::as_str) {
                        return Ok(key.to_owned());
                    }
                    bail!("playit.gg sent no key: {data}");
                }
                Ok(ApiResult::Fail(reason)) if reason == "NotAccepted" || reason == "NotSetup" => {}
                Ok(ApiResult::Fail(reason)) => bail!("playit.gg refused the claim: {reason}"),
                Ok(other) => println!("[playit] claim exchange: {}", describe(&other)),
                Err(error) => println!("[playit] claim exchange failed, retrying: {error:#}"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn save_secret(&self, secret: &str) -> Result<()> {
        if secret.is_empty() || !secret.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("that is not a playit secret key");
        }
        tokio::fs::write(&self.secret_path, format!("{secret}\n")).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&self.secret_path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        Ok(())
    }

    /// Runs the agent, restarting it if it stops, until disconnected.
    fn spawn_agent(self: Arc<Self>, generation: u64) {
        tokio::spawn(self.clone().manage_tunnels(generation));
        tokio::spawn(async move {
            let mut wait = Duration::from_secs(5);
            while self.is_current(generation) {
                let child = tokio::process::Command::new(&self.program)
                    .arg("--secret-path")
                    .arg(&self.secret_path)
                    .arg("--platform-docker")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn();
                let mut child = match child {
                    Ok(child) => child,
                    Err(error) => {
                        self.set(State::Failed { message: format!("Could not start the playit agent ({}): {error}", self.program) });
                        return;
                    }
                };
                self.set(State::Running);
                println!("[playit] agent started");
                for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), child.stderr.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)].into_iter().flatten() {
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(pipe).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            println!("[playit] {line}");
                        }
                    });
                }
                let started = Instant::now();
                // Stop the agent when disconnected; otherwise wait for it to exit.
                let exit = loop {
                    tokio::select! {
                        status = child.wait() => break Some(status),
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {
                            if !self.is_current(generation) {
                                let _ = child.kill().await;
                                break None;
                            }
                        }
                    }
                };
                let Some(status) = exit else { return };
                let message = match status {
                    Ok(status) => format!("The playit agent stopped ({status}). Restarting…"),
                    Err(error) => format!("The playit agent stopped ({error}). Restarting…"),
                };
                println!("[playit] {message}");
                self.set(State::Failed { message });
                if started.elapsed() > Duration::from_secs(60) {
                    wait = Duration::from_secs(5);
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(120));
            }
        });
    }
}

/// This machine's address on the home network (the one it would use to
/// reach the internet). The video tunnel points here, because the video
/// sockets listen on the network addresses, not on 127.0.0.1.
fn lan_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("1.1.1.1:80")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_owned())
}

fn describe(result: &ApiResult) -> String {
    match result {
        ApiResult::Success(v) | ApiResult::Fail(v) | ApiResult::Error(v) => v.to_string(),
    }
}

/// Ten hex characters, like `playit setup` makes.
fn claim_code() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    bytes[..5].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_codes_are_ten_hex_characters() {
        let code = claim_code();
        assert_eq!(code.len(), 10);
        assert!(code.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(code, claim_code());
    }

    #[test]
    fn api_answers_parse() {
        let ok: ApiResult = serde_json::from_str(r#"{"status":"success","data":"WaitingForUser"}"#).unwrap();
        assert!(matches!(ok, ApiResult::Success(Value::String(s)) if s == "WaitingForUser"));
        let key: ApiResult = serde_json::from_str(r#"{"status":"success","data":{"secret_key":"abcd"}}"#).unwrap();
        assert!(matches!(key, ApiResult::Success(v) if v["secret_key"] == "abcd"));
        let fail: ApiResult = serde_json::from_str(r#"{"status":"fail","data":"NotAccepted"}"#).unwrap();
        assert!(matches!(fail, ApiResult::Fail(v) if v == "NotAccepted"));
        let error: ApiResult = serde_json::from_str(r#"{"status":"error","data":{"message":"x"}}"#).unwrap();
        assert!(matches!(error, ApiResult::Error(_)));
    }
}
