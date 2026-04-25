use std::time::Instant;

use crate::types::StreamId;

/// Explicit state machine for a backend connection's lifecycle.
#[derive(Debug)]
pub enum BackendState {
    /// Establishing WebSocket connection to the backend.
    Connecting,
    /// Connected and idle, ready to accept a request.
    Ready { since: Instant },
    /// Currently processing a request.
    Busy { since: Instant, stream_id: StreamId },
    /// Request done, draining remaining data before disconnect.
    Draining { since: Instant },
    /// Not connected. Will reconnect.
    Disconnected,
}

impl BackendState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Busy { .. })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Ready { .. } => "ready",
            Self::Busy { .. } => "busy",
            Self::Draining { .. } => "draining",
            Self::Disconnected => "disconnected",
        }
    }
}

impl std::fmt::Display for BackendState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
