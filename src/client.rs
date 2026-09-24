//! Event-driven APRS messaging client shared by the CLI and the web UI.
//!
//! The client never touches the network: callers feed events and apply the
//! returned [`Action`]s (transmit an info field, show UI, quit).

use std::collections::HashMap;
use std::time::Duration;

use crate::aprs::{self, Message};
use crate::ax25::Address;
use crate::clock;
use crate::decode::{self, Packet};
use crate::heard::Heard;

/// First retry after 30 s, then doubling up to 4 minutes.
pub const RETRY_BASE: Duration = Duration::from_secs(30);
pub const MAX_RETRY_SHIFT: u32 = 3;
pub const MAX_TRIES: u32 = 5;
/// Digipeated copies of one message arrive within seconds; ack them once.
pub const ACK_HOLDOFF: Duration = Duration::from_secs(10);
pub const SEEN_TTL: Duration = Duration::from_secs(30 * 60);

pub const COMMANDS: &str = "\
commands:
  msg CALL text   send a numbered message, retried until acknowledged
                  e.g.  msg EMAIL-2 friend@example.com Hello from the FTX-1
  pending         list messages still waiting for an ack
  cancel ID|all   stop retrying a message, e.g.  cancel 784
  mon             toggle decoded display of other stations' packets
  raw             toggle raw packet lines under each event (debugging)
  digis           list digipeaters heard repeating packets (radio only)
  quit            exit (Ctrl-D exits once pending messages are settled)

