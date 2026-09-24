import init, { Session } from "./pkg/aprsmsg.js?v=5";

const $ = (sel) => document.querySelector(sel);
const form = $("#connect");
const compose = $("#compose");
const logEl = $("#log");
const hint = $("#hint");
const statusEl = $("#status");
const connectBtn = $("#connect-btn");
const disconnectBtn = $("#disconnect-btn");

const RELEASE = "https://github.com/mashu/aprsmsg/releases/latest/download";
const RELEASE_PAGE = "https://github.com/mashu/aprsmsg/releases/latest";

let session = null;
let socket = null;
let pollTimer = null;
let keepaliveTimer = null;
let linkMode = null;

const DEFAULTS = {
  "aprs-is": "wss://ametx.com:8888",
  kiss: "ws://127.0.0.1:8765",
};

function detectPlatform() {
  const ua = navigator.userAgent || "";
  const platform = navigator.platform || "";
  const arch =
    navigator.userAgentData?.architecture ||
    (/arm|aarch64/i.test(ua) ? "arm" : "x86");

  if (/Win/i.test(platform) || /Windows/i.test(ua)) {
    return {
      label: "Windows x86_64",
      bridge: "aprsmsg-bridge-windows-x86_64.exe",
      cli: "aprsmsg-windows-x86_64.exe",
    };
  }
  if (/Mac/i.test(platform) || /Mac OS/i.test(ua)) {
    // Apple Silicon is the common case on current macOS; Intel link stays on “All platforms”.
    const appleSilicon = arch === "arm" || /Macintosh/.test(ua);
    return {
      label: appleSilicon ? "macOS aarch64" : "macOS x86_64",
      bridge: appleSilicon
        ? "aprsmsg-bridge-macos-aarch64"
        : "aprsmsg-bridge-macos-x86_64",
      cli: appleSilicon ? "aprsmsg-macos-aarch64" : "aprsmsg-macos-x86_64",
    };
  }
  return {
    label: "Linux x86_64",
    bridge: "aprsmsg-bridge-linux-x86_64",
    cli: "aprsmsg-linux-x86_64",
  };
}

function setupDownloads() {
  const p = detectPlatform();
  const bridge = $("#dl-bridge");
  const cli = $("#dl-cli");
  const all = $("#dl-all");
  const detect = $("#dl-detect");
  if (!bridge) return;
  bridge.href = `${RELEASE}/${p.bridge}`;
  bridge.textContent = `Download bridge (${p.label})`;
  cli.href = `${RELEASE}/${p.cli}`;
  all.href = RELEASE_PAGE;
  detect.textContent = `Detected ${p.label}`;
}

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
      const bytes = b64ToBytes(a.data);
      if (linkMode === "aprs-is") {
        socket.send(new TextDecoder().decode(bytes));
      } else {
        socket.send(bytes);
      }
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
      ? "download the bridge below, then Connect"
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
  linkMode = null;
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
  setupDownloads();

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
    const raw = form.raw.checked;

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
      () => new Session(call, mode, path, "APZRST", chan, undefined, filter, monitor, raw)
    );
    if (!session) return;
    linkMode = mode;

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
        if (login) {
          // Text frame is accepted by javAPRSSrvr and avoids binary encoding quirks.
          socket.send(new TextDecoder().decode(login));
        }
        keepaliveTimer = setInterval(() => {
          if (socket?.readyState === WebSocket.OPEN) {
            const ka = runWasm(() => session.keepaliveBytes());
            if (ka) socket.send(new TextDecoder().decode(ka));
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
      let bytes;
      if (typeof ev.data === "string") {
        bytes = new TextEncoder().encode(ev.data);
      } else if (ev.data instanceof Blob) {
        ev.data.arrayBuffer().then((buf) => {
          applyActions(runWasm(() => session.onBytes(new Uint8Array(buf))));
        });
        return;
      } else {
        bytes = new Uint8Array(ev.data);
      }
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

setupDownloads();
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
