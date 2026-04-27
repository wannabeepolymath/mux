/// State machine for a backend connection.
///
/// We don't model `Connecting` or `Draining` as distinct states because
/// connections in this multiplexer are short-lived (one connection per request,
/// established lazily by the forwarding task). `Disconnected` covers the gap
/// between requests; `Busy` covers the entire span from "connecting" through
/// "draining". This keeps the state machine minimal while preserving correct
/// dispatch semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    /// Connected and idle, ready to accept a request.
    Ready,
    /// Currently processing a request.
    Busy,
    /// Not connected. May be in post-failure cooldown.
    Disconnected,
}

impl BackendState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::Disconnected => "disconnected",
        }
    }
}

impl std::fmt::Display for BackendState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
