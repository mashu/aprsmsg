//! Transport-neutral received packets (`Heard`) and the APRS-IS passcode.

use crate::ax25::UiFrame;

const GENERIC_ALIASES: [&str; 8] = ["WIDE", "TRACE", "RELAY", "TEMP", "ECHO", "GATE", "TCPIP", "TCPXX"];

#[derive(Clone, Debug)]
pub struct Hop {
    pub call: String,
    pub used: bool,
}

#[derive(Clone, Debug)]
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
                .map(|(call, used)| Hop {
                    call: call.to_string(),
                    used,
                })
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
    use crate::ax25::Address;

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
            digis: vec![
                (addr("SK0TM-10"), true),
                (addr("WIDE1"), true),
                (addr("WIDE2-1"), false),
            ],
            info: b">test".to_vec(),
        };
        let heard = Heard::from_frame(frame);
        assert_eq!(heard.header(), "SA0KAM-1>APZRST,SK0TM-10,WIDE1*,WIDE2-1");
        assert_eq!(heard.repeated_by().as_deref(), Some("SK0TM-10"));
        assert_eq!(heard.route(), "via SK0TM-10");
    }
}