markers:  → sent   ↻ repeated by a digipeater   ✓ delivered   ✗ failed
          ← message for you   · other traffic (mon)";

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub call: Address,
    pub path: Vec<Address>,
    pub tocall: Address,
    pub monitor: bool,
    /// True when the link is Direwolf/KISS (digipeater tracking makes sense).
    pub radio: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiMsg {
    Info(String),
    Error(String),
    Sent {
        to: String,
        id: String,
        text: String,
        attempt: u32,
        max: u32,
    },
    Retry {
        to: String,
        id: String,
        attempt: u32,
        max: u32,
    },
    Repeated {
        digi: String,
        id: String,
    },
    Delivered {
        from: String,
        id: String,
        tries: u32,
    },
    Rejected {
        from: String,
        id: String,
    },
    GaveUp {
        to: String,
        id: String,
        tries: u32,
    },
    Cancelled {
        to: String,
        id: String,
    },
    Incoming {
        from: String,
        text: String,
        id: Option<String>,
        route: String,
    },
    Monitor {
        station: String,
        summary: String,
        route: String,
    },
    Raw(String),
    Print(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Transmit this APRS information field on the current link.
    Send(Vec<u8>),
    Ui(UiMsg),
    Quit,
}

struct Outgoing {
    to: String,
    id: String,
    info: Vec<u8>,
    tries: u32,
    next_at: Duration,
    heard_via: Vec<String>,
}

struct Seen {
    first: Duration,
    last_ack: Option<Duration>,
}

struct DigiHeard {
    packets: u32,
    last: Duration,
}

pub struct Client {
    cfg: ClientConfig,
    mycall: String,
    next_id: u32,
    pending: Vec<Outgoing>,
    seen: HashMap<(String, String), Seen>,
    digis: HashMap<String, DigiHeard>,
    monitor: bool,
    raw: bool,
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Client {
        let next_id = (unix_seconds() % 1000) as u32 + 1;
        Client {
            mycall: cfg.call.to_string(),
            monitor: cfg.monitor,
            raw: false,
            cfg,
            next_id,
            pending: Vec::new(),
            seen: HashMap::new(),
            digis: HashMap::new(),
        }
    }

    pub fn call(&self) -> &Address {
        &self.cfg.call
    }

    pub fn tocall(&self) -> &Address {
        &self.cfg.tocall
    }

    pub fn path(&self) -> &[Address] {
        &self.cfg.path
    }

    pub fn is_radio(&self) -> bool {
        self.cfg.radio
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn next_deadline(&self) -> Option<Duration> {
        self.pending.iter().map(|m| m.next_at).min()
    }

    /// How long until [`Self::poll`] should run again.
    pub fn time_until_deadline(&self) -> Option<Duration> {
        let deadline = self.next_deadline()?;
        Some(deadline.saturating_sub(now()))
    }

    pub fn on_heard(&mut self, heard: Heard) -> Vec<Action> {
        let mut out = Vec::new();
        if self.raw {
            out.push(Action::Ui(UiMsg::Raw(format!(
                "{}:{}",
                heard.header(),
                decode::printable(&heard.info)
            ))));
        }
        if !heard.from_internet {
            if let Some(digi) = heard.repeated_by() {
                let now = now();
                let entry = self.digis.entry(digi).or_insert(DigiHeard {
                    packets: 0,
                    last: now,
                });
                entry.packets += 1;
                entry.last = now;
            }
        }

        let (source, info) = aprs::unwrap_third_party(&heard.source, &heard.info);
        if source.eq_ignore_ascii_case(&self.mycall) {
            out.extend(self.on_own_echo(&heard, &info));
            return out;
        }
        let route = heard.route();

        match aprs::parse_message(&info) {
            Some(Message::Ack { to, id }) if self.is_me(&to) => {
                out.extend(self.on_ack(&source, &id));
            }
            Some(Message::Rej { to, id }) if self.is_me(&to) => {
                out.extend(self.on_rej(&source, &id));
            }
            Some(Message::Text {
                to,
                text,
                id,
                reply_ack,
            }) if self.is_me(&to) => {
                if let Some(acked_id) = reply_ack {
                    out.extend(self.on_ack(&source, &acked_id));
                }
                out.extend(self.on_text(&source, &text, id.as_deref(), &route));
            }
            _ if self.monitor => {
                let mut packet = decode::decode(heard.dest_call(), &heard.info);
                while let Packet::ThirdParty { inner, .. } = packet {
                    packet = *inner;
                }
                out.push(Action::Ui(UiMsg::Monitor {
                    station: source,
                    summary: packet.to_string(),
                    route,
                }));
            }
            _ => {}
        }
        out
    }

    pub fn on_notice(&mut self, notice: &str) -> Vec<Action> {
        if notice.contains("unverified") {
            vec![Action::Ui(UiMsg::Error(format!(
                "{notice} — wrong passcode? packets you send will be dropped"
            )))]
        } else {
            vec![Action::Ui(UiMsg::Info(notice.to_owned()))]
        }
    }

    /// Returns false in the last action sense via [`Action::Quit`].
    pub fn on_command(&mut self, line: &str) -> Vec<Action> {
        let line = line.trim();
        let (command, rest) = match line.split_once(char::is_whitespace) {
            Some((command, rest)) => (command, rest.trim()),
            None => (line, ""),
        };
        match command.to_ascii_lowercase().as_str() {
            "" => Vec::new(),
            "m" | "msg" => match rest.split_once(char::is_whitespace) {
                Some((to, text)) if !text.trim().is_empty() => self.send_message(to, text.trim()),
                _ => vec![Action::Ui(UiMsg::Print("usage: msg CALL text".into()))],
            },
            "p" | "pending" => vec![Action::Ui(UiMsg::Print(self.pending_text()))],
            "cancel" => self.cancel(rest),
            "d" | "digis" => vec![Action::Ui(UiMsg::Print(self.digis_text()))],
            "mon" => {
                self.monitor = !self.monitor;
                vec![Action::Ui(UiMsg::Info(format!(
                    "monitor {}",
                    on_off(self.monitor)
                )))]
            }
            "raw" => {
                self.raw = !self.raw;
                vec![Action::Ui(UiMsg::Info(format!(
                    "raw packets {}",
                    on_off(self.raw)
                )))]
            }
            "h" | "help" | "?" => vec![Action::Ui(UiMsg::Print(COMMANDS.to_owned()))],
            "q" | "quit" | "exit" => vec![Action::Quit],
            other => vec![Action::Ui(UiMsg::Print(format!(
                "unknown command {other:?}; type help"
            )))],
        }
    }

    pub fn poll(&mut self) -> Vec<Action> {
        let now = now();
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].next_at > now {
                i += 1;
                continue;
            }
            if self.pending[i].tries >= MAX_TRIES {
                let gave_up = self.pending.remove(i);
                out.push(Action::Ui(UiMsg::GaveUp {
                    to: gave_up.to,
                    id: gave_up.id,
                    tries: gave_up.tries,
                }));
                continue;
            }
            let info = self.pending[i].info.clone();
            out.push(Action::Send(info));
            let m = &mut self.pending[i];
            m.tries += 1;
            m.next_at = now + RETRY_BASE * 2u32.pow((m.tries - 1).min(MAX_RETRY_SHIFT));
            out.push(Action::Ui(UiMsg::Retry {
                to: m.to.clone(),
                id: m.id.clone(),
                attempt: m.tries,
                max: MAX_TRIES,
            }));
            i += 1;
        }
        out
    }

    fn send_message(&mut self, to: &str, text: &str) -> Vec<Action> {
        let to = to.to_ascii_uppercase();
        let id = self.take_id();
        let info = match aprs::format_message(&to, text, &id) {
            Ok(info) => info,
            Err(e) => return vec![Action::Ui(UiMsg::Error(format!("not sent: {e}")))],
        };
        self.pending.push(Outgoing {
            to: to.clone(),
            id: id.clone(),
            info: info.clone(),
            tries: 1,
            next_at: now() + RETRY_BASE,
            heard_via: Vec::new(),
        });
        vec![
            Action::Send(info),
            Action::Ui(UiMsg::Sent {
                to,
                id,
                text: text.to_owned(),
                attempt: 1,
                max: MAX_TRIES,
            }),
        ]
    }

    fn take_pending(&mut self, from: &str, id: &str) -> Option<Outgoing> {
        let pos = self
            .pending
            .iter()
            .position(|m| m.id == id && m.to.eq_ignore_ascii_case(from))?;
        Some(self.pending.remove(pos))
    }

    fn on_ack(&mut self, from: &str, id: &str) -> Vec<Action> {
        if let Some(acked) = self.take_pending(from, id) {
            vec![Action::Ui(UiMsg::Delivered {
                from: from.to_owned(),
                id: acked.id,
                tries: acked.tries,
            })]
        } else {
            Vec::new()
        }
    }

    fn on_rej(&mut self, from: &str, id: &str) -> Vec<Action> {
        if let Some(rejected) = self.take_pending(from, id) {
            vec![Action::Ui(UiMsg::Rejected {
                from: from.to_owned(),
                id: rejected.id,
            })]
        } else {
            Vec::new()
        }
    }

    fn on_own_echo(&mut self, heard: &Heard, info: &[u8]) -> Vec<Action> {
        let Some(digi) = heard.repeated_by() else {
            return Vec::new();
        };
        let Some(Message::Text { id: Some(id), .. }) = aprs::parse_message(info) else {
            return Vec::new();
        };
        if let Some(m) = self.pending.iter_mut().find(|m| m.id == id) {
            if !m.heard_via.contains(&digi) {
                m.heard_via.push(digi.clone());
                return vec![Action::Ui(UiMsg::Repeated { digi, id })];
            }
        }
        Vec::new()
    }

    fn on_text(&mut self, from: &str, text: &str, id: Option<&str>, route: &str) -> Vec<Action> {
        let Some(id) = id else {
            return vec![Action::Ui(UiMsg::Incoming {
                from: from.to_owned(),
                text: text.to_owned(),
                id: None,
                route: route.to_owned(),
            })];
        };

        let now = now();
        self.seen.retain(|_, seen| now.saturating_sub(seen.first) < SEEN_TTL);
        let key = (from.to_ascii_uppercase(), id.to_owned());
        let is_new = !self.seen.contains_key(&key);
        let seen = self.seen.entry(key).or_insert(Seen {
            first: now,
            last_ack: None,
        });
        let ack_due = seen
            .last_ack
            .is_none_or(|last| now.saturating_sub(last) >= ACK_HOLDOFF);
        if ack_due {
            seen.last_ack = Some(now);
        }

        let mut out = Vec::new();
        if is_new {
            out.push(Action::Ui(UiMsg::Incoming {
                from: from.to_owned(),
                text: text.to_owned(),
                id: Some(id.to_owned()),
                route: route.to_owned(),
            }));
        }
        if ack_due {
            match aprs::format_ack(from, id) {
                Ok(ack) => out.push(Action::Send(ack)),
                Err(e) => out.push(Action::Ui(UiMsg::Error(format!("cannot ack {from}: {e}")))),
            }
        }
        out
    }

    fn cancel(&mut self, which: &str) -> Vec<Action> {
        let which = which.trim().trim_start_matches('#');
        if which.is_empty() {
            return vec![Action::Ui(UiMsg::Print(
                "usage: cancel ID | cancel all".into(),
            ))];
        }
        let all = which.eq_ignore_ascii_case("all");
        let (cancelled, kept): (Vec<_>, Vec<_>) =
            self.pending.drain(..).partition(|m| all || m.id == which);
        self.pending = kept;
        if cancelled.is_empty() {
            return vec![Action::Ui(UiMsg::Print(format!(
                "no pending message #{which}; type pending to list them"
            )))];
        }
        cancelled
            .into_iter()
            .map(|m| {
                Action::Ui(UiMsg::Cancelled {
                    to: m.to,
                    id: m.id,
                })
            })
            .collect()
    }

    fn digis_text(&self) -> String {
        if !self.cfg.radio {
            return "digipeaters are only tracked on the radio link".into();
        }
        if self.digis.is_empty() {
            return "no digipeaters heard yet (only stations heard directly)".into();
        }
        let now = now();
        let mut heard: Vec<_> = self.digis.iter().collect();
        heard.sort_by(|a, b| b.1.packets.cmp(&a.1.packets).then(a.0.cmp(b.0)));
        let mut lines = Vec::new();
        for (call, h) in heard {
            lines.push(format!(
                "  {call:<10} {:>4} packets, last {} s ago",
                h.packets,
                now.saturating_sub(h.last).as_secs()
            ));
        }
        lines.join("\n")
    }

    fn pending_text(&self) -> String {
        if self.pending.is_empty() {
            return "no messages waiting for an ack".into();
        }
        let now = now();
        let mut lines = Vec::new();
        for m in &self.pending {
            lines.push(format!(
                "  #{} to {}: try {}/{MAX_TRIES}, next in {} s",
                m.id,
                m.to,
                m.tries,
                m.next_at.saturating_sub(now).as_secs()
            ));
        }
        lines.join("\n")
    }

    fn is_me(&self, addressee: &str) -> bool {
        addressed_to_me(&self.cfg.call, addressee)
    }

    fn take_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id = self.next_id % 99_999 + 1;
        id.to_string()
    }
}

