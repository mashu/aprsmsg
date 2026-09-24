//! Links to the APRS network: Direwolf's KISS TCP port (radio) or an APRS-IS
//! server (internet), plus `Heard`, a received packet in a transport-neutral
//! form so the client handles both the same way.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use crate::ax25::{Address, UiFrame};
use crate::kiss;

/// APRS-IS servers drop silent clients; send a comment line this often.
const KEEPALIVE: Duration = Duration::from_secs(300);
const MAX_LINE: usize = 1024;
const GENERIC_ALIASES: [&str; 8] = ["WIDE", "TRACE", "RELAY", "TEMP", "ECHO", "GATE", "TCPIP", "TCPXX"];

// ---------------------------------------------------------------------------
// Received packets
// ---------------------------------------------------------------------------

pub struct Hop {
    pub call: String,
    pub used: bool,
}

pub struct Heard {
    pub source: String,
    pub dest: String,
    pub path: Vec<Hop>,
    pub info: Vec<u8>,
    pub from_internet: bool,
}

impl Heard {
    pub fn from_frame(frame: UiFrame) -> Heard {
        Heard {
            source: frame.src.to_string(),
            dest: frame.dest.to_string(),
            path: frame
                .digis
                .into_iter()
                .map(|(call, used)| Hop { call: call.to_string(), used })
                .collect(),
            info: frame.info,
            from_internet: false,
        }
    }

    /// Parses an APRS-IS line in TNC2 format: `SRC>DEST,PATH:info`.
    pub fn parse_tnc2(line: &[u8]) -> Option<Heard> {
        let colon = line.iter().position(|&b| b == b':')?;
        let header = std::str::from_utf8(&line[..colon]).ok()?;
        let (source, route) = header.split_once('>')?;
        let mut parts = route.split(',');
        let dest = parts.next().filter(|d| !d.is_empty())?;
        if source.is_empty() {
            return None;
        }
        let path = parts
            .filter(|hop| !hop.is_empty())
            .map(|hop| Hop {
                call: hop.trim_end_matches('*').to_owned(),
                used: hop.ends_with('*'),
            })
            .collect();
        Some(Heard {
            source: source.to_owned(),
            dest: dest.to_owned(),
            path,
            info: line[colon + 1..].to_vec(),
            from_internet: true,
        })
    }

    /// Destination callsign without SSID (Mic-E encodes latitude there).
    pub fn dest_call(&self) -> &str {
        self.dest.split('-').next().unwrap_or("")
    }

    /// Monitor header `SRC>DEST,HOP,HOP*` with a star on the last used hop.
    pub fn header(&self) -> String {
        let mut header = format!("{}>{}", self.source, self.dest);
        let last_used = self.path.iter().rposition(|hop| hop.used);
        for (i, hop) in self.path.iter().enumerate() {
            header.push(',');
            header.push_str(&hop.call);
            if Some(i) == last_used {
                header.push('*');
            }
        }
        header
    }

    /// The last station that actually repeated this packet on RF.
    pub fn repeated_by(&self) -> Option<String> {
        self.path
            .iter()
            .rev()
            .filter(|hop| hop.used && !is_generic_alias(&hop.call))
            .map(|hop| hop.call.clone())
            .next()
    }

    /// How the packet reached us, for display.
    pub fn route(&self) -> String {
        if self.info.first() == Some(&b'}') {
            return format!("iGate {}", self.source);
        }
        if self.from_internet {
            return match self.gated_by() {
                Some(igate) => format!("APRS-IS, iGate {igate}"),
                None => "APRS-IS".to_owned(),
            };
        }
        match self.repeated_by() {
            Some(digi) => format!("via {digi}"),
            None => "direct".to_owned(),
        }
    }

    /// For internet packets that came from RF: the iGate named after `qAR`/`qAO`.
    fn gated_by(&self) -> Option<&str> {
        let q = self.path.iter().position(|hop| hop.call == "qAR" || hop.call == "qAO")?;
        self.path.get(q + 1).map(|hop| hop.call.as_str())
    }
}

