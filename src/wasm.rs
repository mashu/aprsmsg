//! WebAssembly bindings: protocol + client state machine for the browser UI.
//!
//! Networking stays in JavaScript (WebSocket). This module parses KISS or
//! APRS-IS frames, runs retries/acks, and returns JSON actions to apply.

use serde::Serialize;
use wasm_bindgen::prelude::*;

use crate::ax25::{Address, UiFrame};
use crate::client::{Action, Client, ClientConfig, UiMsg, message_group_filter};
use crate::heard::{passcode, Heard};
use crate::kiss::{self, Decoder};

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JsAction {
    Send {
        /// Base64-encoded bytes to write on the WebSocket.
        data: String,
    },
    Log {
        kind: String,
        text: String,
    },
    Quit,
}

#[wasm_bindgen]
pub struct Session {
    client: Client,
    mode: Mode,
    kiss: Decoder,
    chan: u8,
    passcode: u16,
    filter: String,
    line_buf: Vec<u8>,
}

enum Mode {
    Kiss,
    AprsIs,
}

#[wasm_bindgen]
impl Session {
    /// `mode` is `"kiss"` (Direwolf via local bridge) or `"aprs-is"`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        call: &str,
        mode: &str,
        path: &str,
        tocall: &str,
        chan: u8,
        passcode_opt: Option<u16>,
        filter: &str,
        monitor: bool,
        raw: bool,
    ) -> Result<Session, JsValue> {
        let call = Address::parse(call).map_err(|e| JsValue::from_str(&e))?;
        let tocall = Address::parse(tocall).map_err(|e| JsValue::from_str(&e))?;
        let path = if path.trim().is_empty() || path.eq_ignore_ascii_case("none") {
            Vec::new()
        } else {
            path.split(',')
                .map(Address::parse)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| JsValue::from_str(&e))?
        };
        let mode = match mode {
            "kiss" => Mode::Kiss,
            "aprs-is" => Mode::AprsIs,
            other => {
                return Err(JsValue::from_str(&format!(
                    "mode must be kiss or aprs-is, not {other:?}"
                )))
            }
        };
        let radio = matches!(mode, Mode::Kiss);
        let pass = passcode_opt.unwrap_or_else(|| passcode(&call.call));
        let filter = message_group_filter(&call, filter);
        let client = Client::new(ClientConfig {
            call,
            path,
            tocall,
            monitor,
            raw,
            radio,
        });
        Ok(Session {
            client,
            mode,
            kiss: Decoder::default(),
            chan,
            passcode: pass,
            filter,
            line_buf: Vec::new(),
        })
    }

    /// APRS-IS login line to send right after the WebSocket opens.
    #[wasm_bindgen(js_name = loginBytes)]
    pub fn login_bytes(&self) -> Vec<u8> {
        format!(
            "user {} pass {} vers aprsmsg {} filter {}\r\n",
            self.client.call(),
            self.passcode,
            env!("CARGO_PKG_VERSION"),
            self.filter
        )
        .into_bytes()
    }

    /// Computed or configured APRS-IS passcode.
    pub fn passcode(&self) -> u16 {
        self.passcode
    }

    /// Feed raw WebSocket payload (binary or text, as bytes).
    #[wasm_bindgen(js_name = onBytes)]
    pub fn on_bytes(&mut self, data: &[u8]) -> Result<JsValue, JsValue> {
        let mut actions = Vec::new();
        match self.mode {
            Mode::Kiss => {
                for frame in self.kiss.push(data) {
                    if frame.channel != self.chan {
                        continue;
                    }
                    if let Some(ui) = UiFrame::decode(&frame.payload) {
                        actions.extend(self.client.on_heard(Heard::from_frame(ui)));
                    }
                }
            }
            Mode::AprsIs => {
                actions.extend(self.feed_aprs_is(data));
            }
        }
        to_js(self.encode_actions(actions))
    }

    #[wasm_bindgen(js_name = onCommand)]
    pub fn on_command(&mut self, line: &str) -> Result<JsValue, JsValue> {
        let actions = self.client.on_command(line);
        to_js(self.encode_actions(actions))
    }

    /// Run retry timers; call about once a second while connected.
    pub fn poll(&mut self) -> Result<JsValue, JsValue> {
        let actions = self.client.poll();
        to_js(self.encode_actions(actions))
    }

    /// APRS-IS keepalive comment bytes.
    #[wasm_bindgen(js_name = keepaliveBytes)]
    pub fn keepalive_bytes(&self) -> Vec<u8> {
        b"# aprsmsg keepalive\r\n".to_vec()
    }

    pub fn pending(&self) -> usize {
        self.client.pending_count()
    }
}

#[wasm_bindgen(js_name = computePasscode)]
pub fn compute_passcode(call: &str) -> u16 {
    passcode(call)
}

