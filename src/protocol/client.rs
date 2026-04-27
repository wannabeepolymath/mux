use serde::{Deserialize, Serialize};

/// Messages sent from Client -> Multiplexer.
///
/// Additional fields in the wire JSON (e.g. `priority`) are ignored.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Start {
        stream_id: String,
        text: String,
        #[serde(default)]
        speaker_id: u32,
    },
    Cancel {
        stream_id: String,
    },
    Close,
}

/// Messages sent from Multiplexer -> Client (text frames)
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Queued {
        stream_id: String,
        queue_depth: usize,
    },
    Done {
        stream_id: String,
        audio_duration: f64,
        total_time: f64,
        rtf: f64,
    },
    Error {
        stream_id: String,
        message: String,
    },
}

impl ServerMessage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ServerMessage serialization cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_start_message_with_priority_ignored() {
        let json = r#"{"type":"start","stream_id":"s1","text":"Hello","speaker_id":0,"priority":10}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            ClientMessage::Start {
                stream_id,
                text,
                speaker_id,
            } => {
                assert_eq!(stream_id, "s1");
                assert_eq!(text, "Hello");
                assert_eq!(speaker_id, 0);
            }
            _ => panic!("expected Start"),
        }
    }

    #[test]
    fn parse_cancel_message() {
        let json = r#"{"type":"cancel","stream_id":"s1"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ClientMessage::Cancel { stream_id } if stream_id == "s1"));
    }

    #[test]
    fn parse_close_message() {
        let json = r#"{"type":"close"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ClientMessage::Close));
    }

    #[test]
    fn serialize_queued() {
        let msg = ServerMessage::Queued {
            stream_id: "s1".into(),
            queue_depth: 3,
        };
        let json = msg.to_json();
        assert!(json.contains("\"type\":\"queued\""));
        assert!(json.contains("\"stream_id\":\"s1\""));
    }

    #[test]
    fn serialize_done() {
        let msg = ServerMessage::Done {
            stream_id: "s1".into(),
            audio_duration: 5.2,
            total_time: 4.8,
            rtf: 1.08,
        };
        let json = msg.to_json();
        assert!(json.contains("\"type\":\"done\""));
    }

    #[test]
    fn serialize_error() {
        let msg = ServerMessage::Error {
            stream_id: "s1".into(),
            message: "backend unavailable after 2 retries".into(),
        };
        let json = msg.to_json();
        assert!(json.contains("\"type\":\"error\""));
    }
}
