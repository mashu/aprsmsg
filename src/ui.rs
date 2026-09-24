//! Terminal output for the CLI: one line per [`UiMsg`](crate::client::UiMsg).

use std::env;
use std::io::{self, IsTerminal};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::client::UiMsg;

const CALL_WIDTH: usize = 9;
/// Width of "08:51:07Z ← " plus the station column and its trailing space.
const BODY_INDENT: usize = 10 + 2 + CALL_WIDTH + 1;

#[derive(Clone, Copy)]
enum Style {
    Dim,
    Bold,
    Cyan,
    Green,
    Yellow,
    YellowText,
    Red,
    Blue,
}

impl Style {
    fn sgr(self) -> &'static str {
        match self {
            Style::Dim => "2",
            Style::Bold => "1",
            Style::Cyan => "1;36",
            Style::Green => "1;32",
            Style::Yellow => "1;33",
            Style::YellowText => "33",
            Style::Red => "1;31",
            Style::Blue => "1;34",
        }
    }
}

pub struct Ui {
    color: bool,
}

impl Ui {
    pub fn new(no_color: bool) -> Ui {
        let color = !no_color && env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal();
        Ui { color }
    }

    pub fn apply(&self, msg: &UiMsg) {
        match msg {
            UiMsg::Info(text) => self.info(text),
            UiMsg::Error(text) => self.error(text),
            UiMsg::Sent {
                to,
                id,
                text,
                attempt,
                max,
            } => self.sent(to, id, text, *attempt, *max),
            UiMsg::Retry {
                to,
                id,
                attempt,
                max,
            } => self.retry(to, id, *attempt, *max),
            UiMsg::Repeated { digi, id } => self.repeated(digi, id),
            UiMsg::Delivered { from, id, tries } => self.delivered(from, id, *tries),
            UiMsg::Rejected { from, id } => self.rejected(from, id),
            UiMsg::GaveUp { to, id, tries } => self.gave_up(to, id, *tries),
            UiMsg::Cancelled { to, id } => self.cancelled(to, id),
            UiMsg::Incoming {
                from,
                text,
                id,
                route,
            } => self.incoming(from, text, id.as_deref(), route),
            UiMsg::Monitor {
                station,
                summary,
                route,
            } => self.monitor(station, summary, route),
            UiMsg::Raw(frame) => self.raw(frame),
            UiMsg::Print(text) => println!("{text}"),
        }
    }

    fn paint(&self, style: Style, text: &str) -> String {
        if self.color && !text.is_empty() {
            format!("\x1b[{}m{text}\x1b[0m", style.sgr())
        } else {
            text.to_owned()
        }
    }

    fn event(&self, marker: &str, marker_style: Style, station: &str, body: &str, detail: &str) {
        let mut line = format!(
            "{} {} {}",
            self.paint(Style::Dim, &stamp()),
            self.paint(marker_style, marker),
            self.paint(Style::Bold, &format!("{station:<CALL_WIDTH$}"))
        );
        if !body.is_empty() {
            line.push(' ');
            line.push_str(body);
        }
        if !detail.is_empty() {
            line.push_str("  ");
            line.push_str(&self.paint(Style::Dim, detail));
        }
        println!("{line}");
    }

    fn info(&self, text: &str) {
        println!(
            "{} {}",
            self.paint(Style::Dim, &stamp()),
            self.paint(Style::Dim, text)
        );
    }

    fn error(&self, text: &str) {
        println!(
            "{} {}",
            self.paint(Style::Dim, &stamp()),
            self.paint(Style::Red, text)
        );
    }

    fn sent(&self, to: &str, id: &str, text: &str, attempt: u32, max: u32) {
        self.event(
            "→",
            Style::Cyan,
            to,
            &format!("#{id} {text}"),
            &format!("try {attempt}/{max}"),
        );
    }

    fn retry(&self, to: &str, id: &str, attempt: u32, max: u32) {
        self.event(
            "→",
            Style::Cyan,
            to,
            &format!("#{id}"),
            &format!("retry {attempt}/{max}"),
        );
    }

    fn repeated(&self, digi: &str, id: &str) {
        self.event("↻", Style::Blue, digi, &format!("repeated your #{id}"), "");
    }

    fn delivered(&self, from: &str, id: &str, tries: u32) {
        let word = if tries == 1 { "try" } else { "tries" };
        let body = self.paint(Style::Green, &format!("#{id} delivered"));
        self.event(
            "✓",
            Style::Green,
            from,
            &body,
            &format!("after {tries} {word}"),
        );
    }

    fn rejected(&self, from: &str, id: &str) {
        let body = self.paint(Style::Red, &format!("#{id} rejected"));
        self.event("✗", Style::Red, from, &body, "");
    }

    fn gave_up(&self, to: &str, id: &str, tries: u32) {
        let body = self.paint(Style::Red, &format!("#{id} not delivered"));
        self.event(
            "✗",
            Style::Red,
            to,
            &body,
            &format!("no ack after {tries} tries"),
        );
    }

    fn cancelled(&self, to: &str, id: &str) {
        self.event(
            "✗",
            Style::Dim,
            to,
            &format!("#{id} cancelled"),
            "no more retries",
        );
    }

    fn incoming(&self, from: &str, text: &str, id: Option<&str>, route: &str) {
        let detail = match id {
            Some(id) => format!("#{id} {route}"),
            None => route.to_owned(),
        };
        self.event(
            "←",
            Style::Yellow,
            from,
            &self.paint(Style::YellowText, text),
            &detail,
        );
    }

    fn monitor(&self, station: &str, summary: &str, route: &str) {
        self.event("·", Style::Dim, station, summary, route);
    }

    fn raw(&self, frame: &str) {
        println!("{:BODY_INDENT$}{}", "", self.paint(Style::Dim, frame));
    }
}

fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        % 86_400;
    format!("{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}
