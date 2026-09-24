//! AX.25 UI frames: the only frame type APRS uses.

use std::fmt;

const ADDR_LEN: usize = 7;
const MAX_DIGIS: usize = 8;
const CONTROL_UI: u8 = 0x03;
const POLL_FINAL: u8 = 0x10;
const PID_NO_LAYER3: u8 = 0xF0;
/// Command bit on destination/source, has-been-repeated bit on digipeaters.
const HIGH_BIT: u8 = 0x80;
const RESERVED_BITS: u8 = 0x60;
const EXTENSION_BIT: u8 = 0x01;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub call: String,
    pub ssid: u8,
}

impl Address {
    /// Parses `CALL` or `CALL-SSID` (1-6 alphanumerics, SSID 0-15).
    pub fn parse(text: &str) -> Result<Address, String> {
        let upper = text.trim().to_ascii_uppercase();
        let (call, ssid) = match upper.split_once('-') {
            Some((call, ssid)) => (
                call,
                ssid.parse::<u8>()
                    .map_err(|_| format!("invalid SSID in {text:?}"))?,
            ),
            None => (upper.as_str(), 0),
        };
        let call_ok = (1..=6).contains(&call.len()) && call.bytes().all(|b| b.is_ascii_alphanumeric());
        if !call_ok || ssid > 15 {
            return Err(format!("invalid callsign {text:?}"));
        }
        Ok(Address {
            call: call.to_owned(),
            ssid,
        })
    }

    fn encode(&self, high_bit: bool, last: bool, out: &mut Vec<u8>) {
        let mut field = [b' ' << 1; 6];
        for (slot, b) in field.iter_mut().zip(self.call.bytes()) {
            *slot = b << 1;
        }
        out.extend_from_slice(&field);
        let mut ssid_byte = RESERVED_BITS | (self.ssid << 1);
        if high_bit {
            ssid_byte |= HIGH_BIT;
        }
        if last {
            ssid_byte |= EXTENSION_BIT;
        }
        out.push(ssid_byte);
    }

    /// Returns the address and its raw SSID byte (for the flag bits).
    fn decode(field: &[u8]) -> Option<(Address, u8)> {
        let call: String = field[..6].iter().map(|&b| (b >> 1) as char).collect();
        let call = call.trim_end();
        if call.is_empty() || !call.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let ssid_byte = field[6];
        Some((
            Address {
                call: call.to_owned(),
                ssid: (ssid_byte >> 1) & 0x0F,
            },
            ssid_byte,
        ))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ssid == 0 {
            write!(f, "{}", self.call)
        } else {
            write!(f, "{}-{}", self.call, self.ssid)
        }
    }
}

pub struct UiFrame {
    pub dest: Address,
    pub src: Address,
    /// Digipeater path; the flag is the has-been-repeated bit.
    pub digis: Vec<(Address, bool)>,
    pub info: Vec<u8>,
}

