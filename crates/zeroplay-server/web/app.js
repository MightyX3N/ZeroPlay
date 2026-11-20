"use strict";

// ── Small helpers ────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);
const show = (el, on) => el.classList.toggle("hidden", !on);
const screens = ["home", "login", "setpass", "notsetup", "signin", "waking", "stream"];
function showScreen(name) {
  for (const s of screens) show($(s), s === name);
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

function send(message) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(message));
}

// ── Getting in: the password ────────────────────────────────────────────────

async function checkAccess() {
  try {
    const response = await fetch("/api/access", { cache: "no-store" });
    const access = await response.json();
    if (access.verdict === "ok") return connect();
    showScreen(access.verdict === "loginNeeded" ? "login" : "notsetup");
    if (access.verdict === "loginNeeded") $("login-password").focus();
  } catch {
    setTimeout(checkAccess, 2000);
  }
}

async function postPassword(url, password) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ password }),
  });
  if (response.ok) return null;
  const body = await response.json().catch(() => ({}));
  return body.error || "That did not work.";
}

$("login-form").onsubmit = async (event) => {
  event.preventDefault();
  const error = await postPassword("/api/login", $("login-password").value);
  show($("login-error"), !!error);
  $("login-error").textContent = error || "";
  if (!error) { $("login-password").value = ""; checkAccess(); }
};

$("setpass-form").onsubmit = async (event) => {
  event.preventDefault();
  const password = $("setpass-password").value;
  let error = password !== $("setpass-again").value ? "The two passwords are not the same." : null;
  if (!error) error = await postPassword("/api/password", password);
  show($("setpass-error"), !!error);
  $("setpass-error").textContent = error || "";
  if (!error) { $("setpass-password").value = $("setpass-again").value = ""; }
};

