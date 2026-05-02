use std::net::SocketAddr;

use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::types::BackendId;

use super::circuit::CircuitBreaker;
use super::scoring::BackendScoring;
use super::state::BackendState;

/// The owned WebSocket connection type held in a backend's warm slot.
pub type BackendWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Represents a single backend worker and all its associated metadata.
#[derive(Debug)]
pub struct BackendConn {
    pub id: BackendId,
    pub addr: SocketAddr,
    pub state: BackendState,
    pub circuit: CircuitBreaker,
    pub scoring: BackendScoring,
    /// Warm WebSocket connection. Populated iff `state == Ready`.
    pub conn: Option<BackendWs>,
    /// Consecutive failed connect attempts; drives backoff schedule.
    /// Reset to 0 on every successful ConnectionEstablished.
    pub reconnect_attempt: u32,
}

impl BackendConn {
    pub fn new(
        id: BackendId,
        addr: SocketAddr,
        circuit_threshold: u32,
        circuit_cooldown: std::time::Duration,
    ) -> Self {
        Self {
            id,
            addr,
            state: BackendState::Disconnected,
            circuit: CircuitBreaker::new(circuit_threshold, circuit_cooldown),
            scoring: BackendScoring::new(),
            conn: None,
            reconnect_attempt: 0,
        }
    }

    /// Whether this backend can accept a new request right now.
    /// Requires both a warm slot (state == Ready) and a closed/half-open circuit.
    pub fn is_available(&mut self) -> bool {
        #[cfg(debug_assertions)]
        self.assert_invariants();
        self.state.is_ready() && self.conn.is_some() && self.circuit.can_attempt()
    }

    /// Atomically transition Ready → Busy and take the warm slot.
    /// Returns None if no warm slot was available (caller should re-queue).
    /// The state mutation and slot take happen together so the invariant
    /// `conn.is_some() ⇔ state == Ready` is never transiently violated.
    pub fn take_for_dispatch(&mut self) -> Option<BackendWs> {
        if !matches!(self.state, BackendState::Ready) || self.conn.is_none() {
            return None;
        }
        self.state = BackendState::Busy;
        self.conn.take()
    }

    /// Debug-only check: warm slot must be present iff state is Ready.
    /// The spec invariant — `conn.is_some() ⇔ state == Ready` — is documented
    /// in §3.1 of the design doc; this is the runtime guard.
    #[cfg(debug_assertions)]
    pub fn assert_invariants(&self) {
        debug_assert_eq!(
            self.conn.is_some(),
            matches!(self.state, BackendState::Ready),
            "BackendConn invariant violated: conn.is_some()={}, state={:?}",
            self.conn.is_some(),
            self.state,
        );
    }
}
