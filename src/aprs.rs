//! APRS messages (APRS 1.0.1 chapter 14) including the reply-ack extension.

pub const MAX_TEXT: usize = 67;
const ADDRESSEE_LEN: usize = 9;
const MAX_ID_LEN: usize = 5;
const MAX_THIRD_PARTY_DEPTH: usize = 3;

#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Text {
        to: String,
        text: String,
        id: Option<String>,
        /// Reply-ack: acknowledges one of our messages inside a new message.
        reply_ack: Option<String>,
    },
    Ack {
        to: String,
        id: String,
    },
    Rej {
        to: String,
        id: String,
    },
}

/// Builds the information field `:ADDRESSEE:text{id`.
pub fn format_message(to: &str, text: &str, id: &str) -> Result<Vec<u8>, String> {
    validate_addressee(to)?;
    if text.is_empty() {
        return Err("empty message".into());
    }
    if text.len() > MAX_TEXT {
        return Err(format!(
            "message is {} bytes; APRS allows at most {MAX_TEXT}",
            text.len()
        ));
    }
    if let Some(c) = text.chars().find(|&c| matches!(c, '|' | '~' | '{') || c.is_control()) {
        return Err(format!("character {c:?} is not allowed in APRS messages"));
    }
    if !is_message_id(id) {
        return Err(format!("invalid message id {id:?}"));
    }
    Ok(format!(":{to:<ADDRESSEE_LEN$}:{text}{{{id}").into_bytes())
}

/// Builds the information field `:ADDRESSEE:ackID`.
pub fn format_ack(to: &str, id: &str) -> Result<Vec<u8>, String> {
    validate_addressee(to)?;
    if !is_message_id(id) {
        return Err(format!("invalid message id {id:?}"));
    }
    Ok(format!(":{to:<ADDRESSEE_LEN$}:ack{id}").into_bytes())
}

/// Parses an information field as an APRS message, ack or rej.
pub fn parse_message(info: &[u8]) -> Option<Message> {
    if info.len() < ADDRESSEE_LEN + 2 || info[0] != b':' || info[ADDRESSEE_LEN + 1] != b':' {
        return None;
    }
    let to = String::from_utf8_lossy(&info[1..=ADDRESSEE_LEN]).trim().to_owned();
    let body = String::from_utf8_lossy(&info[ADDRESSEE_LEN + 2..]);
    let body = body.trim_end_matches(['\r', '\n']);

    if let Some(id) = body.strip_prefix("ack").and_then(parse_ack_id) {
        return Some(Message::Ack { to, id });
    }
    if let Some(id) = body.strip_prefix("rej").and_then(parse_ack_id) {
        return Some(Message::Rej { to, id });
    }

    let (text, id, reply_ack) = split_message_id(body);
    Some(Message::Text {
        to,
        text: text.to_owned(),
        id,
        reply_ack,
    })
}

/// Unwraps third-party packets (`}SRC>DEST,PATH:info`), which is how iGates
/// relay messages from the internet to RF. Returns the originating source and
/// the inner information field.
pub fn unwrap_third_party(source: &str, info: &[u8]) -> (String, Vec<u8>) {
    let mut source = source.to_owned();
    let mut info = info.to_vec();
    for _ in 0..MAX_THIRD_PARTY_DEPTH {
        let Some(inner) = info.strip_prefix(b"}") else { break };
        let Some(colon) = inner.iter().position(|&b| b == b':') else { break };
        let header = String::from_utf8_lossy(&inner[..colon]);
        let Some((src, _)) = header.split_once('>') else { break };
        let src = src.trim().to_owned();
        let rest = inner[colon + 1..].to_vec();
        source = src;
        info = rest;
    }
    (source, info)
}

/// Splits `text{MM}AA` into text, message id MM and reply-ack AA.
fn split_message_id(body: &str) -> (&str, Option<String>, Option<String>) {
    let Some(brace) = body.rfind('{') else {
        return (body, None, None);
    };
    let tail = &body[brace + 1..];
    let (id, reply_ack) = match tail.split_once('}') {
        Some((id, reply_ack)) => (id, Some(reply_ack)),
        None => (tail, None),
    };
    if !is_message_id(id) {
        return (body, None, None);
    }
    let reply_ack = reply_ack.filter(|ack| is_message_id(ack)).map(str::to_owned);
    (&body[..brace], Some(id.to_owned()), reply_ack)
}

