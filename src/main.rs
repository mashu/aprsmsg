//! aprsmsg — interactive APRS messaging client.
//!
//! Talks to the APRS network either by radio, through Direwolf's KISS TCP
//! port, or over the internet, through an APRS-IS server. Sends numbered
//! messages and retries them until acknowledged, automatically acknowledges
//! messages addressed to you, decodes other stations' traffic in monitor
//! mode, and keeps track of which digipeaters you can hear.

mod aprs;
mod ax25;
mod decode;
mod kiss;
mod link;
mod ui;

use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead};
use std::process;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aprs::Message;
use ax25::Address;
use decode::Packet;
use link::{AprsIsLogin, Heard, Link, LinkEvent};
use ui::Ui;

/// First retry after 30 s, then doubling up to 4 minutes.
const RETRY_BASE: Duration = Duration::from_secs(30);
const MAX_RETRY_SHIFT: u32 = 3;
const MAX_TRIES: u32 = 5;
/// Digipeated copies of one message arrive within seconds; ack them once.
const ACK_HOLDOFF: Duration = Duration::from_secs(10);
const SEEN_TTL: Duration = Duration::from_secs(30 * 60);
/// After stdin closes and all messages settle, keep acking late replies.
const LINGER: Duration = Duration::from_secs(30);
const IDLE_WAIT: Duration = Duration::from_secs(3600);
const DEFAULT_KISS: &str = "127.0.0.1:8001";
const DEFAULT_APRS_IS: &str = "euro.aprs2.net:14580";

const USAGE: &str = "\
usage: aprsmsg --call MYCALL [radio or internet options] [--monitor] [--no-color]

  --call MYCALL      your callsign-SSID, e.g. SA0KAM-1        (required)

radio (default), through Direwolf:
  --kiss HOST:PORT   Direwolf KISS TCP port                   (default 127.0.0.1:8001)
  --chan N           radio channel, 0 = first                 (default 0)
  --path P           digipeater path, or \"none\"               (default WIDE1-1,WIDE2-1)

internet, no radio needed:
  --aprs-is          connect to an APRS-IS server instead of Direwolf
  --server HOST:PORT APRS-IS server                           (default euro.aprs2.net:14580)
  --passcode N       APRS-IS passcode                         (default: computed from --call)
  --filter F         extra server filter for the monitor, e.g. r/59.33/18.07/50
                     (messages to MYCALL are always received)

common:
  --tocall T         AX.25 destination (software identifier)  (default APZRST)
  --monitor          show other stations' packets from the start
  --no-color         plain output (also when NO_COLOR is set or output is piped)";

const COMMANDS: &str = "\
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

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

enum Transport {
    Radio { kiss: String, chan: u8 },
    Internet { server: String, passcode: u16, filter: String },
}

struct Config {
    call: Address,
    transport: Transport,
    path: Vec<Address>,
    tocall: Address,
    monitor: bool,
    no_color: bool,
}

fn parse_args() -> Result<Config, String> {
    let mut call = None;
    let mut kiss = String::from(DEFAULT_KISS);
    let mut chan: u8 = 0;
    let mut path = String::from("WIDE1-1,WIDE2-1");
    let mut aprs_is = false;
    let mut server = String::from(DEFAULT_APRS_IS);
    let mut passcode: Option<u16> = None;
    let mut filter = String::new();
    let mut tocall = String::from("APZRST");
    let mut monitor = false;
    let mut no_color = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--call" => call = Some(value_of(&mut args, "--call")?),
            "--kiss" => kiss = value_of(&mut args, "--kiss")?,
            "--chan" => {
                chan = value_of(&mut args, "--chan")?
                    .parse()
                    .ok()
                    .filter(|&c| c <= 15)
                    .ok_or("--chan must be 0-15")?
            }
            "--path" => path = value_of(&mut args, "--path")?,
            "--aprs-is" => aprs_is = true,
            "--server" => server = value_of(&mut args, "--server")?,
            "--passcode" => {
                passcode = Some(
                    value_of(&mut args, "--passcode")?
                        .parse()
                        .map_err(|_| "--passcode must be a number")?,
                )
            }
            "--filter" => filter = value_of(&mut args, "--filter")?,
            "--tocall" => tocall = value_of(&mut args, "--tocall")?,
            "--monitor" => monitor = true,
            "--no-color" => no_color = true,
            "-h" | "--help" => {
                println!("{USAGE}\n\n{COMMANDS}");
                process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    let call = Address::parse(&call.ok_or("--call is required")?)?;
    let path = if path.trim().is_empty() || path.eq_ignore_ascii_case("none") {
        Vec::new()
    } else {
        path.split(',').map(Address::parse).collect::<Result<Vec<_>, _>>()?
    };
    if path.len() > 8 {
        return Err("--path allows at most 8 digipeaters".into());
    }
    let tocall = Address::parse(&tocall)?;
    let transport = if aprs_is {
        Transport::Internet {
            server,
            passcode: passcode.unwrap_or_else(|| link::passcode(&call.call)),
            filter: format!("g/{call} {filter}").trim().to_owned(),
        }
    } else {
        Transport::Radio { kiss, chan }
    };

    Ok(Config { call, transport, path, tocall, monitor, no_color })
}

fn value_of(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next().ok_or(format!("{name} needs a value"))
}

fn open_link(cfg: &Config) -> io::Result<(Link, String)> {
    match &cfg.transport {
        Transport::Radio { kiss, chan } => {
            let link = Link::kiss(kiss, *chan)?;
            Ok((link, format!("radio via Direwolf {kiss}, channel {chan}")))
        }
        Transport::Internet { server, passcode, filter } => {
            let call = cfg.call.to_string();
            let login = AprsIsLogin { server, call: &call, passcode: *passcode, filter };
            let link = Link::aprs_is(&login)?;
            Ok((link, format!("APRS-IS {server}, filter {filter}")))
        }
    }
}

// ---------------------------------------------------------------------------
// Events from the link and stdin threads
// ---------------------------------------------------------------------------

enum Event {
    Link(LinkEvent),
    Line(String),
    InputClosed,
}

fn spawn_stdin_reader(events: Sender<Event>) {
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if events.send(Event::Line(line)).is_err() {
                return;
            }
        }
        let _ = events.send(Event::InputClosed);
    });
}

