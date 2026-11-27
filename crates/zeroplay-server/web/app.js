"use strict";

// ── Small helpers ────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);
const show = (el, on) => el.classList.toggle("hidden", !on);
const screens = ["home", "login", "setup", "notsetup", "signin", "waking", "stream"];
function showScreen(name) {
  for (const s of screens) show($(s), s === name);
  // A text field on a screen that is now hidden must not keep the focus:
  // on iOS that takes the controller input away from the page.
  const active = document.activeElement;
  if (active && active !== document.body && active.closest && active.closest(".hidden")) active.blur();
}

let toastTimer = 0;
function toast(message, info) {
  const el = $("toast");
  el.textContent = message;
  el.classList.toggle("info", !!info);
  show(el, true);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => show(el, false), 6000);
}

// Xbox button bits (same as the server's).
const BTN = {
  nexus: 2, menu: 4, view: 8, a: 16, b: 32, x: 64, y: 128,
  up: 256, down: 512, left: 1024, right: 2048, lb: 4096, rb: 8192, ls: 16384, rs: 32768,
};
// Browser "standard" gamepad layout, button index → Xbox bit.
const STANDARD = [BTN.a, BTN.b, BTN.x, BTN.y, BTN.lb, BTN.rb, 0, 0, BTN.view, BTN.menu, BTN.ls, BTN.rs,
  BTN.up, BTN.down, BTN.left, BTN.right, BTN.nexus];
const KEYBOARD_LOCAL = 9;

// ── Connection to the server ────────────────────────────────────────────────

let ws = null;
let status = null;
let lastError = "";
let seenStatus = false;
let prevStreamState = "";

function send(message) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(message));
}

// ── Getting in: the password ────────────────────────────────────────────────

let requestsOpen = false;
let requestMode = false;

async function checkAccess() {
  try {
    const response = await fetch("/api/access", { cache: "no-store" });
    const access = await response.json();
    requestsOpen = !!access.requestsOpen;
    if (access.verdict === "ok") return connect();
    if (access.verdict === "loginNeeded") {
      setRequestMode(false);
      showScreen("login");
      // No automatic focus on phones: a text field that keeps the focus after
      // logging in takes the controller input away from the page on iOS.
      if (window.matchMedia("(pointer: fine)").matches) $("login-name").focus();
    } else {
      showScreen("notsetup");
    }
  } catch {
    setTimeout(checkAccess, 2000);
  }
}

async function postForm(url, body) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (response.ok) return null;
  const answer = await response.json().catch(() => ({}));
  return answer.error || "That did not work.";
}

function setRequestMode(on) {
  requestMode = on;
  $("login-title").textContent = on ? "Request access" : "ZeroPlay";
  show($("login-intro"), on);
  $("login-submit").textContent = on ? "Send request" : "Log in";
  $("login-switch").textContent = on ? "Back to log in" : "Request access";
  show($("login-switch"), requestsOpen || on);
  $("login-password").autocomplete = on ? "new-password" : "current-password";
  show($("login-error"), false);
  show($("login-done"), false);
}

$("login-switch").onclick = () => setRequestMode(!requestMode);

$("login-form").onsubmit = async (event) => {
  event.preventDefault();
  const body = { name: $("login-name").value, password: $("login-password").value };
  const error = await postForm(requestMode ? "/api/request" : "/api/login", body);
  show($("login-error"), !!error);
  $("login-error").textContent = error || "";
  if (error) return;
  $("login-password").value = "";
  // Give the focus back to the page, or iOS keeps sending input to the
  // (now hidden) text field and the controller is never seen.
  if (document.activeElement && document.activeElement !== document.body) document.activeElement.blur();
  if (requestMode) {
    setRequestMode(false);
    show($("login-done"), true);
    $("login-done").textContent = "Request sent. You can log in once the admin approves it.";
  } else {
    checkAccess();
  }
};

$("setup-form").onsubmit = async (event) => {
  event.preventDefault();
  const password = $("setup-password").value;
  let error = password !== $("setup-again").value ? "The two passwords are not the same." : null;
  if (!error) error = await postForm("/api/setup", { name: $("setup-name").value, password });
  show($("setup-error"), !!error);
  $("setup-error").textContent = error || "";
  if (!error) $("setup-password").value = $("setup-again").value = "";
};

$("btn-logout").onclick = async () => {
  await fetch("/api/logout", { method: "POST" }).catch(() => {});
  if (ws) ws.close();
};

// ── Profiles (admins) ───────────────────────────────────────────────────────

let profilesOpen = false;
let lastAdmin = "";
let editing = null;

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  Object.assign(node, props);
  node.append(...children.filter((c) => c !== null && c !== undefined));
  return node;
}

// Picture source: the Xbox's remote play picture, or a capture card.
let lastCapture = "";
function renderCapture(s) {
  const c = s && s.capture;
  if (!c) return;
  const key = JSON.stringify(c);
  if (key === lastCapture) return;
  lastCapture = key;
  const option = (value, text) => el("option", { value, textContent: text });
  $("cap-device").replaceChildren(
    option("", "Xbox remote play"),
    ...c.devices.map((d) => option(d.path, `Capture card: ${d.name} (${d.path})${d.reachable ? "" : " – not reachable yet"}`)));
  if (c.device && !c.devices.find((d) => d.path === c.device)) $("cap-device").append(option(c.device, `${c.device} (not found now)`));
  $("cap-device").value = c.device || "";
  $("cap-audio").replaceChildren(
    option("", "Xbox remote play sound"),
    option("auto", "From the capture card (found automatically)"),
    ...c.audio.map((a) => option(a.id, `Sound card: ${a.name}`)));
  $("cap-audio").value = c.audioChoice || "";
  $("cap-audio").disabled = !c.device;
  $("cap-sharp").value = c.sharp ? "1" : "";
  $("cap-sharp").disabled = !c.device;
  const lines = [];
  if (c.device && !c.active && !c.gpu) lines.push("No graphics chip visible to ZeroPlay, so the processor encodes (uses more of it).");
  if (c.note) lines.push(c.note);
  if (c.active && c.input) lines.push(`The card sends ${c.input}. Right now: ${c.fps ?? 0} frames a second.`);
  $("cap-note").textContent = lines.join(" ");
  show($("cap-help"), c.devices.length === 0 || c.devices.some((d) => !d.reachable));
}

const sendCapture = () => send({ t: "adminCapture", device: $("cap-device").value || null, audio: $("cap-device").value ? ($("cap-audio").value || null) : null, sharp: $("cap-sharp").value === "1" });
$("cap-device").onchange = () => {
  // Picking a card takes its sound too, unless chosen otherwise.
  if ($("cap-device").value && !$("cap-audio").value) $("cap-audio").value = "auto";
  sendCapture();
};
$("cap-audio").onchange = sendCapture;
$("cap-sharp").onchange = sendCapture;

function renderProfiles(s) {
  const admin = s && s.admin;
  renderCapture(s);
  show($("btn-profiles"), !!admin);
  const timeRequests = (admin && admin.timeRequests) || [];
  show($("requests-badge"), !!(admin && (admin.requests.length || timeRequests.length)));
  show($("profiles"), profilesOpen && !!admin);
  if (!admin) return;
  const key = JSON.stringify(admin) + "|" + editing;
  if (key === lastAdmin) return;
  lastAdmin = key;
  $("allow-requests").checked = admin.settings.allowRequests;
  $("timed-play").checked = admin.settings.timedPlay;
  $("turn-minutes").value = String(admin.settings.turnMinutes);
  if ($("turn-minutes").value !== String(admin.settings.turnMinutes)) {
    $("turn-minutes").append(el("option", { value: admin.settings.turnMinutes, textContent: `${admin.settings.turnMinutes} minutes` }));
    $("turn-minutes").value = String(admin.settings.turnMinutes);
  }

  show($("requests-box"), admin.requests.length + timeRequests.length > 0);
  $("requests").replaceChildren(
    ...admin.requests.map((r) => el("div", { className: "item" },
      el("div", { className: "item-head" },
        el("span", { className: "name", textContent: `${r.name} wants a profile` }),
        el("button", { className: "btn primary small", textContent: "Approve", onclick: () => send({ t: "adminAnswer", id: r.id, approve: true }) }),
        el("button", { className: "btn danger small", textContent: "Reject", onclick: () => send({ t: "adminAnswer", id: r.id, approve: false }) })))),
    ...timeRequests.map((r) => el("div", { className: "item" },
      el("div", { className: "item-head" },
        el("span", { className: "name", textContent: `${r.name} asks for ${r.minutes} more minutes` }),
        el("button", { className: "btn primary small", textContent: "Give time", onclick: () => send({ t: "adminAnswerTime", id: r.id, approve: true }) }),
        el("button", { className: "btn danger small", textContent: "No", onclick: () => send({ t: "adminAnswerTime", id: r.id, approve: false }) })))));

  const me = s.you && s.you.profile && s.you.profile.id;
  $("profile-list").replaceChildren(...admin.profiles.map((p) => {
    const head = el("div", { className: "item-head" },
      el("span", { className: "name", textContent: p.name }),
      p.admin ? el("span", { className: "tag", textContent: "Admin" }) : null,
      p.id === me ? el("span", { className: "tag", textContent: "You" }) : null,
      el("button", { className: "btn secondary small", textContent: editing === p.id ? "Close" : "Edit", onclick: () => { editing = editing === p.id ? null : p.id; renderProfiles(status); } }));
    const item = el("div", { className: "item" }, head);
    if (editing === p.id) item.append(profileEditor(p, p.id === me));
    return item;
  }));
}

