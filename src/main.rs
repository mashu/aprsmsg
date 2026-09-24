//! aprsmsg — interactive APRS messaging client for Direwolf's KISS TCP port.
//!
//! Sends numbered messages and retries them until acknowledged, automatically
//! acknowledges messages addressed to you (including ones relayed from the
//! internet by iGates), and can monitor all received traffic.

mod aprs;
mod ax25;
mod kiss;

use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;
use std::process;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aprs::Message;
use ax25::{Address, UiFrame};

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

const USAGE: &str = "\
usage: aprsmsg --call MYCALL [--kiss HOST:PORT] [--chan N] [--path P] [--tocall T] [--monitor]

  --call MYCALL     your callsign-SSID, e.g. SA0KAM-1       (required)
  --kiss HOST:PORT  Direwolf KISS TCP port                  (default 127.0.0.1:8001)
  --chan N          radio channel, 0 = first                (default 0)
  --path P          digipeater path, or \"none\"              (default WIDE1-1,WIDE2-1)
  --tocall T        AX.25 destination (software identifier) (default APZRST)
  --monitor         show every received packet from the start";

const COMMANDS: &str = "\
commands:
  msg CALL text   send a numbered message, retried until acknowledged
                  e.g.  msg EMAIL-2 friend@example.com Hello from the FTX-1
  pending         list messages still waiting for an ack
  mon             toggle monitoring of all received packets
  quit            exit (Ctrl-D exits once pending messages are settled)";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

struct Config {
    call: Address,
    kiss: String,
    chan: u8,
    path: Vec<Address>,
    tocall: Address,
    monitor: bool,
}

fn parse_args() -> Result<Config, String> {
    let mut call = None;
    let mut kiss = String::from("127.0.0.1:8001");
    let mut chan: u8 = 0;
    let mut path = String::from("WIDE1-1,WIDE2-1");
    let mut tocall = String::from("APZRST");
    let mut monitor = false;

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
            "--tocall" => tocall = value_of(&mut args, "--tocall")?,
            "--monitor" => monitor = true,
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

    Ok(Config { call, kiss, chan, path, tocall, monitor })
}

fn value_of(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next().ok_or(format!("{name} needs a value"))
}

// ---------------------------------------------------------------------------
// Events from the reader and stdin threads
// ---------------------------------------------------------------------------

enum Event {
    Frame(UiFrame),
    Line(String),
    InputClosed,
    LinkClosed(String),
}

