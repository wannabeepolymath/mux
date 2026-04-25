use std::time::Instant;

/// Status of a single stream within a client connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStatus {
    /// Request is in the global queue, not yet dispatched.
    Queued,
    /// Request has been dispatched to a backend and is actively streaming.
    Active,
    /// Stream completed successfully.
    Done,
    /// Stream was cancelled by the client.
    Cancelled,
    /// Stream failed after exhausting retries.
    Failed,
}

/// Per-stream state tracked by the client handler.
#[derive(Debug)]
pub struct StreamState {
    pub started_at: Instant,
    pub status: StreamStatus,
}

impl StreamState {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            status: StreamStatus::Queued,
        }
    }

    /// Whether this stream has reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            StreamStatus::Done | StreamStatus::Cancelled | StreamStatus::Failed
        )
    }
}
