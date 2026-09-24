import init, { Session } from "./pkg/aprsmsg.js";

const $ = (sel) => document.querySelector(sel);
const form = $("#connect");
const compose = $("#compose");
const logEl = $("#log");
const hint = $("#hint");
const connectBtn = $("#connect-btn");
const disconnectBtn = $("#disconnect-btn");

let session = null;
let socket = null;
let pollTimer = null;
let keepaliveTimer = null;

const DEFAULTS = {
  "aprs-is": "wss://ametx.com:8888",
  kiss: "ws://127.0.0.1:8765",
};

function b64ToBytes(b64) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function stamp() {
  return new Date().toISOString().slice(11, 19) + "Z";
}

function log(kind, text) {
  const line = document.createElement("div");
  line.className = kind || "info";
  line.textContent = `${stamp()} ${text}`;
  logEl.appendChild(line);
  logEl.scrollTop = logEl.scrollHeight;
}

function applyActions(actions) {
  for (const a of actions) {
    if (a.type === "send" && socket?.readyState === WebSocket.OPEN) {
      socket.send(b64ToBytes(a.data));
    } else if (a.type === "log") {
      log(a.kind, a.text);
    } else if (a.type === "quit") {
      disconnect();
    }
  }
}

function setModeUi() {
  const mode = form.mode.value;
  form.url.value = DEFAULTS[mode];
  document.querySelectorAll(".kiss-only").forEach((el) => {
    el.classList.toggle("hidden", mode !== "kiss");
  });
  document.querySelectorAll(".is-only").forEach((el) => {
    el.classList.toggle("hidden", mode !== "aprs-is");
  });
  hint.textContent =
    mode === "kiss"
      ? "Run locally: cargo run --bin aprsmsg-bridge  (Direwolf KISS on 127.0.0.1:8001)"
      : "Uses APRS-IS WebSocket (WSS). Browsers cannot open plain TCP :14580.";
}

function disconnect() {
  if (pollTimer) clearInterval(pollTimer);
  if (keepaliveTimer) clearInterval(keepaliveTimer);
  pollTimer = keepaliveTimer = null;
  if (socket) {
    socket.onclose = null;
    socket.close();
    socket = null;
  }
  session = null;
  compose.hidden = true;
  connectBtn.disabled = false;
  disconnectBtn.disabled = true;
  form.querySelectorAll("input, select").forEach((el) => {
    el.disabled = false;
  });
}

form.mode.addEventListener("change", setModeUi);
setModeUi();

form.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  disconnect();

  const call = form.call.value.trim();
  const mode = form.mode.value;
  const url = form.url.value.trim();
  const path = form.path.value.trim() || "none";
  const chan = Number(form.chan.value) || 0;
  const filter = form.filter.value.trim();
  const monitor = form.monitor.checked;

  try {
    session = new Session(call, mode, path, "APZRST", chan, undefined, filter, monitor);
  } catch (e) {
    log("error", String(e));
    return;
  }

  connectBtn.disabled = true;
  form.querySelectorAll("input, select").forEach((el) => {
    el.disabled = true;
  });

  socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";

  socket.onopen = () => {
    log("info", `${call} connected to ${url}`);
    if (mode === "aprs-is") {
      socket.send(session.loginBytes());
      keepaliveTimer = setInterval(() => {
        if (socket?.readyState === WebSocket.OPEN) {
          socket.send(session.keepaliveBytes());
        }
      }, 280_000);
    }
    compose.hidden = false;
    disconnectBtn.disabled = false;
    pollTimer = setInterval(() => {
      if (session) applyActions(session.poll());
    }, 1000);
  };

  socket.onmessage = (ev) => {
    const bytes =
      typeof ev.data === "string"
        ? new TextEncoder().encode(ev.data)
        : new Uint8Array(ev.data);
    applyActions(session.onBytes(bytes));
  };

  socket.onerror = () => log("error", "WebSocket error");
  socket.onclose = () => {
    log("info", "disconnected");
    disconnect();
  };
});

disconnectBtn.addEventListener("click", () => {
  log("info", "disconnecting");
  disconnect();
});

compose.addEventListener("submit", (ev) => {
  ev.preventDefault();
  if (!session) return;
  const to = compose.to.value.trim();
  const text = compose.text.value.trim();
  applyActions(session.onCommand(`msg ${to} ${text}`));
  compose.text.value = "";
  compose.text.focus();
});

await init();