function profileEditor(p, isMe) {
  const name = el("input", { value: p.name, maxLength: 24, ariaLabel: "Name" });
  const password = el("input", { type: "password", placeholder: "New password (leave empty to keep)", minLength: 8, autocomplete: "new-password", ariaLabel: "New password" });
  const admin = el("input", { type: "checkbox", checked: p.admin });
  const home = el("input", { type: "checkbox", checked: p.xboxHome });
  // Games: all, or a chosen list.
  const allGames = (status && status.allGames) || [];
  const everyGame = el("input", { type: "checkbox", checked: !p.games });
  const gameBoxes = allGames.map((g) => ({ id: g.id, box: el("input", { type: "checkbox", checked: !!(p.games && p.games.includes(g.id)) }), name: g.name }));
  const gameList = el("div", { className: "game-checks" },
    ...(allGames.length
      ? gameBoxes.map((g) => el("label", { className: "check" }, g.box, g.name))
      : [el("p", { className: "muted small", textContent: "No games found yet. They show up once a console is on or in standby." })]));
  show(gameList, !everyGame.checked);
  everyGame.onchange = () => show(gameList, !everyGame.checked);
  const quality = el("select", { ariaLabel: "Highest quality" },
    ...["max", "high", "medium", "low"].map((q) => el("option", { value: q, textContent: q[0].toUpperCase() + q.slice(1), selected: p.maxQuality === q })));
  const save = el("button", { className: "btn primary small", textContent: "Save", onclick: () => {
    const changes = { name: name.value, admin: admin.checked, xboxHome: home.checked, maxQuality: quality.value,
      games: everyGame.checked ? null : gameBoxes.filter((g) => g.box.checked).map((g) => g.id) };
    if (password.value) changes.password = password.value;
    send({ t: "adminUpdate", id: p.id, changes });
    editing = null;
  } });
  const remove = el("button", { className: "btn danger small", textContent: "Delete profile", onclick: () => {
    if (window.confirm(`Delete the profile "${p.name}"? They are logged out everywhere.`)) send({ t: "adminDelete", id: p.id });
  } });
  return el("div", { className: "edit" },
    el("label", { className: "field" }, "Name", name),
    el("label", { className: "field" }, "Password", password),
    el("label", { className: "check" }, admin, "Admin (manages profiles and remote access)"),
    el("label", { className: "check" }, home, "May open Xbox Home and press the Xbox button"),
    el("label", { className: "field" }, "Highest stream quality", quality),
    el("label", { className: "check full" }, everyGame, "May play every game"),
    el("div", { className: "full" }, gameList),
    el("div", { className: "item-actions full" }, save, isMe ? null : remove));
}

$("btn-profiles").onclick = () => { profilesOpen = true; lastAdmin = ""; renderProfiles(status); };
$("btn-profiles-close").onclick = () => { profilesOpen = false; editing = null; renderProfiles(status); };
$("allow-requests").onchange = (event) => send({ t: "adminAllowRequests", allow: event.target.checked });
const sendTimedPlay = () => send({ t: "adminTimedPlay", enabled: $("timed-play").checked, minutes: Number($("turn-minutes").value) });
$("timed-play").onchange = sendTimedPlay;
$("turn-minutes").onchange = sendTimedPlay;
$("add-profile").onsubmit = (event) => {
  event.preventDefault();
  send({ t: "adminCreate", name: $("add-name").value, password: $("add-password").value, admin: $("add-admin").checked });
  $("add-name").value = $("add-password").value = "";
  $("add-admin").checked = false;
};
$("btn-ms-signout").onclick = () => {
  if (window.confirm("Sign this server out of your Microsoft account? Nobody can stream until it is signed in again.")) send({ t: "logout" });
};

function connect() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(`${proto}//${location.host}/ws`);
  ws.binaryType = "arraybuffer";
  ws.onopen = () => {
    const name = localStorage.getItem("zp-name");
    if (name) send({ t: "name", name });
  };
  ws.onmessage = (event) => {
    if (event.data instanceof ArrayBuffer) {
      if (feed) feed.push(event.data);
      return;
    }
    const message = JSON.parse(event.data);
    if (message.t === "status") onStatus(message);
    else if (message.t === "answer") onAnswer(message.sdp);
    else if (message.t === "rumble") onRumble(message);
    else if (message.t === "error") toast(message.message);
    else if (message.t === "loggedOut") ws.close();
    else if (message.t === "notice") onNotice(message);
  };
  ws.onclose = () => {
    $("console-info").textContent = "Lost the connection to the server, retrying…";
    stopWatching();
    setTimeout(checkAccess, 2000);
  };
}

// ── What the server tells us ────────────────────────────────────────────────

let selected = null;
let wantWatch = false;

let remoteOpen = false;

