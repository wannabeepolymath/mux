use std::net::SocketAddr;

use crate::types::BackendId;

use super::circuit::CircuitBreaker;
use super::scoring::BackendScoring;
use super::state::BackendState;

/// Represents a single backend worker and all its associated metadata.
#[derive(Debug)]
pub struct BackendConn {
    pub id: BackendId,
    pub addr: SocketAddr,
    pub state: BackendState,
    pub circuit: CircuitBreaker,
    pub scoring: BackendScoring,
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
        }
    }

    /// Whether this backend can accept a new request right now.
    pub fn is_available(&mut self) -> bool {
        self.state.is_ready() && self.circuit.can_attempt()
    }
}
