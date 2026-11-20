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
use zeroplay_xbox::session::{
    start_home_session, MediaPacket, Session, SessionEvent, StreamOptions,
};

use crate::access::{Access, Profile};
use crate::config::Config;
use crate::control::{Effect, Rules, Seats};
use crate::playit::Playit;

/// A message sent to one player's connection only.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Direct {
    Answer {
        sdp: String,
    },
    Rumble {
        local: u8,
        left: f32,
        right: f32,
        left_trigger: f32,
        right_trigger: f32,
        ms: u16,
    },
    Error {
        message: String,
    },
    /// This player's profile was deleted or changed; the page checks its login again.
    LoggedOut,
    /// Something to show this player; `countdown` shows a big countdown.
    Notice {
        text: String,
        countdown: Option<u32>,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum StreamState {
    Idle,
    Starting {
        console_id: String,
        console_name: String,
        step: String,
    },
    Live {
        console_id: String,
        console_name: String,
        title: Option<String>,
    },
    /// The connection to the console dropped; trying to get it back.
    Reconnecting {
        console_id: String,
        console_name: String,
        attempt: u32,
    },
    Failed {
        message: String,
    },
}

struct Viewer {
    name: String,
    /// On the home network (through the home port).
    home: bool,
    /// The logged-in profile (none only during first setup).
    profile: Option<Profile>,
    direct: mpsc::UnboundedSender<Direct>,
}

struct Inner {
    signed_in: bool,
    login: Option<(String, String)>,
    login_error: Option<String>,
    consoles: Vec<Console>,
    console_error: Option<String>,
    stream: StreamState,
    viewers: HashMap<u64, Viewer>,
    seats: Seats,
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
    pub playit: Arc<Playit>,
    pub access: Access,
}

impl App {
    pub async fn new(config: Config) -> Result<Arc<App>> {
        tokio::fs::create_dir_all(&config.data_dir).await?;
        let auth = Auth::load(config.data_dir.join("tokens.json")).await?;
        let signed_in = auth.is_signed_in();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let (media, _) = broadcast::channel(2048);
        let (updates, _) = broadcast::channel(64);
        let free_ports = config.rtc_ports.iter().rev().copied().collect();
        let playit = Playit::new(
            &config.data_dir,
            config.remote_addr.port(),
            config.rtc_ports.clone(),
            http.clone(),
            updates.clone(),
        );
        let access = Access::load(&config.data_dir).await?;
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
                seats: Seats::default(),
                free_ports,
                last_keyframe: None,
            }),
            session: Mutex::new(None),
            generation: AtomicU64::new(0),
            next_viewer: AtomicU64::new(1),
            media,
            updates,
            playit,
            access,
        });
        app.playit.start_if_linked().await;
        let ticker = app.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(1));
            loop {
                every.tick().await;
                ticker.tick().await;
            }
        });
        if signed_in {
            let app2 = app.clone();
            tokio::spawn(async move { app2.refresh_consoles().await });
        }
        Ok(app)
    }

    pub fn changed(&self) {
        let _ = self.updates.send(());
    }

    pub fn subscribe_updates(&self) -> broadcast::Receiver<()> {
        self.updates.subscribe()
    }

    // ── Players ────────────────────────────────────────────────────────────

    pub fn add_viewer(
        &self,
        home: bool,
        profile: Option<Profile>,
    ) -> (u64, mpsc::UnboundedReceiver<Direct>) {
        let id = self.next_viewer.fetch_add(1, Ordering::Relaxed);
        let (direct, rx) = mpsc::unbounded_channel();
        let name = profile
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Setup".to_owned());
        self.inner.lock().unwrap().viewers.insert(
            id,
            Viewer {
                name,
                home,
                profile,
                direct,
            },
        );
        self.changed();
        (id, rx)
    }

    /// A player's connection is gone. With a stream running their controllers
    /// are unplugged (games pause) and their slot is held for a minute.
    pub async fn remove_viewer(&self, id: u64) {
        let effects = {
            let mut inner = self.inner.lock().unwrap();
            let profile = inner
                .viewers
                .remove(&id)
                .and_then(|v| v.profile)
                .map(|p| p.id);
            let live = matches!(
                inner.stream,
                StreamState::Live { .. } | StreamState::Reconnecting { .. }
            );
            inner
                .seats
                .disconnect(id, profile.as_deref(), live, Instant::now(), &self.rules())
        };
        self.apply(effects).await;
        self.changed();
    }

    /// The player started watching the stream.
    pub fn start_watching(&self, id: u64) {
        self.inner
            .lock()
            .unwrap()
            .seats
            .start_watching(id, Instant::now());
        self.changed();
    }

    /// The player went back to home: their controller goes to the next in line.
    pub async fn stop_watching(&self, id: u64) {
        let effects = self
            .inner
            .lock()
            .unwrap()
            .seats
            .stop_watching(id, Instant::now());
        self.apply(effects).await;
        self.changed();
    }

    fn rules(&self) -> Rules {
        let settings = self.access.settings();
        Rules {
            turn: settings
                .timed_play
                .then(|| Duration::from_secs(settings.turn_minutes.max(1) as u64 * 60)),
            ..Rules::default()
        }
    }

    /// Carries out what the seat rules decided.
    async fn apply(&self, effects: Vec<Effect>) {
        if effects.is_empty() {
            return;
        }
        let session = self.current_session();
        for effect in effects {
            match effect {
                Effect::Plug(state) => {
                    if let Some(session) = &session {
                        let _ = session.set_gamepad(state).await;
                    }
                }
                Effect::Unplug(slot) => {
                    if let Some(session) = &session {
                        let _ = session.remove_gamepad(slot).await;
                    }
                }
                Effect::Notice {
                    viewer,
                    text,
                    countdown,
                } => self.send_direct(viewer, Direct::Notice { text, countdown }),
                Effect::Stop(why) => {
                    println!("[stream] stopping: {why}");
                    self.stop().await;
                }
            }
        }
        self.changed();
    }

    /// Every second: AFK, held slots, timed play, nobody left.
    async fn tick(&self) {
        let rules = self.rules();
        let effects = {
            let mut inner = self.inner.lock().unwrap();
            let live = matches!(
                inner.stream,
                StreamState::Live { .. } | StreamState::Reconnecting { .. }
            );
            let connected = inner.viewers.len();
            inner.seats.tick(Instant::now(), &rules, live, connected)
        };
        self.apply(effects).await;
    }

    pub fn set_turns(&self, id: u64, on: bool) {
        let mut inner = self.inner.lock().unwrap();
        if on {
            inner.seats.opted_in.insert(id);
        } else {
            inner.seats.opted_in.remove(&id);
        }
        drop(inner);
        self.changed();
    }

    pub fn request_time(&self, id: u64) -> Option<Direct> {
        let asked = self.inner.lock().unwrap().seats.request_time(id, 15);
        self.changed();
        match asked {
            Some(_) => Some(Direct::Notice {
                text:
                    "Asked the admin for 15 more minutes. Your clock is stopped until they answer."
                        .to_owned(),
                countdown: None,
            }),
            None => Some(Direct::Error {
                message: "More time can only be asked for during your turn, once at a time."
                    .to_owned(),
            }),
        }
    }

    pub async fn answer_time(&self, request: u64, approve: bool) {
        let effects = self
            .inner
            .lock()
            .unwrap()
            .seats
            .answer_time(request, approve);
        self.apply(effects).await;
        self.changed();
    }

    /// The profile a player is logged in as, as it is stored now.
    pub fn viewer_profile(&self, id: u64) -> Option<Profile> {
        let profile_id = self
            .inner
            .lock()
            .unwrap()
            .viewers
            .get(&id)?
            .profile
            .as_ref()?
            .id
            .clone();
        self.access.profile(&profile_id)
    }

    /// After profiles change: players pick up their new name and permissions,
    /// and players whose profile is gone are logged out.
    pub fn refresh_profiles(&self) {
        let mut gone = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            for (id, viewer) in inner.viewers.iter_mut() {
                let Some(old) = &viewer.profile else { continue };
                match self.access.profile(&old.id) {
                    Some(fresh) => {
                        viewer.name = fresh.name.clone();
                        viewer.profile = Some(fresh);
                    }
                    None => gone.push(*id),
                }
            }
        }
        for id in gone {
            self.send_direct(id, Direct::LoggedOut);
        }
        self.changed();
    }

    /// After the first (admin) profile is made: the setup connections become it.
    pub fn adopt_first_profile(&self, profile: &Profile) {
        let mut inner = self.inner.lock().unwrap();
        for viewer in inner.viewers.values_mut() {
            if viewer.profile.is_none() && viewer.home {
                viewer.profile = Some(profile.clone());
                viewer.name = profile.name.clone();
            }
        }
        drop(inner);
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
    /// One of a player's controllers changed. The first time a controller
    /// sends anything it gets a console slot (one held for the player's
    /// profile, or a free one).
    pub async fn set_pad(&self, id: u64, local: u8, mut state: GamepadState) {
        let (state, is_new) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(viewer) = inner.viewers.get(&id) else {
                return;
            };
            // Profiles without Xbox Home may not press the Xbox button.
            if viewer.profile.as_ref().is_some_and(|p| !p.xbox_home) {
                state.buttons &= !zeroplay_xbox::input::button::NEXUS;
            }
            let profile = viewer.profile.as_ref().map(|p| p.id.clone());
            let now = Instant::now();
            match inner.seats.claim(id, local, profile.as_deref(), now) {
                Some((slot, is_new)) => (inner.seats.input(id, slot, state, now), is_new),
                None => {
                    drop(inner);
                    self.send_direct(
                        id,
                        Direct::Error {
                            message: "All 4 controller slots are taken. You can still watch."
                                .to_owned(),
                        },
                    );
                    return;
                }
            }
        };
        if let Some(session) = self.current_session() {
            let _ = session.set_gamepad(state).await;
        }
        if is_new {
            self.changed();
        }
    }

    pub async fn unplug_pad(&self, id: u64, local: u8) {
        let Some(slot) = self.inner.lock().unwrap().seats.unplug(id, local) else {
            return;
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
            if inner
                .last_keyframe
                .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
            {
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
            if matches!(
                inner.stream,
                StreamState::Starting { .. } | StreamState::Live { .. }
            ) {
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
                        app.inner.lock().unwrap().stream = StreamState::Failed {
                            message: format!("{error:#}"),
                        };
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
        let options = StreamOptions {
            media: Some(self.media.clone()),
            ..Default::default()
        };
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
        let pads: Vec<GamepadState> = self
            .inner
            .lock()
            .unwrap()
            .seats
            .pad
            .iter()
            .flatten()
            .copied()
            .collect();
        for pad in pads {
            let _ = session.set_gamepad(pad).await;
        }
        *self.session.lock().unwrap() = Some(session.clone());
        {
            let mut inner = self.inner.lock().unwrap();
            let console_name = match &inner.stream {
                StreamState::Starting { console_name, .. }
                | StreamState::Reconnecting { console_name, .. } => console_name.clone(),
                _ => String::new(),
            };
            inner.stream = StreamState::Live {
                console_id: console_id.to_owned(),
                console_name,
                title: None,
            };
            inner.last_keyframe = None;
        }
        self.changed();
        println!("[stream] live");

        if let Some(mut events) = events {
            let app = self.clone();
            let console = console_id.to_owned();
            tokio::spawn(async move {
                let mut reason = None;
                while let Some(event) = events.recv().await {
                    match event {
                        SessionEvent::Ready => app.request_keyframe(),
                        SessionEvent::Connection(state) => {
                            if state == "failed" || state == "closed" || state == "disconnected" {
                                // Lost, not ended: pause the game and get the stream back.
                                if app.generation.load(Ordering::SeqCst) == generation {
                                    println!("[stream] connection {state}; reconnecting");
                                    app.clone().recover(console.clone(), generation).await;
                                }
                                return;
                            }
                        }
                        SessionEvent::TitleInfo(info) => {
                            if let StreamState::Live { title, .. } =
                                &mut app.inner.lock().unwrap().stream
                            {
                                *title = Some(info);
                            }
                            app.changed();
                        }
                        SessionEvent::Vibration(v) => app.route_rumble(v),
                        SessionEvent::Disconnected(why) => {
                            reason = Some(if why.is_empty() {
                                "the console ended the stream".to_owned()
                            } else {
                                why
                            });
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

    /// The stream to the console dropped. Pause the game through Xbox Live
    /// (the Xbox button opens the guide, which pauses most games), then try to
    /// get the stream back. Players keep their slots meanwhile.
    fn recover(
        self: Arc<Self>,
        console_id: String,
        generation: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move { self.recover_now(console_id, generation).await })
    }

    async fn recover_now(self: Arc<Self>, console_id: String, generation: u64) {
        let session = self.session.lock().unwrap().take();
        if let Some(session) = session {
            let _ = session.shutdown().await;
        }
        let console_name = match &self.inner.lock().unwrap().stream {
            StreamState::Live { console_name, .. }
            | StreamState::Reconnecting { console_name, .. } => console_name.clone(),
            _ => String::new(),
        };
        let pause = async {
            let token = self.auth.lock().await.web_token().await?;
            consoles::press_button(&self.http, &token, &console_id, "Guide").await
        };
        match pause.await {
            Ok(()) => println!("[stream] pressed the Xbox button to pause the game"),
            Err(error) => println!("[stream] could not pause the game: {error:#}"),
        }
        for attempt in 1..=3u32 {
            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            self.inner.lock().unwrap().stream = StreamState::Reconnecting {
                console_id: console_id.clone(),
                console_name: console_name.clone(),
                attempt,
            };
            self.changed();
            tokio::time::sleep(Duration::from_secs(if attempt == 1 { 2 } else { 10 })).await;
            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            match self.run_start(&console_id, generation).await {
                Ok(()) => {
                    println!("[stream] reconnected");
                    return;
                }
                Err(error) => println!("[stream] reconnect attempt {attempt} failed: {error:#}"),
            }
        }
        if self.generation.load(Ordering::SeqCst) == generation {
            self.inner.lock().unwrap().stream = StreamState::Failed {
                message: "Lost the connection to the Xbox and could not get it back.".to_owned(),
            };
            self.changed();
        }
    }

    fn route_rumble(&self, v: zeroplay_xbox::input::Vibration) {
        let owner = self
            .inner
            .lock()
            .unwrap()
            .seats
            .owner
            .get(v.gamepad_index as usize)
            .copied()
            .flatten();
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
            StreamState::Starting {
                console_id,
                console_name,
                step,
            } => {
                json!({ "state": "starting", "consoleId": console_id, "consoleName": console_name, "step": step })
            }
            StreamState::Live {
                console_id,
                console_name,
                title,
            } => {
                json!({ "state": "live", "consoleId": console_id, "consoleName": console_name, "title": title })
            }
            StreamState::Reconnecting {
                console_id,
                console_name,
                attempt,
            } => {
                json!({ "state": "reconnecting", "consoleId": console_id, "consoleName": console_name, "attempt": attempt })
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
            .seats
            .owner
            .iter()
            .enumerate()
            .filter_map(|(slot, owner)| {
                let (owner, local) = (*owner)?;
                let name = inner
                    .viewers
                    .get(&owner)
                    .map(|v| v.name.clone())
                    .unwrap_or_default();
                Some(json!({ "slot": slot + 1, "name": name, "you": owner == id, "local": local }))
            })
            .collect();
        let watching: Vec<String> = inner
            .viewers
            .iter()
            .filter(|(vid, _)| inner.seats.slots_of(**vid).is_empty())
            .map(|(_, v)| v.name.clone())
            .collect();
        let admin = inner.viewers.get(&id).and_then(|v| v.profile.as_ref()).filter(|p| self.access.profile(&p.id).is_some_and(|p| p.admin)).map(|_| {
            let mut view = self.access.admin_view();
            view["timeRequests"] = inner
                .seats
                .time_requests
                .iter()
                .map(|r| json!({ "id": r.id, "name": inner.viewers.get(&r.viewer).map(|v| v.name.clone()).unwrap_or_default(), "minutes": r.minutes }))
                .collect::<Vec<_>>()
                .into();
            view
        });
        let settings = self.access.settings();
        let mine = inner.seats.slots_of(id).first().copied();
        let queue = inner.seats.queue();
        let turns = json!({
            "enabled": settings.timed_play,
            "minutes": settings.turn_minutes,
            "optedIn": inner.seats.opted_in.contains(&id),
            "inControl": mine.is_some(),
            "secondsLeft": mine.and_then(|slot| inner.seats.turn_left(slot, Instant::now())),
            "waiting": queue.iter().position(|v| *v == id).map(|i| i + 1),
            "queue": queue.len(),
            "askedForTime": inner.seats.time_requests.iter().any(|r| r.viewer == id),
        });
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
            "you": {
                "id": id,
                "name": inner.viewers.get(&id).map(|v| v.name.clone()),
                "home": inner.viewers.get(&id).is_some_and(|v| v.home),
                "profile": inner.viewers.get(&id).and_then(|v| v.profile.as_ref()).map(Profile::public),
            },
            "setupDone": self.access.has_profiles(),
            "admin": admin,
            "turns": turns,
            "playit": self.playit.status(),
        })
    }
}
