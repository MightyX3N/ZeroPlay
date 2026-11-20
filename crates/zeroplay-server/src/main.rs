//! ZeroPlay server.
//!
//! Runs on a machine at home (a NAS, in Docker), holds one stream from the
//! console and passes it on to every player's browser. Players' controllers
//! each get a console slot.

mod access;
mod app;
mod config;
mod playit;
mod sdp;
mod viewer;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use zeroplay_xbox::input::{GamepadState, KeyboardFrame, MouseFrame};

use access::{at_home, Door, Verdict};
use app::{App, Direct};
use config::Config;
use viewer::ViewerPeer;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let config = Config::from_env()?;
    let home_addr = config.http_addr;
    let remote_addr = config.remote_addr;
    let app = App::new(config).await?;
    println!("ZeroPlay server on http://{home_addr}");
    println!("Data is kept in {}", app.config.data_dir.display());

    let home = tokio::net::TcpListener::bind(home_addr).await?;
    let remote = tokio::net::TcpListener::bind(remote_addr).await?;
    let serve = |listener: tokio::net::TcpListener, door: Door| {
        let router = router(app.clone(), door).into_make_service_with_connect_info::<SocketAddr>();
        let app = app.clone();
        async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    if door == Door::Home {
                        println!("Stopping.");
                        app.stop().await;
                    }
                })
                .await
        }
    };
    let (a, b) = tokio::join!(serve(home, Door::Home), serve(remote, Door::Remote));
    a?;
    b?;
    Ok(())
}

fn router(app: Arc<App>, door: Door) -> Router {
    Router::new()
        .route("/", get(|| async { page(INDEX_HTML, "text/html; charset=utf-8") }))
        .route("/app.js", get(|| async { page(APP_JS, "text/javascript; charset=utf-8") }))
        .route("/style.css", get(|| async { page(STYLE_CSS, "text/css; charset=utf-8") }))
        .route("/api/access", get(access_state))
        .route("/api/password", post(set_password))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/ws", get(ws_upgrade))
        .layer(Extension(door))
        .with_state(app)
}

