//! Everything the server keeps track of: the Microsoft sign-in, the consoles,
//! the one stream from the console, and who holds which controller slot.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use zeroplay_xbox::api::HomeApi;
use zeroplay_xbox::auth::Auth;
use zeroplay_xbox::consoles::{self, Console};
use zeroplay_xbox::input::{GamepadState, KeyboardFrame, MouseFrame};
use zeroplay_xbox::reqwest;
use zeroplay_xbox::session::{start_home_session, MediaPacket, Session, SessionEvent, StreamOptions};

use crate::config::Config;

/// Console slots on Xbox.
pub const SLOTS: usize = 4;

/// A message sent to one player's connection only.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Direct {
    Answer { sdp: String },
    Rumble { local: u8, left: f32, right: f32, left_trigger: f32, right_trigger: f32, ms: u16 },
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq)]
enum StreamState {
    Idle,
    Starting { console_id: String, console_name: String, step: String },
    Live { console_id: String, console_name: String, title: Option<String> },
    Failed { message: String },
}

struct Viewer {
    name: String,
    direct: mpsc::UnboundedSender<Direct>,
}

/// Which player and which of their controllers holds a console slot.
type SlotOwner = (u64, u8);

struct Inner {
    signed_in: bool,
    login: Option<(String, String)>,
    login_error: Option<String>,
    consoles: Vec<Console>,
    console_error: Option<String>,
    stream: StreamState,
    viewers: HashMap<u64, Viewer>,
    slots: [Option<SlotOwner>; SLOTS],
    pads: [Option<GamepadState>; SLOTS],
    free_ports: Vec<u16>,
    last_keyframe: Option<Instant>,
}

pub struct App {
    pub config: Config,
    auth: tokio::sync::Mutex<Auth>,
    http: reqwest::Client,
    inner: Mutex<Inner>,
    session: Mutex<Option<Arc<Session>>>,
    /// Bumped by every start and stop, so a start that finishes after a stop
    /// knows it is no longer wanted.
    generation: AtomicU64,
    next_viewer: AtomicU64,
    /// Every packet from the console. Each player's connection subscribes.
    pub media: broadcast::Sender<MediaPacket>,
    updates: broadcast::Sender<()>,
}

impl App {
    pub async fn new(config: Config) -> Result<Arc<App>> {
        tokio::fs::create_dir_all(&config.data_dir).await?;
        let auth = Auth::load(config.data_dir.join("tokens.json")).await?;
        let signed_in = auth.is_signed_in();
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        let (media, _) = broadcast::channel(2048);
        let (updates, _) = broadcast::channel(64);
        let free_ports = config.rtc_ports.iter().rev().copied().collect();
        let app = Arc::new(App {
            config,
            auth: tokio::sync::Mutex::new(auth),
            http,
            inner: Mutex::new(Inner {
                signed_in,
                login: None,
                login_error: None,
                consoles: Vec::new(),
                console_error: None,
                stream: StreamState::Idle,
                viewers: HashMap::new(),
                slots: [None; SLOTS],
                pads: [None; SLOTS],
                free_ports,
                last_keyframe: None,
            }),
            session: Mutex::new(None),
            generation: AtomicU64::new(0),
            next_viewer: AtomicU64::new(1),
            media,
            updates,
        });
        if signed_in {
            let app2 = app.clone();
            tokio::spawn(async move { app2.refresh_consoles().await });
        }
        Ok(app)
    }

    fn changed(&self) {
        let _ = self.updates.send(());
    }

    pub fn subscribe_updates(&self) -> broadcast::Receiver<()> {
        self.updates.subscribe()
    }

    // ── Players ────────────────────────────────────────────────────────────

    pub fn add_viewer(&self) -> (u64, mpsc::UnboundedReceiver<Direct>) {
        let id = self.next_viewer.fetch_add(1, Ordering::Relaxed);
        let (direct, rx) = mpsc::unbounded_channel();
        self.inner.lock().unwrap().viewers.insert(id, Viewer { name: format!("Player {id}"), direct });
        self.changed();
        (id, rx)
    }

    pub async fn remove_viewer(&self, id: u64) {
        let freed: Vec<u8> = {
            let mut inner = self.inner.lock().unwrap();
            inner.viewers.remove(&id);
            let mut freed = Vec::new();
            for slot in 0..SLOTS {
                if matches!(inner.slots[slot], Some((owner, _)) if owner == id) {
                    inner.slots[slot] = None;
                    inner.pads[slot] = None;
                    freed.push(slot as u8);
                }
            }
            freed
        };
        if let Some(session) = self.current_session() {
            for slot in freed {
                let _ = session.remove_gamepad(slot).await;
            }
        }
        self.changed();
    }