function renderRemote(s) {
  if (!s) return;
  const p = s.playit || { state: "off" };
  show($("remote"), remoteOpen);
  // Only an admin at home (or whoever does the first setup) links or unlinks playit.
  const home = !!(s.you && s.you.home) && (!!s.admin || !s.setupDone);
  show($("remote-off"), home && (p.state === "off" || p.state === "failed"));
  show($("remote-claiming"), p.state === "claiming");
  show($("remote-running"), p.state === "running");
  show($("remote-away"), !home);
  show($("btn-playit-disconnect"), home && p.state !== "off");
  const main = p.secure || p.web;
  show($("remote-address-box"), !!main);
  show($("remote-waiting"), p.state === "running" && !main);
  if (main) {
    $("remote-address").href = main;
    $("remote-address").textContent = main.replace(/^https?:\/\//, "");
  }
  show($("remote-secure-tip"), !!p.secure);
  show($("remote-plain"), !!(p.secure && p.web));
  if (p.secure && p.web) $("remote-plain").textContent = `Also reachable without https at ${p.web.replace(/^https?:\/\//, "")}`;
  show($("remote-reason"), !!p.reason);
  $("remote-reason").textContent = p.reason || "";
  show($("remote-note"), !!p.note);
  $("remote-note").textContent = p.note || "";
  $("btn-playit-disconnect").textContent = p.state === "claiming" ? "Cancel" : "Unlink";
  show($("remote-error"), !!p.error);
  $("remote-error").textContent = p.error || "";
  if (p.url) {
    $("remote-link").href = p.url;
    $("remote-link").textContent = p.url.replace(/^https?:\/\//, "");
  }
  $("btn-remote").classList.toggle("on", p.state === "running");
}

$("btn-remote").onclick = () => { remoteOpen = true; renderRemote(status); };
$("btn-remote-close").onclick = () => { remoteOpen = false; renderRemote(status); };
$("btn-playit-connect").onclick = () => send({ t: "playitConnect" });
$("btn-playit-disconnect").onclick = () => {
  if (status && status.playit && status.playit.state === "claiming") return send({ t: "playitDisconnect" });
  if (window.confirm("Unlink playit.gg? The page will no longer be reachable from outside your home.")) send({ t: "playitDisconnect" });
};

function onStatus(s) {
  status = s;
  renderRemote(s);
  renderProfiles(s);
  $("me-name").textContent = (s.you && s.you.profile && s.you.profile.name) || "Setup";
  $("me-initial").textContent = ((s.you && s.you.profile && s.you.profile.name) || "?").trim().charAt(0).toUpperCase() || "?";
  $("btn-logout").title = `${(s.you && s.you.profile && s.you.profile.name) || "Setup"} – log out`;
  const stream = s.stream;
  // Show a failure when it happens, not again on every page load.
  if (stream.state === "failed" && stream.error !== lastError && seenStatus) toast(stream.error);
  lastError = stream.state === "failed" ? stream.error : "";
  seenStatus = true;
  // Opened from an app to play: join the stream it started (or start it).
  if (autoPlay && s.signedIn && (s.consoles || []).length) {
    autoPlay = false;
    wantWatch = true;
    if (stream.state === "idle" || stream.state === "failed") setTimeout(play, 0);
  }

  if (!s.signedIn) {
    stopWatching();
    renderSignin(s);
    showScreen("signin");
    return;
  }
  if (!s.setupDone) {
    showScreen("setup");
    return;
  }
  if (stream.state === "starting") {
    prevStreamState = "starting";
    renderWaking(stream);
    if (!watching()) showScreen("waking");
    else {
      // Restarting under the players (for example a quality change).
      show($("reconnecting"), true);
      $("reconnect-text").textContent = stream.step === "Changing quality" ? "Changing quality…" : `${stream.step}…`;
    }
    return;
  }
  const reconnecting = stream.state === "reconnecting";
  if (reconnecting) prevStreamState = "reconnecting";
  show($("reconnecting"), reconnecting && watching());
  if (reconnecting) $("reconnect-text").textContent = `Lost the connection to the Xbox. The game was paused. Reconnecting (try ${stream.attempt} of 3)…`;
  if ((stream.state === "live" || (reconnecting && watching())) && wantWatch) {
    // The Xbox stream was started again under us (quality change or
    // reconnect): start the video connection fresh too, so nothing buffered
    // from before puts the picture and sound behind.
    if (watching() && stream.state === "live" && (prevStreamState === "starting" || prevStreamState === "reconnecting")) {
      refreshVideo();
    }
    prevStreamState = stream.state;
    if (!watching()) startWatching();
    renderMenu();
    renderTurnChip(s);
    showScreen("stream");
    return;
  }
  if (stream.state !== "live" && !reconnecting) {
    stopWatching();
    wantWatch = false;
  }
  renderHome(s);
  showScreen("home");
}

function renderSignin(s) {
  show($("signin-start"), !s.login);
  show($("signin-code"), !!s.login);
  if (s.login) {
    $("signin-value").textContent = s.login.code;
    $("signin-link").href = s.login.uri;
    $("signin-link").textContent = s.login.uri.replace(/^https?:\/\//, "");
  }
  show($("signin-error"), !!s.loginError);
  $("signin-error").textContent = s.loginError || "";
}

const STEPS = ["Waking console", "Starting stream", "Authorizing", "Connecting"];
function renderWaking(stream) {
  $("waking-title").textContent = stream.consoleName;
  $("waking-step").textContent = `${stream.step}…`;
  const now = Math.max(0, STEPS.indexOf(stream.step));
  $("waking-steps").replaceChildren(...STEPS.map((step, i) => {
    const li = document.createElement("li");
    li.textContent = step;
    li.className = i < now ? "done" : i === now ? "now" : "";
    return li;
  }));
}

function powerText(power) {
  if (power === "On") return "On";
  if (power === "ConnectedStandby") return "Asleep";
  return power || "Unknown";
}

function renderHome(s) {
  const consoles = s.consoles || [];
  if (!consoles.find((c) => c.id === selected)) {
    const live = s.stream.state === "live" && s.stream.consoleId;
    selected = live || (consoles.find((c) => c.remotePlay) || consoles[0] || {}).id || null;
  }
  const current = consoles.find((c) => c.id === selected);
  const live = s.stream.state === "live";
  const liveHere = live && s.stream.consoleId === selected;

  $("console-name").textContent = current ? current.name : "No console";
  $("console-info").textContent = live
    ? `Streaming ${s.stream.consoleName} · ${s.players.length} playing`
    : current ? `${powerText(current.power)} · ${current.type}` : (s.consoleError || "No consoles found on this account");
  $("power-dot").className = "dot " + (current && current.power === "On" ? "on" : current && current.power === "ConnectedStandby" ? "standby" : "");

  $("shelf-count").textContent = `${consoles.length}`;
  $("tiles").replaceChildren(...consoles.map((c) => {
    const tile = document.createElement("button");
    tile.className = "tile console" + (c.power === "On" || c.power === "ConnectedStandby" ? "" : " off")
      + (homeRow === "consoles" && c.id === selected ? " selected" : "");
    tile.setAttribute("aria-label", c.name);
    const caption = document.createElement("span");
    caption.className = "caption";
    const kind = document.createElement("small");
    kind.textContent = s.stream.consoleId === c.id && live ? "Streaming" : powerText(c.power);
    caption.append(c.name, kind);
    const label = document.createElement("span");
    label.className = "label";
    label.textContent = c.type || "Xbox";
    tile.append(caption, label);
    tile.onclick = () => {
      // First tap picks it (its games show in the dock); a tap on the picked one plays.
      if (homeRow === "consoles" && selected === c.id) { play(); return; }
      selected = c.id; homeRow = "consoles"; renderHome(status);
    };
    return tile;
  }));
  renderGames(s, current);
  renderHero(s, current, live, liveHere);
  // The dock keeps the page at the top (the game big above it); rows below scroll into view.
  const inDock = document.querySelector("#game-tiles .tile.selected");
  const chosen = document.querySelector("#game-grid .tile.selected, #tiles .tile.selected");
  // Only when the controller or the arrow keys moved the focus: status updates (every
  // second or so) and the mouse never scroll the page.
  if (scrollToFocus) {
    scrollToFocus = false;
    if (inDock || homeRow === "island") $("home").scrollTo({ top: 0, behavior: "smooth" });
    else if (chosen) chosen.scrollIntoView({ block: "center", behavior: "smooth" });
  }

  renderPlayers(s);
}

// ── Game library ─────────────────────────────────────────────────────────────
// The games installed on the chosen console (only those this profile may
// play). Picking one starts it on the Xbox and joins the stream.

let scrollToFocus = false;
function focusMoved() { scrollToFocus = true; renderHome(status); renderIslandFocus(); }

// The island's buttons a controller can reach (Up from the top row of games):
// Controls and Refresh.
let islandIndex = 0;
const islandButtons = () => ["btn-controls", "btn-refresh"].map($).filter((b) => b && !b.classList.contains("hidden"));
function renderIslandFocus() {
  const buttons = islandButtons();
  document.querySelectorAll(".island-btn").forEach((b) => b.classList.toggle("focused", homeRow === "island" && b === buttons[islandIndex]));
}
let homeRow = "games"; // which row has the focus: "island", "games" (the dock) or "consoles"
let selectedGame = null;

function gamesOf(s, consoleId) {
  return ((s.games || {})[consoleId]) || [];
}

function renderGames(s, current) {
  const games = current ? gamesOf(s, current.id) : [];
  const note = current ? ((s.gamesNote || {})[current.id] || "") : "";
  show($("games-shelf"), games.length > 0 || !!note);
  show($("games-note"), !!note);
  $("games-note").textContent = note;
  if (!games.find((g) => g.id === selectedGame)) selectedGame = games[0] ? games[0].id : null;
  if (!games.length && homeRow === "games") homeRow = "consoles";
  renderIslandFocus();
  $("games-title").textContent = current ? `Games on ${current.name}` : "Games";
  $("games-count").textContent = `${games.length} game${games.length === 1 ? "" : "s"}`;
  const cols = homeCols();
  const card = (g, labelled) => {
    const tile = document.createElement("button");
    const art = g.hero || g.image;
    tile.className = "tile game" + (art ? " has-art" : "") + (homeRow === "games" && g.id === selectedGame ? " selected" : "");
    tile.setAttribute("aria-label", `Play ${g.name}`);
    if (art) tile.style.backgroundImage = `url("${art}")`;
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = g.name;
    tile.append(name);
    if (labelled) {
      const label = document.createElement("span");
      label.className = "label";
      label.textContent = g.name;
      tile.append(label);
    }
    // A click or tap plays it; the controller moves the focus (and the big title above).
    tile.onclick = () => { selectedGame = g.id; homeRow = "games"; launchGame(g); };
    return tile;
  };
  // The dock: the most recently played games, as many as fit; the rest below.
  $("game-tiles").replaceChildren(...games.slice(0, cols).map((g) => card(g, false)));
  $("game-grid").replaceChildren(...games.slice(cols).map((g) => card(g, true)));
  show($("games-more"), games.length > cols);
}

// How many cards fit in a row (6 on a 16:9 screen, as Korie), from the CSS sizes.
function homeCols() {
  const probe = $("tile-probe");
  const style = getComputedStyle(probe);
  const tile = (probe.offsetWidth - (parseFloat(style.paddingLeft) || 0)) || 200;
  const gap = parseFloat(style.marginRight) || 20;
  const width = $("home").clientWidth || window.innerWidth;
  // As many cards as fit in the dock with a quarter of a card free at each end (room for
  // the focused card to grow 1.2x); then the cards are widened so the row fills the dock.
  // The grid below uses the same size, so its columns line up with the dock's.
  const inset = parseFloat(getComputedStyle($("games-shelf")).marginLeft) || 0;
  const inner = width - 2 * inset - 2;
  const size = (n) => (inner - (n - 1) * gap) / (n + 0.5);
  let cols = 2;
  while (size(cols + 1) >= tile * 0.92) cols++;
  const fitted = size(cols);
  const home = $("home").style;
  home.setProperty("--cols", cols);
  home.setProperty("--tile-w", `${fitted}px`);
  home.setProperty("--tile-h", `${fitted * 3 / 5}px`);
  home.setProperty("--radius", `${fitted * 0.071}px`);
  return cols;
}

window.addEventListener("resize", () => {
  if (status && !$("home").classList.contains("hidden")) renderHome(status);
});

// What has the focus, shown big above the dock: the focused game (with its picture
// filling the screen), or the console.
function renderHero(s, current, live, liveHere) {
  const games = current ? gamesOf(s, current.id) : [];
  const game = homeRow !== "consoles" ? games.find((g) => g.id === selectedGame) : null;
  const art = game ? (game.hero || game.image) : (games[0] && (games[0].hero || games[0].image));
  const backdrop = $("hero-art");
  if (art) backdrop.style.backgroundImage = `url("${art}")`;
  backdrop.classList.toggle("on", !!art);
  if (game) {
    // The line above already names the console.
    $("hero-label").textContent = liveHere ? "Streaming now" : "";
    $("hero-title").textContent = game.name;
    $("hero-meta").textContent = liveHere ? "Starts on the Xbox and joins the stream." : "Wakes the Xbox if it is asleep, starts the game and joins the stream.";
    $("btn-play-label").textContent = "Play";
    $("btn-play").disabled = !current;
  } else {
    $("hero-label").textContent = liveHere ? "Streaming now" : "Your console";
    $("hero-title").textContent = current ? current.name : "No console found";
    $("hero-meta").textContent = current
      ? `${current.type} · ${powerText(current.power)} · Remote play ${current.remotePlay ? "on" : "off – turn on remote features on the console"}`
      : "Turn on remote features on your Xbox, then press refresh.";
    $("btn-play-label").textContent = liveHere ? "Join stream" : "Jump into Xbox";
    $("btn-play").disabled = !current || (live && !liveHere);
  }
  show($("btn-wake"), !!current && current.power !== "On" && !liveHere);
  show($("btn-stop"), live);
}

function launchGame(g) {
  if (!status || !selected) return;
  wantWatch = true;
  send({ t: "launch", console: selected, game: g.id });
  toast(`Starting ${g.name}…`, true);
  if (status.stream.state === "live") onStatus(status);
}

function renderPlayers(s) {
  const chips = s.players.map((p) => {
    const chip = document.createElement("div");
    chip.className = "player";
    const badge = document.createElement("span");
    badge.className = `badge p${p.slot}`;
    badge.textContent = `P${p.slot}`;
    chip.append(badge, p.you ? `${p.name} (you)` : p.name);
    return chip;
  });
  if (s.watching.length) {
    const chip = document.createElement("div");
    chip.className = "player";
    const badge = document.createElement("span");
    badge.className = "badge watch";
    badge.textContent = s.watching.length;
    chip.append(badge, "watching");
    chips.push(chip);
  }
  $("players").replaceChildren(...chips);
}

// ── Buttons ──────────────────────────────────────────────────────────────────

$("btn-login").onclick = () => send({ t: "login" });
$("btn-refresh").onclick = () => send({ t: "refresh" });
$("btn-wake").onclick = () => { if (selected) { send({ t: "wake", id: selected }); toast("Asked the console to wake up.", true); } };
$("btn-stop").onclick = () => send({ t: "stop" });
$("btn-cancel").onclick = () => send({ t: "stop" });
$("btn-play").onclick = () => {
  const games = status && selected ? gamesOf(status, selected) : [];
  const game = homeRow !== "consoles" ? games.find((g) => g.id === selectedGame) : null;
  if (game) launchGame(game);
  else play();
};


function play() {
  if (!status || !selected) return;
  wantWatch = true;
  if (status.stream.state === "live") onStatus(status);
  else send({ t: "start", id: selected });
}

// ── Video ────────────────────────────────────────────────────────────────────

let pc = null;
let lastFrames = -1;
let stalled = 0;
let lastBytes = 0;
let lastStatsAt = 0;

function watching() { return pc !== null || feed !== null; }

// Outside the home the video and sound come inside the page's own connection
// (one tunnel for everything) and the browser decodes them itself.
function useFeed() {
  return !!(status && status.you && !status.you.home) && typeof VideoDecoder !== "undefined";
}

function playStream(media) {
  const video = $("video");
  if (video.srcObject !== media) video.srcObject = media;
  applyAudio();
  video.play().catch(() => {
    video.dataset.autoplayMuted = "1";
    video.muted = true;
    video.play().catch(() => {});
    show($("tap-to-play"), true);
  });
}

let feed = null;

class Feed {
  constructor() {
    this.canvas = $("feed-canvas");
    this.ctx2d = this.canvas.getContext("2d");
    this.media = this.canvas.captureStream();
    this.frames = 0;
    this.bytes = 0;
    this.height = 0;
    this.codec = null;
    this.waitKey = true;
    this.video = null;
    this.audio = null;
    this.audioCtx = null;
    this.nextAudio = 0;
    if (typeof AudioDecoder !== "undefined") {
      try {
        this.audioCtx = new AudioContext({ sampleRate: 48000, latencyHint: "interactive" });
        const out = this.audioCtx.createMediaStreamDestination();
        this.audioOut = out;
        out.stream.getAudioTracks().forEach((t) => this.media.addTrack(t));
        this.audio = new AudioDecoder({ output: (data) => this.play(data), error: () => {} });
        this.audio.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
      } catch {
        this.audio = null;
      }
    }
    if (!this.audio) toast("This browser cannot play the sound from outside the home (it needs a newer version).");
    playStream(this.media);
  }

  push(buffer) {
    const bytes = new Uint8Array(buffer);
    if (bytes.length < 6) return;
    this.bytes += bytes.length;
    const kind = bytes[0];
    const key = bytes[1] === 1;
    const data = bytes.subarray(6);
    const timestamp = Math.round(performance.now() * 1000);
    if (kind === 1) this.pushVideo(data, key, timestamp);
    else if (kind === 2 && this.audio && this.audio.state === "configured") {
      try { this.audio.decode(new EncodedAudioChunk({ type: "key", timestamp, data })); } catch {}
    }
  }

  pushVideo(data, key, timestamp) {
    if (key) {
      const codec = avcCodec(data);
      // A new stream (another quality) brings new picture settings: start the
      // decoder over for them.
      const sps = spsKey(data);
      if (sps && sps !== this.sps) {
        this.sps = sps;
        this.codec = null;
      }
      if (codec && codec !== this.codec) {
        if (this.video) try { this.video.close(); } catch {}
        this.video = new VideoDecoder({
          output: (frame) => this.draw(frame),
          error: () => { this.codec = null; this.waitKey = true; send({ t: "keyframe" }); },
        });
        this.video.configure({ codec, optimizeForLatency: true });
        this.codec = codec;
      }
      this.waitKey = false;
    }
    if (this.waitKey || !this.video || this.video.state !== "configured") return;
    // Too far behind: skip to the next key frame instead of lagging.
    if (this.video.decodeQueueSize > 6) { this.waitKey = true; send({ t: "keyframe" }); return; }
    try {
      this.video.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp, data }));
    } catch {
      this.waitKey = true;
      send({ t: "keyframe" });
    }
  }

  draw(frame) {
    if (this.canvas.width !== frame.displayWidth || this.canvas.height !== frame.displayHeight) {
      this.canvas.width = frame.displayWidth;
      this.canvas.height = frame.displayHeight;
    }
    this.ctx2d.drawImage(frame, 0, 0);
    this.height = frame.displayHeight;
    this.frames++;
    frame.close();
  }

  play(data) {
    const ctx = this.audioCtx;
    if (ctx.state === "suspended") ctx.resume().catch(() => {});
    const buffer = ctx.createBuffer(data.numberOfChannels, data.numberOfFrames, data.sampleRate);
    for (let c = 0; c < data.numberOfChannels; c++) {
      const plane = new Float32Array(data.numberOfFrames);
      data.copyTo(plane, { planeIndex: c, format: "f32-planar" });
      buffer.copyToChannel(plane, c);
    }
    data.close();
    const now = ctx.currentTime;
    // Keep the sound close behind: restart the schedule when it drifts.
    if (this.nextAudio < now + 0.02 || this.nextAudio > now + 0.25) this.nextAudio = now + 0.04;
    const source = ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(this.audioOut);
    source.start(this.nextAudio);
    this.nextAudio += buffer.duration;
  }

  close() {
    for (const decoder of [this.video, this.audio]) if (decoder) try { decoder.close(); } catch {}
    if (this.audioCtx) this.audioCtx.close().catch(() => {});
    this.media.getTracks().forEach((t) => t.stop());
  }
}

