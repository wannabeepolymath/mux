use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::types::BackendId;

use super::circuit::CircuitBreaker;
use super::connection::BackendWs;
use super::scoring::BackendScoring;
use super::state::BackendState;

/// Events sent TO a backend's lifecycle task.
#[derive(Debug)]
pub enum LifecycleEvent {
    /// A forwarding attempt completed; lifecycle updates scoring/circuit
    /// and decides reconnect cadence.
    ForwardDone(BackendOutcome),
    /// The circuit cooldown timer fired — the lifecycle task should attempt
    /// a recovery probe (i.e. a connect).
    CooldownExpired,
    /// Graceful shutdown signal (used by tests; never sent in production).
    #[allow(dead_code)]
    Shutdown,
}

/// Summary of a forward attempt's impact on the backend.
///
/// Smaller than `ForwardOutcome` because the lifecycle task only needs to
/// know what to record against scoring/circuit and whether to reconnect.
/// The dispatcher gets the full `ForwardOutcome` separately so it can
/// generate the user-visible response.
#[derive(Debug, Clone, Copy)]
pub enum BackendOutcome {
    /// Stream completed successfully. TTFC for scoring.
    Success { ttfc_ms: f64 },
    /// Backend failed (crashed, hung, malformed, errored). Counts toward
    /// circuit-open threshold.
    Failure,
    /// Backend explicitly refused with a "busy" error. No scoring change;
    /// the request is requeued without penalty.
    Busy,
    /// Client disconnected or cancelled mid-stream. No scoring change.
    ClientGoneOrCancel,
}

/// Per-backend shared state. The dispatcher holds an `Arc<BackendShard>`
/// for read-mostly access (state for routing, lock briefly to take slot);
/// the per-backend lifecycle task holds the same Arc and is the **single
/// writer** to circuit/scoring/slot/reconnect_attempt.
pub struct BackendShard {
    pub id: BackendId,
    pub addr: SocketAddr,
    /// Cached `addr.to_string()` for metric labels and log fields. Avoids
    /// re-allocating on every state transition. (See bottleneck #8.)
    pub addr_str: Arc<str>,

    /// Coarse state (Disconnected/Connecting/Ready/Busy). Atomic so the
    /// dispatcher's `find_best_backend` scan reads lock-free.
    state: AtomicU8,

    /// Warm WebSocket connection. Populated iff state == Ready (with the
    /// transient exception of dispatch's brief Ready→Busy + slot.take()
    /// done atomically under this lock).
    slot: Mutex<Option<BackendWs>>,

    /// Single-writer (lifecycle task) except for `record_*` methods which
    /// the lifecycle task also gates.
    circuit: Mutex<CircuitBreaker>,
    scoring: Mutex<BackendScoring>,

    /// Channel used by forward tasks and the cooldown timer to send events
    /// to the lifecycle task.
    lifecycle_tx: mpsc::Sender<LifecycleEvent>,

    /// Consecutive failed connect attempts; drives the connect-task backoff
    /// schedule. Reset to 0 on each successful Ready transition.
    pub reconnect_attempt: AtomicU32,
}

impl std::fmt::Debug for BackendShard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendShard")
            .field("id", &self.id)
            .field("addr", &self.addr)
            .field("state", &self.state())
            .finish()
    }
}

impl BackendShard {
    /// Construct a shard plus the receiver end of its lifecycle channel.
    pub fn new(
        id: BackendId,
        addr: SocketAddr,
        circuit_threshold: u32,
        circuit_cooldown: Duration,
    ) -> (Arc<Self>, mpsc::Receiver<LifecycleEvent>) {
        // Capacity: one terminal event per in-flight retry slot, plus a
        // little headroom for cooldown timer events. 16 is generous for
        // the assignment's scale (4 backends, max 4 retries).
        let (tx, rx) = mpsc::channel(16);
        let shard = Arc::new(Self {
            id,
            addr,
            addr_str: Arc::from(addr.to_string()),
            state: AtomicU8::new(state_to_u8(BackendState::Disconnected)),
            slot: Mutex::new(None),
            circuit: Mutex::new(CircuitBreaker::new(circuit_threshold, circuit_cooldown)),
            scoring: Mutex::new(BackendScoring::new()),
            lifecycle_tx: tx,
            reconnect_attempt: AtomicU32::new(0),
        });
        (shard, rx)
    }

    /// Sender handle for lifecycle events. Cloned by forward tasks and
    /// the cooldown timer.
    pub fn lifecycle_sender(&self) -> mpsc::Sender<LifecycleEvent> {
        self.lifecycle_tx.clone()
    }

    pub fn state(&self) -> BackendState {
        u8_to_state(self.state.load(Ordering::Acquire))
    }

    /// Lifecycle-only state setter.
    pub(crate) fn set_state(&self, s: BackendState) {
        self.state.store(state_to_u8(s), Ordering::Release);
    }

    /// Whether this backend can currently accept a new request: Ready state,
    /// warm slot present, and circuit allows it. The atomic read is cheap;
    /// the slot lock is held only briefly. The circuit lock is held briefly
    /// for the `can_attempt` mutation (Open → HalfOpen).
    pub fn is_available(&self) -> bool {
        if !matches!(self.state(), BackendState::Ready) {
            return false;
        }
        // Slot must be present.
        if self.slot.lock().unwrap().is_none() {
            return false;
        }
        self.circuit.lock().unwrap().can_attempt()
    }

