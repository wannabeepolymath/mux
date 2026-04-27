use serde::Deserialize;

/// Parsed text-frame messages received from a backend worker.
///
/// Binary frames (audio data) are forwarded directly without going through
/// this enum.
#[derive(Debug)]
pub enum BackendMessage {
    Queued,
    Done {
        audio_duration: f64,
    },
    Error {
        message: String,
    },
    Unknown(String),
}

#[derive(Debug, Deserialize)]
struct RawBackendText {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    audio_duration: f64,
    #[serde(default)]
    message: Option<String>,
}

impl BackendMessage {
    /// Parse a text frame from the backend into a structured message.
    pub fn from_text(text: &str) -> Self {
        let raw: RawBackendText = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(_) => return Self::Unknown(text.to_owned()),
        };

        match raw.msg_type.as_str() {
            "queued" => Self::Queued,
            "done" => Self::Done {
                audio_duration: raw.audio_duration,
            },
            "error" => Self::Error {
                message: raw.message.unwrap_or_else(|| "unknown backend error".into()),
            },
            _ => Self::Unknown(text.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_queued() {
        let msg = BackendMessage::from_text(r#"{"type":"queued","queue_depth":0}"#);
        assert!(matches!(msg, BackendMessage::Queued));
    }

    #[test]
    fn parse_done() {
        let msg = BackendMessage::from_text(
            r#"{"type":"done","audio_duration":3.2,"total_time":2.5,"rtf":1.28,"total_bytes":153600}"#,
        );
        match msg {
            BackendMessage::Done { audio_duration } => {
                assert!((audio_duration - 3.2).abs() < 0.01);
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn parse_error() {
        let msg = BackendMessage::from_text(r#"{"type":"error","message":"Worker busy"}"#);
        assert!(
            matches!(msg, BackendMessage::Error { message } if message == "Worker busy")
        );
    }

    #[test]
    fn parse_invalid_json() {
        let msg = BackendMessage::from_text("{invalid json [[[");
        assert!(matches!(msg, BackendMessage::Unknown(_)));
    }

    #[test]
    fn parse_unknown_type() {
        let msg = BackendMessage::from_text(r#"{"type":"unexpected_galaxy","data":42}"#);
        assert!(matches!(msg, BackendMessage::Unknown(_)));
    }
}