function connect() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(`${proto}//${location.host}/ws`);
  ws.onopen = () => {
    const name = localStorage.getItem("zp-name");
    if (name) send({ t: "name", name });
  };
  ws.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.t === "status") onStatus(message);
    else if (message.t === "answer") onAnswer(message.sdp);
    else if (message.t === "rumble") onRumble(message);
    else if (message.t === "error") toast(message.message);
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
  const home = !!(s.you && s.you.home);
  show($("remote-off"), home && (p.state === "off" || p.state === "failed"));
  show($("remote-claiming"), p.state === "claiming");
  show($("remote-running"), p.state === "running");
  show($("remote-away"), !home);
  show($("btn-playit-disconnect"), home && p.state !== "off");
  show($("remote-address-box"), !!p.web);
  show($("remote-waiting"), p.state === "running" && !p.web);
  if (p.web) {
    $("remote-address").href = p.web;
    $("remote-address").textContent = p.web.replace(/^https?:\/\//, "");
  }
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
  const stream = s.stream;
  if (stream.state === "failed" && stream.error !== lastError) toast(stream.error);
  lastError = stream.state === "failed" ? stream.error : "";

  if (!s.signedIn) {
    stopWatching();
    renderSignin(s);
    showScreen("signin");
    return;
  }
  if (!s.passwordSet) {
    showScreen("setpass");
    return;
  }
  if (stream.state === "starting") {
    renderWaking(stream);
    if (!watching()) showScreen("waking");
    return;
  }
  if (stream.state === "live" && wantWatch) {
    if (!watching()) startWatching();
    renderMenu();
    showScreen("stream");
    return;
  }
  if (stream.state !== "live") {
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

  $("hero-label").textContent = liveHere ? "Streaming now" : "Your console";
  $("hero-title").textContent = current ? current.name : "No console found";
  $("hero-meta").textContent = current
    ? `${current.type} · ${powerText(current.power)} · Remote play ${current.remotePlay ? "on" : "OFF – turn on remote features on the console"}`
    : "Turn on remote features on your Xbox, then press refresh.";
  $("btn-play-label").textContent = liveHere ? "Join stream" : "Play";
  $("btn-play").disabled = !current || (live && !liveHere);
  show($("btn-wake"), !!current && current.power !== "On" && !liveHere);
  show($("btn-stop"), live);

  $("shelf-count").textContent = `${consoles.length} console${consoles.length === 1 ? "" : "s"}`;
  $("tiles").replaceChildren(...consoles.map((c) => {
    const tile = document.createElement("button");
    tile.className = "tile" + (c.id === selected ? " selected" : "");
    tile.setAttribute("aria-label", c.name);
    const label = document.createElement("span");
    label.className = "label";
    const name = document.createElement("b");
    name.textContent = c.name;
    label.append(name, `${powerText(c.power)}${s.stream.consoleId === c.id && live ? " · streaming" : ""}`);
    tile.append(label);
    tile.onclick = () => { selected = c.id; renderHome(status); };
    return tile;
  }));
  const chosen = document.querySelector(".tile.selected");
  if (chosen) chosen.scrollIntoView({ block: "nearest", inline: "nearest" });

  renderPlayers(s);
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
$("btn-signout").onclick = () => { if (confirmSignOut()) send({ t: "logout" }); };
$("btn-refresh").onclick = () => send({ t: "refresh" });
$("btn-wake").onclick = () => { if (selected) { send({ t: "wake", id: selected }); toast("Asked the console to wake up.", true); } };
$("btn-stop").onclick = () => send({ t: "stop" });
$("btn-cancel").onclick = () => send({ t: "stop" });
$("btn-play").onclick = play;

function confirmSignOut() {
  return window.confirm("Sign this server out of your Microsoft account?");
}

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

function watching() { return pc !== null; }

async function startWatching() {
  pc = new RTCPeerConnection();
  const mine = pc;
  const media = new MediaStream();
  pc.ontrack = (event) => {
    media.addTrack(event.track);
    const video = $("video");
    if (video.srcObject !== media) video.srcObject = media;
    video.play().catch(() => {
      video.muted = true;
      video.play().catch(() => {});
      show($("tap-to-play"), true);
    });
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

function stopWatching() {
  if (pc) {
    pc.close();
    pc = null;
    send({ t: "leave" });
  }
  $("video").srcObject = null;
  closeMenu();
  unplugAll();
}

$("btn-unmute").onclick = () => {
  const video = $("video");
  video.muted = false;
  video.play().catch(() => {});
  show($("tap-to-play"), false);
};

// Ask for a fresh picture if the video stops moving, and keep the stats up to date.
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
    const parts = [];
    if (r.frameHeight) parts.push(`${r.frameHeight}p`);
    if (r.framesPerSecond) parts.push(`${Math.round(r.framesPerSecond)} fps`);
    parts.push(`${mbps.toFixed(1)} Mbps`);
    $("stats").replaceChildren(...parts.map((p) => { const s = document.createElement("span"); s.textContent = p; return s; }));
  });
}, 1000);

// ── Menu (F1, or hold View + Menu) ──────────────────────────────────────────

let menuOpen = false;
let menuIndex = 0;
// Saved per device (this browser). With nothing saved: on for computers with a
// mouse, off for phones and tablets.
const hasMouse = window.matchMedia("(pointer: fine)").matches;
const savedKeyboard = localStorage.getItem("zp-keyboard");
let keyboardOn = savedKeyboard ? savedKeyboard === "on" : hasMouse;
const SENSITIVITIES = [0.25, 0.5, 0.75, 1, 1.5, 2, 3, 4];
let mouseSens = Number(localStorage.getItem("zp-mouse-sens")) || 1;
let statsOn = localStorage.getItem("zp-stats") === "on";
const menuItems = () => [...document.querySelectorAll("#menu .menu-item")];

function renderMenu() {
  if (!status) return;
  $("menu-title").textContent = status.stream.consoleName || "Xbox";
  const n = status.players.length;
  $("menu-players").textContent = `${n} playing · ${status.watching.length} watching`;
  $("m-keyboard").textContent = `Keyboard and mouse: ${keyboardOn ? "on" : "off"}`;
  $("m-sens").textContent = `Mouse sensitivity: ${mouseSens}`;
  show($("m-sens"), hasMouse && keyboardOn);
  $("m-players").textContent = separatePlayers ? "Controllers here: separate players" : "Controllers here: one player";
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
  show($("menu"), false);
}