// ---------------------------------------------------------------------------
// Client state
// ---------------------------------------------------------------------------

struct Outgoing {
    to: String,
    id: String,
    info: Vec<u8>,
    tries: u32,
    next_at: Instant,
    heard_via: Vec<String>,
}

struct Seen {
    first: Instant,
    last_ack: Option<Instant>,
}

struct DigiHeard {
    packets: u32,
    last: Instant,
}

struct Client {
    cfg: Config,
    ui: Ui,
    link: Link,
    mycall: String,
    next_id: u32,
    pending: Vec<Outgoing>,
    seen: HashMap<(String, String), Seen>,
    digis: HashMap<String, DigiHeard>,
    monitor: bool,
    raw: bool,
}

impl Client {
    fn new(cfg: Config, link: Link) -> Self {
        // Seed ids from the clock so a restart does not reuse recent ids.
        let next_id = (unix_seconds() % 1000) as u32 + 1;
        Client {
            ui: Ui::new(cfg.no_color),
            mycall: cfg.call.to_string(),
            monitor: cfg.monitor,
            raw: false,
            link,
            cfg,
            next_id,
            pending: Vec::new(),
            seen: HashMap::new(),
            digis: HashMap::new(),
        }
    }

    fn transmit(&mut self, info: &[u8]) -> io::Result<()> {
        self.link.send(&self.cfg.call, &self.cfg.tocall, &self.cfg.path, info)
    }

    fn is_me(&self, addressee: &str) -> bool {
        addressee.eq_ignore_ascii_case(&self.mycall)
    }