    pub fn set_name(&self, id: u64, name: &str) {
        let name: String = name.trim().chars().filter(|c| !c.is_control()).take(24).collect();
        if name.is_empty() {
            return;
        }
        if let Some(viewer) = self.inner.lock().unwrap().viewers.get_mut(&id) {
            viewer.name = name;
        }
        self.changed();
    }

    fn send_direct(&self, id: u64, message: Direct) {
        if let Some(viewer) = self.inner.lock().unwrap().viewers.get(&id) {
            let _ = viewer.direct.send(message);
        }
    }

    pub fn take_port(&self) -> Option<u16> {
        self.inner.lock().unwrap().free_ports.pop()
    }

    pub fn return_port(&self, port: u16) {
        self.inner.lock().unwrap().free_ports.push(port);
    }

    // ── Input ──────────────────────────────────────────────────────────────

    /// One of a player's controllers changed. The first time a controller
    /// sends anything it is given a free console slot.
    pub async fn set_pad(&self, id: u64, local: u8, mut state: GamepadState) {
        let (slot, is_new) = {
            let mut inner = self.inner.lock().unwrap();
            let existing = inner.slots.iter().position(|s| *s == Some((id, local)));
            match existing {
                Some(slot) => (slot, false),
                None => match inner.slots.iter().position(Option::is_none) {
                    Some(slot) => {
                        inner.slots[slot] = Some((id, local));
                        (slot, true)
                    }
                    None => {
                        drop(inner);
                        self.send_direct(id, Direct::Error { message: "All 4 controller slots are taken. You can still watch.".to_owned() });
                        return;
                    }
                },
            }
        };
        state.index = slot as u8;
        self.inner.lock().unwrap().pads[slot] = Some(state);
        if let Some(session) = self.current_session() {
            let _ = session.set_gamepad(state).await;
        }
        if is_new {
            self.changed();
        }
    }

    pub async fn unplug_pad(&self, id: u64, local: u8) {
        let slot = {
            let mut inner = self.inner.lock().unwrap();
            let Some(slot) = inner.slots.iter().position(|s| *s == Some((id, local))) else { return };
            inner.slots[slot] = None;
            inner.pads[slot] = None;
            slot
        };
        if let Some(session) = self.current_session() {
            let _ = session.remove_gamepad(slot as u8).await;
        }
        self.changed();
    }

    pub fn send_key(&self, frame: KeyboardFrame) {
        if let Some(session) = self.current_session() {
            session.send_key(frame);
        }
    }

    pub fn send_mouse(&self, frame: MouseFrame) {
        if let Some(session) = self.current_session() {
            session.send_mouse(frame);
        }
    }