fn is_generic_alias(call: &str) -> bool {
    let q_construct = call.len() == 3 && call.starts_with('q');
    q_construct || GENERIC_ALIASES.iter().any(|alias| call.starts_with(alias))
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

pub enum LinkEvent {
    Heard(Heard),
    Notice(String),
    Closed(String),
}

pub struct AprsIsLogin<'a> {
    pub server: &'a str,
    pub call: &'a str,
    pub passcode: u16,
    pub filter: &'a str,
}

pub enum Link {
    Kiss { stream: TcpStream, chan: u8 },
    AprsIs { stream: TcpStream, last_tx: Instant },
}

impl Link {
    pub fn kiss(address: &str, chan: u8) -> io::Result<Link> {
        let stream = connect(address, "KISS TCP port")?;
        Ok(Link::Kiss { stream, chan })
    }

    pub fn aprs_is(login: &AprsIsLogin) -> io::Result<Link> {
        let mut stream = connect(login.server, "APRS-IS server")?;
        let line = format!(
            "user {} pass {} vers aprsmsg {} filter {}\r\n",
            login.call,
            login.passcode,
            env!("CARGO_PKG_VERSION"),
            login.filter
        );
        stream.write_all(line.as_bytes())?;
        Ok(Link::AprsIs { stream, last_tx: Instant::now() })
    }