/// True when `addressee` is for our station.
///
/// With a bare callsign (`SA0KAM`, SSID 0), every SSID of that call matches
/// (`SA0KAM`, `SA0KAM-1` … `SA0KAM-15`), which is how APRS messaging works.
/// With an explicit SSID (`SA0KAM-1`), only that exact addressee matches.
pub fn addressed_to_me(mine: &Address, addressee: &str) -> bool {
    let addressee = addressee.trim();
    if addressee.eq_ignore_ascii_case(&mine.to_string()) {
        return true;
    }
    let Ok(other) = Address::parse(addressee) else {
        return false;
    };
    other.call.eq_ignore_ascii_case(&mine.call) && mine.ssid == 0
}

/// APRS-IS `g/` filter so the server delivers messages to our callsign.
pub fn message_group_filter(call: &Address, extra: &str) -> String {
    let group = if call.ssid == 0 {
        format!("g/{}*", call.call)
    } else {
        format!("g/{call}")
    };
    format!("{group} {extra}").trim().to_owned()
}

fn on_off(flag: bool) -> &'static str {
    if flag {
        "on"
    } else {
        "off"
    }
}

fn now() -> Duration {
    clock::now()
}

fn unix_seconds() -> u64 {
    now().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(call: &str) -> ClientConfig {
        ClientConfig {
            call: Address::parse(call).unwrap(),
            path: vec![Address::parse("WIDE1-1").unwrap()],
            tocall: Address::parse("APZRST").unwrap(),
            monitor: false,
            radio: true,
        }
    }

    #[test]
    fn sends_and_acks_message() {
        let mut client = Client::new(cfg("SA0KAM-1"));
        let actions = client.on_command("msg SM0YOS-1 hello");
        assert!(actions.iter().any(|a| matches!(a, Action::Send(_))));
        assert_eq!(client.pending_count(), 1);

        let id = match &actions[1] {
            Action::Ui(UiMsg::Sent { id, .. }) => id.clone(),
            _ => panic!("expected Sent"),
        };
        let info = aprs::format_ack("SA0KAM-1", &id).unwrap();
        let heard = Heard {
            source: "SM0YOS-1".into(),
            dest: "APRS".into(),
            path: vec![],
            info,
            from_internet: true,
        };
        let actions = client.on_heard(heard);
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::Ui(UiMsg::Delivered { .. })
        )));
        assert_eq!(client.pending_count(), 0);
    }

    #[test]
    fn acks_incoming_once_within_holdoff() {
        let mut client = Client::new(cfg("SA0KAM-1"));
        let info = aprs::format_message("SA0KAM-1", "hi", "7").unwrap();
        let heard = Heard {
            source: "SM0YOS-1".into(),
            dest: "APRS".into(),
            path: vec![],
            info,
            from_internet: false,
        };
        let first = client.on_heard(heard.clone());
        assert_eq!(
            first.iter().filter(|a| matches!(a, Action::Send(_))).count(),
            1
        );
        let second = client.on_heard(heard);
        assert_eq!(
            second.iter().filter(|a| matches!(a, Action::Send(_))).count(),
            0
        );
        assert!(second.iter().all(|a| !matches!(a, Action::Ui(UiMsg::Incoming { .. }))));
    }

    #[test]
    fn bare_callsign_matches_all_ssids() {
        let mine = Address::parse("SA0KAM").unwrap();
        assert!(addressed_to_me(&mine, "SA0KAM"));
        assert!(addressed_to_me(&mine, "SA0KAM-1"));
        assert!(addressed_to_me(&mine, "sa0kam-9"));
        assert!(addressed_to_me(&mine, "SA0KAM-15"));
        assert!(!addressed_to_me(&mine, "SA0KAMX"));
        assert!(!addressed_to_me(&mine, "SM0YOS-1"));

        let mine = Address::parse("SA0KAM-1").unwrap();
        assert!(addressed_to_me(&mine, "SA0KAM-1"));
        assert!(addressed_to_me(&mine, "sa0kam-1"));
        assert!(!addressed_to_me(&mine, "SA0KAM"));
        assert!(!addressed_to_me(&mine, "SA0KAM-2"));
    }

    #[test]
    fn message_filter_wildcards_bare_call() {
        assert_eq!(
            message_group_filter(&Address::parse("SA0KAM").unwrap(), ""),
            "g/SA0KAM*"
        );
        assert_eq!(
            message_group_filter(&Address::parse("SA0KAM-1").unwrap(), "r/59/18/50"),
            "g/SA0KAM-1 r/59/18/50"
        );
    }

    #[test]
    fn bare_call_receives_ssid_message() {
        let mut client = Client::new(cfg("SA0KAM"));
        let info = aprs::format_message("SA0KAM-7", "ping", "3").unwrap();
        let heard = Heard {
            source: "SM0YOS-1".into(),
            dest: "APRS".into(),
            path: vec![],
            info,
            from_internet: true,
        };
        let actions = client.on_heard(heard);
        assert!(actions.iter().any(|a| matches!(a, Action::Ui(UiMsg::Incoming { .. }))));
        assert!(actions.iter().any(|a| matches!(a, Action::Send(_))));
    }
}