$("m-resume").onclick = closeMenu;
$("m-keyboard").onclick = () => {
  keyboardOn = !keyboardOn;
  localStorage.setItem("zp-keyboard", keyboardOn ? "on" : "off");
  if (!keyboardOn) {
    if (separatePlayers) {
      send({ t: "unplug", local: KEYBOARD_LOCAL });
      sent.delete(KEYBOARD_LOCAL);
    }
    if (document.pointerLockElement) document.exitPointerLock();
  }
  renderMenu();
};
$("m-players").onclick = () => {
  separatePlayers = !separatePlayers;
  localStorage.setItem("zp-separate", separatePlayers ? "on" : "off");
  unplugAll();
  mergedWasActive = false;
  renderMenu();
};
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

function padState(gp) {
  let buttons = 0;
  const standard = gp.mapping === "standard";
  gp.buttons.forEach((b, i) => {
    if (b.pressed && standard && STANDARD[i]) buttons |= STANDARD[i];
  });
  const axis = (i) => {
    const v = gp.axes[i] || 0;
    return Math.abs(v) < 0.08 ? 0 : v;
  };
  return {
    buttons,
    lx: axis(0), ly: -axis(1), rx: axis(2), ry: -axis(3),
    lt: gp.buttons[6] ? gp.buttons[6].value : 0,
    rt: gp.buttons[7] ? gp.buttons[7].value : 0,
  };
}

function sendPad(local, state) {
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
  if (menuOpen) {
    const items = menuItems().filter((el) => !el.classList.contains("hidden"));
    if (pressed(local, bits, BTN.down)) menuIndex = (menuIndex + 1) % items.length;
    if (pressed(local, bits, BTN.up)) menuIndex = (menuIndex + items.length - 1) % items.length;
    if (pressed(local, bits, BTN.a)) items[menuIndex].click();
    if (pressed(local, bits, BTN.b)) closeMenu();
    if (menuOpen) renderMenu();
  } else if (onHome && status) {
    const ids = (status.consoles || []).map((c) => c.id);
    const i = ids.indexOf(selected);
    if (pressed(local, bits, BTN.right) && i < ids.length - 1) { selected = ids[i + 1]; renderHome(status); }
    if (pressed(local, bits, BTN.left) && i > 0) { selected = ids[i - 1]; renderHome(status); }
    if (pressed(local, bits, BTN.a)) play();
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
  const inStream = pc !== null && !$("stream").classList.contains("hidden");
  const playing = inStream && !menuOpen;
  const states = new Map(); // local index → state, for this tick
  let combo = false;
  for (const gp of navigator.getGamepads ? navigator.getGamepads() : []) {
    if (!gp || !gp.connected) continue;
    const state = padState(gp);
    if ((state.buttons & BTN.view) && (state.buttons & BTN.menu)) combo = true;
    if (playing) states.set(gp.index, state);
    else navigate(state.buttons, gp.index);
    prevButtons.set(gp.index, state.buttons);
  }
  updateMouseStick();
  const kbActive = playing && keyboardOn && (keysDown.size > 0 || mouseLocked());
  if (kbActive) states.set(KEYBOARD_LOCAL, keyboardState());

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
  show($("mouse-hint"), playing && keyboardOn && hasMouse && !mouseLocked());
}
setInterval(pollPads, 8);

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

$("video").addEventListener("click", () => {
  if (keyboardOn && !menuOpen && !mouseLocked()) {
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
  if (event.code === "F1" || event.code === "Escape") {
    event.preventDefault();
    if (menuOpen) closeMenu(); else openMenu();
    return;
  }
  if (menuOpen || !keyboardOn) return;
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
  const gp = (navigator.getGamepads ? navigator.getGamepads() : [])[r.local];
  if (!gp || !gp.vibrationActuator || !r.ms) return;
  gp.vibrationActuator.playEffect("dual-rumble", {
    duration: r.ms,
    strongMagnitude: Math.min(1, r.left),
    weakMagnitude: Math.min(1, r.right),
    leftTrigger: Math.min(1, r.leftTrigger),
    rightTrigger: Math.min(1, r.rightTrigger),
  }).catch(() => {});
}

checkAccess();