fn spawn_kiss_reader(mut stream: TcpStream, chan: u8, events: Sender<Event>) {
    thread::spawn(move || {
        let mut decoder = kiss::Decoder::default();
        let mut buf = [0u8; 4096];
        let reason = loop {
            match stream.read(&mut buf) {
                Ok(0) => break "Direwolf closed the KISS connection".to_owned(),
                Ok(n) => {
                    for kiss_frame in decoder.push(&buf[..n]) {
                        if kiss_frame.channel != chan {
                            continue;
                        }
                        if let Some(frame) = UiFrame::decode(&kiss_frame.payload) {
                            if events.send(Event::Frame(frame)).is_err() {
                                return;
                            }
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break format!("KISS connection error: {e}"),
            }
        };
        let _ = events.send(Event::LinkClosed(reason));
    });
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

struct Client {
    cfg: Config,
    mycall: String,
    stream: TcpStream,
    next_id: u32,
    pending: Vec<Outgoing>,
    seen: HashMap<(String, String), Seen>,
    monitor: bool,
}

impl Client {
    fn new(cfg: Config, stream: TcpStream) -> Self {
        // Seed ids from the clock so a restart does not reuse recent ids.
        let next_id = (unix_seconds() % 1000) as u32 + 1;
        Client {
            mycall: cfg.call.to_string(),
            monitor: cfg.monitor,
            cfg,
            stream,
            next_id,
            pending: Vec::new(),
            seen: HashMap::new(),
        }
    }

    fn transmit(&mut self, info: Vec<u8>) -> io::Result<()> {
        let frame = UiFrame {
            dest: self.cfg.tocall.clone(),
            src: self.cfg.call.clone(),
            digis: self.cfg.path.iter().map(|digi| (digi.clone(), false)).collect(),
            info,
        };
        self.stream.write_all(&kiss::encode(self.cfg.chan, &frame.encode()))
    }

    fn is_me(&self, addressee: &str) -> bool {
        addressee.eq_ignore_ascii_case(&self.mycall)
    }

    fn take_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id = self.next_id % 99_999 + 1;
        id.to_string()
    }

    // ---- outgoing ---------------------------------------------------------

    fn send_message(&mut self, to: &str, text: &str) -> io::Result<()> {
        let to = to.to_ascii_uppercase();
        let id = self.take_id();
        let info = match aprs::format_message(&to, text, &id) {
            Ok(info) => info,
            Err(e) => {
                println!("{} !! not sent: {e}", stamp());
                return Ok(());
            }
        };
        self.transmit(info.clone())?;
        println!("{} >> #{id} to {to} (1/{MAX_TRIES}): {text}", stamp());
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

    fn next_deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|m| m.next_at).min()
    }

    fn run_retries(&mut self) -> io::Result<()> {
        let now = Instant::now();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].next_at > now {
                i += 1;
                continue;
            }
            if self.pending[i].tries >= MAX_TRIES {
                let gave_up = self.pending.remove(i);
                println!(
                    "{} !! #{} to {}: no ack after {} tries",
                    stamp(),
                    gave_up.id,
                    gave_up.to,
                    gave_up.tries
                );
                continue;
            }
            let info = self.pending[i].info.clone();
            self.transmit(info)?;
            let m = &mut self.pending[i];
            m.tries += 1;
            m.next_at = now + RETRY_BASE * 2u32.pow((m.tries - 1).min(MAX_RETRY_SHIFT));
            println!("{} >> #{} to {} ({}/{MAX_TRIES}) retry", stamp(), m.id, m.to, m.tries);
            i += 1;
        }
        Ok(())
    }

    fn on_ack(&mut self, from: &str, id: &str) {
        let found = self
            .pending
            .iter()
            .position(|m| m.id == id && m.to.eq_ignore_ascii_case(from));
        if let Some(pos) = found {
            let acked = self.pending.remove(pos);
            let tries = if acked.tries == 1 { "try" } else { "tries" };
            println!("{} ok #{} acked by {} after {} {tries}", stamp(), acked.id, from, acked.tries);
        }
    }

    fn on_rej(&mut self, from: &str, id: &str) {
        let found = self
            .pending
            .iter()
            .position(|m| m.id == id && m.to.eq_ignore_ascii_case(from));
        if let Some(pos) = found {
            let rejected = self.pending.remove(pos);
            println!("{} !! #{} rejected by {}", stamp(), rejected.id, from);
        }
    }

    /// Our own frame came back through a digipeater: show who repeated it.
    fn on_own_echo(&mut self, frame: &UiFrame, info: &[u8]) {
        let Some(digi) = frame.repeated_by() else { return };
        let Some(Message::Text { id: Some(id), .. }) = aprs::parse_message(info) else { return };
        if let Some(m) = self.pending.iter_mut().find(|m| m.id == id) {
            if !m.heard_via.contains(&digi) {
                println!("{} .. #{} repeated by {digi}", stamp(), m.id);
                m.heard_via.push(digi);
            }
        }
    }

    // ---- incoming ---------------------------------------------------------

    fn on_frame(&mut self, frame: UiFrame) -> io::Result<()> {
        if self.monitor {
            println!(
                "{} {}:{}",
                stamp(),
                frame.header(),
                String::from_utf8_lossy(&frame.info).trim_end()
            );
        }
        let (source, info) = aprs::unwrap_third_party(&frame.src.to_string(), &frame.info);
        if source.eq_ignore_ascii_case(&self.mycall) {
            self.on_own_echo(&frame, &info);
            return Ok(());
        }
        match aprs::parse_message(&info) {
            Some(Message::Ack { to, id }) if self.is_me(&to) => self.on_ack(&source, &id),
            Some(Message::Rej { to, id }) if self.is_me(&to) => self.on_rej(&source, &id),
            Some(Message::Text { to, text, id, reply_ack }) if self.is_me(&to) => {
                if let Some(acked_id) = reply_ack {
                    self.on_ack(&source, &acked_id);
                }
                self.on_text(&source, &text, id.as_deref())?;
            }
            _ => {}
        }
        Ok(())
    }

    fn on_text(&mut self, from: &str, text: &str, id: Option<&str>) -> io::Result<()> {
        let Some(id) = id else {
            println!("{} << {from}: {text}", stamp());
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
            println!("{} << {from} #{id}: {text}", stamp());
        }
        if ack_due {
            match aprs::format_ack(from, id) {
                Ok(ack) => self.transmit(ack)?,
                Err(e) => println!("{} !! cannot ack {from}: {e}", stamp()),
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
            "mon" => {
                self.monitor = !self.monitor;
                println!("monitor {}", if self.monitor { "on" } else { "off" });
            }
            "h" | "help" | "?" => println!("{COMMANDS}"),
            "q" | "quit" | "exit" => return Ok(false),
            other => println!("unknown command {other:?}; type help"),
        }
        Ok(true)
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

// ---------------------------------------------------------------------------

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn stamp() -> String {
    let secs = unix_seconds() % 86_400;
    format!("{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

fn run(cfg: Config) -> io::Result<()> {
    let stream = TcpStream::connect(&cfg.kiss).map_err(|e| {
        io::Error::new(e.kind(), format!("cannot reach KISS TCP port at {}: {e}", cfg.kiss))
    })?;
    stream.set_nodelay(true)?;

    let (events_tx, events) = mpsc::channel();
    spawn_kiss_reader(stream.try_clone()?, cfg.chan, events_tx.clone());
    spawn_stdin_reader(events_tx);

    println!(
        "{} connected to {} as {} on channel {} (type help)",
        stamp(),
        cfg.kiss,
        cfg.call,
        cfg.chan
    );
    let mut client = Client::new(cfg, stream);
    let mut input_open = true;
    let mut linger_until: Option<Instant> = None;

    loop {
        let deadline = [client.next_deadline(), linger_until].into_iter().flatten().min();
        let timeout = deadline.map_or(IDLE_WAIT, |d| d.saturating_duration_since(Instant::now()));
        match events.recv_timeout(timeout) {
            Ok(Event::Frame(frame)) => client.on_frame(frame)?,
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
                println!("{} input closed; waiting for {} pending message(s)", stamp(), client.pending.len());
            }
            Ok(Event::LinkClosed(reason)) => {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, reason))
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        client.run_retries()?;

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