    /// Transmits one packet. The RF `path` is used only on the radio link;
    /// packets sent to APRS-IS carry the standard `TCPIP*` path.
    pub fn send(&mut self, src: &Address, tocall: &Address, path: &[Address], info: &[u8]) -> io::Result<()> {
        match self {
            Link::Kiss { stream, chan } => {
                let frame = UiFrame {
                    dest: tocall.clone(),
                    src: src.clone(),
                    digis: path.iter().map(|digi| (digi.clone(), false)).collect(),
                    info: info.to_vec(),
                };
                stream.write_all(&kiss::encode(*chan, &frame.encode()))
            }
            Link::AprsIs { stream, last_tx } => {
                let mut line = format!("{src}>{tocall},TCPIP*:").into_bytes();
                line.extend_from_slice(info);
                line.extend_from_slice(b"\r\n");
                stream.write_all(&line)?;
                *last_tx = Instant::now();
                Ok(())
            }
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        match self {
            Link::Kiss { .. } => None,
            Link::AprsIs { last_tx, .. } => Some(*last_tx + KEEPALIVE),
        }
    }

    /// Sends an APRS-IS keepalive comment when the link has been quiet.
    pub fn maintain(&mut self) -> io::Result<()> {
        if let Link::AprsIs { stream, last_tx } = self {
            if last_tx.elapsed() >= KEEPALIVE {
                stream.write_all(b"# aprsmsg keepalive\r\n")?;
                *last_tx = Instant::now();
            }
        }
        Ok(())
    }

    pub fn is_radio(&self) -> bool {
        matches!(self, Link::Kiss { .. })
    }

    /// Starts a thread that reads the link and hands events to `deliver`,
    /// which returns false once nobody is listening any more.
    pub fn spawn_reader<F>(&self, deliver: F) -> io::Result<()>
    where
        F: Fn(LinkEvent) -> bool + Send + 'static,
    {
        match self {
            Link::Kiss { stream, chan } => {
                let stream = stream.try_clone()?;
                let chan = *chan;
                thread::spawn(move || {
                    let reason = read_kiss(stream, chan, &deliver);
                    deliver(LinkEvent::Closed(reason));
                });
            }
            Link::AprsIs { stream, .. } => {
                let stream = stream.try_clone()?;
                thread::spawn(move || {
                    let reason = read_aprs_is(stream, &deliver);
                    deliver(LinkEvent::Closed(reason));
                });
            }
        }
        Ok(())
    }
}

fn connect(address: &str, what: &str) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(address)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot reach {what} at {address}: {e}")))?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

fn read_kiss(mut stream: TcpStream, chan: u8, deliver: &impl Fn(LinkEvent) -> bool) -> String {
    let mut decoder = kiss::Decoder::default();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return "Direwolf closed the KISS connection".to_owned(),
            Ok(n) => {
                for kiss_frame in decoder.push(&buf[..n]) {
                    if kiss_frame.channel != chan {
                        continue;
                    }
                    if let Some(frame) = UiFrame::decode(&kiss_frame.payload) {
                        if !deliver(LinkEvent::Heard(Heard::from_frame(frame))) {
                            return String::new();
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return format!("KISS connection error: {e}"),
        }
    }
}

fn read_aprs_is(stream: TcpStream, deliver: &impl Fn(LinkEvent) -> bool) -> String {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::with_capacity(MAX_LINE);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => return "APRS-IS server closed the connection".to_owned(),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return format!("APRS-IS connection error: {e}"),
        }
        while matches!(line.last(), Some(b'\r' | b'\n')) {
            line.pop();
        }
        let event = if line.starts_with(b"#") {
            let comment = String::from_utf8_lossy(&line[1..]).trim().to_owned();
            if !comment.starts_with("logresp") {
                continue;
            }
            LinkEvent::Notice(comment)
        } else {
            match Heard::parse_tnc2(&line) {
                Some(heard) => LinkEvent::Heard(heard),
                None => continue,
            }
        };
        if !deliver(event) {
            return String::new();
        }
    }
}

/// The standard APRS-IS passcode for a callsign (SSID ignored).
pub fn passcode(call: &str) -> u16 {
    let base = call.split('-').next().unwrap_or("").to_ascii_uppercase();
    let mut hash: u16 = 0x73E2;
    for pair in base.as_bytes().chunks(2) {
        hash ^= u16::from(pair[0]) << 8;
        if let Some(&low) = pair.get(1) {
            hash ^= u16::from(low);
        }
    }
    hash & 0x7FFF
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> Address {
        Address::parse(text).unwrap()
    }

    #[test]
    fn passcode_matches_reference() {
        assert_eq!(passcode("N0CALL"), 13023);
        assert_eq!(passcode("n0call-9"), 13023);
    }

    #[test]
    fn parses_aprs_is_line_from_rf() {
        let heard =
            Heard::parse_tnc2(b"SM0ABC-9>APRS,SK0RYG-1*,WIDE1*,qAR,SK0TM-10:!5921.09N/01757.36E>").unwrap();
        assert_eq!(heard.source, "SM0ABC-9");
        assert_eq!(heard.dest_call(), "APRS");
        assert_eq!(heard.header(), "SM0ABC-9>APRS,SK0RYG-1,WIDE1*,qAR,SK0TM-10");
        assert_eq!(heard.route(), "APRS-IS, iGate SK0TM-10");
        assert_eq!(heard.info, b"!5921.09N/01757.36E>");
    }

    #[test]
    fn parses_internet_only_message() {
        let heard = Heard::parse_tnc2(b"WXBOT>APRS,TCPIP*,qAC,T2TEST::SA0KAM-1 :ack12").unwrap();
        assert_eq!(heard.route(), "APRS-IS");
        assert_eq!(heard.repeated_by(), None);
        assert!(Heard::parse_tnc2(b"# comment without packet").is_none());
    }

    #[test]
    fn route_and_repeats_for_radio_frames() {
        let frame = UiFrame {
            dest: addr("APZRST"),
            src: addr("SA0KAM-1"),
            digis: vec![(addr("SK0TM-10"), true), (addr("WIDE1"), true), (addr("WIDE2-1"), false)],
            info: b">test".to_vec(),
        };
        let heard = Heard::from_frame(frame);
        assert_eq!(heard.header(), "SA0KAM-1>APZRST,SK0TM-10,WIDE1*,WIDE2-1");
        assert_eq!(heard.repeated_by().as_deref(), Some("SK0TM-10"));
        assert_eq!(heard.route(), "via SK0TM-10");
    }
}