impl UiFrame {
    /// Encodes the frame without FCS (the TNC adds it).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ADDR_LEN * (2 + self.digis.len()) + 2 + self.info.len());
        self.dest.encode(true, false, &mut out);
        self.src.encode(false, self.digis.is_empty(), &mut out);
        for (i, (digi, repeated)) in self.digis.iter().enumerate() {
            digi.encode(*repeated, i + 1 == self.digis.len(), &mut out);
        }
        out.push(CONTROL_UI);
        out.push(PID_NO_LAYER3);
        out.extend_from_slice(&self.info);
        out
    }

    /// Decodes a frame as delivered by a KISS TNC; `None` unless it is an APRS UI frame.
    pub fn decode(frame: &[u8]) -> Option<UiFrame> {
        let mut addresses = Vec::new();
        let mut pos = 0;
        loop {
            let (address, ssid_byte) = Address::decode(frame.get(pos..pos + ADDR_LEN)?)?;
            addresses.push((address, ssid_byte & HIGH_BIT != 0));
            pos += ADDR_LEN;
            if ssid_byte & EXTENSION_BIT != 0 {
                break;
            }
            if addresses.len() >= 2 + MAX_DIGIS {
                return None;
            }
        }
        if addresses.len() < 2 {
            return None;
        }
        let control = *frame.get(pos)?;
        let pid = *frame.get(pos + 1)?;
        if control & !POLL_FINAL != CONTROL_UI || pid != PID_NO_LAYER3 {
            return None;
        }
        let mut addresses = addresses.into_iter();
        let (dest, _) = addresses.next()?;
        let (src, _) = addresses.next()?;
        Some(UiFrame {
            dest,
            src,
            digis: addresses.collect(),
            info: frame[pos + 2..].to_vec(),
        })
    }

    /// TNC2 monitor header: `SRC>DEST,DIGI1,DIGI2*` (star on the last repeated hop).
    pub fn header(&self) -> String {
        let mut header = format!("{}>{}", self.src, self.dest);
        let last_repeated = self.digis.iter().rposition(|(_, repeated)| *repeated);
        for (i, (digi, _)) in self.digis.iter().enumerate() {
            header.push(',');
            header.push_str(&digi.to_string());
            if Some(i) == last_repeated {
                header.push('*');
            }
        }
        header
    }

    /// The last station that actually repeated this frame, skipping generic
    /// aliases such as WIDE1 that a digipeater marks as used.
    pub fn repeated_by(&self) -> Option<String> {
        self.digis
            .iter()
            .rev()
            .filter(|(_, repeated)| *repeated)
            .map(|(digi, _)| digi)
            .find(|digi| !is_generic_alias(&digi.call))
            .map(|digi| digi.to_string())
    }
}

fn is_generic_alias(call: &str) -> bool {
    ["WIDE", "TRACE", "RELAY", "TEMP", "ECHO", "GATE"]
        .iter()
        .any(|alias| call.starts_with(alias))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> Address {
        Address::parse(text).unwrap()
    }

    #[test]
    fn encodes_address_fields() {
        let frame = UiFrame {
            dest: addr("APZRST"),
            src: addr("SA0KAM-1"),
            digis: vec![],
            info: b"x".to_vec(),
        };
        let bytes = frame.encode();
        let shifted = |s: &[u8]| s.iter().map(|b| b << 1).collect::<Vec<_>>();
        assert_eq!(bytes[..6], shifted(b"APZRST")[..]);
        assert_eq!(bytes[6], 0xE0);
        assert_eq!(bytes[7..13], shifted(b"SA0KAM")[..]);
        assert_eq!(bytes[13], 0x60 | (1 << 1) | 1);
        assert_eq!(&bytes[14..], &[0x03, 0xF0, b'x']);
    }

    #[test]
    fn round_trip_with_path() {
        let frame = UiFrame {
            dest: addr("APZRST"),
            src: addr("SA0KAM-1"),
            digis: vec![
                (addr("SK0TM-10"), true),
                (addr("WIDE1"), true),
                (addr("WIDE2-1"), false),
            ],
            info: b":EMAIL-2  :hi{1".to_vec(),
        };
        let decoded = UiFrame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.header(), "SA0KAM-1>APZRST,SK0TM-10,WIDE1*,WIDE2-1");
        assert_eq!(decoded.repeated_by().as_deref(), Some("SK0TM-10"));
        assert_eq!(decoded.info, frame.info);
    }

    #[test]
    fn rejects_invalid_callsigns() {
        for bad in ["", "TOOLONG1", "SA0KAM-16", "SA0KAM-", "SA0/AM"] {
            assert!(Address::parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(addr("sa0kam-1").to_string(), "SA0KAM-1");
    }

    #[test]
    fn rejects_non_ui_frames() {
        let mut bytes = UiFrame {
            dest: addr("SM0YOS"),
            src: addr("SA0KAM"),
            digis: vec![],
            info: vec![],
        }
        .encode();
        bytes[14] = 0x3F; // SABM
        assert!(UiFrame::decode(&bytes).is_none());
    }
}
