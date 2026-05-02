/// State machine for a backend connection.
///
/// `Connecting` and `Draining` are explicit phases that align with the
/// assignment spec's required state machine. `Draining` is what makes the
/// multiplexer the passive TCP closer (we drain reads to receive the peer's
/// FIN before our own drop sends ours).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    /// Connect handshake in progress (TCP+WS).
    Connecting,
    /// Warm connection in the slot, idle, eligible for dispatch.
    Ready,
    /// Forwarding task is using the connection.
    Busy,
    /// Forwarding task is draining the close handshake (passive-close window).
    Draining,
    /// No connection, no in-flight connect. Transient — should immediately
    /// be followed by spawning a connect task (unless circuit is open).
    Disconnected,
}

impl BackendState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::Draining => "draining",
            Self::Disconnected => "disconnected",
        }
    }
}

impl std::fmt::Display for BackendState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