    /// Atomically transition Ready → Busy and take the warm slot.
    /// Returns None if no warm slot was available (caller falls through to
    /// next backend in the scan).
    ///
    /// The state mutation and slot take happen under the same slot lock so
    /// two concurrent dispatch attempts cannot both win.
    pub fn take_for_dispatch(&self) -> Option<BackendWs> {
        let mut slot = self.slot.lock().unwrap();
        if !matches!(self.state(), BackendState::Ready) {
            return None;
        }
        let ws = slot.take()?;
        self.set_state(BackendState::Busy);
        Some(ws)
    }

    /// Lifecycle-only: install a freshly-connected ws and mark Ready.
    pub(crate) fn install_warm_ws(&self, ws: BackendWs) {
        let mut slot = self.slot.lock().unwrap();
        *slot = Some(ws);
        self.set_state(BackendState::Ready);
        self.reconnect_attempt.store(0, Ordering::Relaxed);
    }

    /// Lifecycle-only: clear any held ws (e.g. on failure transition).
    pub(crate) fn drop_slot(&self) {
        let mut slot = self.slot.lock().unwrap();
        *slot = None;
    }

    // --- read helpers used by routing + /health ---

    pub fn ttfc_ewma_ms(&self) -> f64 {
        self.scoring.lock().unwrap().ttfc_ewma_ms()
    }

    pub fn error_rate(&self) -> f64 {
        self.scoring.lock().unwrap().error_rate()
    }

    pub fn total_requests(&self) -> u64 {
        self.scoring.lock().unwrap().total_requests
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.circuit.lock().unwrap().consecutive_failures()
    }

    pub fn circuit_state(&self) -> super::circuit::CircuitState {
        self.circuit.lock().unwrap().state()
    }

    // --- write helpers used by the lifecycle task only ---

    pub(crate) fn record_ttfc_ms(&self, ms: f64) {
        self.scoring.lock().unwrap().record_ttfc(ms);
    }

    pub(crate) fn record_result(&self, success: bool) {
        self.scoring.lock().unwrap().record_result(success);
    }

    /// Returns (just_opened, current_cooldown). `just_opened` is true if the
    /// failure transitioned the circuit from Closed/HalfOpen to Open.
    pub(crate) fn record_circuit_failure(&self) -> (bool, Duration) {
        let mut circuit = self.circuit.lock().unwrap();
        let was_open_already =
            matches!(circuit.state(), super::circuit::CircuitState::Open);
        circuit.record_failure();
        let now_open =
            matches!(circuit.state(), super::circuit::CircuitState::Open);
        (now_open && !was_open_already, circuit.current_cooldown())
    }

    pub(crate) fn record_circuit_success(&self) {
        self.circuit.lock().unwrap().record_success();
    }

    /// Returns the new cooldown duration for the rescheduled timer.
    pub(crate) fn record_open_failure(&self) -> Duration {
        let mut circuit = self.circuit.lock().unwrap();
        circuit.record_open_failure();
        circuit.current_cooldown()
    }

    /// Used by lifecycle to check if a connect failure happened during the
    /// active cooldown window (= recovery probe path) vs normal closed-circuit
    /// reconnect.
    pub(crate) fn cooldown_elapsed(&self) -> bool {
        self.circuit.lock().unwrap().cooldown_elapsed()
    }
}

fn state_to_u8(s: BackendState) -> u8 {
    match s {
        BackendState::Connecting => 0,
        BackendState::Ready => 1,
        BackendState::Busy => 2,
        BackendState::Draining => 3,
        BackendState::Disconnected => 4,
    }
}

fn u8_to_state(v: u8) -> BackendState {
    match v {
        0 => BackendState::Connecting,
        1 => BackendState::Ready,
        2 => BackendState::Busy,
        3 => BackendState::Draining,
        _ => BackendState::Disconnected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shard_for_tests() -> Arc<BackendShard> {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let (shard, _rx) = BackendShard::new(BackendId(0), addr, 3, Duration::from_secs(1));
        shard
    }

    #[test]
    fn starts_disconnected_no_slot() {
        let s = shard_for_tests();
        assert_eq!(s.state(), BackendState::Disconnected);
        assert!(s.slot.lock().unwrap().is_none());
        assert!(!s.is_available());
    }

    #[test]
    fn take_for_dispatch_only_when_ready() {
        let s = shard_for_tests();
        assert!(s.take_for_dispatch().is_none());
    }

    #[test]
    fn addr_str_matches_addr_to_string() {
        let s = shard_for_tests();
        assert_eq!(s.addr_str.as_ref(), s.addr.to_string());
    }

    #[test]
    fn record_circuit_failure_signals_just_opened_once() {
        let s = shard_for_tests();
        let (a, _) = s.record_circuit_failure();
        let (b, _) = s.record_circuit_failure();
        let (c, _) = s.record_circuit_failure();
        assert!(!a);
        assert!(!b);
        assert!(c, "third failure should flip Closed → Open");
        let (d, _) = s.record_circuit_failure();
        assert!(!d, "subsequent failures while already Open must not signal");
    }
}
