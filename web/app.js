import init, { Session } from "./pkg/aprsmsg.js?v=2";

const $ = (sel) => document.querySelector(sel);
const form = $("#connect");
const compose = $("#compose");
const logEl = $("#log");
const hint = $("#hint");
const statusEl = $("#status");
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

function setStatus(state, text) {
  statusEl.dataset.state = state;
  statusEl.textContent = text;
}

function log(kind, text) {
  const line = document.createElement("div");
  line.className = kind || "info";
  line.textContent = `${stamp()} ${text}`;
  logEl.appendChild(line);
  logEl.scrollTop = logEl.scrollHeight;
}

function runWasm(fn) {
  try {
    return fn();
  } catch (e) {
    console.error(e);
    log("error", e?.message || String(e));
    setStatus("error", "error");
    return null;
  }
}

function applyActions(actions) {
  if (!actions) return;
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
      ? "bridge: cargo run --bin aprsmsg-bridge"
      : "APRS-IS via wss://ametx.com:8888 — no bridge";
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
  connectBtn.textContent = "Connect";
  disconnectBtn.disabled = true;
  form.querySelectorAll("input, select").forEach((el) => {
    el.disabled = false;
  });
  setStatus("idle", "idle");
}

function wireUi() {
  form.mode.addEventListener("change", setModeUi);
  setModeUi();

  form.addEventListener("submit", (ev) => {
    ev.preventDefault();
    disconnect();

    const call = form.call.value.trim();
    const mode = form.mode.value;
    let url = form.url.value.trim();
    const path = form.path.value.trim() || "none";
    const chan = Number(form.chan.value) || 0;
    const filter = form.filter.value.trim();
    const monitor = form.monitor.checked;

    if (mode === "aprs-is" && /127\.0\.0\.1|localhost/i.test(url)) {
      log(
        "error",
        "APRS-IS needs wss://ametx.com:8888 — localhost is only for Direwolf."
      );
      url = DEFAULTS["aprs-is"];
      form.url.value = url;
    }
    if (mode === "aprs-is" && url.startsWith("ws://")) {
      log("error", "Use wss:// on HTTPS pages (not ws://).");
      return;
    }

    session = runWasm(
      () => new Session(call, mode, path, "APZRST", chan, undefined, filter, monitor)
    );
    if (!session) return;

    connectBtn.disabled = true;
    connectBtn.textContent = "Connecting…";
    form.querySelectorAll("input, select").forEach((el) => {
      el.disabled = true;
    });
    setStatus("idle", "connecting");

    socket = new WebSocket(url);
    socket.binaryType = "arraybuffer";

    socket.onopen = () => {
      log("info", `${call} ↔ ${url}`);
      if (mode === "aprs-is") {
        const login = runWasm(() => session.loginBytes());
        if (login) socket.send(login);
        keepaliveTimer = setInterval(() => {
          if (socket?.readyState === WebSocket.OPEN) {
            const ka = runWasm(() => session.keepaliveBytes());
            if (ka) socket.send(ka);
          }
        }, 280_000);
      }
      compose.hidden = false;
      disconnectBtn.disabled = false;
      connectBtn.textContent = "Connected";
      setStatus("live", "on air");
      pollTimer = setInterval(() => {
        if (session) applyActions(runWasm(() => session.poll()));
      }, 1000);
    };

    socket.onmessage = (ev) => {
      const bytes =
        typeof ev.data === "string"
          ? new TextEncoder().encode(ev.data)
          : new Uint8Array(ev.data);
      applyActions(runWasm(() => session.onBytes(bytes)));
    };

    socket.onerror = () => {
      log("error", `WebSocket error: ${url}`);
      setStatus("error", "socket error");
    };
    socket.onclose = () => {
      log("info", "link closed");
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
    applyActions(runWasm(() => session.onCommand(`msg ${to} ${text}`)));
    compose.text.value = "";
    compose.text.focus();
  });
}

connectBtn.disabled = true;
try {
  await init();
  wireUi();
  connectBtn.disabled = false;
  connectBtn.textContent = "Connect";
  setStatus("idle", "ready");
} catch (e) {
  console.error(e);
  connectBtn.textContent = "Init failed";
  setStatus("error", "wasm failed");
  log("error", `WASM init failed: ${e?.message || e}`);
}
