//! playit.gg tunnel: linking the agent with a claim link, running it, and
//! creating its tunnels.
//!
//! The claim works like `playit setup`: we make a random code, the person
//! opens playit.gg/claim/<code> and approves it, and playit's API then hands
//! us the agent's secret key. The key is kept in the data folder and the
//! playit agent (`playitd`) is started with it. With the same key we create
//! one tunnel for the page (https on a playit domain when the account can,
//! otherwise TCP) and read back its public address. Players outside the home
//! get the video inside the page's connection, so nothing else is needed.

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
    Claiming { url: String, reason: Option<String> },
    Running,
    Failed { message: String },
}

/// The tunnels' public addresses, once playit has made them.
#[derive(Clone, Debug, Default, PartialEq)]
struct Tunnels {
    /// `host:port` of the page.
    web: Option<String>,
    /// `https://…` address of the HTTPS tunnel (Premium, on a playit domain).
    secure: Option<String>,
    /// Something the person should know (for example a playit limit).
    note: Option<String>,
}

const WEB_TUNNEL: &str = "ZeroPlay web";
const VIDEO_TUNNEL: &str = "ZeroPlay video";
const HTTPS_TUNNEL: &str = "ZeroPlay https";

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
        let program = std::env::var("ZEROPLAY_PLAYIT_BIN")
            .ok()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "playit".to_owned());
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
        if tokio::fs::try_exists(&self.secret_path)
            .await
            .unwrap_or(false)
        {
            let generation = self.next_generation();
            self.clone().spawn_agent(generation);
        }
    }

    pub fn status(&self) -> Value {
        match &*self.state.lock().unwrap() {
            State::Off => json!({ "state": "off" }),
            State::Claiming { url, reason } => {
                json!({ "state": "claiming", "url": url, "reason": reason })
            }
            State::Running => {
                let t = self.tunnels.lock().unwrap().clone();
                json!({
                    "state": "running",
                    "web": t.web,
                    "secure": t.secure,
                    "note": t.note,
                })
            }
            State::Failed { message } => json!({ "state": "failed", "error": message }),
        }
    }

    /// Makes a claim link and waits in the background for it to be approved.
    pub fn connect(self: &Arc<Self>) {
        if matches!(
            *self.state.lock().unwrap(),
            State::Claiming { .. } | State::Running
        ) {
            return;
        }
        self.start_claim(None);
    }

    fn start_claim(self: &Arc<Self>, reason: Option<String>) {
        let code = claim_code();
        let url = format!("https://playit.gg/claim/{code}");
        let generation = self.next_generation();
        self.set(State::Claiming { url, reason });
        let playit = self.clone();
        tokio::spawn(async move {
            match playit.claim(&code, generation).await {
                Ok(secret) => {
                    if !playit.is_current(generation) {
                        return;
                    }
                    let _ = tokio::fs::write(playit.claim_type_path(), "self-managed\n").await;
                    match playit.save_secret(&secret).await {
                        Ok(()) => playit.clone().spawn_agent(generation),
                        Err(error) => playit.set(State::Failed {
                            message: format!("Could not save the key: {error:#}"),
                        }),
                    }
                }
                Err(error) => {
                    if playit.is_current(generation) {
                        playit.set(State::Failed {
                            message: format!("{error:#}"),
                        });
                    }
                }
            }
        });
    }

    fn claim_type_path(&self) -> PathBuf {
        self.secret_path.with_file_name("playit-claim-type")
    }

    /// Stops the agent and forgets its key.
    pub async fn disconnect(&self) {
        self.next_generation();
        let _ = tokio::fs::remove_file(&self.secret_path).await;
        let _ = tokio::fs::remove_file(self.claim_type_path()).await;
        *self.tunnels.lock().unwrap() = Tunnels::default();
        self.set(State::Off);
    }

    async fn call(&self, path: &str, body: Value) -> Result<ApiResult> {
        let response = self
            .http
            .post(format!("{API}{path}"))
            .json(&body)
            .send()
            .await?;
        let text = response.text().await?;
        serde_json::from_str(&text)
            .map_err(|_| anyhow!("playit.gg answered something unexpected: {text}"))
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
        serde_json::from_str(&text)
            .map_err(|_| anyhow!("playit.gg answered something unexpected: {text}"))
    }

    /// Keeps the tunnel in place and their addresses up to date.
    async fn manage_tunnels(self: Arc<Self>, generation: u64) {
        let mut tried: Vec<String> = Vec::new();
        tokio::time::sleep(Duration::from_secs(3)).await;
        while self.is_current(generation) {
            match self.check_tunnels(&mut tried).await {
                Ok(true) => {}
                Ok(false) => {
                    // Linked the old way: this key may not create tunnels. Link again
                    // as a self-managed agent, which may.
                    let self_managed = tokio::fs::read_to_string(self.claim_type_path())
                        .await
                        .is_ok_and(|t| t.trim() == "self-managed");
                    if !self_managed {
                        println!("[playit] the agent key is read-only; asking to link again");
                        self.disconnect().await;
                        self.start_claim(Some(
                            "playit has to be linked once more so ZeroPlay can create its tunnels."
                                .to_owned(),
                        ));
                        return;
                    }
                    let note = format!(
                        "playit does not let this agent create tunnels. Create one on playit.gg for this agent: TCP to 127.0.0.1 port {}.",
                        self.web_port
                    );
                    self.tunnels.lock().unwrap().note = Some(note);
                    let _ = self.updates.send(());
                }
                Err(error) => {
                    println!("[playit] tunnels: {error:#}");
                    self.tunnels.lock().unwrap().note = Some(format!("{error:#}"));
                    let _ = self.updates.send(());
                }
            }
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    }

    /// Returns `Ok(false)` when playit says this key may not create tunnels.
    async fn check_tunnels(&self, tried: &mut Vec<String>) -> Result<bool> {
        let data = match self.call_as_agent("/v1/agents/rundata", json!({})).await? {
            ApiResult::Success(data) => data,
            other => bail!("playit.gg would not list the tunnels: {}", describe(&other)),
        };
        let premium = data
            .pointer("/permissions/has_premium")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let agent_id = data
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("playit.gg sent no agent id"))?
            .to_owned();
        let listed = |key: &str| -> Vec<Value> {
            data.get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let tunnels = listed("tunnels");
        let pending = listed("pending");
        // A tunnel counts by its name, or by where it points (so tunnels made by
        // hand on playit.gg are found too).
        let video_port = self.rtc_ports.first().copied().unwrap_or(50000);
        let target = |name: &str| {
            if name == VIDEO_TUNNEL {
                ("udp", video_port)
            } else {
                ("tcp", self.web_port)
            }
        };
        let matches = |t: &Value, name: &str| {
            if t.get("name").and_then(Value::as_str) == Some(name) {
                return true;
            }
            let (kind, port) = target(name);
            let field = |wanted: &str| {
                t.pointer("/agent_config/fields")
                    .and_then(Value::as_array)
                    .and_then(|fields| {
                        fields
                            .iter()
                            .find(|f| f.get("name").and_then(Value::as_str) == Some(wanted))
                    })
                    .and_then(|f| f.get("value").and_then(Value::as_str))
                    .and_then(|v| v.trim().parse::<u16>().ok())
            };
            let is_https = t.get("tunnel_type").and_then(Value::as_str) == Some("https");
            if name == HTTPS_TUNNEL {
                return is_https && field("http_port") == Some(self.web_port);
            }
            if is_https {
                return false;
            }
            t.get("port_type").and_then(Value::as_str) == Some(kind)
                && field("local_port") == Some(port)
        };
        let find = |name: &str| tunnels.iter().find(|t| matches(t, name)).cloned();
        let is_pending = |name: &str| {
            pending
                .iter()
                .any(|t| t.get("name").and_then(Value::as_str) == Some(name))
        };
        let read_only =
            |error: &anyhow::Error| error.to_string().contains("NotAllowedWithReadOnly");

        let mut notes: Vec<String> = Vec::new();
        let delete = |tunnel: &Value| {
            let id = tunnel
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            async move {
                self.call_as_agent("/tunnels/delete", json!({ "tunnel_id": id }))
                    .await
            }
        };

        // One tunnel only: the page, and the video inside the page's connection.
        // A separate video tunnel from earlier versions is removed.
        if let Some(video) = find(VIDEO_TUNNEL) {
            if let Ok(ApiResult::Success(_)) = delete(&video).await {
                println!("[playit] removed the separate video tunnel (video now goes through the page tunnel)");
            }
        }

        // With Premium and a playit domain, the page tunnel is https (also what
        // the iPhone home-screen app needs).
        let mut https = find(HTTPS_TUNNEL);
        if premium && https.is_none() && !is_pending(HTTPS_TUNNEL) {
            match self
                .create_https_tunnel(&agent_id, find(WEB_TUNNEL), tried)
                .await
            {
                Ok(None) => {}
                Ok(Some(message)) => notes.push(message),
                Err(error) if read_only(&error) => {
                    tried.clear();
                    return Ok(false);
                }
                Err(error) => notes.push(format!("Could not create the https tunnel: {error:#}")),
            }
            https = None;
        }
        let made_https =
            is_pending(HTTPS_TUNNEL) || tried.iter().any(|t| t.starts_with("https:ok"));

        match (&https, find(WEB_TUNNEL)) {
            // https is there: the plain tunnel is not needed.
            (Some(_), Some(plain)) => {
                if let Ok(ApiResult::Success(_)) = delete(&plain).await {
                    println!("[playit] removed the plain page tunnel (https is in place)");
                }
            }
            // No https (no Premium or no domain): a plain page tunnel.
            (None, None)
                if !made_https
                    && !is_pending(WEB_TUNNEL)
                    && !tried.iter().any(|t| t == WEB_TUNNEL) =>
            {
                tried.push(WEB_TUNNEL.to_owned());
                if let Err(error) = self.create_web_tunnel(&agent_id, premium).await {
                    if read_only(&error) {
                        tried.clear();
                        return Ok(false);
                    }
                    notes.push(format!("Could not create the page tunnel: {error:#}"));
                }
            }
            _ => {}
        }

        let address = |tunnel: &Value| {
            tunnel
                .get("display_address")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let secure = https
            .as_ref()
            .and_then(address)
            .map(|a| format!("https://{}", a.trim_end_matches(":443")));
        let web = if secure.is_some() {
            None
        } else {
            find(WEB_TUNNEL).and_then(|t| {
                let is_https = t.get("tunnel_type").and_then(Value::as_str) == Some("https");
                address(&t).map(|a| format!("{}{a}", if is_https { "https://" } else { "http://" }))
            })
        };
        let mut current = self.tunnels.lock().unwrap();
        // Keep only the lasting "free account" note once nothing new went wrong.
        let lasting = |n: &String| n.starts_with("For an https address");
        let note = if notes.is_empty() {
            current.note.clone().filter(lasting)
        } else {
            Some(notes.join(" "))
        };
        let new = Tunnels { web, secure, note };
        if *current != new {
            *current = new;
            drop(current);
            let _ = self.updates.send(());
        }
        Ok(true)
    }

    /// Creates a tunnel to this agent, with the request body playit.gg's own
    /// website sends (the same one Korie uses).
    async fn create_tunnel(
        &self,
        agent_id: &str,
        name: &str,
        protocol: Value,
        fields: Value,
        region: bool,
    ) -> Result<()> {
        let mut body = json!({
            "name": name,
            "protocol": protocol,
            "origin": { "type": "agent", "data": { "agent_id": agent_id, "config": { "fields": fields } } },
            "enabled": true,
        });
        if region {
            body["endpoint"] =
                json!({ "type": "region", "details": { "region": "global", "port": null } });
        }
        match self.call_as_agent("/v1/tunnels/create", body).await? {
            ApiResult::Success(_) => {
                println!("[playit] created tunnel \"{name}\"");
                Ok(())
            }
            other => bail!("{}", describe(&other)),
        }
    }

    /// The page. With playit Premium a raw TCP port (plain http://address:port);
    /// without it an HTTPS tunnel (free, certificate by playit). Each falls back
    /// to the other if playit refuses it.
    async fn create_web_tunnel(&self, agent_id: &str, premium: bool) -> Result<()> {
        let port = self.web_port.to_string();
        let raw_tcp = || {
            (
                json!({ "type": "raw-ports", "details": { "port_type": "tcp", "port_count": 1, "software_description": "ZeroPlay game streaming (web page)" } }),
                json!([{ "name": "local_ip", "value": "127.0.0.1" }, { "name": "local_port", "value": port }]),
                true,
            )
        };
        let https = |region: bool| {
            (
                json!({ "type": "tunnel-type", "details": "https" }),
                json!([
                    { "name": "local_ip", "value": "127.0.0.1" },
                    { "name": "http_port", "value": port },
                    { "name": "https_port", "value": port },
                    { "name": "tls_termination", "value": "letsencrypt" },
                ]),
                region,
            )
        };
        let tries = if premium {
            vec![raw_tcp(), https(true), https(false)]
        } else {
            vec![https(true), https(false), raw_tcp()]
        };
        let mut errors = Vec::new();
        for (protocol, fields, region) in tries {
            match self
                .create_tunnel(agent_id, WEB_TUNNEL, protocol, fields, region)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    println!("[playit] page tunnel attempt refused: {error:#}");
                    if error.to_string().contains("NotAllowedWithReadOnly") {
                        return Err(error);
                    }
                    errors.push(format!("{error:#}"));
                }
            }
        }
        errors.dedup();
        bail!("{}", errors.join("; "))
    }

    /// Finds a free domain on the playit account and puts an HTTPS tunnel on
    /// it. Returns a message for the person when there is no free domain.
    async fn create_https_tunnel(
        &self,
        agent_id: &str,
        web_tunnel: Option<Value>,
        tried: &mut Vec<String>,
    ) -> Result<Option<String>> {
        let domains = match self.call_as_agent("/domains/list", json!({})).await? {
            ApiResult::Success(data) => data
                .get("domains")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            other => bail!("playit.gg would not list the domains: {}", describe(&other)),
        };
        let free: Vec<&Value> = domains
            .iter()
            .filter(|d| d.get("target").is_none_or(Value::is_null))
            .collect();
        let pick = free
            .iter()
            .find(|d| {
                d.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.to_ascii_lowercase().contains("zeroplay"))
            })
            .or_else(|| free.first())
            .copied();
        // A domain put on ZeroPlay's own plain page tunnel (playit only lets
        // you add a domain to that one) moves to the https tunnel: the plain
        // tunnel is removed, and made again without the domain afterwards.
        let web_host = web_tunnel
            .as_ref()
            .and_then(|t| t.get("display_address").and_then(Value::as_str))
            .map(|a| {
                a.rsplit_once(':')
                    .map(|(h, _)| h)
                    .unwrap_or(a)
                    .to_ascii_lowercase()
            });
        let web_id = web_tunnel
            .as_ref()
            .and_then(|t| t.get("id").and_then(Value::as_str))
            .map(str::to_owned);
        let on_plain = || {
            domains.iter().find(|d| {
                let targets_plain = d
                    .pointer("/target/details/tunnel_id")
                    .and_then(Value::as_str)
                    == web_id.as_deref()
                    && web_id.is_some();
                targets_plain
                    || d.get("name")
                        .and_then(Value::as_str)
                        .map(str::to_ascii_lowercase)
                        == web_host
            })
        };
        let pick = match (pick, web_tunnel.as_ref(), on_plain()) {
            (Some(domain), _, _) => Some(domain),
            (None, Some(tunnel), Some(domain)) => {
                let tunnel_id = tunnel.get("id").and_then(Value::as_str).unwrap_or_default();
                match self
                    .call_as_agent("/tunnels/delete", json!({ "tunnel_id": tunnel_id }))
                    .await?
                {
                    ApiResult::Success(_) => {
                        println!(
                            "[playit] moved {} from the plain page tunnel to https",
                            domain.get("name").and_then(Value::as_str).unwrap_or("")
                        );
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        Some(domain)
                    }
                    other => bail!(
                        "could not take the domain off the plain page tunnel: {}",
                        describe(&other)
                    ),
                }
            }
            _ => None,
        };
        let Some(domain) = pick else {
            return Ok(Some(
                "For an https address (needed for the iPhone home-screen app), claim a free .playit.plus domain on playit.gg (Premium includes 3). ZeroPlay then uses it by itself."
                    .to_owned(),
            ));
        };
        let id = domain
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let sub_id = domain
            .get("sub_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let name = domain
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if id.is_empty() || tried.contains(&id) {
            return Ok(None);
        }
        tried.push(id.clone());

        let port = self.web_port.to_string();
        let protocol = json!({ "type": "tunnel-type", "details": "https" });
        let fields = json!([
            { "name": "local_ip", "value": "127.0.0.1" },
            { "name": "http_port", "value": port },
            { "name": "https_port", "value": port },
            { "name": "tls_termination", "value": "letsencrypt" },
        ]);
        // The request playit's own agent code describes for a tunnel on a domain
        // (`ports` plus a `hostname` allocation), with the domain's id or sub id;
        // then the shape playit's website sends. The agent (playitd 2.0.2) gets
        // the Let's Encrypt certificate itself because of `tls_termination`.
        let origin = json!({ "type": "agent", "data": { "agent_id": agent_id, "config": { "fields": fields } } });
        let mut bodies = Vec::new();
        for hostname_id in [&id, &sub_id] {
            if hostname_id.is_empty() {
                continue;
            }
            bodies.push(json!({
                "name": HTTPS_TUNNEL,
                "ports": protocol,
                "origin": origin,
                "enabled": true,
                "alloc": { "type": "hostname", "details": { "hostname_id": hostname_id } },
                "firewall_id": null,
            }));
        }
        for extra in [
            json!({ "endpoint": { "type": "hostname", "details": { "hostname_id": id } } }),
            json!({ "endpoint": { "type": "domain", "details": { "domain_id": id } } }),
        ] {
            let mut body = json!({ "name": HTTPS_TUNNEL, "protocol": protocol, "origin": origin, "enabled": true });
            for (key, value) in extra.as_object().unwrap() {
                body[key] = value.clone();
            }
            bodies.push(body);
        }
        let mut errors = Vec::new();
        for body in bodies {
            match self.call_as_agent("/v1/tunnels/create", body).await? {
                ApiResult::Success(_) => {
                    println!("[playit] created the https tunnel on {name}");
                    tried.push("https:ok".to_owned());
                    return Ok(None);
                }
                other => {
                    let why = describe(&other);
                    println!("[playit] https tunnel on {name} refused: {why}");
                    errors.push(why);
                }
            }
        }
        errors.dedup();
        bail!("on {name}: {}", errors.join("; "))
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
            let body = json!({ "code": code, "agent_type": "self-managed", "version": concat!("ZeroPlay ", env!("CARGO_PKG_VERSION")) });
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
            tokio::fs::set_permissions(&self.secret_path, std::fs::Permissions::from_mode(0o600))
                .await?;
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
                        self.set(State::Failed {
                            message: format!(
                                "Could not start the playit agent ({}): {error}",
                                self.program
                            ),
                        });
                        return;
                    }
                };
                self.set(State::Running);
                println!("[playit] agent started");
                for pipe in [
                    child
                        .stdout
                        .take()
                        .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                    child
                        .stderr
                        .take()
                        .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                ]
                .into_iter()
                .flatten()
                {
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

fn describe(result: &ApiResult) -> String {
    let value = match result {
        ApiResult::Success(v) | ApiResult::Fail(v) | ApiResult::Error(v) => v,
    };
    let text = value
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| value.to_string());
    if text == "RequiresPlayitPremium" || text == "PublicPortRequiresPlayitPremium" {
        return "this needs playit Premium on your playit.gg account".to_owned();
    }
    text
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
        let ok: ApiResult =
            serde_json::from_str(r#"{"status":"success","data":"WaitingForUser"}"#).unwrap();
        assert!(matches!(ok, ApiResult::Success(Value::String(s)) if s == "WaitingForUser"));
        let key: ApiResult =
            serde_json::from_str(r#"{"status":"success","data":{"secret_key":"abcd"}}"#).unwrap();
        assert!(matches!(key, ApiResult::Success(v) if v["secret_key"] == "abcd"));
        let fail: ApiResult =
            serde_json::from_str(r#"{"status":"fail","data":"NotAccepted"}"#).unwrap();
        assert!(matches!(fail, ApiResult::Fail(v) if v == "NotAccepted"));
        let error: ApiResult =
            serde_json::from_str(r#"{"status":"error","data":{"message":"x"}}"#).unwrap();
        assert!(matches!(error, ApiResult::Error(_)));
    }
}