    /// Asks the console for a full frame, at most once a second.
    pub fn request_keyframe(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.last_keyframe.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
                return;
            }
            inner.last_keyframe = Some(Instant::now());
        }
        if let Some(session) = self.current_session() {
            tokio::spawn(async move {
                let _ = session.request_keyframe().await;
            });
        }
    }

    fn current_session(&self) -> Option<Arc<Session>> {
        self.session.lock().unwrap().clone()
    }

    // ── Microsoft sign-in ──────────────────────────────────────────────────

    pub async fn login(self: &Arc<Self>) {
        if self.inner.lock().unwrap().login.is_some() {
            return;
        }
        let code = {
            let auth = self.auth.lock().await;
            auth.start_device_code().await
        };
        let code = match code {
            Ok(code) => code,
            Err(error) => {
                self.inner.lock().unwrap().login_error = Some(format!("{error:#}"));
                self.changed();
                return;
            }
        };
        {
            let mut inner = self.inner.lock().unwrap();
            inner.login = Some((code.verification_uri.clone(), code.user_code.clone()));
            inner.login_error = None;
        }
        self.changed();
        let app = self.clone();
        tokio::spawn(async move {
            let result = app.auth.lock().await.finish_device_code(&code).await;
            {
                let mut inner = app.inner.lock().unwrap();
                inner.login = None;
                match &result {
                    Ok(()) => inner.signed_in = true,
                    Err(error) => inner.login_error = Some(format!("{error:#}")),
                }
            }
            app.changed();
            if result.is_ok() {
                app.refresh_consoles().await;
            }
        });
    }

    pub async fn logout(&self) {
        self.stop().await;
        let result = self.auth.lock().await.sign_out().await;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.signed_in = false;
            inner.consoles.clear();
            if let Err(error) = result {
                inner.login_error = Some(format!("{error:#}"));
            }
        }
        self.changed();
    }

    // ── Consoles ───────────────────────────────────────────────────────────

    pub async fn refresh_consoles(&self) {
        let result = async {
            let token = self.auth.lock().await.web_token().await?;
            consoles::list(&self.http, &token).await
        }
        .await;
        {
            let mut inner = self.inner.lock().unwrap();
            match result {
                Ok(list) => {
                    inner.consoles = list;
                    inner.console_error = None;
                }
                Err(error) => inner.console_error = Some(format!("{error:#}")),
            }
        }
        self.changed();
    }

    pub async fn wake(&self, console_id: &str) -> Result<()> {
        let token = self.auth.lock().await.web_token().await?;
        consoles::wake(&self.http, &token, console_id).await
    }

    // ── The stream ─────────────────────────────────────────────────────────

    /// Starts streaming from a console in the background. Does nothing if a
    /// stream is already starting or running.
    pub fn start(self: &Arc<Self>, console_id: &str) -> Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            if !inner.signed_in {
                bail!("sign in first");
            }
            if matches!(inner.stream, StreamState::Starting { .. } | StreamState::Live { .. }) {
                return Ok(());
            }
            let console = inner
                .consoles
                .iter()
                .find(|c| c.id == console_id)
                .ok_or_else(|| anyhow!("that console is not on this account"))?;
            inner.stream = StreamState::Starting {
                console_id: console_id.to_owned(),
                console_name: console.name.clone(),
                step: "Waking console".to_owned(),
            };
        }
        self.changed();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let app = self.clone();
        let console_id = console_id.to_owned();
        tokio::spawn(async move {
            match app.run_start(&console_id, generation).await {
                Ok(()) => {}
                Err(error) => {
                    if app.generation.load(Ordering::SeqCst) == generation {
                        app.inner.lock().unwrap().stream = StreamState::Failed { message: format!("{error:#}") };
                        app.changed();
                    }
                }
            }
        });
        Ok(())
    }

    fn set_step(&self, step: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let StreamState::Starting { step: current, .. } = &mut inner.stream {
            *current = step.to_owned();
        }
        drop(inner);
        self.changed();
    }

    async fn run_start(self: &Arc<Self>, console_id: &str, generation: u64) -> Result<()> {
        // Wake it first. A console that is already on ignores this.
        if let Err(error) = self.wake(console_id).await {
            println!("wake request failed (continuing anyway): {error:#}");
        }
        self.set_step("Starting stream");
        let signaling = {
            let mut auth = self.auth.lock().await;
            let token = auth.home_streaming_token().await?;
            let api = HomeApi::new(&token)?;
            let app = self.clone();
            let status = move |line: &str| {
                println!("[stream] {line}");
                if line.contains("authorization") {
                    app.set_step("Authorizing");
                } else if line.contains("ready") {
                    app.set_step("Connecting");
                }
            };
            start_home_session(&api, &mut auth, console_id, &status).await?
        };
        self.set_step("Connecting");
        let signaling = Arc::new(signaling);
        let options = StreamOptions { media: Some(self.media.clone()), ..Default::default() };
        let mut session = match Session::connect(signaling.clone(), options).await {
            Ok(session) => session,
            Err(error) => {
                use zeroplay_xbox::session::Signaling;
                let _ = signaling.stop().await;
                return Err(error);
            }
        };
        let events = session.take_events();
        let session = Arc::new(session);

        if self.generation.load(Ordering::SeqCst) != generation {
            let _ = session.shutdown().await;
            return Ok(());
        }

        // Plug in the controllers players were already holding.
        let pads: Vec<GamepadState> = self.inner.lock().unwrap().pads.iter().flatten().copied().collect();
        for pad in pads {
            let _ = session.set_gamepad(pad).await;
        }
        *self.session.lock().unwrap() = Some(session.clone());
        {
            let mut inner = self.inner.lock().unwrap();
            let console_name = match &inner.stream {
                StreamState::Starting { console_name, .. } => console_name.clone(),
                _ => String::new(),
            };
            inner.stream = StreamState::Live { console_id: console_id.to_owned(), console_name, title: None };
            inner.last_keyframe = None;
        }
        self.changed();
        println!("[stream] live");

        if let Some(mut events) = events {
            let app = self.clone();
            tokio::spawn(async move {
                let mut reason = None;
                while let Some(event) = events.recv().await {
                    match event {
                        SessionEvent::Ready => app.request_keyframe(),
                        SessionEvent::Connection(state) => {
                            if state == "failed" || state == "closed" || state == "disconnected" {
                                reason = Some(format!("the connection to the console was {state}"));
                                break;
                            }
                        }
                        SessionEvent::TitleInfo(info) => {
                            if let StreamState::Live { title, .. } = &mut app.inner.lock().unwrap().stream {
                                *title = Some(info);
                            }
                            app.changed();
                        }
                        SessionEvent::Vibration(v) => app.route_rumble(v),
                        SessionEvent::Disconnected(why) => {
                            reason = Some(if why.is_empty() { "the console ended the stream".to_owned() } else { why });
                            break;
                        }
                        SessionEvent::VideoSize { .. } | SessionEvent::ServerMessage { .. } => {}
                    }
                }
                if app.generation.load(Ordering::SeqCst) == generation {
                    app.end_stream(reason).await;
                }
            });
        }
        Ok(())
    }

    fn route_rumble(&self, v: zeroplay_xbox::input::Vibration) {
        let owner = self.inner.lock().unwrap().slots.get(v.gamepad_index as usize).copied().flatten();
        if let Some((id, local)) = owner {
            self.send_direct(
                id,
                Direct::Rumble {
                    local,
                    left: v.left_motor,
                    right: v.right_motor,
                    left_trigger: v.left_trigger_motor,
                    right_trigger: v.right_trigger_motor,
                    ms: v.duration_ms,
                },
            );
        }
    }

    async fn end_stream(&self, reason: Option<String>) {
        let session = self.session.lock().unwrap().take();
        if let Some(session) = session {
            let _ = session.shutdown().await;
        }
        self.inner.lock().unwrap().stream = match reason {
            Some(message) => StreamState::Failed { message },
            None => StreamState::Idle,
        };
        println!("[stream] ended");
        self.changed();
    }

    /// Stops the stream (or a start in progress) and frees the console.
    pub async fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.end_stream(None).await;
    }

    // ── What each page shows ───────────────────────────────────────────────

    pub fn status_for(&self, id: u64) -> Value {
        let inner = self.inner.lock().unwrap();
        let stream = match &inner.stream {
            StreamState::Idle => json!({ "state": "idle" }),
            StreamState::Starting { console_id, console_name, step } => {
                json!({ "state": "starting", "consoleId": console_id, "consoleName": console_name, "step": step })
            }
            StreamState::Live { console_id, console_name, title } => {
                json!({ "state": "live", "consoleId": console_id, "consoleName": console_name, "title": title })
            }
            StreamState::Failed { message } => json!({ "state": "failed", "error": message }),
        };
        let consoles: Vec<Value> = inner
            .consoles
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "name": c.name,
                    "type": c.console_type,
                    "power": c.power_state,
                    "remotePlay": c.console_streaming_enabled,
                })
            })
            .collect();
        let players: Vec<Value> = inner
            .slots
            .iter()
            .enumerate()
            .filter_map(|(slot, owner)| {
                let (owner, local) = (*owner)?;
                let name = inner.viewers.get(&owner).map(|v| v.name.clone()).unwrap_or_default();
                Some(json!({ "slot": slot + 1, "name": name, "you": owner == id, "local": local }))
            })
            .collect();
        let watching: Vec<String> = inner
            .viewers
            .iter()
            .filter(|(vid, _)| !inner.slots.iter().flatten().any(|(owner, _)| owner == *vid))
            .map(|(_, v)| v.name.clone())
            .collect();
        json!({
            "t": "status",
            "signedIn": inner.signed_in,
            "login": inner.login.as_ref().map(|(uri, code)| json!({ "uri": uri, "code": code })),
            "loginError": inner.login_error,
            "consoles": consoles,
            "consoleError": inner.console_error,
            "stream": stream,
            "players": players,
            "watching": watching,
            "you": { "id": id, "name": inner.viewers.get(&id).map(|v| v.name.clone()) },
        })
    }
}