// The SPS of an Annex B frame as text, to notice when it changes.
function spsKey(data) {
  for (let i = 0; i + 4 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1 && (data[i + 3] & 0x1f) === 7) {
      let end = i + 4;
      while (end + 2 < data.length && !(data[end] === 0 && data[end + 1] === 0 && (data[end + 2] === 1 || data[end + 2] === 0))) end++;
      return Array.from(data.subarray(i + 3, Math.min(end, i + 64))).join(",");
    }
  }
  return null;
}

// "avc1.PPCCLL" from the SPS in an Annex B frame.
function avcCodec(data) {
  for (let i = 0; i + 7 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1 && (data[i + 3] & 0x1f) === 7) {
      const hex = (b) => b.toString(16).padStart(2, "0");
      return `avc1.${hex(data[i + 4])}${hex(data[i + 5])}${hex(data[i + 6])}`;
    }
  }
  return null;
}

async function startWatching() {
  if (useFeed()) {
    feed = new Feed();
    lastFrames = -1;
    stalled = 0;
    send({ t: "media" });
    return;
  }
  pc = new RTCPeerConnection();
  const mine = pc;
  const media = new MediaStream();
  pc.ontrack = (event) => {
    media.addTrack(event.track);
    playStream(media);
  };
  pc.onconnectionstatechange = () => {
    if (mine.connectionState === "failed") toast("The video connection failed. Check the media ports (ZEROPLAY_RTC_PORTS) are reachable.");
  };
  pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  await new Promise((resolve) => {
    if (mine.iceGatheringState === "complete") return resolve();
    const timer = setTimeout(resolve, 2000);
    mine.addEventListener("icegatheringstatechange", () => {
      if (mine.iceGatheringState === "complete") { clearTimeout(timer); resolve(); }
    });
  });
  if (pc !== mine) return;
  send({ t: "offer", sdp: pc.localDescription.sdp });
  lastFrames = -1;
  stalled = 0;
}

async function onAnswer(sdp) {
  if (!pc) return;
  try {
    await pc.setRemoteDescription({ type: "answer", sdp });
  } catch (error) {
    toast(`Could not start the video: ${error.message}`);
  }
}

// A new video connection, keeping the player's place and controllers.
function refreshVideo() {
  if (recorder) stopRecording();
  if (pc) { pc.close(); pc = null; }
  if (feed) { feed.close(); feed = null; }
  startWatching();
}

function stopWatching() {
  stopRecording();
  if (pc || feed) {
    if (pc) pc.close();
    if (feed) feed.close();
    pc = null;
    feed = null;
    send({ t: "leave" });
  }
  $("video").srcObject = null;
  closeMenu();
  unplugAll();
}

$("btn-unmute").onclick = () => {
  const video = $("video");
  delete video.dataset.autoplayMuted;
  video.muted = muted;
  video.play().catch(() => {});
  if (feed && feed.audioCtx) feed.audioCtx.resume().catch(() => {});
  show($("tap-to-play"), false);
};

// Ask for a fresh picture if the video stops moving, and keep the stats up to date.
function showStats(height, fps, mbps) {
  const parts = [];
  if (height) parts.push(`${height}p`);
  if (fps) parts.push(`${Math.round(fps)} fps`);
  parts.push(`${mbps.toFixed(1)} Mbps`);
  $("stats").replaceChildren(...parts.map((p) => { const s = document.createElement("span"); s.textContent = p; return s; }));
}

setInterval(() => {
  if (!feed) return;
  const frames = feed.frames;
  if (status && status.stream.state === "live" && frames === lastFrames) {
    stalled++;
    if (stalled >= 2) { send({ t: "keyframe" }); feed.waitKey = true; stalled = 0; }
  } else {
    stalled = 0;
  }
  const fps = lastFrames >= 0 ? frames - lastFrames : 0;
  lastFrames = frames;
  showStats(feed.height, fps, (feed.bytes * 8) / 1e6);
  feed.bytes = 0;
}, 1000);

setInterval(async () => {
  if (!pc) return;
  const report = await pc.getStats();
  report.forEach((r) => {
    if (r.type !== "inbound-rtp" || r.kind !== "video") return;
    const frames = r.framesDecoded || 0;
    if (status && status.stream.state === "live" && frames === lastFrames) {
      stalled++;
      if (stalled >= 2) { send({ t: "keyframe" }); stalled = 0; }
    } else {
      stalled = 0;
    }
    lastFrames = frames;
    const now = performance.now();
    const mbps = lastStatsAt ? ((r.bytesReceived - lastBytes) * 8) / ((now - lastStatsAt) * 1000) : 0;
    lastBytes = r.bytesReceived;
    lastStatsAt = now;
    showStats(r.frameHeight, r.framesPerSecond, mbps);
  });
}, 1000);

// ── Menu (F1, or hold View + Menu) ──────────────────────────────────────────

let menuOpen = false;
let menuIndex = 0;
// The keyboard and mouse need no switch: a key that is pressed plays, and the
// tip about the mouse only shows for a few seconds after a mouse moves over the
// picture (never on a TV, phone or tablet without one).
let mouseSeenAt = 0;
let mouseUsed = localStorage.getItem("zp-mouse-used") === "1";
const SENSITIVITIES = [0.25, 0.5, 0.75, 1, 1.5, 2, 3, 4];
let mouseSens = Number(localStorage.getItem("zp-mouse-sens")) || 1;
let statsOn = localStorage.getItem("zp-stats") === "on";
const menuItems = () => [...document.querySelectorAll("#menu .menu-item")];

// Shared quality: one setting for everyone, each player limited to their
// profile's maximum. Changing it restarts the stream for a few seconds.
const QUALITIES = ["max", "high", "medium", "low"];
const QUALITY_NAMES = { max: "Max", high: "High (1080p)", medium: "Medium (720p)", low: "Low (720p)" };

function allowedQualities() {
  const q = (status && status.quality) || {};
  const limit = QUALITIES.indexOf(q.limit || "max");
  return QUALITIES.slice(Math.max(0, limit));
}

// What the stream runs at right now.
function currentQuality() {
  const q = (status && status.quality) || {};
  return q.current || q.setting || "max";
}