    fn take_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id = self.next_id % 99_999 + 1;
        id.to_string()
    }

    fn next_deadline(&self) -> Option<Instant> {
        let retry = self.pending.iter().map(|m| m.next_at).min();
        [retry, self.link.next_deadline()].into_iter().flatten().min()
    }

    // ---- outgoing ---------------------------------------------------------

    fn send_message(&mut self, to: &str, text: &str) -> io::Result<()> {
        let to = to.to_ascii_uppercase();
        let id = self.take_id();
        let info = match aprs::format_message(&to, text, &id) {
            Ok(info) => info,
            Err(e) => {
                self.ui.error(&format!("not sent: {e}"));
                return Ok(());
            }
        };
        self.transmit(&info)?;
        self.ui.sent(&to, &id, text, 1, MAX_TRIES);
        self.pending.push(Outgoing {
            to,
            id,
            info,
            tries: 1,
            next_at: Instant::now() + RETRY_BASE,
            heard_via: Vec::new(),
        });
        Ok(())
    }

    fn run_timers(&mut self) -> io::Result<()> {
        self.link.maintain()?;
        let now = Instant::now();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].next_at > now {
                i += 1;
                continue;
            }
            if self.pending[i].tries >= MAX_TRIES {
                let gave_up = self.pending.remove(i);
                self.ui.gave_up(&gave_up.to, &gave_up.id, gave_up.tries);
                continue;
            }
            let info = self.pending[i].info.clone();
            self.transmit(&info)?;
            let m = &mut self.pending[i];
            m.tries += 1;
            m.next_at = now + RETRY_BASE * 2u32.pow((m.tries - 1).min(MAX_RETRY_SHIFT));
            self.ui.retry(&m.to, &m.id, m.tries, MAX_TRIES);
            i += 1;
        }
        Ok(())
    }

    fn take_pending(&mut self, from: &str, id: &str) -> Option<Outgoing> {
        let pos = self
            .pending
            .iter()
            .position(|m| m.id == id && m.to.eq_ignore_ascii_case(from))?;
        Some(self.pending.remove(pos))
    }

    fn on_ack(&mut self, from: &str, id: &str) {
        if let Some(acked) = self.take_pending(from, id) {
            self.ui.delivered(from, &acked.id, acked.tries);
        }
    }

    fn on_rej(&mut self, from: &str, id: &str) {
        if let Some(rejected) = self.take_pending(from, id) {
            self.ui.rejected(from, &rejected.id);
        }
    }

    /// Our own packet came back through a digipeater: show who repeated it.
    fn on_own_echo(&mut self, heard: &Heard, info: &[u8]) {
        let Some(digi) = heard.repeated_by() else { return };
        let Some(Message::Text { id: Some(id), .. }) = aprs::parse_message(info) else { return };
        if let Some(m) = self.pending.iter_mut().find(|m| m.id == id) {
            if !m.heard_via.contains(&digi) {
                self.ui.repeated(&digi, &m.id);
                m.heard_via.push(digi);
            }
        }
    }

    // ---- incoming ---------------------------------------------------------

    fn on_link_event(&mut self, event: LinkEvent) -> io::Result<()> {
        match event {
            LinkEvent::Heard(heard) => self.on_heard(heard),
            LinkEvent::Notice(notice) => {
                if notice.contains("unverified") {
                    self.ui.error(&format!("{notice} — wrong passcode? packets you send will be dropped"));
                } else {
                    self.ui.info(&notice);
                }
                Ok(())
            }
            LinkEvent::Closed(reason) => Err(io::Error::new(io::ErrorKind::ConnectionAborted, reason)),
        }
    }

    fn on_heard(&mut self, heard: Heard) -> io::Result<()> {
        if self.raw {
            self.ui.raw(&format!("{}:{}", heard.header(), decode::printable(&heard.info)));
        }
        if !heard.from_internet {
            if let Some(digi) = heard.repeated_by() {
                let now = Instant::now();
                let entry = self.digis.entry(digi).or_insert(DigiHeard { packets: 0, last: now });
                entry.packets += 1;
                entry.last = now;
            }
        }

        let (source, info) = aprs::unwrap_third_party(&heard.source, &heard.info);
        if source.eq_ignore_ascii_case(&self.mycall) {
            self.on_own_echo(&heard, &info);
            return Ok(());
        }
        let route = heard.route();

        match aprs::parse_message(&info) {
            Some(Message::Ack { to, id }) if self.is_me(&to) => self.on_ack(&source, &id),
            Some(Message::Rej { to, id }) if self.is_me(&to) => self.on_rej(&source, &id),
            Some(Message::Text { to, text, id, reply_ack }) if self.is_me(&to) => {
                if let Some(acked_id) = reply_ack {
                    self.on_ack(&source, &acked_id);
                }
                self.on_text(&source, &text, id.as_deref(), &route)?;
            }
            _ if self.monitor => {
                let mut packet = decode::decode(heard.dest_call(), &heard.info);
                while let Packet::ThirdParty { inner, .. } = packet {
                    packet = *inner;
                }
                self.ui.monitor(&source, &packet.to_string(), &route);
            }
            _ => {}
        }
        Ok(())
    }

    fn on_text(&mut self, from: &str, text: &str, id: Option<&str>, route: &str) -> io::Result<()> {
        let Some(id) = id else {
            self.ui.incoming(from, text, None, route);
            return Ok(());
        };

        let now = Instant::now();
        self.seen.retain(|_, seen| now.duration_since(seen.first) < SEEN_TTL);
        let key = (from.to_ascii_uppercase(), id.to_owned());
        let is_new = !self.seen.contains_key(&key);
        let seen = self.seen.entry(key).or_insert(Seen { first: now, last_ack: None });
        let ack_due = seen
            .last_ack
            .map_or(true, |last| now.duration_since(last) >= ACK_HOLDOFF);
        if ack_due {
            seen.last_ack = Some(now);
        }

        if is_new {
            self.ui.incoming(from, text, Some(id), route);
        }
        if ack_due {
            match aprs::format_ack(from, id) {
                Ok(ack) => self.transmit(&ack)?,
                Err(e) => self.ui.error(&format!("cannot ack {from}: {e}")),
            }
        }
        Ok(())
    }

    // ---- commands ---------------------------------------------------------

    /// Returns false when the user asked to quit.
    fn on_command(&mut self, line: &str) -> io::Result<bool> {
        let line = line.trim();
        let (command, rest) = match line.split_once(char::is_whitespace) {
            Some((command, rest)) => (command, rest.trim()),
            None => (line, ""),
        };
        match command.to_ascii_lowercase().as_str() {
            "" => {}
            "m" | "msg" => match rest.split_once(char::is_whitespace) {
                Some((to, text)) if !text.trim().is_empty() => self.send_message(to, text.trim())?,
                _ => println!("usage: msg CALL text"),
            },
            "p" | "pending" => self.print_pending(),
            "cancel" => self.cancel(rest),
            "d" | "digis" => self.print_digis(),
            "mon" => {
                self.monitor = !self.monitor;
                self.ui.info(&format!("monitor {}", on_off(self.monitor)));
            }
            "raw" => {
                self.raw = !self.raw;
                self.ui.info(&format!("raw packets {}", on_off(self.raw)));
            }
            "h" | "help" | "?" => println!("{COMMANDS}"),
            "q" | "quit" | "exit" => return Ok(false),
            other => println!("unknown command {other:?}; type help"),
        }
        Ok(true)
    }

    fn cancel(&mut self, which: &str) {
        let which = which.trim().trim_start_matches('#');
        if which.is_empty() {
            println!("usage: cancel ID | cancel all");
            return;
        }
        let all = which.eq_ignore_ascii_case("all");
        let (cancelled, kept): (Vec<_>, Vec<_>) = self
            .pending
            .drain(..)
            .partition(|m| all || m.id == which);
        self.pending = kept;
        if cancelled.is_empty() {
            println!("no pending message #{which}; type pending to list them");
        }
        for m in cancelled {
            self.ui.cancelled(&m.to, &m.id);
        }
    }

    fn print_digis(&self) {
        if !self.link.is_radio() {
            println!("digipeaters are only tracked on the radio link");
            return;
        }
        if self.digis.is_empty() {
            println!("no digipeaters heard yet (only stations heard directly)");
            return;
        }
        let now = Instant::now();
        let mut heard: Vec<_> = self.digis.iter().collect();
        heard.sort_by(|a, b| b.1.packets.cmp(&a.1.packets).then(a.0.cmp(b.0)));
        for (call, h) in heard {
            println!(
                "  {call:<10} {:>4} packets, last {} s ago",
                h.packets,
                now.duration_since(h.last).as_secs()
            );
        }
    }

    fn print_pending(&self) {
        if self.pending.is_empty() {
            println!("no messages waiting for an ack");
            return;
        }
        let now = Instant::now();
        for m in &self.pending {
            println!(
                "  #{} to {}: try {}/{MAX_TRIES}, next in {} s",
                m.id,
                m.to,
                m.tries,
                m.next_at.saturating_duration_since(now).as_secs()
            );
        }
    }
}