fn parse_ack_id(rest: &str) -> Option<String> {
    let id = rest.split_once('}').map_or(rest, |(id, _)| id).trim_end();
    is_message_id(id).then(|| id.to_owned())
}

fn is_message_id(id: &str) -> bool {
    (1..=MAX_ID_LEN).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn validate_addressee(to: &str) -> Result<(), String> {
    let ok = (1..=ADDRESSEE_LEN).contains(&to.len()) && to.bytes().all(|b| b.is_ascii_graphic() && b != b':');
    if ok {
        Ok(())
    } else {
        Err(format!("invalid addressee {to:?} (1-{ADDRESSEE_LEN} characters)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(to: &str, text: &str, id: Option<&str>, reply_ack: Option<&str>) -> Message {
        Message::Text {
            to: to.into(),
            text: text.into(),
            id: id.map(Into::into),
            reply_ack: reply_ack.map(Into::into),
        }
    }

    #[test]
    fn formats_padded_message_and_ack() {
        assert_eq!(
            format_message("EMAIL-2", "a@b.se hi", "12").unwrap(),
            b":EMAIL-2  :a@b.se hi{12"
        );
        assert_eq!(
            format_message("SM0YOS-12", "hi", "7").unwrap(),
            b":SM0YOS-12:hi{7"
        );
        assert_eq!(format_ack("EMAIL-2", "AB").unwrap(), b":EMAIL-2  :ackAB");
    }

    #[test]
    fn rejects_invalid_messages() {
        assert!(format_message("EMAIL-2", &"x".repeat(68), "1").is_err());
        assert!(format_message("EMAIL-2", "a{b", "1").is_err());
        assert!(format_message("EMAIL-2", "a|b", "1").is_err());
        assert!(format_message("EMAIL-2", "", "1").is_err());
        assert!(format_message("TOOLONGCALL", "hi", "1").is_err());
        assert!(format_message("EMAIL-2", "hi", "123456").is_err());
    }

    #[test]
    fn parses_ack_and_rej() {
        assert_eq!(
            parse_message(b":SA0KAM-1 :ack12"),
            Some(Message::Ack { to: "SA0KAM-1".into(), id: "12".into() })
        );
        assert_eq!(
            parse_message(b":SA0KAM-1 :rej7\r"),
            Some(Message::Rej { to: "SA0KAM-1".into(), id: "7".into() })
        );
        assert_eq!(
            parse_message(b":SA0KAM-1 :ack12}"),
            Some(Message::Ack { to: "SA0KAM-1".into(), id: "12".into() })
        );
    }

    #[test]
    fn parses_text_with_id_and_reply_ack() {
        assert_eq!(
            parse_message(b":SA0KAM-1 :Email sent{AB}12"),
            Some(text("SA0KAM-1", "Email sent", Some("AB"), Some("12")))
        );
        assert_eq!(
            parse_message(b":SA0KAM-1 :hello{3"),
            Some(text("SA0KAM-1", "hello", Some("3"), None))
        );
        assert_eq!(
            parse_message(b":SA0KAM   :no id here"),
            Some(text("SA0KAM", "no id here", None, None))
        );
        assert_eq!(
            parse_message(b":SA0KAM   :acknowledged"),
            Some(text("SA0KAM", "acknowledged", None, None))
        );
        assert_eq!(parse_message(b"!5924.00N/01757.00E-"), None);
    }

    #[test]
    fn unwraps_third_party_packets() {
        let (source, info) = unwrap_third_party(
            "SK0TM-10",
            b"}EMAIL-2>APJIE4,TCPIP,SK0TM-10*::SA0KAM-1 :ack12",
        );
        assert_eq!(source, "EMAIL-2");
        assert_eq!(
            parse_message(&info),
            Some(Message::Ack { to: "SA0KAM-1".into(), id: "12".into() })
        );
        let (source, info) = unwrap_third_party("SM0YOS-12", b":SA0KAM-1 :hi{1");
        assert_eq!(source, "SM0YOS-12");
        assert_eq!(info, b":SA0KAM-1 :hi{1");
    }
}
