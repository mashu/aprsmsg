//! Human-readable decoding of APRS information fields for the monitor view:
//! positions (plain, timestamped, compressed), Mic-E, objects, items, status,
//! messages and third-party packets relayed from the internet.

use std::fmt;

use crate::aprs::{self, Message};

const KNOTS_TO_KMH: f64 = 1.852;
const FEET_TO_M: f64 = 0.3048;
const MIC_E_ALTITUDE_OFFSET_M: f64 = 10_000.0;
const MOVING_KNOTS: f64 = 0.5;
const MAX_DEPTH: usize = 3;

pub struct Fix {
    pub lat: f64,
    pub lon: f64,
    pub table: u8,
    pub code: u8,
    pub course: Option<u16>,
    pub speed_knots: Option<f64>,
    pub altitude_m: Option<f64>,
    pub comment: String,
}

pub enum Packet {
    Position(Fix),
    MicE {
        fix: Fix,
        status: &'static str,
        device: Option<&'static str>,
    },
    Object {
        name: String,
        killed: bool,
        fix: Fix,
    },
    Item {
        name: String,
        killed: bool,
        fix: Fix,
    },
    Status(String),
    Message(Message),
    ThirdParty {
        source: String,
        inner: Box<Packet>,
    },
    Other(&'static str),
    Unknown,
}

/// Decodes an information field; `dest_call` is the AX.25 destination
/// callsign without SSID (Mic-E encodes the latitude there).
pub fn decode(dest_call: &str, info: &[u8]) -> Packet {
    decode_at(dest_call, info, 0)
}

/// Renders bytes for display, showing control characters as `<0x0d>`.
pub fn printable(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for c in String::from_utf8_lossy(bytes).chars() {
        if c.is_control() {
            out.push_str(&format!("<0x{:02x}>", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn decode_at(dest_call: &str, info: &[u8], depth: usize) -> Packet {
    let info = trim_line_end(info);
    let Some((&kind, body)) = info.split_first() else {
        return Packet::Unknown;
    };
    let decoded = match kind {
        b'!' | b'=' => position(body).map(Packet::Position),
        b'/' | b'@' => body.get(7..).and_then(position).map(Packet::Position),
        b'`' | b'\'' | 0x1C | 0x1D => mic_e(dest_call, body),
        b';' => object(body),
        b')' => item(body),
        b'>' => Some(Packet::Status(status_text(body))),
        b':' => aprs::parse_message(info).map(Packet::Message),
        b'}' if depth < MAX_DEPTH => third_party(body, depth),
        b'_' => Some(Packet::Other("Weather report")),
        b'T' if body.first() == Some(&b'#') => Some(Packet::Other("Telemetry")),
        b'<' => Some(Packet::Other("Station capabilities")),
        b'?' => Some(Packet::Other("Query")),
        b'$' => Some(Packet::Other("Raw GPS (NMEA)")),
        b'{' => Some(Packet::Other("User-defined data")),
        _ => None,
    };
    decoded.unwrap_or(Packet::Unknown)
}

fn trim_line_end(info: &[u8]) -> &[u8] {
    let end = info
        .iter()
        .rposition(|&b| b != b'\r' && b != b'\n')
        .map_or(0, |i| i + 1);
    &info[..end]
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ---------------------------------------------------------------------------
// Plain and compressed positions
// ---------------------------------------------------------------------------

fn position(data: &[u8]) -> Option<Fix> {
    let first = *data.first()?;
    if first.is_ascii_digit() || first == b' ' {
        uncompressed(data)
    } else {
        compressed(data)
    }
}

/// `DDMM.hhN` + table + `DDDMM.hhE` + symbol, then optional `CSE/SPD`.
fn uncompressed(data: &[u8]) -> Option<Fix> {
    if data.len() < 19 {
        return None;
    }
    let mut fix = Fix {
        lat: parse_lat(&data[0..8])?,
        lon: parse_lon(&data[9..18])?,
        table: data[8],
        code: data[18],
        course: None,
        speed_knots: None,
        altitude_m: None,
        comment: String::new(),
    };
    let mut rest = &data[19..];
    if let Some((course, speed)) = course_speed(rest) {
        fix.course = course;
        fix.speed_knots = Some(speed);
        rest = &rest[7..];
    }
    fix.comment = lossy(rest);
    take_feet_altitude(&mut fix);
    Some(fix)
}

/// Table, 4-char base-91 latitude, 4-char longitude, symbol, `cs`, type.
fn compressed(data: &[u8]) -> Option<Fix> {
    if data.len() < 13 {
        return None;
    }
    let table = match data[0] {
        t @ b'a'..=b'j' => t - b'a' + b'0',
        t => t,
    };
    let lat = 90.0 - f64::from(base91(&data[1..5])?) / 380_926.0;
    let lon = -180.0 + f64::from(base91(&data[5..9])?) / 190_463.0;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let mut fix = Fix {
        lat,
        lon,
        table,
        code: data[9],
        course: None,
        speed_knots: None,
        altitude_m: None,
        comment: lossy(&data[13..]),
    };
    let (c, s, t) = (data[10], data[11], data[12]);
    if c != b' ' {
        let (c33, s33, t33) = (c.checked_sub(33)?, s.checked_sub(33)?, t.checked_sub(33)?);
        let altitude_source = (t33 >> 3) & 0x03 == 2;
        if altitude_source {
            let exponent = i32::from(c33) * 91 + i32::from(s33);
            fix.altitude_m = Some(1.002_f64.powi(exponent) * FEET_TO_M);
        } else if c33 <= 89 {
            fix.course = Some(u16::from(c33) * 4);
            fix.speed_knots = Some(1.08_f64.powi(i32::from(s33)) - 1.0);
        }
    }
    take_feet_altitude(&mut fix);
    Some(fix)
}

fn parse_lat(field: &[u8]) -> Option<f64> {
    if field[4] != b'.' {
        return None;
    }
    let d = |i: usize| digit_or_ambiguous(field[i]);
    let degrees = d(0)? * 10 + d(1)?;
    let minutes = f64::from(d(2)? * 10 + d(3)?) + f64::from(d(5)? * 10 + d(6)?) / 100.0;
    if degrees > 90 || minutes >= 60.0 {
        return None;
    }
    let value = f64::from(degrees) + minutes / 60.0;
    match field[7] {
        b'N' => Some(value),
        b'S' => Some(-value),
        _ => None,
    }
}

fn parse_lon(field: &[u8]) -> Option<f64> {
    if field[5] != b'.' {
        return None;
    }
    let d = |i: usize| digit_or_ambiguous(field[i]);
    let degrees = d(0)? * 100 + d(1)? * 10 + d(2)?;
    let minutes = f64::from(d(3)? * 10 + d(4)?) + f64::from(d(6)? * 10 + d(7)?) / 100.0;
    if degrees > 180 || minutes >= 60.0 {
        return None;
    }
    let value = f64::from(degrees) + minutes / 60.0;
    match field[8] {
        b'E' => Some(value),
        b'W' => Some(-value),
        _ => None,
    }
}

/// Position ambiguity replaces trailing digits with spaces.
fn digit_or_ambiguous(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some(u32::from(b - b'0')),
        b' ' => Some(0),
        _ => None,
    }
}

/// `CCC/SSS`: course in degrees (000 = unknown) and speed in knots.
fn course_speed(rest: &[u8]) -> Option<(Option<u16>, f64)> {
    let field = rest.get(..7)?;
    if field[3] != b'/' {
        return None;
    }
    let number = |bytes: &[u8]| -> Option<u16> {
        bytes.iter().try_fold(0u16, |acc, &b| {
            b.is_ascii_digit().then(|| acc * 10 + u16::from(b - b'0'))
        })
    };
    let course = number(&field[..3])?;
    let speed = number(&field[4..])?;
    Some(((1..=360).contains(&course).then_some(course), f64::from(speed)))
}

/// Removes `/A=nnnnnn` (feet) from the comment and records it in metres.
fn take_feet_altitude(fix: &mut Fix) {
    let Some(start) = fix.comment.find("/A=") else { return };
    let Some(value) = fix.comment.get(start + 3..start + 9) else { return };
    let Ok(feet) = value.parse::<i32>() else { return };
    if fix.altitude_m.is_none() {
        fix.altitude_m = Some(f64::from(feet) * FEET_TO_M);
    }
    fix.comment.replace_range(start..start + 9, "");
}

fn base91(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0u32, |acc, &b| {
        let digit = b.checked_sub(33).filter(|&d| d < 91)?;
        Some(acc * 91 + u32::from(digit))
    })
}

// ---------------------------------------------------------------------------
// Mic-E: latitude and status bits in the destination, the rest in `info`
// ---------------------------------------------------------------------------

fn mic_e(dest_call: &str, body: &[u8]) -> Option<Packet> {
    let dest = dest_call.as_bytes();
    if dest.len() != 6 || body.len() < 8 {
        return None;
    }

    let mut digits = [0u32; 6];
    let mut message_kinds = [0u8; 3];
    for (i, &c) in dest.iter().enumerate() {
        let (digit, kind) = mic_e_char(c)?;
        digits[i] = digit;
        if i < 3 {
            message_kinds[i] = kind;
        }
    }
    let north = dest[3] >= b'P';
    let lon_offset = dest[4] >= b'P';
    let west = dest[5] >= b'P';

    let lat_minutes = f64::from(digits[2] * 10 + digits[3]) + f64::from(digits[4] * 10 + digits[5]) / 100.0;
    let lat = f64::from(digits[0] * 10 + digits[1]) + lat_minutes / 60.0;

    let field = |i: usize| i32::from(body[i]) - 28;
    let mut lon_deg = field(0) + if lon_offset { 100 } else { 0 };
    if (180..=189).contains(&lon_deg) {
        lon_deg -= 80;
    } else if (190..=199).contains(&lon_deg) {
        lon_deg -= 190;
    }
    let mut lon_min = field(1);
    if lon_min >= 60 {
        lon_min -= 60;
    }
    let lon_hundredths = field(2);
    let (sp, dc, se) = (field(3), field(4), field(5));
    let valid = (0..=179).contains(&lon_deg)
        && (0..=59).contains(&lon_min)
        && (0..=99).contains(&lon_hundredths)
        && sp >= 0
        && dc >= 0
        && se >= 0
        && lat <= 90.0;
    if !valid {
        return None;
    }
    let lon = f64::from(lon_deg) + (f64::from(lon_min) + f64::from(lon_hundredths) / 100.0) / 60.0;

    let mut speed = sp * 10 + dc / 10;
    if speed >= 800 {
        speed -= 800;
    }
    let mut course = (dc % 10) * 100 + se;
    if course >= 400 {
        course -= 400;
    }

    let (device, altitude_m, comment) = mic_e_extras(&body[8..]);
    let mut fix = Fix {
        lat: if north { lat } else { -lat },
        lon: if west { -lon } else { lon },
        table: body[7],
        code: body[6],
        course: u16::try_from(course).ok().filter(|c| (1..=360).contains(c)),
        speed_knots: Some(f64::from(speed)),
        altitude_m,
        comment,
    };
    take_feet_altitude(&mut fix);
    Some(Packet::MicE {
        fix,
        status: mic_e_status(message_kinds),
        device,
    })
}

/// Returns the digit and message-bit kind: 0 = bit clear, 1 = custom, 2 = standard.
fn mic_e_char(c: u8) -> Option<(u32, u8)> {
    match c {
        b'0'..=b'9' => Some((u32::from(c - b'0'), 0)),
        b'A'..=b'J' => Some((u32::from(c - b'A'), 1)),
        b'K' => Some((0, 1)),
        b'L' => Some((0, 0)),
        b'P'..=b'Y' => Some((u32::from(c - b'P'), 2)),
        b'Z' => Some((0, 2)),
        _ => None,
    }
}

fn mic_e_status(kinds: [u8; 3]) -> &'static str {
    const STANDARD: [&str; 8] = [
        "Emergency", "Priority", "Special", "Committed", "Returning", "In Service", "En Route", "Off Duty",
    ];
    const CUSTOM: [&str; 8] = [
        "Emergency", "Custom-6", "Custom-5", "Custom-4", "Custom-3", "Custom-2", "Custom-1", "Custom-0",
    ];
    let value = kinds.iter().fold(0usize, |acc, &k| acc << 1 | usize::from(k != 0));
    let custom = kinds.contains(&1);
    let standard = kinds.contains(&2);
    match (custom, standard) {
        (true, true) => "Unknown status",
        (true, false) => CUSTOM[value],
        _ => STANDARD[value],
    }
}

/// Splits the Mic-E comment into device type, altitude and remaining text.
fn mic_e_extras(raw: &[u8]) -> (Option<&'static str>, Option<f64>, String) {
    let mut text = lossy(raw);
    let prefix = text.chars().next();
    if matches!(prefix, Some('>' | ']' | '`' | '\'')) {
        text.remove(0);
    }

    let mut altitude = None;
    let bytes = text.as_bytes();
    if bytes.len() >= 4 && bytes[3] == b'}' {
        if let Some(value) = base91(&bytes[..3]) {
            altitude = Some(f64::from(value) - MIC_E_ALTITUDE_OFFSET_M);
            text.drain(..4);
        }
    }

    let device = match prefix {
        Some(kenwood @ ('>' | ']')) => {
            let model = match (kenwood, text.chars().last()) {
                ('>', Some('=')) => Some("Kenwood TH-D72"),
                ('>', Some('^')) => Some("Kenwood TH-D74"),
                ('>', Some('&')) => Some("Kenwood TH-D75"),
                (']', Some('=')) => Some("Kenwood TM-D710"),
                _ => None,
            };
            if model.is_some() {
                text.pop();
            }
            Some(model.unwrap_or(if kenwood == '>' { "Kenwood TH-D7A" } else { "Kenwood TM-D700" }))
        }
        Some(p @ ('`' | '\'')) => {
            let model = text.len().checked_sub(2).and_then(|at| {
                let suffix = text.get(at..)?;
                match (p, suffix) {
                    ('`', "_ ") => Some("Yaesu VX-8"),
                    ('`', "_\"") => Some("Yaesu FTM-350"),
                    ('`', "_#") => Some("Yaesu VX-8G"),
                    ('`', "_$") => Some("Yaesu FT1D"),
                    ('`', "_%") => Some("Yaesu FTM-400DR"),
                    ('`', "_)") => Some("Yaesu FTM-100D"),
                    ('`', "_(") => Some("Yaesu FT2D"),
                    ('\'', "|3") => Some("Byonics TinyTrak3"),
                    ('\'', "|4") => Some("Byonics TinyTrak4"),
                    _ => None,
                }
            });
            if model.is_some() {
                text.truncate(text.len() - 2);
            }
            model
        }
        _ => None,
    };
    (device, altitude, text)
}

// ---------------------------------------------------------------------------
// Objects, items, status, third-party
// ---------------------------------------------------------------------------

/// `;NAME_____*DDHHMMz` + position (name is exactly 9 characters).
fn object(body: &[u8]) -> Option<Packet> {
    let name = String::from_utf8_lossy(body.get(..9)?).trim_end().to_owned();
    let killed = match body.get(9)? {
        b'*' => false,
        b'_' => true,
        _ => return None,
    };
    let fix = position(body.get(17..)?)?;
    Some(Packet::Object { name, killed, fix })
}

/// `)NAME!` + position (3-9 character name, `!` live or `_` killed).
fn item(body: &[u8]) -> Option<Packet> {
    let end = body.iter().take(10).position(|&b| b == b'!' || b == b'_')?;
    if end < 3 {
        return None;
    }
    let fix = position(&body[end + 1..])?;
    Some(Packet::Item {
        name: lossy(&body[..end]),
        killed: body[end] == b'_',
        fix,
    })
}

/// Status text, without an optional leading `DDHHMMz` timestamp.
fn status_text(body: &[u8]) -> String {
    let timestamped = body.len() >= 7 && body[..6].iter().all(u8::is_ascii_digit) && body[6] == b'z';
    lossy(if timestamped { &body[7..] } else { body })
}

/// `}SRC>DEST,PATH:info` — a packet relayed from the internet by an iGate.
fn third_party(body: &[u8], depth: usize) -> Option<Packet> {
    let colon = body.iter().position(|&b| b == b':')?;
    let header = String::from_utf8_lossy(&body[..colon]);
    let (source, route) = header.split_once('>')?;
    let dest_call = route.split(',').next()?.split('-').next()?;
    let inner = decode_at(dest_call, &body[colon + 1..], depth + 1);
    Some(Packet::ThirdParty {
        source: source.trim().to_owned(),
        inner: Box::new(inner),
    })
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

impl fmt::Display for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Packet::Position(fix) => write!(f, "Position: {fix}"),
            Packet::MicE { fix, status, device } => {
                write!(f, "Mic-E")?;
                if let Some(device) = device {
                    write!(f, " {device}")?;
                }
                write!(f, ", {status}: {fix}")
            }
            Packet::Object { name, killed, fix } => {
                write!(f, "Object {name}{}: {fix}", if *killed { " (killed)" } else { "" })
            }
            Packet::Item { name, killed, fix } => {
                write!(f, "Item {name}{}: {fix}", if *killed { " (killed)" } else { "" })
            }
            Packet::Status(text) => write!(f, "Status: {text}"),
            Packet::Message(Message::Text { to, text, id, .. }) => {
                let kind = if to.starts_with("BLN") { "Bulletin" } else { "Message to" };
                write!(f, "{kind} {to}: {text}")?;
                if let Some(id) = id {
                    write!(f, " (#{id})")?;
                }
                Ok(())
            }
            Packet::Message(Message::Ack { to, id }) => write!(f, "Ack #{id} to {to}"),
            Packet::Message(Message::Rej { to, id }) => write!(f, "Reject #{id} to {to}"),
            Packet::ThirdParty { source, inner } => write!(f, "Via internet from {source}: {inner}"),
            Packet::Other(what) => write!(f, "{what}"),
            Packet::Unknown => write!(f, "Not decoded"),
        }
    }
}

impl fmt::Display for Fix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {} {}",
            symbol_label(self.table, self.code),
            format_angle(self.lat, 2, 'N', 'S'),
            format_angle(self.lon, 3, 'E', 'W')
        )?;
        if let Some(knots) = self.speed_knots.filter(|&k| k >= MOVING_KNOTS) {
            write!(f, ", {:.0} km/h", knots * KNOTS_TO_KMH)?;
            if let Some(course) = self.course {
                write!(f, ", course {course}°")?;
            }
        }
        if let Some(altitude) = self.altitude_m {
            write!(f, ", alt {altitude:.0} m")?;
        }
        let comment = self.comment.trim();
        if !comment.is_empty() {
            write!(f, " \"{comment}\"")?;
        }
        Ok(())
    }
}

