//! Everything the server keeps track of: the Microsoft sign-in, the consoles,
//! the one stream from the console, and who holds which controller slot.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

use crate::access::{Access, Profile, Quality};
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
    /// Players with the stream on screen.
    watchers: HashSet<u64>,
    /// The quality the running stream was started with.
    stream_quality: Option<Quality>,
    /// Installed games per console, and when they were read.
    games: HashMap<String, (Instant, Vec<Game>)>,
    /// Why a console's game list is empty (an error, or nothing found).
    games_note: HashMap<String, String>,
}

/// A game installed on a console.
#[derive(Clone, Debug)]
pub struct Game {
    /// The Store id, used to launch it.
    pub id: String,
    pub name: String,
    pub image: Option<String>,
}

/// Whether a profile may launch this game (no profile = first setup at home).
fn may_play(profile: Option<&Profile>, game: &str) -> bool {
    match profile {
        None => true,
        Some(p) if p.admin => true,
        Some(p) => p
            .games
            .as_ref()
            .is_none_or(|list| list.iter().any(|g| g == game)),
    }
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
    /// The console's own packets; passed on to `media` unless a capture card
    /// supplies that part.
    console_media: broadcast::Sender<MediaPacket>,
    capture: Mutex<Option<crate::capture::Capture>>,
    capture_video: AtomicBool,
    capture_audio: AtomicBool,
    /// What the capture card is doing (or why it is not), for the admin.
    capture_note: Mutex<Option<String>>,
    /// Card modes per device that did not deliver their frame rate.
    capture_skip: Mutex<HashMap<String, Vec<String>>>,
    /// Bumped by every capture start, so an old check leaves a new one alone.
    capture_runs: AtomicU64,
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
        let (console_media, _) = broadcast::channel(2048);
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
                watchers: HashSet::new(),
                stream_quality: None,
                games: HashMap::new(),
                games_note: HashMap::new(),
            }),
            session: Mutex::new(None),
            generation: AtomicU64::new(0),
            next_viewer: AtomicU64::new(1),
            media,
            console_media,
            capture: Mutex::new(None),
            capture_video: AtomicBool::new(false),
            capture_audio: AtomicBool::new(false),
            capture_note: Mutex::new(None),
            capture_skip: Mutex::new(HashMap::new()),
            capture_runs: AtomicU64::new(0),
            updates,
            playit,
            access,
        });
        app.playit.start_if_linked().await;
        // The console's packets reach players unless the capture card covers them.
        let relay = app.clone();
        tokio::spawn(async move {
            let mut from_console = relay.console_media.subscribe();
            loop {
                match from_console.recv().await {
                    Ok(MediaPacket::Video(p)) => {
                        if !relay.capture_video.load(Ordering::Relaxed) {
                            let _ = relay.media.send(MediaPacket::Video(p));
                        }
                    }
                    Ok(MediaPacket::Audio(p)) => {
                        if !relay.capture_audio.load(Ordering::Relaxed) {
                            let _ = relay.media.send(MediaPacket::Audio(p));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        let ticker = app.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(1));
            loop {
                every.tick().await;
                ticker.tick().await;
                // Keep the admin's capture frame rate current.
                if ticker.capture.lock().unwrap().is_some() {
                    ticker.changed();
                }
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
    pub async fn remove_viewer(self: &Arc<Self>, id: u64) {
        let effects = {
            let mut inner = self.inner.lock().unwrap();
            inner.watchers.remove(&id);
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
        self.match_quality();
    }

    /// The player started watching the stream.
    pub fn start_watching(self: &Arc<Self>, id: u64) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.seats.start_watching(id, Instant::now());
            inner.watchers.insert(id);
        }
        self.changed();
        self.match_quality();
    }

    // ── Shared quality ─────────────────────────────────────────────────────

    /// The quality the stream should run at: the shared setting, lowered to
    /// the lowest limit among the people watching (or, before anyone watches,
    /// among everyone connected).
    fn wanted_quality(&self) -> Quality {
        let setting = self.access.settings().quality;
        let profiles: Vec<String> = {
            let inner = self.inner.lock().unwrap();
            let ids: Vec<u64> = if inner.watchers.is_empty() {
                inner.viewers.keys().copied().collect()
            } else {
                inner.watchers.iter().copied().collect()
            };
            ids.iter()
                .filter_map(|id| {
                    inner
                        .viewers
                        .get(id)?
                        .profile
                        .as_ref()
                        .map(|p| p.id.clone())
                })
                .collect()
        };
        profiles
            .iter()
            .filter_map(|profile| self.access.profile(profile))
            .fold(setting, |q, p| q.min(p.max_quality))
    }

    /// A player picks the shared quality, up to their own limit.
    pub async fn set_quality(self: &Arc<Self>, id: u64, quality: Quality) -> Option<Direct> {
        let limit = self
            .viewer_profile(id)
            .map(|p| p.max_quality)
            .unwrap_or(Quality::Max);
        if !limit.allows(quality) {
            return Some(Direct::Error {
                message: "Your profile cannot pick a quality that high.".to_owned(),
            });
        }
        if let Err(error) = self.access.set_quality(quality).await {
            return Some(Direct::Error {
                message: format!("{error:#}"),
            });
        }
        self.changed();
        self.match_quality();
        None
    }

    /// Restarts a running stream when it should run at another quality (the
    /// console only takes the picture size and bitrate when a stream starts).
    pub fn match_quality(self: &Arc<Self>) {
        let wanted = self.wanted_quality();
        let console_id = {
            let inner = self.inner.lock().unwrap();
            match (&inner.stream, inner.stream_quality) {
                (StreamState::Live { console_id, .. }, Some(current)) if current != wanted => {
                    console_id.clone()
                }
                _ => return,
            }
        };
        println!("[stream] switching quality to {wanted:?}");
        // With a capture card only the encoder starts again; the Xbox stays connected.
        if self.capture.lock().unwrap().is_some() {
            self.inner.lock().unwrap().stream_quality = Some(wanted);
            let app = self.clone();
            tokio::spawn(async move { app.start_capture(wanted).await });
            self.changed();
            return;
        }
        self.restart_stream(console_id, "Changing quality");
    }

    /// Starts the Xbox stream again under the players (they keep their place).
    pub fn restart_stream(self: &Arc<Self>, console_id: String, step: &'static str) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let app = self.clone();
        tokio::spawn(async move {
            let session = app.session.lock().unwrap().take();
            if let Some(session) = session {
                let _ = session.shutdown().await;
            }
            {
                let mut inner = app.inner.lock().unwrap();
                let console_name = match &inner.stream {
                    StreamState::Live { console_name, .. } => console_name.clone(),
                    _ => String::new(),
                };
                inner.stream = StreamState::Starting {
                    console_id: console_id.clone(),
                    console_name,
                    step: step.to_owned(),
                };
            }
            app.changed();
            if let Err(error) = app.run_start(&console_id, generation).await {
                if app.generation.load(Ordering::SeqCst) == generation {
                    app.inner.lock().unwrap().stream = StreamState::Failed {
                        message: format!("{error:#}"),
                    };
                    app.changed();
                }
            }
        });
    }

    /// The player went back to home: their controller goes to the next in line.
    pub async fn stop_watching(self: &Arc<Self>, id: u64) {
        let effects = {
            let mut inner = self.inner.lock().unwrap();
            inner.watchers.remove(&id);
            inner.seats.stop_watching(id, Instant::now())
        };
        self.apply(effects).await;
        self.changed();
        self.match_quality();
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
        let awake: Vec<String> = self
            .inner
            .lock()
            .unwrap()
            .consoles
            .iter()
            .map(|c| c.id.clone())
            .collect();
        for console in awake {
            self.refresh_games(&console, false).await;
        }
    }

    // ── Capture card ───────────────────────────────────────────────────────

    /// Starts (or restarts) the capture card at `quality`. If it does not
    /// work, the Xbox's own remote play picture is used instead.
    fn start_capture(
        self: &Arc<Self>,
        quality: Quality,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.start_capture_now(quality))
    }

    async fn start_capture_now(self: &Arc<Self>, quality: Quality) {
        self.stop_capture();
        let run = self.capture_runs.fetch_add(1, Ordering::SeqCst) + 1;
        let settings = self.access.settings().capture;
        let Some(device) = settings.device.clone() else {
            return;
        };
        let card_name = crate::capture::video_devices()
            .into_iter()
            .find(|d| d.path == device)
            .map(|d| d.name);
        let audio = match settings.audio.as_deref() {
            None => None,
            Some("auto") => crate::capture::sound_of_card(&device)
                .or_else(|| {
                    card_name
                        .as_deref()
                        .and_then(crate::capture::matching_audio)
                })
                .map(|a| a.id),
            Some(id) => Some(id.to_owned()),
        };
        let skip = self
            .capture_skip
            .lock()
            .unwrap()
            .get(&device)
            .cloned()
            .unwrap_or_default();
        let note = match crate::capture::start(
            &device,
            audio.as_deref(),
            quality,
            self.media.clone(),
            &skip,
        )
        .await
        {
            Ok(capture) => {
                let mut note = format!(
                    "Picture from {} ({}), encoded on the {}.",
                    card_name.as_deref().unwrap_or("the capture card"),
                    device,
                    capture.encoder,
                );
                if let Some(why) = &capture.gpu_error {
                    note.push_str(&format!(
                        " The graphics chip did not work ({why}), so the processor encodes."
                    ));
                }
                note.push_str(if capture.sound {
                    " Sound from the capture card."
                } else {
                    " Sound from the Xbox."
                });
                if let Some(why) = &capture.sound_error {
                    note.push_str(&format!(" The card's sound did not work ({why})."));
                }
                if !skip.is_empty() {
                    note.push_str(&format!(
                        " Left out card modes that did not deliver their frame rate: {}.",
                        skip.join(", ")
                    ));
                }
                // Check the card really delivers the frame rate it promised;
                // cheap cards send many empty frames in some modes.
                if let Some(key) = capture.mode_key.clone() {
                    let expected = capture.expected_fps;
                    let fps = capture.fps.clone();
                    let app = self.clone();
                    let device = device.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(6)).await;
                        if app.capture_runs.load(Ordering::SeqCst) != run {
                            return;
                        }
                        let got = fps.load(Ordering::Relaxed) as f64;
                        if expected > 0.0 && got < expected * 0.75 {
                            println!("[capture] {key} gives {got} of {expected} fps; trying another mode");
                            app.capture_skip
                                .lock()
                                .unwrap()
                                .entry(device)
                                .or_default()
                                .push(key);
                            app.start_capture(quality).await;
                        }
                    });
                }
                self.capture_video.store(true, Ordering::Relaxed);
                self.capture_audio.store(capture.sound, Ordering::Relaxed);
                *self.capture.lock().unwrap() = Some(capture);
                self.request_keyframe();
                note
            }
            Err(error) => {
                let message = format!("{error:#} The Xbox's remote play picture is used instead.");
                let watchers: Vec<u64> = self
                    .inner
                    .lock()
                    .unwrap()
                    .watchers
                    .iter()
                    .copied()
                    .collect();
                for viewer in watchers {
                    self.send_direct(
                        viewer,
                        Direct::Error {
                            message: message.clone(),
                        },
                    );
                }
                message
            }
        };
        *self.capture_note.lock().unwrap() = Some(note);
        self.changed();
    }

    fn stop_capture(&self) {
        self.capture_video.store(false, Ordering::Relaxed);
        self.capture_audio.store(false, Ordering::Relaxed);
        self.capture.lock().unwrap().take();
    }

    /// The admin picks the picture source. A running stream starts again with it.
    pub async fn set_capture(
        self: &Arc<Self>,
        settings: crate::capture::CaptureSettings,
    ) -> Result<()> {
        self.access.set_capture(settings).await?;
        *self.capture_note.lock().unwrap() = None;
        let live = match &self.inner.lock().unwrap().stream {
            StreamState::Live { console_id, .. } => Some(console_id.clone()),
            _ => None,
        };
        if let Some(console_id) = live {
            self.stop_capture();
            self.restart_stream(console_id, "Switching picture source");
        }
        self.changed();
        Ok(())
    }

    /// What the admin sees about capture cards.
    fn capture_status(&self) -> Value {
        let settings = self.access.settings().capture;
        json!({
            "devices": crate::capture::video_devices(),
            "audio": crate::capture::audio_devices(),
            "gpu": crate::capture::gpu().is_some(),
            "device": settings.device,
            "audioChoice": settings.audio,
            "active": self.capture.lock().unwrap().is_some(),
            "fps": self.capture.lock().unwrap().as_ref().map(|c| c.fps.load(Ordering::Relaxed)),
            "input": self.capture.lock().unwrap().as_ref().map(|c| c.input.clone()),
            "note": self.capture_note.lock().unwrap().clone(),
        })
    }

    // ── Game library ───────────────────────────────────────────────────────

    /// Reads the games installed on a console (at most once a minute unless
    /// `force`), with cover art where Xbox Live has it.
    pub async fn refresh_games(&self, console_id: &str, force: bool) {
        let fresh = self
            .inner
            .lock()
            .unwrap()
            .games
            .get(console_id)
            .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(60));
        if fresh && !force {
            return;
        }
        let result = async {
            let token = self.auth.lock().await.web_token().await?;
            let mut apps = consoles::installed_apps(&self.http, &token, console_id).await?;
            let reported = apps.len();
            println!("[games] {console_id} reported {reported} installed apps");
            apps.retain(|a| a.looks_like_a_game() && a.one_store_product_id.is_some());
            // Most recently played first.
            apps.sort_by(|a, b| b.last_active_time.cmp(&a.last_active_time));
            apps.dedup_by(|a, b| a.one_store_product_id == b.one_store_product_id);
            let ids: Vec<u64> = apps
                .iter()
                .map(|a| a.title_id)
                .filter(|id| *id != 0)
                .collect();
            let art = match consoles::title_art(&self.http, &token, &ids).await {
                Ok(art) => art,
                Err(error) => {
                    println!("[games] no cover art: {error:#}");
                    Default::default()
                }
            };
            let mut games: Vec<Game> = apps
                .into_iter()
                .map(|a| Game {
                    id: a.one_store_product_id.unwrap_or_default(),
                    name: a.name.unwrap_or_else(|| "Game".to_owned()),
                    image: art.get(&a.title_id).cloned(),
                })
                .collect();
            // The Store has art for games Xbox Live has none for.
            let missing: Vec<String> = games
                .iter()
                .filter(|g| g.image.is_none())
                .map(|g| g.id.clone())
                .collect();
            if !missing.is_empty() {
                match consoles::store_art(&self.http, &missing).await {
                    Ok(store) => {
                        for game in games.iter_mut().filter(|g| g.image.is_none()) {
                            game.image = store.get(&game.id).cloned();
                        }
                    }
                    Err(error) => println!("[games] no Store art: {error:#}"),
                }
            }
            anyhow::Ok((reported, games))
        }
        .await;
        {
            let mut inner = self.inner.lock().unwrap();
            match result {
                Ok((reported, games)) => {
                    if games.is_empty() {
                        inner.games_note.insert(
                            console_id.to_owned(),
                            format!(
                                "No games found (the console listed {reported} installed apps)."
                            ),
                        );
                    } else {
                        inner.games_note.remove(console_id);
                    }
                    inner
                        .games
                        .insert(console_id.to_owned(), (Instant::now(), games));
                }
                Err(error) => {
                    println!("[games] could not read the games on {console_id}: {error:#}");
                    inner.games_note.insert(
                        console_id.to_owned(),
                        format!("Could not read the games: {error:#}"),
                    );
                }
            }
        }
        self.changed();
    }

    /// Starts a game: wakes the console, launches the game, and starts the
    /// stream if it is not running yet.
    pub fn launch(self: &Arc<Self>, viewer: u64, console_id: &str, game: &str) -> Result<()> {
        let profile = self.viewer_profile(viewer);
        if !may_play(profile.as_ref(), game) {
            bail!("Your profile may not play this game.");
        }
        let app = self.clone();
        let console = console_id.to_owned();
        let console_id = console.clone();
        let game = game.to_owned();
        tokio::spawn(async move {
            let _ = app.wake(&console_id).await;
            let mut last_error = None;
            // A console just woken from standby needs a moment before it takes commands.
            for attempt in 0..6 {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                let result = async {
                    let token = app.auth.lock().await.web_token().await?;
                    consoles::launch(&app.http, &token, &console_id, &game).await
                }
                .await;
                match result {
                    Ok(()) => {
                        println!("[games] launched {game}");
                        last_error = None;
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            if let Some(error) = last_error {
                app.send_direct(
                    viewer,
                    Direct::Error {
                        message: format!("Could not start the game: {error:#}"),
                    },
                );
            }
        });
        let running = matches!(
            self.inner.lock().unwrap().stream,
            StreamState::Starting { .. }
                | StreamState::Live { .. }
                | StreamState::Reconnecting { .. }
        );
        if !running {
            self.start(&console)?;
        }
        Ok(())
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
        let quality = self.wanted_quality();
        // With a capture card the remote play picture is only a backup: ask
        // the console for the lightest one.
        let capturing = self.access.settings().capture.device.is_some();
        let console_quality = if capturing { Quality::Low } else { quality };
        let signaling = {
            let mut auth = self.auth.lock().await;
            let token = auth.home_streaming_token().await?;
            let api = HomeApi::new(&token)?.with_720p(console_quality.is_720p());
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
            media: Some(self.console_media.clone()),
            max_video_kbps: console_quality.max_video_kbps(),
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
            inner.stream_quality = Some(quality);
        }
        self.changed();
        println!("[stream] live at {quality:?}");
        if capturing {
            self.start_capture(quality).await;
        }

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
                        SessionEvent::IdleWarning { seconds } => {
                            let minutes = (seconds + 30) / 60;
                            let when = if minutes >= 1 {
                                format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
                            } else {
                                format!("{seconds} seconds")
                            };
                            let text = format!("The Xbox ends the stream in {when} if nothing is pressed. Press any button to keep playing.");
                            let watchers: Vec<u64> =
                                app.inner.lock().unwrap().watchers.iter().copied().collect();
                            for viewer in watchers {
                                app.send_direct(
                                    viewer,
                                    Direct::Notice {
                                        text: text.clone(),
                                        countdown: None,
                                    },
                                );
                            }
                        }
                        SessionEvent::Disconnected(why) => {
                            reason = Some(disconnect_reason(&why));
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
        self.stop_capture();
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
        let viewer_profile = inner
            .viewers
            .get(&id)
            .and_then(|v| v.profile.as_ref())
            .and_then(|p| self.access.profile(&p.id));
        let games: serde_json::Map<String, Value> = inner
            .games
            .iter()
            .map(|(console, (_, games))| {
                let list: Vec<Value> = games
                    .iter()
                    .filter(|g| may_play(viewer_profile.as_ref(), &g.id))
                    .map(|g| json!({ "id": g.id, "name": g.name, "image": g.image }))
                    .collect();
                (console.clone(), Value::from(list))
            })
            .collect();
        let capture = admin.is_some().then(|| self.capture_status());
        let all_games = admin.is_some().then(|| {
            let mut all: Vec<(String, String)> = inner
                .games
                .values()
                .flat_map(|(_, g)| g.iter().map(|g| (g.id.clone(), g.name.clone())))
                .collect();
            all.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
            all.dedup_by(|a, b| a.0 == b.0);
            all.into_iter()
                .map(|(id, name)| json!({ "id": id, "name": name }))
                .collect::<Vec<_>>()
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
            "games": games,
            // Every game on every console, for the admin's profile editor.
            "allGames": all_games,
            "gamesNote": inner.games_note.clone(),
            "capture": capture,
            "admin": admin,
            "turns": turns,
            "quality": {
                "setting": self.access.settings().quality,
                "current": inner.stream_quality,
                "limit": inner.viewers.get(&id).and_then(|v| v.profile.as_ref()).and_then(|p| self.access.profile(&p.id)).map(|p| p.max_quality).unwrap_or_default(),
            },
            "playit": self.playit.status(),
        })
    }
}

/// A readable line for why the console ended the stream.
fn disconnect_reason(raw: &str) -> String {
    let parsed: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let reason = parsed.get("reason").and_then(Value::as_str).unwrap_or(raw);
    let lower = reason.to_ascii_lowercase();
    if reason.is_empty() {
        "The Xbox ended the stream.".to_owned()
    } else if lower.contains("idle") {
        "The Xbox ended the stream because nothing was pressed for a while.".to_owned()
    } else if lower.contains("another") || lower.contains("taken") || lower.contains("replaced") {
        "The Xbox ended the stream because another device started remote play.".to_owned()
    } else {
        format!("The Xbox ended the stream ({reason}).")
    }
}

#[cfg(test)]
mod disconnect_tests {
    #[test]
    fn readable_reasons() {
        assert_eq!(
            super::disconnect_reason(r#"{"reason":"Idle"}"#),
            "The Xbox ended the stream because nothing was pressed for a while."
        );
        assert_eq!(super::disconnect_reason(""), "The Xbox ended the stream.");
        assert_eq!(
            super::disconnect_reason("Other"),
            "The Xbox ended the stream (Other)."
        );
    }
}