impl Session {
    fn feed_aprs_is(&mut self, data: &[u8]) -> Vec<Action> {
        self.line_buf.extend_from_slice(data);
        let mut actions = Vec::new();
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.line_buf.drain(..=pos).collect();
            trim_line_end(&mut line);
            actions.extend(self.handle_aprs_is_line(&line));
        }
        // javAPRSSrvr WebSocket usually sends one complete line per binary
        // frame, often without a trailing newline — flush the remainder.
        if !self.line_buf.is_empty() {
            let mut line = std::mem::take(&mut self.line_buf);
            trim_line_end(&mut line);
            actions.extend(self.handle_aprs_is_line(&line));
        }
        actions
    }

    fn handle_aprs_is_line(&mut self, line: &[u8]) -> Vec<Action> {
        if line.is_empty() {
            return Vec::new();
        }
        let text = String::from_utf8_lossy(line);
        let mut actions = self.client.on_wire(text.as_ref());
        if line.starts_with(b"#") {
            let comment = text[1..].trim().to_owned();
            if !comment.is_empty() {
                actions.extend(self.client.on_notice(&comment));
            }
            return actions;
        }
        if let Some(heard) = Heard::parse_tnc2(line) {
            actions.extend(self.client.on_heard(heard));
        }
        actions
    }

    fn encode_actions(&self, actions: Vec<Action>) -> Vec<JsAction> {
        let mut out = Vec::with_capacity(actions.len());
        for action in actions {
            match action {
                Action::Send(info) => {
                    let data = match self.mode {
                        Mode::Kiss => {
                            let frame = UiFrame {
                                dest: self.client.tocall().clone(),
                                src: self.client.call().clone(),
                                digis: self
                                    .client
                                    .path()
                                    .iter()
                                    .map(|d| (d.clone(), false))
                                    .collect(),
                                info,
                            };
                            kiss::encode(self.chan, &frame.encode())
                        }
                        Mode::AprsIs => {
                            let mut line =
                                format!("{}>{},TCPIP*:", self.client.call(), self.client.tocall())
                                    .into_bytes();
                            line.extend_from_slice(&info);
                            line.extend_from_slice(b"\r\n");
                            line
                        }
                    };
                    out.push(JsAction::Send {
                        data: base64_encode(&data),
                    });
                }
                Action::Ui(msg) => out.push(JsAction::Log {
                    kind: ui_kind(&msg).into(),
                    text: format_ui(&msg),
                }),
                Action::Quit => out.push(JsAction::Quit),
            }
        }
        out
    }
}

fn ui_kind(msg: &UiMsg) -> &'static str {
    match msg {
        UiMsg::Info(_) | UiMsg::Print(_) => "info",
        UiMsg::Error(_) => "error",
        UiMsg::Sent { .. } | UiMsg::Retry { .. } => "sent",
        UiMsg::Repeated { .. } => "repeat",
        UiMsg::Delivered { .. } => "ok",
        UiMsg::Rejected { .. } | UiMsg::GaveUp { .. } | UiMsg::Cancelled { .. } => "fail",
        UiMsg::Incoming { .. } => "in",
        UiMsg::Monitor { .. } | UiMsg::Raw(_) => "mon",
    }
}

fn format_ui(msg: &UiMsg) -> String {
    match msg {
        UiMsg::Info(t) | UiMsg::Error(t) | UiMsg::Print(t) | UiMsg::Raw(t) => t.clone(),
        UiMsg::Sent {
            to,
            id,
            text,
            attempt,
            max,
        } => format!("→ {to} #{id} {text}  try {attempt}/{max}"),
        UiMsg::Retry {
            to,
            id,
            attempt,
            max,
        } => format!("→ {to} #{id}  retry {attempt}/{max}"),
        UiMsg::Repeated { digi, id } => format!("↻ {digi} repeated your #{id}"),
        UiMsg::Delivered { from, id, tries } => {
            let word = if *tries == 1 { "try" } else { "tries" };
            format!("✓ {from} #{id} delivered after {tries} {word}")
        }
        UiMsg::Rejected { from, id } => format!("✗ {from} #{id} rejected"),
        UiMsg::GaveUp { to, id, tries } => {
            format!("✗ {to} #{id} not delivered (no ack after {tries} tries)")
        }
        UiMsg::Cancelled { to, id } => format!("✗ {to} #{id} cancelled"),
        UiMsg::Incoming {
            from,
            text,
            id,
            route,
        } => match id {
            Some(id) => format!("← {from} {text}  #{id} {route}"),
            None => format!("← {from} {text}  {route}"),
        },
        UiMsg::Monitor {
            station,
            summary,
            route,
        } => format!("· {station} {summary}  {route}"),
    }
}

fn to_js(actions: Vec<JsAction>) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(&actions).map_err(|e| JsValue::from_str(&e.to_string()))
}

fn trim_line_end(line: &mut Vec<u8>) {
    while matches!(line.last(), Some(b'\r' | b'\n')) {
        line.pop();
    }
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