// A drop-down: pick once, so the stream restarts once.
let qualityOpen = false;

function renderQuality() {
  const now = currentQuality();
  $("m-quality").textContent = `Quality: ${QUALITY_NAMES[now] || now} ${qualityOpen ? "▴" : "▾"}`;
  const allowed = allowedQualities();
  document.querySelectorAll("#menu [data-quality]").forEach((el) => {
    const q = el.dataset.quality;
    show(el, qualityOpen && allowed.includes(q));
    el.textContent = `${q === now ? "✓ " : ""}${QUALITY_NAMES[q]}`;
  });
}

$("m-quality").onclick = () => {
  qualityOpen = !qualityOpen;
  renderMenu();
};

document.querySelectorAll("#menu [data-quality]").forEach((el) => {
  el.onclick = () => {
    const pick = el.dataset.quality;
    qualityOpen = false;
    // Back onto the Quality line itself.
    const visible = () => menuItems().filter((item) => !item.classList.contains("hidden"));
    if (pick !== currentQuality()) {
      send({ t: "quality", quality: pick });
      toast(`Quality: ${QUALITY_NAMES[pick]}. The picture comes back in a few seconds.`, true);
    }
    renderMenu();
    menuIndex = Math.max(0, visible().indexOf($("m-quality")));
    renderMenu();
  };
});

function renderMenu() {
  if (!status) return;
  $("menu-title").textContent = status.stream.consoleName || "Xbox";
  const n = status.players.length;
  $("menu-players").textContent = `${n} playing · ${status.watching.length} watching`;
  $("m-sens").textContent = `Mouse sensitivity: ${mouseSens}`;
  show($("m-sens"), mouseUsed);
  const turns = status.turns || {};
  show($("m-turns"), !!turns.enabled);
  $("m-turns").textContent = `Take turns: ${turns.optedIn ? "on" : "off"}`;
  show($("m-more-time"), !!(turns.enabled && turns.inControl && turns.secondsLeft != null && !turns.askedForTime));
  const turnText = !turns.enabled || !turns.optedIn ? ""
    : turns.inControl ? (turns.secondsLeft != null ? `Your turn: ${clock(turnLeft())} left` : "Your turn (nobody waiting)")
    : turns.waiting ? `In line: number ${turns.waiting}` : "";
  show($("menu-turn"), !!turnText);
  $("menu-turn").textContent = turnText;
  renderQuality();
  $("m-volume").textContent = muted ? "Volume: muted  ◀ ▶" : `Volume: ${volume}%  ◀ ▶`;
  $("m-record").textContent = recorder ? `Stop recording (${clock(recordSeconds())})` : "Start recording";
  $("m-stats").textContent = statsOn ? "Hide stats" : "Show stats";
  show($("stats"), statsOn);
  const visible = menuItems().filter((el) => !el.classList.contains("hidden"));
  menuItems().forEach((el) => el.classList.toggle("active", el === visible[menuIndex]));
  // Keep the selected item in view when moving with a controller.
  if (menuOpen && visible[menuIndex]) visible[menuIndex].scrollIntoView({ block: "nearest" });
}

function openMenu() {
  if (document.pointerLockElement) document.exitPointerLock();
  menuOpen = true;
  menuIndex = 0;
  releaseAll();
  renderMenu();
  show($("menu"), true);
}

function closeMenu() {
  menuOpen = false;
  qualityOpen = false;
  show($("menu"), false);
}

$("m-resume").onclick = closeMenu;
$("c-players").onclick = () => {
  separatePlayers = !separatePlayers;
  localStorage.setItem("zp-separate", separatePlayers ? "on" : "off");
  unplugAll();
  mergedWasActive = false;
  renderControls();
};
$("m-turns").onclick = () => send({ t: "turns", on: !(status && status.turns && status.turns.optedIn) });
$("m-more-time").onclick = () => send({ t: "requestTime" });
$("m-volume").onclick = () => { muted = !muted; applyAudio(); renderMenu(); };
$("m-record").onclick = () => { if (recorder) stopRecording(); else startRecording(); renderMenu(); };
$("m-sens").onclick = () => {
  mouseSens = SENSITIVITIES[(SENSITIVITIES.indexOf(mouseSens) + 1) % SENSITIVITIES.length];
  localStorage.setItem("zp-mouse-sens", String(mouseSens));
  renderMenu();
};
$("m-fullscreen").onclick = () => {
  if (document.fullscreenElement) document.exitFullscreen();
  else document.documentElement.requestFullscreen().catch(() => {});
  closeMenu();
};
$("m-stats").onclick = () => {
  statsOn = !statsOn;
  localStorage.setItem("zp-stats", statsOn ? "on" : "off");
  renderMenu();
};
$("m-home").onclick = () => { wantWatch = false; stopWatching(); onStatus(status); };
$("m-end").onclick = () => { wantWatch = false; send({ t: "stop" }); };

// ── Controllers ──────────────────────────────────────────────────────────────

const sent = new Map(); // local index → { key, at }
let comboSince = 0;
let prevButtons = new Map(); // local index → bits, for menu and home navigation

// ── Reading controllers ─────────────────────────────────────────────────────
// Every controller is read with the standard layout (A B X Y, bumpers,
// triggers, View, Menu, sticks, d-pad, Xbox button), whatever layout name the
// browser reports for it: browsers often report none even for ordinary
// controllers, and ignoring the buttons then made the controller do nothing.

function buttonDown(b) {
  if (!b) return false;
  if (typeof b === "number") return b > 0.5;
  return !!b.pressed || (typeof b.value === "number" && b.value > 0.5);
}

function buttonValue(b) {
  if (!b) return 0;
  if (typeof b === "number") return Math.max(0, Math.min(1, b));
  if (typeof b.value === "number" && b.value > 0) return Math.min(1, b.value);
  return b.pressed ? 1 : 0;
}

function padState(gp) {
  let buttons = 0;
  let lt = 0;
  let rt = 0;
  const list = gp.buttons || [];
  // Each physical button does what it is bound to (by default the standard
  // layout; extra buttons such as back paddles do nothing until bound).
  for (let i = 0; i < list.length; i++) {
    const target = targetOf(gp.id, i);
    if (!target) continue;
    if (target === "lt") lt = Math.max(lt, buttonValue(list[i]));
    else if (target === "rt") rt = Math.max(rt, buttonValue(list[i]));
    else if (buttonDown(list[i])) buttons |= BTN[target] || 0;
  }
  const axes = gp.axes || [];
  const axis = (i) => {
    const v = Number(axes[i]) || 0;
    return Math.abs(v) < 0.08 ? 0 : Math.max(-1, Math.min(1, v));
  };
  return {
    buttons,
    lx: axis(0), ly: -axis(1), rx: axis(2), ry: -axis(3),
    lt,
    rt,
  };
}

// ── Controls: button rebinding and gyro ─────────────────────────────────────
// Saved per device (this browser), per controller model.

const TARGETS = ["a", "b", "x", "y", "lb", "rb", "lt", "rt", "view", "menu", "ls", "rs", "up", "down", "left", "right", "nexus"];
const TARGET_NAMES = {
  a: "A", b: "B", x: "X", y: "Y", lb: "LB", rb: "RB", lt: "LT", rt: "RT", view: "View", menu: "Menu",
  ls: "Left stick press", rs: "Right stick press", up: "D-pad up", down: "D-pad down", left: "D-pad left",
  right: "D-pad right", nexus: "Xbox button",
};

function loadJson(key, fallback) {
  try { return JSON.parse(localStorage.getItem(key)) || fallback; } catch { return fallback; }
}

let binds = loadJson("zp-binds", {}); // controller id → { physical index: target ("" = nothing) }

function targetOf(padId, index) {
  const own = binds[padId];
  if (own && Object.prototype.hasOwnProperty.call(own, index)) return own[index];
  return TARGETS[index] || "";
}

// What a physical button is called: by the standard layout, or "Extra button".
function physicalName(index) {
  return index < TARGETS.length ? TARGET_NAMES[TARGETS[index]] : `Extra button ${index - TARGETS.length + 1}`;
}

// Puts `target` on physical button `index`. If that button had a job, the
// button that had `target` gets it (a swap), so nothing is lost; a button
// without a job (an extra button such as a back paddle) simply copies
// `target`, and the original button keeps it too.
function bindButton(padId, target, index, count) {
  const own = { ...(binds[padId] || {}) };
  const old = targetOf(padId, index);
  if (old) {
    for (let i = 0; i < Math.max(count, TARGETS.length); i++) {
      if (i !== index && targetOf(padId, i) === target) own[i] = old;
    }
  }
  own[index] = target;
  binds[padId] = own;
  localStorage.setItem("zp-binds", JSON.stringify(binds));
}

function resetButtons(padId) {
  delete binds[padId];
  localStorage.setItem("zp-binds", JSON.stringify(binds));
}

let controlsOpen = false;
let controlsIndex = 0;
let controlsPad = null; // the chosen controller's id
let capture = null; // { target, padId, baseline: Set, until }

function openControls() {
  if (menuOpen) closeMenu();
  if (document.pointerLockElement) document.exitPointerLock();
  releaseAll();
  controlsOpen = true;
  controlsIndex = 0;
  renderControls();
  show($("controls"), true);
}

function closeControls() {
  controlsOpen = false;
  capture = null;
  show($("controls"), false);
}

