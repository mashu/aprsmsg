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
    /// Lenient like Direwolf: the callsign may be empty or contain any
    /// printable ASCII, because real stations transmit such frames.
    fn decode(field: &[u8]) -> Option<(Address, u8)> {
        let call: String = field[..6].iter().map(|&b| (b >> 1) as char).collect();
        let call = call.trim();
        if !call.bytes().all(|b| b.is_ascii_graphic()) {
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
        if dest.call.is_empty() || src.call.is_empty() {
            return None;
        }
        Some(UiFrame {
            dest,
            src,
            // Some iGate firmware inserts an empty path entry; skip it.
            digis: addresses.filter(|(digi, _)| !digi.call.is_empty()).collect(),
            info: frame[pos + 2..].to_vec(),
        })
    }
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
        assert_eq!(decoded.src, frame.src);
        assert_eq!(decoded.dest, frame.dest);
        assert_eq!(decoded.digis, frame.digis);
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
    fn accepts_frame_with_empty_path_entry() {
        // Heard from SM0RGQ-4: SM0RGQ-4>APMI06,:}SP9DAT-7>...::SA0KAM-1 :ack785
        let info = b"}SP9DAT-7>APLRFT,TCPIP,SM0RGQ-4*::SA0KAM-1 :ack785".to_vec();
        let frame = UiFrame {
            dest: addr("APMI06"),
            src: addr("SM0RGQ-4"),
            digis: vec![(Address { call: String::new(), ssid: 0 }, false)],
            info: info.clone(),
        };
        let decoded = UiFrame::decode(&frame.encode()).expect("frame should decode");
        assert!(decoded.digis.is_empty());
        assert_eq!(decoded.src.to_string(), "SM0RGQ-4");
        assert_eq!(decoded.info, info);
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