/// Degrees and decimal minutes, e.g. `59°21.09'N` / `017°57.36'E`.
fn format_angle(value: f64, degree_width: usize, positive: char, negative: char) -> String {
    let hemisphere = if value < 0.0 { negative } else { positive };
    let hundredths_of_minutes = (value.abs() * 6000.0).round() as u64;
    let degrees = hundredths_of_minutes / 6000;
    let minutes = hundredths_of_minutes % 6000;
    format!(
        "{degrees:0degree_width$}°{:02}.{:02}'{hemisphere}",
        minutes / 100,
        minutes % 100
    )
}

fn symbol_label(table: u8, code: u8) -> String {
    let overlay = matches!(table, b'0'..=b'9' | b'A'..=b'Z').then_some(table as char);
    let name = match table {
        b'/' => primary_symbol(code),
        b'\\' => alternate_symbol(code),
        _ if overlay.is_some() => alternate_symbol(code),
        _ => None,
    };
    match (name, overlay) {
        (Some(name), Some(overlay)) => format!("{name} (overlay {overlay})"),
        (Some(name), None) => name.to_owned(),
        (None, _) => format!("symbol {}{}", table as char, code as char),
    }
}

fn primary_symbol(code: u8) -> Option<&'static str> {
    Some(match code {
        b'!' => "police station",
        b'#' => "digipeater",
        b'$' => "phone",
        b'%' => "DX cluster",
        b'&' => "HF gateway",
        b'\'' => "small aircraft",
        b'(' => "mobile satellite station",
        b')' => "wheelchair",
        b'*' => "snowmobile",
        b'+' => "Red Cross",
        b',' => "Boy Scouts",
        b'-' => "house",
        b'.' => "X",
        b'/' => "red dot",
        b'0'..=b'9' => "numbered circle",
        b':' => "fire",
        b';' => "campground",
        b'<' => "motorcycle",
        b'=' => "railroad engine",
        b'>' => "car",
        b'?' => "file server",
        b'@' => "hurricane",
        b'A' => "aid station",
        b'B' => "BBS",
        b'C' => "canoe",
        b'E' => "eyeball",
        b'F' => "tractor",
        b'G' => "grid square",
        b'H' => "hotel",
        b'I' => "TCP/IP",
        b'K' => "school",
        b'L' => "PC user",
        b'M' => "MacAPRS",
        b'N' => "NTS station",
        b'O' => "balloon",
        b'P' => "police",
        b'R' => "RV",
        b'S' => "space shuttle",
        b'T' => "SSTV",
        b'U' => "bus",
        b'V' => "ATV",
        b'W' => "weather service site",
        b'X' => "helicopter",
        b'Y' => "yacht",
        b'Z' => "WinAPRS",
        b'[' => "jogger",
        b'\\' => "triangle (DF)",
        b']' => "mailbox",
        b'^' => "large aircraft",
        b'_' => "weather station",
        b'`' => "dish antenna",
        b'a' => "ambulance",
        b'b' => "bicycle",
        b'c' => "incident command post",
        b'd' => "fire department",
        b'e' => "horse",
        b'f' => "fire truck",
        b'g' => "glider",
        b'h' => "hospital",
        b'i' => "IOTA",
        b'j' => "jeep",
        b'k' => "truck",
        b'l' => "laptop",
        b'm' => "Mic-E repeater",
        b'n' => "node",
        b'o' => "EOC",
        b'p' => "rover",
        b'q' => "grid square",
        b'r' => "repeater",
        b's' => "boat",
        b't' => "truck stop",
        b'u' => "semi truck",
        b'v' => "van",
        b'w' => "water station",
        b'x' => "X-APRS",
        b'y' => "Yagi at QTH",
        _ => return None,
    })
}

