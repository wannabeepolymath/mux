use thiserror::Error;

#[derive(Debug, Error)]
pub enum MuxError {
    #[error("backend unavailable")]
    BackendUnavailable,

    #[error("backend hung (no data within timeout)")]
    BackendHung,

    #[error("backend crashed mid-stream")]
    BackendCrashed,

    #[error("backend sent malformed response: {0}")]
    MalformedResponse(String),

    #[error("queue full ({depth}/{max})")]
    QueueFull { depth: usize, max: usize },

    #[error("stream limit exceeded ({limit} per connection)")]
    StreamLimitExceeded { limit: usize },

    #[error("backend unavailable after {attempts} retries")]
    RetryExhausted { attempts: u32 },

    #[error("client disconnected")]
    ClientDisconnected,

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("stream not found: {0}")]
    StreamNotFound(String),

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("internal: {0}")]
    Internal(String),
}