fn page(body: &'static str, content_type: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

/// What the page should show: the app, a login, or "set up at home first".
async fn access_state(
    State(app): State<Arc<App>>,
    Extension(door): Extension<Door>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let verdict = app.access.verdict(door, peer, &headers);
    Json(json!({ "verdict": verdict, "home": at_home(door, peer), "passwordSet": app.access.has_password() })).into_response()
}

#[derive(Deserialize)]
struct PasswordForm {
    password: String,
}

fn with_cookie(cookie: String) -> Response {
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

fn refused(message: impl std::fmt::Display) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": message.to_string() }))).into_response()
}

async fn set_password(
    State(app): State<Arc<App>>,
    Extension(door): Extension<Door>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(form): Json<PasswordForm>,
) -> Response {
    let result = if app.access.has_password() {
        app.access.change_password(door, peer, &headers, &form.password).await
    } else {
        app.access.set_password(door, peer, &form.password).await
    };
    match result {
        Ok(token) => {
            app.changed();
            with_cookie(access::session_cookie(&token))
        }
        Err(error) => refused(format!("{error:#}")),
    }
}

async fn login(State(app): State<Arc<App>>, Json(form): Json<PasswordForm>) -> Response {
    match app.access.login(&form.password).await {
        Ok(token) => with_cookie(access::session_cookie(&token)),
        Err(error) => refused(format!("{error:#}")),
    }
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    app.access.logout(&headers).await;
    with_cookie(access::clear_cookie())
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(app): State<Arc<App>>,
    Extension(door): Extension<Door>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if app.access.verdict(door, peer, &headers) != Verdict::Ok {
        return (StatusCode::UNAUTHORIZED, "Log in first").into_response();
    }
    let home = at_home(door, peer);
    ws.on_upgrade(move |socket| client(app, socket, home))
}

/// Messages from the page.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
enum FromPage {
    Name { name: String },
    Login,
    Logout,
    Refresh,
    Wake { id: String },
    Start { id: String },
    Stop,
    Offer { sdp: String },
    Leave,
    Pad { local: u8, buttons: u16, lx: f64, ly: f64, rx: f64, ry: f64, lt: f64, rt: f64 },
    Unplug { local: u8 },
    Key { code: u8, down: bool },
    Mouse { dx: i32, dy: i32, buttons: u8, wheel: i32 },
    Keyframe,
    PlayitConnect,
    PlayitDisconnect,
}

async fn client(app: Arc<App>, socket: WebSocket, home: bool) {
    let (id, mut direct) = app.add_viewer(home);
    let (mut sink, mut stream) = socket.split();
    let mut updates = app.subscribe_updates();
    let mut peer: Option<ViewerPeer> = None;

    let mut send_ok = sink.send(Message::Text(app.status_for(id).to_string().into())).await.is_ok();
    while send_ok {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<FromPage>(&text) {
                        Ok(message) => {
                            if let Some(reply) = handle(&app, id, home, message, &mut peer).await {
                                send_ok = sink.send(Message::Text(serde_json::to_string(&reply).unwrap_or_default().into())).await.is_ok();
                            }
                        }
                        Err(error) => println!("player {id} sent something odd: {error}"),
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            update = updates.recv() => {
                if let Err(tokio::sync::broadcast::error::RecvError::Closed) = update {
                    break;
                }
                send_ok = sink.send(Message::Text(app.status_for(id).to_string().into())).await.is_ok();
            }
            Some(message) = direct.recv() => {
                send_ok = sink.send(Message::Text(serde_json::to_string(&message).unwrap_or_default().into())).await.is_ok();
            }
        }
    }

    if let Some(peer) = peer.take() {
        peer.close().await;
    }
    app.remove_viewer(id).await;
}

fn error(message: impl std::fmt::Display) -> Option<Direct> {
    Some(Direct::Error { message: message.to_string() })
}

async fn handle(app: &Arc<App>, id: u64, home: bool, message: FromPage, peer: &mut Option<ViewerPeer>) -> Option<Direct> {
    match message {
        FromPage::Name { name } => app.set_name(id, &name),
        FromPage::Login => app.login().await,
        FromPage::Logout => app.logout().await,
        FromPage::Refresh => app.refresh_consoles().await,
        FromPage::Wake { id: console } => {
            if let Err(e) = app.wake(&console).await {
                return error(format!("Could not wake the console: {e:#}"));
            }
            let app = app.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                app.refresh_consoles().await;
            });
        }
        FromPage::Start { id: console } => {
            if let Err(e) = app.start(&console) {
                return error(format!("{e:#}"));
            }
        }
        FromPage::Stop => app.stop().await,
        FromPage::Offer { sdp } => {
            if let Some(old) = peer.take() {
                old.close().await;
            }
            match viewer::connect(app, sdp).await {
                Ok((new_peer, answer)) => {
                    *peer = Some(new_peer);
                    return Some(Direct::Answer { sdp: answer });
                }
                Err(e) => return error(format!("Could not start the video: {e:#}")),
            }
        }
        FromPage::Leave => {
            if let Some(old) = peer.take() {
                old.close().await;
            }
        }
        FromPage::Pad { local, buttons, lx, ly, rx, ry, lt, rt } => {
            let clamp = |v: f64, lo: f64| if v.is_finite() { v.clamp(lo, 1.0) } else { 0.0 };
            let state = GamepadState {
                index: 0,
                buttons,
                left_x: clamp(lx, -1.0),
                left_y: clamp(ly, -1.0),
                right_x: clamp(rx, -1.0),
                right_y: clamp(ry, -1.0),
                left_trigger: clamp(lt, 0.0),
                right_trigger: clamp(rt, 0.0),
            };
            app.set_pad(id, local, state).await;
        }
        FromPage::Unplug { local } => app.unplug_pad(id, local).await,
        FromPage::Key { code, down } => app.send_key(KeyboardFrame { pressed: down, key_code: code }),
        FromPage::Mouse { dx, dy, buttons, wheel } => {
            app.send_mouse(MouseFrame { x: dx, y: dy, wheel_y: wheel, buttons, ..Default::default() })
        }
        FromPage::Keyframe => app.request_keyframe(),
        FromPage::PlayitConnect | FromPage::PlayitDisconnect if !home => {
            return error("Remote access can only be changed from your home network.");
        }
        FromPage::PlayitConnect => app.playit.connect(),
        FromPage::PlayitDisconnect => app.playit.disconnect().await,
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_messages_parse() {
        let pad: FromPage = serde_json::from_str(r#"{"t":"pad","local":1,"buttons":16,"lx":0.5,"ly":-1,"rx":0,"ry":0,"lt":0,"rt":1}"#).unwrap();
        assert!(matches!(pad, FromPage::Pad { local: 1, buttons: 16, .. }));
        let start: FromPage = serde_json::from_str(r#"{"t":"start","id":"F400"}"#).unwrap();
        assert!(matches!(start, FromPage::Start { id } if id == "F400"));
        assert!(matches!(serde_json::from_str::<FromPage>(r#"{"t":"keyframe"}"#).unwrap(), FromPage::Keyframe));
    }

    #[test]
    fn direct_messages_have_the_shape_the_page_expects() {
        let rumble = serde_json::to_value(Direct::Rumble { local: 0, left: 1.0, right: 0.5, left_trigger: 0.0, right_trigger: 0.0, ms: 100 }).unwrap();
        assert_eq!(rumble["t"], "rumble");
        assert_eq!(rumble["leftTrigger"], 0.0);
        let answer = serde_json::to_value(Direct::Answer { sdp: "v=0".to_owned() }).unwrap();
        assert_eq!(answer, serde_json::json!({ "t": "answer", "sdp": "v=0" }));
    }
}