fn on_off(flag: bool) -> &'static str {
    if flag {
        "on"
    } else {
        "off"
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------------------

fn run(cfg: Config) -> io::Result<()> {
    let (link, description) = open_link(&cfg)?;
    let (events_tx, events) = mpsc::channel();
    let link_tx = events_tx.clone();
    link.spawn_reader(move |event| link_tx.send(Event::Link(event)).is_ok())?;
    spawn_stdin_reader(events_tx);

    let banner = format!("{} on {description} — type help", cfg.call);
    let mut client = Client::new(cfg, link);
    client.ui.info(&banner);
    let mut input_open = true;
    let mut linger_until: Option<Instant> = None;

    loop {
        let deadline = [client.next_deadline(), linger_until].into_iter().flatten().min();
        let timeout = deadline.map_or(IDLE_WAIT, |d| d.saturating_duration_since(Instant::now()));
        match events.recv_timeout(timeout) {
            Ok(Event::Link(event)) => client.on_link_event(event)?,
            Ok(Event::Line(line)) => {
                if !client.on_command(&line)? {
                    return Ok(());
                }
            }
            Ok(Event::InputClosed) => {
                if client.pending.is_empty() {
                    return Ok(());
                }
                input_open = false;
                client.ui.info(&format!(
                    "input closed; waiting for {} pending message(s)",
                    client.pending.len()
                ));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        client.run_timers()?;

        if !input_open && client.pending.is_empty() {
            let until = *linger_until.get_or_insert_with(|| Instant::now() + LINGER);
            if Instant::now() >= until {
                return Ok(());
            }
        }
    }
}

fn main() {
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("aprsmsg: {msg}\n\n{USAGE}");
            process::exit(2);
        }
    };
    if let Err(e) = run(cfg) {
        eprintln!("aprsmsg: {e}");
        process::exit(1);
    }
}
