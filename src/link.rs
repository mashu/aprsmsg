//! TCP links: Direwolf's KISS port (radio) or an APRS-IS server (internet).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use crate::ax25::{Address, UiFrame};
use crate::heard::Heard;
use crate::kiss;

/// APRS-IS servers drop silent clients; send a comment line this often.
const KEEPALIVE: Duration = Duration::from_secs(300);
const MAX_LINE: usize = 1024;

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
        Ok(Link::AprsIs {
            stream,
            last_tx: Instant::now(),
        })
    }

    /// Transmits one packet. The RF `path` is used only on the radio link;
    /// packets sent to APRS-IS carry the standard `TCPIP*` path.
    pub fn send(
        &mut self,
        src: &Address,
        tocall: &Address,
        path: &[Address],
        info: &[u8],
    ) -> io::Result<()> {
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
