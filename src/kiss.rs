//! KISS framing between host and TNC, as spoken on Direwolf's KISS TCP port.

const FEND: u8 = 0xC0;
const FESC: u8 = 0xDB;
const TFEND: u8 = 0xDC;
const TFESC: u8 = 0xDD;
const CMD_DATA: u8 = 0x00;
const MAX_FRAME: usize = 2048;

/// A KISS data frame: the radio channel and the AX.25 frame it carries.
pub struct KissFrame {
    pub channel: u8,
    pub payload: Vec<u8>,
}

/// Wraps an AX.25 frame as a KISS data frame for `channel` (0-15).
pub fn encode(channel: u8, frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 4);
    out.push(FEND);
    out.push((channel & 0x0F) << 4 | CMD_DATA);
    for &b in frame {
        match b {
            FEND => out.extend_from_slice(&[FESC, TFEND]),
            FESC => out.extend_from_slice(&[FESC, TFESC]),
            _ => out.push(b),
        }
    }
    out.push(FEND);
    out
}

/// Incremental decoder: feed it bytes as they arrive from the TCP stream.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    escaped: bool,
    overflow: bool,
}

impl Decoder {
    /// Consumes `bytes` and returns every data frame completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<KissFrame> {
        let mut frames = Vec::new();
        for &b in bytes {
            if b == FEND {
                if !self.overflow {
                    frames.extend(self.take_frame());
                }
                self.buf.clear();
                self.escaped = false;
                self.overflow = false;
                continue;
            }
            if self.overflow {
                continue;
            }
            if self.escaped {
                self.escaped = false;
                self.buf.push(match b {
                    TFEND => FEND,
                    TFESC => FESC,
                    other => other,
                });
            } else if b == FESC {
                self.escaped = true;
            } else {
                self.buf.push(b);
            }
            if self.buf.len() > MAX_FRAME {
                self.overflow = true;
            }
        }
        frames
    }

    fn take_frame(&self) -> Option<KissFrame> {
        let (&command, payload) = self.buf.split_first()?;
        if command & 0x0F != CMD_DATA || payload.is_empty() {
            return None;
        }
        Some(KissFrame {
            channel: command >> 4,
            payload: payload.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_escapes() {
        let payload = vec![0x01, FEND, 0x02, FESC, 0x03];
        let wire = encode(2, &payload);
        assert_eq!(
            wire,
            vec![FEND, 0x20, 0x01, FESC, TFEND, 0x02, FESC, TFESC, 0x03, FEND]
        );
        let frames = Decoder::default().push(&wire);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].channel, 2);
        assert_eq!(frames[0].payload, payload);
    }

    #[test]
    fn frame_split_across_reads() {
        let wire = encode(0, b"hello");
        let mut decoder = Decoder::default();
        assert!(decoder.push(&wire[..3]).is_empty());
        let frames = decoder.push(&wire[3..]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, b"hello");
    }

    #[test]
    fn ignores_command_and_empty_frames() {
        let mut decoder = Decoder::default();
        // TXDELAY command (type 1) and back-to-back FENDs carry no data.
        assert!(decoder.push(&[FEND, 0x01, 30, FEND, FEND, FEND]).is_empty());
    }
}