function renderControls() {
  if (!controlsOpen) return;
  const pads = readPads();
  if (!pads.find((p) => p.id === controlsPad)) controlsPad = pads[0] ? pads[0].id : null;
  const pad = pads.find((p) => p.id === controlsPad);
  const short = (id) => id.replace(/\s*\(.*$/, "").slice(0, 40) || "Controller";
  $("controls-pads").replaceChildren(
    pad
      ? el("button", { className: "menu-item", textContent: `Controller: ${short(pad.id)}${pads.length > 1 ? "  ◀ ▶" : ""}`, onclick: () => {
          const ids = [...new Set(pads.map((p) => p.id))];
          controlsPad = ids[(ids.indexOf(controlsPad) + 1) % ids.length];
          renderControls();
        } })
      : el("p", { className: "muted small", textContent: "Connect a controller and press a button on it to change its buttons." }));
  const count = pad ? (pad.buttons || []).length : 0;
  const physicalFor = (target) => {
    const found = [];
    for (let i = 0; i < Math.max(count, TARGETS.length); i++) if (targetOf(controlsPad, i) === target) found.push(physicalName(i));
    return found.length ? found.join(", ") : "—";
  };
  $("controls-binds").replaceChildren(...(pad ? [
    ...TARGETS.map((t) => {
      const item = el("button", { className: "menu-item", onclick: () => startCapture(t) },
        TARGET_NAMES[t], el("span", { className: "bind", textContent: physicalFor(t) }));
      return item;
    }),
    el("button", { className: "menu-item", textContent: "Reset buttons to default", onclick: () => { resetButtons(controlsPad); renderControls(); } }),
    el("p", { className: "muted small", textContent: "Pick an action, then press the button for it. Extra buttons, such as back paddles, can be picked too when the browser sees them." }),
  ] : []));
  $("c-players").textContent = separatePlayers ? "Controllers here: each one is its own player" : "Controllers here: all play as one player";
  $("g-on").textContent = `Gyro: ${gyro.on ? "on" : "off"}`;
  $("g-stick").textContent = `Gyro moves: ${gyro.stick === "left" ? "left" : "right"} stick`;
  $("g-sens").textContent = `Gyro sensitivity: ${gyro.sens}  ◀ ▶`;
  $("g-invx").textContent = `Invert left/right: ${gyro.invX ? "on" : "off"}`;
  $("g-invy").textContent = `Invert up/down: ${gyro.invY ? "on" : "off"}`;
  $("g-hold").textContent = `Gyro only while LT is held: ${gyro.hold ? "on" : "off"}`;
  show($("g-note"), !!gyroNote);
  $("g-note").textContent = gyroNote;
  show($("capture"), !!capture);
  if (capture) $("capture").textContent = `Press the button for ${TARGET_NAMES[capture.target]}… (Esc cancels)`;
  const items = controlsItems();
  if (controlsIndex >= items.length) controlsIndex = items.length - 1;
  items.forEach((item, i) => item.classList.toggle("active", i === controlsIndex));
  if (items[controlsIndex]) items[controlsIndex].scrollIntoView({ block: "nearest" });
}

function controlsItems() {
  return [...document.querySelectorAll("#controls .menu-item")].filter((el) => !el.classList.contains("hidden"));
}

function rawPressed(pad) {
  const down = new Set();
  (pad.buttons || []).forEach((b, i) => { if (buttonDown(b)) down.add(i); });
  return down;
}

function startCapture(target) {
  const pad = readPads().find((p) => p.id === controlsPad);
  if (!pad) return;
  capture = { target, padId: controlsPad, baseline: rawPressed(pad), until: performance.now() + 6000 };
  renderControls();
}

// Waits for a button that was not already held when picking started.
function runCapture() {
  const pad = readPads().find((p) => p.id === capture.padId);
  if (!pad || performance.now() > capture.until) { capture = null; renderControls(); return; }
  const down = rawPressed(pad);
  for (const i of [...capture.baseline]) if (!down.has(i)) capture.baseline.delete(i);
  for (const i of down) {
    if (!capture.baseline.has(i)) {
      bindButton(capture.padId, capture.target, i, (pad.buttons || []).length);
      capture = null;
      // Do not let this press also click the item under the cursor.
      prevButtons.set(pad.index, 0xffff);
      renderControls();
      return;
    }
  }
}

$("btn-controls").onclick = openControls;
// Controllers plugged in or out while the panel is open.
setInterval(() => { if (controlsOpen && !capture) renderControls(); }, 1000);
window.addEventListener("keydown", (event) => {
  if (!controlsOpen || event.code !== "Escape") return;
  event.preventDefault();
  event.stopImmediatePropagation();
  if (capture) { capture = null; renderControls(); } else closeControls();
}, true);
$("m-controls").onclick = openControls;
$("c-close").onclick = closeControls;

// Gyro: this device's motion sensor (a phone or tablet held with a clip-on
// controller) moves a stick. Controllers' own gyros are not readable by
// browsers' controller support.
let gyro = { on: false, stick: "right", sens: 1, invX: false, invY: false, hold: false, ...loadJson("zp-gyro", {}) };
let gyroNote = "";
let motion = null; // { yaw, pitch, at }
const saveGyro = () => localStorage.setItem("zp-gyro", JSON.stringify(gyro));
const GYRO_SENS = [0.25, 0.5, 0.75, 1, 1.5, 2, 3, 4];

window.addEventListener("devicemotion", (event) => {
  const r = event.rotationRate;
  if (!r || (r.alpha == null && r.beta == null && r.gamma == null)) return;
  const beta = r.beta || 0;
  const gamma = r.gamma || 0;
  // Turning and tilting as seen on the screen, whichever way the device is held.
  const angle = (screen.orientation && screen.orientation.angle) || window.orientation || 0;
  let yaw;
  let pitch;
  switch ((angle + 360) % 360) {
    case 90: yaw = beta; pitch = -gamma; break;
    case 270: yaw = -beta; pitch = gamma; break;
    case 180: yaw = -gamma; pitch = -beta; break;
    default: yaw = gamma; pitch = beta;
  }
  motion = { yaw, pitch, at: performance.now() };
  if (gyroNote) { gyroNote = ""; renderControls(); }
});

async function setGyro(on) {
  if (on && typeof DeviceMotionEvent !== "undefined" && typeof DeviceMotionEvent.requestPermission === "function") {
    try {
      if ((await DeviceMotionEvent.requestPermission()) !== "granted") {
        gyroNote = "Motion access was not allowed.";
        on = false;
      }
    } catch {
      gyroNote = "This browser only allows motion access on an https address.";
      on = false;
    }
  }
  gyro.on = on;
  saveGyro();
  if (on) {
    setTimeout(() => {
      if (gyro.on && (!motion || performance.now() - motion.at > 1500)) {
        gyroNote = window.isSecureContext ? "No motion sensor found on this device." : "No motion data: this browser may only give it on an https address.";
        renderControls();
      }
    }, 1500);
  } else {
    gyroNote = "";
  }
  renderControls();
}

function changeGyroSens(step) {
  const i = GYRO_SENS.indexOf(gyro.sens);
  gyro.sens = GYRO_SENS[Math.max(0, Math.min(GYRO_SENS.length - 1, (i < 0 ? 3 : i) + step))];
  saveGyro();
  renderControls();
}

$("g-on").onclick = () => setGyro(!gyro.on);
$("g-stick").onclick = () => { gyro.stick = gyro.stick === "left" ? "right" : "left"; saveGyro(); renderControls(); };
$("g-sens").onclick = () => changeGyroSens(GYRO_SENS.indexOf(gyro.sens) >= GYRO_SENS.length - 1 ? -(GYRO_SENS.length - 1) : 1);
$("g-invx").onclick = () => { gyro.invX = !gyro.invX; saveGyro(); renderControls(); };
$("g-invy").onclick = () => { gyro.invY = !gyro.invY; saveGyro(); renderControls(); };
$("g-hold").onclick = () => { gyro.hold = !gyro.hold; saveGyro(); renderControls(); };

// The stick movement from the gyro right now ([0, 0] when off).
function gyroStick(ltHeld) {
  if (!gyro.on || !motion || performance.now() - motion.at > 250) return [0, 0];
  if (gyro.hold && !ltHeld) return [0, 0];
  const dead = (v) => (Math.abs(v) < 1.5 ? 0 : v);
  const x = (dead(motion.yaw) * gyro.sens / 120) * (gyro.invX ? 1 : -1);
  const y = (dead(motion.pitch) * gyro.sens / 120) * (gyro.invY ? -1 : 1);
  const clamp = (v) => Math.max(-1, Math.min(1, v));
  return [clamp(x), clamp(y)];
}

// Adds the gyro to a controller state's chosen stick.
function addGyro(state) {
  const [gx, gy] = gyroStick(state.lt > 0.3);
  if (!gx && !gy) return state;
  const clamp = (v) => Math.max(-1, Math.min(1, v));
  const out = { ...state };
  if (gyro.stick === "left") { out.lx = clamp(out.lx + gx); out.ly = clamp(out.ly + gy); }
  else { out.rx = clamp(out.rx + gx); out.ry = clamp(out.ry + gy); }
  return out;
}

// Controllers the page was told about, for browsers whose getGamepads() list
// stays empty or out of date.
const knownPads = new Map();
window.addEventListener("gamepadconnected", (e) => { if (e.gamepad) knownPads.set(e.gamepad.index, e.gamepad); });
window.addEventListener("gamepaddisconnected", (e) => { if (e.gamepad) knownPads.delete(e.gamepad.index); });

function readPads() {
  const pads = new Map();
  let list = [];
  try {
    list = navigator.getGamepads ? navigator.getGamepads() || [] : [];
  } catch {
    list = [];
  }
  for (const gp of list) {
    if (gp && gp.connected !== false) pads.set(gp.index, gp);
  }
  for (const [index, gp] of knownPads) {
    if (!pads.has(index) && gp.connected !== false) pads.set(index, gp);
  }
  return [...pads.values()];
}

function sendPad(local, state) {
  if (state.buttons || state.lx || state.ly || state.rx || state.ry || state.lt || state.rt) hideCountdownOnInput();
  const key = JSON.stringify(state);
  const last = sent.get(local);
  const now = performance.now();
  if (last && last.key === key && now - last.at < 250) return;
  sent.set(local, { key, at: now });
  send({ t: "pad", local, ...state });
}

function neutral() {
  return { buttons: 0, lx: 0, ly: 0, rx: 0, ry: 0, lt: 0, rt: 0 };
}

function releaseAll() {
  for (const local of sent.keys()) sendPad(local, neutral());
}

function unplugAll() {
  for (const local of sent.keys()) send({ t: "unplug", local });
  sent.clear();
}

function pressed(local, bits, bit) {
  const before = prevButtons.get(local) || 0;
  return (bits & bit) && !(before & bit);
}

function navigate(bits, local) {
  const onHome = !$("home").classList.contains("hidden");
  if (controlsOpen) {
    if (capture) return;
    const items = controlsItems();
    if (!items.length) return;
    if (pressed(local, bits, BTN.down)) controlsIndex = (controlsIndex + 1) % items.length;
    if (pressed(local, bits, BTN.up)) controlsIndex = (controlsIndex + items.length - 1) % items.length;
    if (pressed(local, bits, BTN.a)) items[controlsIndex].click();
    if (items[controlsIndex] && items[controlsIndex].id === "g-sens") {
      if (pressed(local, bits, BTN.right)) changeGyroSens(1);
      if (pressed(local, bits, BTN.left)) changeGyroSens(-1);
    }
    if (pressed(local, bits, BTN.b)) closeControls();
    renderControls();
    return;
  }
  if (menuOpen) {
    const items = menuItems().filter((el) => !el.classList.contains("hidden"));
    if (pressed(local, bits, BTN.down)) menuIndex = (menuIndex + 1) % items.length;
    if (pressed(local, bits, BTN.up)) menuIndex = (menuIndex + items.length - 1) % items.length;
    if (pressed(local, bits, BTN.a)) items[menuIndex].click();
    if (items[menuIndex] && items[menuIndex].dataset.adjust === "volume") {
      if (pressed(local, bits, BTN.right)) changeVolume(10);
      if (pressed(local, bits, BTN.left)) changeVolume(-10);
    }
    if (pressed(local, bits, BTN.b)) closeMenu();
    if (menuOpen) renderMenu();
  } else if (onHome && status) {
    const games = gamesOf(status, selected);
    const cols = homeCols();
    if (homeRow === "games" && games.length) {
      // The dock is the first row of the games; the grid below continues in the same columns.
      let i = Math.max(0, games.findIndex((g) => g.id === selectedGame));
      const col = i % cols;
      if (pressed(local, bits, BTN.right) && col < cols - 1 && i + 1 < games.length) i += 1;
      if (pressed(local, bits, BTN.left) && col > 0) i -= 1;
      if (pressed(local, bits, BTN.up)) {
        if (i - cols >= 0) i -= cols;
        else { homeRow = "island"; islandIndex = 0; focusMoved(); return; }
      }
      if (pressed(local, bits, BTN.down)) {
        if (i + cols < games.length) i += cols;
        else if (Math.floor(i / cols) < Math.floor((games.length - 1) / cols)) i = games.length - 1;
        else if ((status.consoles || []).length) { homeRow = "consoles"; focusMoved(); return; }
      }
      if (games[i].id !== selectedGame) { selectedGame = games[i].id; focusMoved(); }
      if (pressed(local, bits, BTN.a)) launchGame(games[i]);
    } else if (homeRow === "island") {
      const buttons = islandButtons();
      if (pressed(local, bits, BTN.right) && islandIndex < buttons.length - 1) { islandIndex++; renderIslandFocus(); }
      if (pressed(local, bits, BTN.left) && islandIndex > 0) { islandIndex--; renderIslandFocus(); }
      if (pressed(local, bits, BTN.down)) { homeRow = games.length ? "games" : "consoles"; focusMoved(); return; }
      if (pressed(local, bits, BTN.a) && buttons[islandIndex]) buttons[islandIndex].click();
    } else {
      if (pressed(local, bits, BTN.up)) {
        homeRow = games.length ? "games" : "island";
        islandIndex = 0;
        focusMoved();
        return;
      }
      const ids = (status.consoles || []).map((c) => c.id);
      const i = ids.indexOf(selected);
      if (pressed(local, bits, BTN.right) && i < ids.length - 1) { selected = ids[i + 1]; focusMoved(); }
      if (pressed(local, bits, BTN.left) && i > 0) { selected = ids[i - 1]; focusMoved(); }
      if (pressed(local, bits, BTN.a)) play();
    }
  }
}

// One player per browser by default: every controller, the keyboard and the
// mouse are merged, so you can switch between them (or use them together).
// "Separate players" gives each controller its own console slot instead.
let separatePlayers = localStorage.getItem("zp-separate") === "on";
const MERGED_LOCAL = 0;

function combine(states) {
  const out = neutral();
  const bigger = (a, b) => (Math.hypot(b[0], b[1]) > Math.hypot(a[0], a[1]) ? b : a);
  let left = [0, 0];
  let right = [0, 0];
  for (const s of states) {
    out.buttons |= s.buttons;
    left = bigger(left, [s.lx, s.ly]);
    right = bigger(right, [s.rx, s.ry]);
    out.lt = Math.max(out.lt, s.lt);
    out.rt = Math.max(out.rt, s.rt);
  }
  [out.lx, out.ly] = left;
  [out.rx, out.ry] = right;
  return out;
}

let mergedWasActive = false;

function pollPads() {
  if (capture) runCapture();
  const inStream = watching() && !$("stream").classList.contains("hidden");
  const playing = inStream && !menuOpen && !controlsOpen;
  const states = new Map(); // local index → state, for this tick
  let combo = false;
  for (const gp of readPads()) {
    const state = padState(gp);
    if ((state.buttons & BTN.view) && (state.buttons & BTN.menu)) combo = true;
    if (playing) states.set(gp.index, state);
    else navigate(state.buttons, gp.index);
    prevButtons.set(gp.index, state.buttons);
  }
  updateMouseStick();
  const kbActive = playing && (keysDown.size > 0 || mouseLocked());
  if (kbActive) states.set(KEYBOARD_LOCAL, keyboardState());

  // The gyro goes onto the first controller (or the merged one).
  if (playing && gyro.on) {
    const first = separatePlayers ? (states.keys().next().value ?? KEYBOARD_LOCAL) : null;
    if (separatePlayers) states.set(first, addGyro(states.get(first) || neutral()));
    else if (states.size || (motion && performance.now() - motion.at < 250)) {
      const merged = addGyro(states.size ? combine([...states.values()]) : neutral());
      states.clear();
      states.set(MERGED_LOCAL, merged);
    }
  }

  if (separatePlayers) {
    for (const [local, state] of states) sendPad(local, state);
    if (!kbActive && kbWasActive) sendPad(KEYBOARD_LOCAL, neutral());
    // Controllers that went away are unplugged on the console too.
    for (const local of [...sent.keys()]) {
      if (local !== KEYBOARD_LOCAL && !states.has(local) && playing) {
        send({ t: "unplug", local });
        sent.delete(local);
      }
    }
  } else if (states.size) {
    sendPad(MERGED_LOCAL, combine([...states.values()]));
    mergedWasActive = true;
  } else if (mergedWasActive) {
    sendPad(MERGED_LOCAL, neutral());
    mergedWasActive = false;
  }
  kbWasActive = kbActive;

  // Holding View + Menu for 0.8 s opens or closes the menu.
  if (combo && inStream) {
    if (!comboSince) comboSince = performance.now();
    else if (performance.now() - comboSince > 800 && comboSince !== -1) {
      comboSince = -1;
      if (menuOpen) closeMenu(); else openMenu();
    }
  } else {
    comboSince = 0;
  }
  show($("mouse-hint"), playing && !mouseLocked() && performance.now() - mouseSeenAt < 4000);
}
// Read the controllers every frame, like games do, with a timer as a backup
// for when the browser slows frames down.
let lastPoll = 0;
function pollTick() {
  const now = performance.now();
  if (now - lastPoll < 4) return;
  lastPoll = now;
  pollPads();
}
function pollFrame() {
  pollTick();
  requestAnimationFrame(pollFrame);
}
requestAnimationFrame(pollFrame);
setInterval(pollTick, 8);

// ── Keyboard as a controller (fixed layout for now) ─────────────────────────

const KEY_BUTTONS = {
  Space: BTN.a, KeyB: BTN.b, Backspace: BTN.b, KeyX: BTN.x, KeyY: BTN.y,
  KeyQ: BTN.lb, KeyE: BTN.rb, Tab: BTN.view, Enter: BTN.menu, KeyH: BTN.nexus,
  KeyF: BTN.ls, KeyG: BTN.rs,
  ArrowUp: BTN.up, ArrowDown: BTN.down, ArrowLeft: BTN.left, ArrowRight: BTN.right,
};
const KEY_OTHER = new Set(["KeyW", "KeyA", "KeyS", "KeyD", "KeyI", "KeyJ", "KeyK", "KeyL", "KeyZ", "KeyC"]);
const keysDown = new Set();

let kbWasActive = false;

function keyboardState() {
  const k = (code) => (keysDown.has(code) ? 1 : 0);
  const clamp = (v) => Math.max(-1, Math.min(1, v));
  let buttons = mouseButtons | wheelButtons();
  for (const code of keysDown) buttons |= KEY_BUTTONS[code] || 0;
  return {
    buttons,
    lx: k("KeyD") - k("KeyA"), ly: k("KeyW") - k("KeyS"),
    rx: clamp(k("KeyL") - k("KeyJ") + shapedX), ry: clamp(k("KeyI") - k("KeyK") + shapedY),
    lt: Math.max(k("KeyZ"), mouseLT), rt: Math.max(k("KeyC"), mouseRT),
  };
}

// ── Mouse as the right stick (while the pointer is captured) ────────────────
// Left click = RT, right click = LT, middle click = RS, wheel = RB / LB,
// back / forward side buttons = B / Y.

let mouseDX = 0;
let mouseDY = 0;
let stickX = 0;
let stickY = 0;
let mouseButtons = 0;
let mouseLT = 0;
let mouseRT = 0;
let wheelBit = 0;
let wheelUntil = 0;

function mouseLocked() {
  return document.pointerLockElement === $("video");
}

function wheelButtons() {
  return performance.now() < wheelUntil ? wheelBit : 0;
}

// Mouse movement is turned into stick speed. Games ignore small stick
// movements (their dead zone), so any movement starts past it.
function updateMouseStick() {
  const scale = mouseSens / 10;
  const smooth = (current, delta) => current * 0.5 + delta * scale * 0.5;
  stickX = smooth(stickX, mouseDX);
  stickY = smooth(stickY, -mouseDY);
  mouseDX = 0;
  mouseDY = 0;
  const shape = (v) => {
    const a = Math.abs(v);
    if (a < 0.01) return 0;
    return Math.sign(v) * Math.min(1, 0.2 + a * 0.8);
  };
  stickX = Math.abs(stickX) < 0.01 ? 0 : stickX;
  stickY = Math.abs(stickY) < 0.01 ? 0 : stickY;
  stickX = Math.max(-1.5, Math.min(1.5, stickX));
  stickY = Math.max(-1.5, Math.min(1.5, stickY));
  shapedX = shape(stickX);
  shapedY = shape(stickY);
}
let shapedX = 0;
let shapedY = 0;

// A real mouse moving over the picture (not a touch, and not a TV remote).
$("video").addEventListener("pointermove", (event) => {
  if (event.pointerType === "mouse" && (event.movementX || event.movementY) && !mouseLocked()) {
    mouseSeenAt = performance.now();
  }
});

$("video").addEventListener("click", (event) => {
  if (event.pointerType && event.pointerType !== "mouse") return;
  if (!menuOpen && !controlsOpen && !mouseLocked()) {
    mouseSeenAt = 0;
    if (!mouseUsed) { mouseUsed = true; localStorage.setItem("zp-mouse-used", "1"); }
    const request = $("video").requestPointerLock({ unadjustedMovement: true });
    if (request && request.catch) request.catch(() => $("video").requestPointerLock());
  }
});

document.addEventListener("pointerlockchange", () => {
  if (!mouseLocked()) {
    mouseButtons = 0;
    mouseLT = 0;
    mouseRT = 0;
    stickX = stickY = shapedX = shapedY = 0;
  }
});

document.addEventListener("mousemove", (event) => {
  if (!mouseLocked()) return;
  mouseDX += event.movementX;
  mouseDY += event.movementY;
});

const MOUSE_BUTTONS = { 1: BTN.rs, 3: BTN.b, 4: BTN.y };
function onMouseButton(event, down) {
  if (!mouseLocked()) return;
  event.preventDefault();
  if (event.button === 0) mouseRT = down ? 1 : 0;
  else if (event.button === 2) mouseLT = down ? 1 : 0;
  else if (MOUSE_BUTTONS[event.button]) {
    if (down) mouseButtons |= MOUSE_BUTTONS[event.button];
    else mouseButtons &= ~MOUSE_BUTTONS[event.button];
  }
}
document.addEventListener("mousedown", (event) => onMouseButton(event, true));
document.addEventListener("mouseup", (event) => onMouseButton(event, false));
document.addEventListener("contextmenu", (event) => { if (mouseLocked()) event.preventDefault(); });
document.addEventListener("wheel", (event) => {
  if (!mouseLocked() || !event.deltaY) return;
  wheelBit = event.deltaY < 0 ? BTN.rb : BTN.lb;
  wheelUntil = performance.now() + 120;
}, { passive: true });

window.addEventListener("keydown", (event) => {
  const inStream = !$("stream").classList.contains("hidden");
  if (!inStream) return;
  if (menuOpen && document.activeElement === $("m-volume") && (event.code === "ArrowLeft" || event.code === "ArrowRight")) {
    event.preventDefault();
    changeVolume(event.code === "ArrowRight" ? 10 : -10);
    return;
  }
  if (event.code === "F1" || event.code === "Escape") {
    event.preventDefault();
    if (menuOpen) closeMenu(); else openMenu();
    return;
  }
  if (menuOpen || controlsOpen) return;
  if (KEY_BUTTONS[event.code] !== undefined || KEY_OTHER.has(event.code)) {
    event.preventDefault();
    keysDown.add(event.code);
  }
});

window.addEventListener("keyup", (event) => {
  if (!keysDown.delete(event.code)) return;
  event.preventDefault();
});

window.addEventListener("blur", () => {
  keysDown.clear();
});

// ── Rumble ───────────────────────────────────────────────────────────────────

function onRumble(r) {
  const gp = readPads().find((p) => p.index === r.local);
  if (!gp || !gp.vibrationActuator || !r.ms) return;
  gp.vibrationActuator.playEffect("dual-rumble", {
    duration: r.ms,
    strongMagnitude: Math.min(1, r.left),
    weakMagnitude: Math.min(1, r.right),
    leftTrigger: Math.min(1, r.leftTrigger),
    rightTrigger: Math.min(1, r.rightTrigger),
  }).catch(() => {});
}

// ── Notices, countdowns, turn clock ─────────────────────────────────────────

function clock(seconds) {
  const s = Math.max(0, Math.round(seconds));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

let countdownTimer = 0;
function onNotice(message) {
  if (!message.countdown) return toast(message.text, true);
  let left = message.countdown;
  $("countdown-text").textContent = message.text;
  $("countdown-number").textContent = left;
  show($("countdown"), true);
  clearInterval(countdownTimer);
  countdownTimer = setInterval(() => {
    left -= 1;
    $("countdown-number").textContent = left;
    if (left <= 0) { clearInterval(countdownTimer); show($("countdown"), false); }
  }, 1000);
}

// Any input during an AFK countdown hides it again (the server resets too).
function hideCountdownOnInput() {
  if (!$("countdown").classList.contains("hidden") && /input/i.test($("countdown-text").textContent)) {
    clearInterval(countdownTimer);
    show($("countdown"), false);
  }
}

let turnInfoAt = 0;
let turnInfoLeft = null;
function turnLeft() {
  return turnInfoLeft == null ? null : turnInfoLeft - (performance.now() - turnInfoAt) / 1000;
}
function renderTurnChip(s) {
  const turns = (s && s.turns) || {};
  turnInfoLeft = turns.inControl && turns.secondsLeft != null ? turns.secondsLeft : null;
  turnInfoAt = performance.now();
}
setInterval(() => {
  const left = turnLeft();
  show($("turn-chip"), left != null);
  if (left != null) $("turn-chip").textContent = `Turn ${clock(left)}${status && status.turns && status.turns.askedForTime ? " (paused)" : ""}`;
  if (recorder) $("rec-time").textContent = clock(recordSeconds());
  if (menuOpen) renderMenu();
}, 1000);

// ── Audio (per device) ──────────────────────────────────────────────────────

let volume = Number(localStorage.getItem("zp-volume") ?? 100);
let muted = localStorage.getItem("zp-muted") === "on";
function applyAudio() {
  const video = $("video");
  video.volume = Math.max(0, Math.min(1, volume / 100));
  video.muted = muted || video.dataset.autoplayMuted === "1";
  localStorage.setItem("zp-volume", String(volume));
  localStorage.setItem("zp-muted", muted ? "on" : "off");
}
function changeVolume(step) {
  volume = Math.max(0, Math.min(100, volume + step));
  if (step > 0) muted = false;
  applyAudio();
  renderMenu();
}
applyAudio();

// ── Recording (on this device; the stream is not touched) ───────────────────

let recorder = null;
let recordChunks = [];
let recordStarted = 0;
let recordType = "";
function recordSeconds() {
  return recorder ? (performance.now() - recordStarted) / 1000 : 0;
}
function startRecording() {
  const stream = $("video").srcObject;
  if (!stream || typeof MediaRecorder === "undefined") return toast("This browser cannot record.");
  const types = ["video/mp4;codecs=avc1.640028,mp4a.40.2", "video/mp4", "video/webm;codecs=vp9,opus", "video/webm"];
  recordType = types.find((t) => MediaRecorder.isTypeSupported(t)) || "";
  try {
    recorder = new MediaRecorder(stream, recordType ? { mimeType: recordType, videoBitsPerSecond: 16_000_000 } : undefined);
  } catch (error) {
    recorder = null;
    return toast(`Could not start recording: ${error.message}`);
  }
  recordChunks = [];
  recorder.ondataavailable = (event) => { if (event.data.size) recordChunks.push(event.data); };
  recorder.onstop = saveRecording;
  recorder.start(1000);
  recordStarted = performance.now();
  show($("rec-chip"), true);
  toast("Recording on this device.", true);
}
function stopRecording() {
  if (!recorder) return;
  const r = recorder;
  recorder = null;
  show($("rec-chip"), false);
  r.stop();
}
function saveRecording() {
  const type = recordType.split(";")[0] || "video/webm";
  const blob = new Blob(recordChunks, { type });
  recordChunks = [];
  const stamp = new Date().toISOString().slice(0, 19).replace(/[T:]/g, "-");
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = `ZeroPlay ${stamp}.${type.includes("mp4") ? "mp4" : "webm"}`;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 60_000);
  toast("Recording saved to this device.", true);
}

// ── App on the home screen ──────────────────────────────────────────────────

// Service workers need https (or the home network's http://localhost); on a
// plain http address the page still installs on iOS through the manifest.
if ("serviceWorker" in navigator && window.isSecureContext) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}
// iOS home-screen apps have a WebKit bug where controllers sometimes do not
// show up (most often after the app was closed with the controller still on).
// Tapping the page or turning the controller off and on brings it back, so
// say so when no controller has been seen.
const iosApp = navigator.standalone === true || (window.matchMedia("(display-mode: standalone)").matches && /iP(hone|ad|od)/.test(navigator.userAgent));
let padSeen = false;
let padHintClosed = false;
window.addEventListener("gamepadconnected", () => { padSeen = true; show($("pad-hint"), false); });
$("pad-hint-close").onclick = () => { padHintClosed = true; show($("pad-hint"), false); };
if (iosApp) {
  setInterval(() => {
    const pads = readPads();
    if (pads.length) padSeen = true;
    const onScreen = !$("home").classList.contains("hidden") || !$("stream").classList.contains("hidden");
    show($("pad-hint"), iosApp && !padSeen && !padHintClosed && onScreen && performance.now() > 4000);
  }, 1000);
}

// iPhones have no full-screen mode for pages; the home-screen app is full screen already.
show($("m-fullscreen"), !!document.fullscreenEnabled && !window.matchMedia("(display-mode: standalone)").matches);

// The island's clock.
function tick() {
  const now = new Date();
  $("clock").textContent = `${String(now.getHours()).padStart(2, "0")}:${String(now.getMinutes()).padStart(2, "0")}`;
}
tick();
setInterval(tick, 10000);

// Opened from an app (Korie's ZeroPlay add-on): "#session=…" hands over its sign-in, so
// nobody types a password with a remote; "?play=1&console=…" goes straight to the stream.
let autoPlay = false;
async function fromApp() {
  const hash = new URLSearchParams(location.hash.slice(1));
  const token = hash.get("session");
  if (token) {
    try {
      await fetch("/api/session", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ token }) });
    } catch {}
  }
  const query = new URLSearchParams(location.search);
  if (query.get("console")) selected = query.get("console");
  if (query.has("play")) autoPlay = true;
  if (token || query.has("play") || query.has("console")) history.replaceState(null, "", location.pathname);
}

fromApp().finally(checkAccess);