fn alternate_symbol(code: u8) -> Option<&'static str> {
    Some(match code {
        b'#' => "digipeater",
        b'&' => "gateway",
        b'>' => "car",
        b'_' => "weather site",
        b'a' => "ARES",
        b'k' => "SUV",
        b'u' => "truck",
        b'v' => "van",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn decodes_kenwood_mic_e() {
        // Heard on 144.800: SA0ANX-9>U9RQ09,WIDE1-1:`-U@mg{k/]"40}=<0x0d>
        let packet = decode("U9RQ09", b"`-U@mg{k/]\"40}=\r");
        let Packet::MicE { fix, status, device } = &packet else {
            panic!("expected Mic-E");
        };
        assert!(approx(fix.lat, 59.0 + 21.09 / 60.0));
        assert!(approx(fix.lon, 17.0 + 57.36 / 60.0));
        assert_eq!(fix.speed_knots, Some(17.0));
        assert_eq!(fix.course, Some(195));
        assert_eq!(fix.altitude_m, Some(25.0));
        assert_eq!((fix.table, fix.code), (b'/', b'k'));
        assert_eq!(*status, "In Service");
        assert_eq!(*device, Some("Kenwood TM-D710"));
        assert_eq!(
            packet.to_string(),
            "Mic-E Kenwood TM-D710, In Service: truck at 59°21.09'N 017°57.36'E, 31 km/h, course 195°, alt 25 m"
        );
    }

    #[test]
    fn decodes_uncompressed_position_with_extensions() {
        let packet = decode("APRS", b"!5921.09N/01757.36E>088/036/A=000100Test run");
        let Packet::Position(fix) = &packet else { panic!("expected position") };
        assert_eq!(fix.course, Some(88));
        assert_eq!(fix.speed_knots, Some(36.0));
        assert!(approx(fix.altitude_m.unwrap(), 30.48));
        assert_eq!(fix.comment, "Test run");
        assert_eq!(
            packet.to_string(),
            "Position: car at 59°21.09'N 017°57.36'E, 67 km/h, course 88°, alt 30 m \"Test run\""
        );
    }

    #[test]
    fn decodes_timestamped_and_ambiguous_positions() {
        let packet = decode("APRS", b"@092345z5921.  N/01757.  E_Weather");
        let Packet::Position(fix) = &packet else { panic!("expected position") };
        assert!(approx(fix.lat, 59.35));
        assert_eq!(fix.code, b'_');
    }

    #[test]
    fn decodes_compressed_position() {
        // Example from the APRS 1.0.1 specification.
        let Packet::Position(fix) = decode("APRS", b"=/5L!!<*e7>7P[") else {
            panic!("expected position")
        };
        assert!(approx(fix.lat, 49.5));
        assert!(approx(fix.lon, -72.75));
        assert_eq!(fix.course, Some(88));
        assert!((fix.speed_knots.unwrap() - 36.2).abs() < 0.1);
    }

    #[test]
    fn decodes_object_and_item() {
        let packet = decode("APRS", b";LEADER   *092345z4903.50N/07201.75W>088/036");
        assert!(matches!(&packet, Packet::Object { name, killed: false, .. } if name == "LEADER"));
        let packet = decode("APRS", b")AID #2!4903.50N/07201.75WA");
        assert!(matches!(&packet, Packet::Item { name, killed: false, .. } if name == "AID #2"));
    }

    #[test]
    fn decodes_third_party_message_and_status() {
        let packet = decode("APRX29", b"}EMAIL-2>APJIE4,TCPIP,SK0TM-10*::SA0KAM-1 :ack12");
        assert_eq!(packet.to_string(), "Via internet from EMAIL-2: Ack #12 to SA0KAM-1");
        assert_eq!(decode("APRS", b">Net tonight").to_string(), "Status: Net tonight");
        assert_eq!(decode("APRS", b">092345zOn air").to_string(), "Status: On air");
        assert_eq!(decode("APRS", b"garbage").to_string(), "Not decoded");
    }

    #[test]
    fn printable_escapes_control_characters() {
        assert_eq!(printable(b"abc\r"), "abc<0x0d>");
    }
}
