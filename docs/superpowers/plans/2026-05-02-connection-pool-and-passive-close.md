# Connection Pool with Passive-Close Discipline — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor the multiplexer so the dispatcher owns a per-backend warm WebSocket connection in an explicit `Connecting → Ready → Busy → Draining → Disconnected → Connecting` state machine, eliminating `EADDRNOTAVAIL` under chaos load by making the multiplexer the passive TCP closer in ≥95% of backend connections.

**Architecture:** `Dispatcher` (already a single-threaded actor) gains an `Option<BackendWs>` slot per backend. Forwarding tasks receive `ws` by value, drain the close handshake before drop, and signal the dispatcher to reconnect via two new events (`ConnectionEstablished`, `ConnectionFailed`). The `Draining` state encodes the passive-close fix.

**Tech Stack:** Rust 2024, tokio 1.x, tokio-tungstenite 0.26, futures-util 0.3.

**Spec:** `docs/superpowers/specs/2026-05-02-connection-pool-and-passive-close-design.md`.

**Branch:** `eph` (already isolated from `main`).

---

## File Structure

| File | Role | Status |
|---|---|---|
| `src/backend/state.rs` | `BackendState` enum and helpers | Modify (add Connecting/Draining) |
| `src/backend/connection.rs` | `BackendConn` struct (id, state, circuit, scoring) + new owned `conn` slot | Modify |
| `src/backend/mod.rs` | Re-exports | Modify (add type alias re-export) |
| `src/forward.rs` | Per-request forwarding loop. Will no longer open connections — receives `ws` by value | Heavy modify |
| `src/dispatch/mod.rs` | Dispatcher actor. Adds connect_task spawning, new event handlers, removes 500ms recovery timer | Heavy modify |
| `src/dispatch/connect.rs` | New file — connect_task + backoff helper | Create |
| `src/health.rs` | Surfaces backend states in `/health` | No code change required (uses `state.name()`) |
| `src/config.rs` | `drain_timeout_ms`, `connect_timeout_secs` CLI args | Modify |
| `src/main.rs` | Pass-through | No change |

---

## Pre-flight check

- [ ] **Step 0.1:** Confirm working tree is clean and on branch `eph`.

```bash
git status
git rev-parse --abbrev-ref HEAD
```

Expected: clean tree, branch `eph`.

- [ ] **Step 0.2:** Run baseline tests; record they pass.

```bash
cargo test --release
```

Expected: `test result: ok. 30 passed; 0 failed`.

---

## Task 1: Expand `BackendState` with `Connecting` and `Draining`

**Files:**
- Modify: `src/backend/state.rs`

This task is purely additive — no existing match site is exhaustive on `BackendState`, so adding variants won't break anything. Verified via `grep -rn "match.*BackendState\b" src/` showing only `matches!()` usage (non-exhaustive).

- [ ] **Step 1.1: Replace the contents of `src/backend/state.rs` with:**

```rust
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
```

- [ ] **Step 1.2: Build to confirm no compile errors.**

```bash
cargo build --release 2>&1 | tail -20
```

Expected: `Finished release`. No errors.

- [ ] **Step 1.3: Run all tests.**

```bash
cargo test --release
```

Expected: `30 passed; 0 failed`.

- [ ] **Step 1.4: Commit.**

```bash
git add src/backend/state.rs
git commit -m "$(cat <<'EOF'
backend: expand BackendState with Connecting and Draining

Aligns the state machine with the assignment spec. No call-site changes
yet — variants are added, all helpers updated. Existing matches use
matches!() (non-exhaustive) so this is purely additive.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add `drain_timeout_ms` and `connect_timeout_secs` to config

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 2.1: Add fields to `CliArgs` in `src/config.rs`.** Insert after the existing `max_retries` arg (around line 43):

```rust
    /// Connection drain timeout in ms. Bounded read-to-EOF window after
    /// the terminal frame so the multiplexer ends as the passive TCP closer.
    #[arg(long, default_value = "100")]
    pub drain_timeout_ms: u64,

    /// Connect timeout in seconds for backend WebSocket handshake.
    #[arg(long, default_value = "3")]
    pub connect_timeout_secs: u64,
```

- [ ] **Step 2.2: Add fields to `Config` struct (around line 56):**

```rust
    pub drain_timeout: Duration,
    pub connect_timeout: Duration,
```

- [ ] **Step 2.3: Initialize them in `Config::from_cli` (around line 79):**

```rust
            drain_timeout: Duration::from_millis(args.drain_timeout_ms),
            connect_timeout: Duration::from_secs(args.connect_timeout_secs),
```

- [ ] **Step 2.4: Build to confirm.**

```bash
cargo build --release 2>&1 | tail -10
```

Expected: `Finished release`. No errors.

- [ ] **Step 2.5: Commit.**

```bash
git add src/config.rs
git commit -m "$(cat <<'EOF'
config: add drain_timeout_ms and connect_timeout_secs

Defaults: 100ms drain, 3s connect. Drain timeout bounds the
read-to-EOF window in the new Draining state. Connect timeout
moves out of forward.rs (currently hard-coded there).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Create `src/dispatch/connect.rs` with `backoff_for_attempt` and unit tests

**Files:**
- Create: `src/dispatch/connect.rs`
- Modify: `src/dispatch/mod.rs` (add `pub mod connect;`)

The `connect_task` itself depends on `DispatchEvent::ConnectionEstablished/ConnectionFailed`, which we add in Task 4. For now we add the file with just the backoff helper + tests.

- [ ] **Step 3.1: Create `src/dispatch/connect.rs` with this content:**

```rust
use std::time::Duration;

/// Compute the reconnect backoff for a given attempt number.
///
/// Attempt 0 is the immediate first try (no delay). Subsequent attempts
/// follow an exponential schedule with ±25% jitter, capped at 5s.
///
/// Schedule (center values, before jitter):
///   0 → 0ms,  1 → 100ms,  2 → 250ms,  3 → 500ms,
///   4 → 1000ms, 5 → 2000ms, 6+ → 5000ms (capped)
pub fn backoff_for_attempt(attempt: u32) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    let center_ms: u64 = match attempt {
        1 => 100,
        2 => 250,
        3 => 500,
        4 => 1000,
        5 => 2000,
        _ => 5000,
    };
    let jitter_ms = jitter_pct(center_ms, 25);
    Duration::from_millis(center_ms.saturating_add_signed(jitter_ms))
}

/// Symmetric jitter: returns a value in [-pct%, +pct%] of `center_ms`.
fn jitter_pct(center_ms: u64, pct: u64) -> i64 {
    use std::time::SystemTime;
    let max = (center_ms * pct / 100) as i64;
    if max == 0 {
        return 0;
    }
    // Cheap pseudo-random based on nanos (good enough for jitter — we don't
    // need cryptographic quality here).
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i64)
        .unwrap_or(0);
    (nanos % (2 * max + 1)) - max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_zero_is_immediate() {
        assert_eq!(backoff_for_attempt(0), Duration::ZERO);
    }

    #[test]
    fn schedule_is_monotonic_at_center() {
        // Compare a small backoff with a larger one over many samples;
        // statistical: the larger one should be larger more often than not.
        let mut larger_count = 0;
        let trials = 100;
        for _ in 0..trials {
            let small = backoff_for_attempt(1);
            let large = backoff_for_attempt(4);
            if large > small {
                larger_count += 1;
            }
            // Tiny sleep so jitter source advances.
            std::thread::sleep(Duration::from_micros(50));
        }
        assert!(
            larger_count >= trials * 9 / 10,
            "expected attempt 4 > attempt 1 in ≥90% of {trials} trials, got {larger_count}",
        );
    }

    #[test]
    fn caps_at_five_seconds_plus_jitter() {
        // Even at attempt 100, must not exceed 5000ms + 25% jitter = 6250ms.
        for _ in 0..50 {
            let b = backoff_for_attempt(100);
            assert!(
                b <= Duration::from_millis(6250),
                "attempt 100 backoff {b:?} exceeds cap+jitter",
            );
            assert!(
                b >= Duration::from_millis(3750),
                "attempt 100 backoff {b:?} below cap-jitter",
            );
        }
    }

    #[test]
    fn jitter_within_bounds() {
        for _ in 0..50 {
            let b = backoff_for_attempt(2);
            // center 250ms ± 25% = [187, 312]
            assert!(
                b >= Duration::from_millis(187) && b <= Duration::from_millis(313),
                "attempt 2 backoff {b:?} outside [187, 313]",
            );
        }
    }
}
```

- [ ] **Step 3.2: Add module declaration in `src/dispatch/mod.rs`.** Edit line 1 area (currently `pub mod queue;`):

```rust
pub mod connect;
pub mod queue;
```

- [ ] **Step 3.3: Run the new tests.**

```bash
cargo test --release dispatch::connect
```

Expected: `4 passed; 0 failed`.

- [ ] **Step 3.4: Run all tests.**

```bash
cargo test --release
```

Expected: `34 passed; 0 failed`.

- [ ] **Step 3.5: Commit.**

```bash
git add src/dispatch/connect.rs src/dispatch/mod.rs
git commit -m "$(cat <<'EOF'
dispatch: add connect module with backoff_for_attempt helper

Exponential schedule (100ms → 5s) with ±25% jitter, capped. Used by
the upcoming connect_task. Stand-alone with unit tests so we can land
it before the rest of the refactor.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Add new `DispatchEvent` variants and `BackendWs` type alias

**Files:**
- Modify: `src/backend/connection.rs` (add type alias)
- Modify: `src/backend/mod.rs` (re-export)
- Modify: `src/dispatch/mod.rs` (add variants + stub handlers)

We add the variants and stub-route them to a no-op log in `Dispatcher::run`. Real handling lands in Task 8.

- [ ] **Step 4.1: Add the type alias to `src/backend/connection.rs`.** At the top of the file, add imports and the alias:

```rust
use std::net::SocketAddr;

use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::types::BackendId;

use super::circuit::CircuitBreaker;
use super::scoring::BackendScoring;
use super::state::BackendState;

/// The owned WebSocket connection type held in a backend's warm slot.
pub type BackendWs = WebSocketStream<MaybeTlsStream<TcpStream>>;
```

(Replace the existing `use std::net::SocketAddr;` and following `use crate::types::BackendId;` with the imports above. The struct definition below stays as it is for now — we extend it in Task 5.)

- [ ] **Step 4.2: Re-export `BackendWs` from `src/backend/mod.rs`.**

Read the existing content first to match its style:

```bash
cat src/backend/mod.rs
```

Then ensure it includes `pub use connection::BackendWs;` (or whatever export style is used). For example, if `mod.rs` currently has only `pub mod connection;` etc., add:

```rust
pub use connection::BackendWs;
```

- [ ] **Step 4.3: Add new `DispatchEvent` variants in `src/dispatch/mod.rs`.** Edit the `DispatchEvent` enum (around lines 25-36) to add two variants:

```rust
pub enum DispatchEvent {
    NewRequest(PendingRequest),
    StreamCompleted(ForwardResult),
    BackendRecovered(BackendId),
    CancelStream { stream_id: StreamId },
    QueryHealth(oneshot::Sender<HealthSnapshot>),
    /// Connect task succeeded — hand over the warm ws.
    ConnectionEstablished {
        backend_id: BackendId,
        ws: crate::backend::BackendWs,
    },
    /// Connect task failed — schedule retry (subject to circuit state).
    ConnectionFailed {
        backend_id: BackendId,
        error: String,
    },
}
```

- [ ] **Step 4.4: Stub-route the new variants in `Dispatcher::run`.** Edit the `match event` block (around lines 132-142) to add two arms:

```rust
                DispatchEvent::ConnectionEstablished { backend_id, ws } => {
                    self.handle_connection_established(backend_id, ws);
                }
                DispatchEvent::ConnectionFailed { backend_id, error } => {
                    self.handle_connection_failed(backend_id, error);
                }
```

And add stub method bodies on `impl Dispatcher` (place these near `handle_backend_recovered`):

```rust
    fn handle_connection_established(
        &mut self,
        backend_id: BackendId,
        _ws: crate::backend::BackendWs,
    ) {
        // Real implementation lands in Task 8.
        tracing::debug!(backend = %backend_id, "stub: connection established (drop ws)");
    }

    fn handle_connection_failed(&mut self, backend_id: BackendId, error: String) {
        // Real implementation lands in Task 8.
        tracing::debug!(backend = %backend_id, error = %error, "stub: connection failed");
    }
```

- [ ] **Step 4.5: Build to confirm everything compiles.**

```bash
cargo build --release 2>&1 | tail -15
```

Expected: `Finished release`. The `_ws` parameter will trigger an unused-import warning if we don't reference `BackendWs` from the `backend` mod — adjust `use` lines if needed.

- [ ] **Step 4.6: Run all tests.**

```bash
cargo test --release
```

Expected: `34 passed; 0 failed`.

- [ ] **Step 4.7: Commit.**

```bash
git add src/backend/connection.rs src/backend/mod.rs src/dispatch/mod.rs
git commit -m "$(cat <<'EOF'
dispatch: add ConnectionEstablished/ConnectionFailed events

Adds the two new DispatchEvent variants and a BackendWs type alias.
Handlers are stubs (drop the ws, log) — wired up properly in Task 8
once BackendConn has a slot to store it.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Extend `BackendConn` with the warm-slot field

**Files:**
- Modify: `src/backend/connection.rs`

- [ ] **Step 5.1: Update the `BackendConn` struct and `new()` in `src/backend/connection.rs`:**

```rust
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
        self.state.is_ready() && self.conn.is_some() && self.circuit.can_attempt()
    }
}
```

- [ ] **Step 5.2: Update `Dispatcher::new` initialization in `src/dispatch/mod.rs`** (around lines 88-102). Currently it sets `conn.state = BackendState::Ready`. Change that line to leave the state at the constructor default (`Disconnected`):

Locate this block (around line 92):

```rust
            .map(|(i, &addr)| {
                let mut conn = BackendConn::new(
                    BackendId(i),
                    addr,
                    config.circuit_threshold,
                    config.circuit_cooldown,
                );
                conn.state = BackendState::Ready;
                conn
            })
```

Replace with:

```rust
            .map(|(i, &addr)| {
                BackendConn::new(
                    BackendId(i),
                    addr,
                    config.circuit_threshold,
                    config.circuit_cooldown,
                )
            })
```

This will leave all backends in `Disconnected` at startup. Until Task 8 wires the auto-connect, **dispatch will fail** because no backend is available. We confirm the build still passes; runtime behavior is broken until Task 8.

- [ ] **Step 5.3: Build.**

```bash
cargo build --release 2>&1 | tail -10
```

Expected: `Finished release`. Possibly an unused-import warning for `BackendState` in dispatch/mod.rs — leave it for now (resolved in Task 8) or use `#[allow(unused_imports)]` if the warning blocks.

- [ ] **Step 5.4: Run unit tests.**

```bash
cargo test --release
```

Expected: `34 passed; 0 failed`. (Unit tests don't exercise dispatch.)

- [ ] **Step 5.5: Commit.**

```bash
git add src/backend/connection.rs src/dispatch/mod.rs
git commit -m "$(cat <<'EOF'
backend: add warm-slot field to BackendConn

Adds Option<BackendWs> conn slot and reconnect_attempt counter.
is_available() now requires a warm slot, not just state==Ready.

Removes the startup hack that pre-marked backends Ready before any
connection existed. Backends now start Disconnected; auto-connect
lands in Task 8.

Note: runtime is intentionally non-functional after this commit and
recovers in Task 8.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Add `connect_task` to `src/dispatch/connect.rs`

**Files:**
- Modify: `src/dispatch/connect.rs`

- [ ] **Step 6.1: Append `connect_task` to `src/dispatch/connect.rs`:**

```rust
use std::net::SocketAddr;

use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::MaybeTlsStream;

use crate::dispatch::DispatchEvent;
use crate::types::BackendId;

/// Open a WebSocket connection to a backend, then send either
/// ConnectionEstablished or ConnectionFailed back to the dispatcher.
///
/// `attempt` controls the backoff delay (0 = immediate, higher = exponential).
pub async fn connect_task(
    addr: SocketAddr,
    backend_id: BackendId,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    connect_timeout: std::time::Duration,
    attempt: u32,
) {
    let backoff = backoff_for_attempt(attempt);
    if !backoff.is_zero() {
        tokio::time::sleep(backoff).await;
    }

    let url = format!("ws://{addr}/v1/ws/speech");

    let result =
        tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(&url)).await;

    match result {
        Ok(Ok((ws, _resp))) => {
            // Disable Nagle for low TTFC.
            nodelay_backend_stream(ws.get_ref());
            let _ = dispatch_tx
                .send(DispatchEvent::ConnectionEstablished { backend_id, ws })
                .await;
        }
        Ok(Err(e)) => {
            let _ = dispatch_tx
                .send(DispatchEvent::ConnectionFailed {
                    backend_id,
                    error: format!("connect: {e}"),
                })
                .await;
        }
        Err(_) => {
            let _ = dispatch_tx
                .send(DispatchEvent::ConnectionFailed {
                    backend_id,
                    error: "connect timeout".into(),
                })
                .await;
        }
    }
}

fn nodelay_backend_stream(stream: &MaybeTlsStream<TcpStream>) {
    if let MaybeTlsStream::Plain(tcp) = stream {
        let _ = tcp.set_nodelay(true);
    }
}
```

(The `nodelay_backend_stream` helper duplicates the one in `forward.rs`; we'll remove the duplicate in Task 7 when forward.rs no longer opens connections.)

- [ ] **Step 6.2: Build.**

```bash
cargo build --release 2>&1 | tail -10
```

Expected: `Finished release`. Possibly a duplicate-symbol warning for `nodelay_backend_stream` — fine, both are private.

- [ ] **Step 6.3: Run tests.**

```bash
cargo test --release
```

Expected: `34 passed; 0 failed`.

- [ ] **Step 6.4: Commit.**

```bash
git add src/dispatch/connect.rs
git commit -m "$(cat <<'EOF'
dispatch: add connect_task with backoff and timeout

Async function that opens a WS to the backend (with optional backoff
delay and connect timeout) and sends the result back via
ConnectionEstablished or ConnectionFailed. Spawned by the dispatcher
on startup, after each request completion, and on circuit recovery
(wired up in Task 8).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Refactor `forward.rs` to take `ws` by value, add liveness check + `drain_close`

**Files:**
- Modify: `src/forward.rs`

This is the largest single edit. The forwarding function no longer opens its own connection — it takes one and uses it. Liveness check guards against pre-start data on warm slots. `drain_close` runs before every terminal return.

- [ ] **Step 7.1: Replace `src/forward.rs` with the following:**

```rust
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::backend::BackendWs;
use crate::dispatch::{ClientEvent, PendingRequest};
use crate::metrics::Metrics;
use crate::protocol::backend::BackendMessage;
use crate::protocol::frame::encode_binary_frame;
use crate::types::{BackendId, StreamId};

/// Outcome of a forwarding attempt.
#[derive(Debug)]
pub enum ForwardOutcome {
    Success {
        ttfc: Duration,
        audio_duration: f64,
    },
    BackendCrashed,
    BackendHung,
    MalformedResponse(String),
    /// Backend accepted the connection but refused the start (contention).
    BackendBusy,
    BackendError(String),
    ClientGone,
    Cancelled,
}

/// Full result returned to the Dispatcher, carrying request data back for retries.
pub struct ForwardResult {
    pub backend_id: BackendId,
    pub stream_id: StreamId,
    pub outcome: ForwardOutcome,
    pub client_tx: mpsc::Sender<ClientEvent>,
    pub text: String,
    pub speaker_id: u32,
    pub retries_remaining: u32,
    pub created_at: Instant,
    pub active_count: Arc<AtomicUsize>,
    pub tried_backends: Vec<BackendId>,
}

/// Execute a single forwarding attempt using the warm `ws` connection passed in.
///
/// `cancel_rx` lets the dispatcher abort an in-flight stream on client request.
/// `drain_timeout` bounds the post-terminal read-to-EOF window.
pub async fn run_forwarding(
    backend_id: BackendId,
    mut ws: BackendWs,
    request: PendingRequest,
    hang_timeout: Duration,
    drain_timeout: Duration,
    cancel_rx: oneshot::Receiver<()>,
    metrics: Arc<Metrics>,
) -> ForwardResult {
    let stream_id = request.stream_id.clone();

    let outcome = do_forward(
        &mut ws,
        &stream_id,
        &request.text,
        request.speaker_id,
        &request.client_tx,
        hang_timeout,
        drain_timeout,
        cancel_rx,
        &metrics,
    )
    .await;

    // ws is dropped here — by this point either drain_close has consumed the
    // peer's FIN (passive-close path) or the peer never sent FIN (active-close
    // path: hang/cancel). Either way, the slot is consumed.
    drop(ws);

    ForwardResult {
        backend_id,
        stream_id,
        outcome,
        client_tx: request.client_tx,
        text: request.text,
        speaker_id: request.speaker_id,
        retries_remaining: request.retries_remaining,
        created_at: request.created_at,
        active_count: request.active_count,
        tried_backends: request.tried_backends,
    }
}

#[allow(clippy::too_many_arguments)]
async fn do_forward(
    ws: &mut BackendWs,
    stream_id: &StreamId,
    text: &str,
    speaker_id: u32,
    client_tx: &mpsc::Sender<ClientEvent>,
    hang_timeout: Duration,
    drain_timeout: Duration,
    mut cancel_rx: oneshot::Receiver<()>,
    metrics: &Arc<Metrics>,
) -> ForwardOutcome {
    // Liveness check: if the warm slot already has buffered data (pre-start
    // error from REFUSE chaos, server crashed during the gap, etc.), handle it
    // before sending start.
    if let Some(early) = check_warm_slot_liveness(ws).await {
        drain_close(ws, drain_timeout).await;
        return early;
    }

    let start_json = serde_json::json!({
        "type": "start",
        "text": text,
        "speaker_id": speaker_id,
    });
    if let Err(e) = ws
        .send(Message::Text(start_json.to_string().into()))
        .await
    {
        tracing::warn!("send start failed: {e}");
        drain_close(ws, drain_timeout).await;
        return ForwardOutcome::BackendCrashed;
    }

    let request_start = Instant::now();
    let mut first_chunk_time: Option<Instant> = None;

    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                tracing::debug!(stream = %stream_id, "stream cancelled");
                // Best-effort: try to send WS Close, then short drain. Backend
                // doesn't support cancel, so we'll likely end as active closer.
                let _ = tokio::time::timeout(
                    Duration::from_millis(50),
                    ws.close(None),
                ).await;
                drain_close(ws, drain_timeout).await;
                return ForwardOutcome::Cancelled;
            }
            read = tokio::time::timeout(hang_timeout, ws.next()) => {
                match read {
                    Err(_elapsed) => {
                        tracing::warn!(stream = %stream_id, "hung — no data in {hang_timeout:?}");
                        // No drain — server is sleeping and won't FIN.
                        return ForwardOutcome::BackendHung;
                    }
                    Ok(None) => {
                        tracing::warn!(stream = %stream_id, "connection closed without done");
                        // Peer already closed; drain is a no-op but cheap.
                        drain_close(ws, drain_timeout).await;
                        return ForwardOutcome::BackendCrashed;
                    }
                    Ok(Some(Err(e))) => {
                        tracing::warn!(stream = %stream_id, "ws error: {e}");
                        drain_close(ws, drain_timeout).await;
                        return ForwardOutcome::BackendCrashed;
                    }
                    Ok(Some(Ok(msg))) => match msg {
                        Message::Binary(data) => {
                            if first_chunk_time.is_none() {
                                first_chunk_time = Some(Instant::now());
                            }

                            let chunk_start = Instant::now();
                            let tagged = encode_binary_frame(&stream_id.0, &data);
                            tokio::select! {
                                biased;
                                _ = &mut cancel_rx => {
                                    let _ = tokio::time::timeout(
                                        Duration::from_millis(50),
                                        ws.close(None),
                                    ).await;
                                    drain_close(ws, drain_timeout).await;
                                    return ForwardOutcome::Cancelled;
                                }
                                send = client_tx.send(ClientEvent::Binary(tagged)) => {
                                    if send.is_err() {
                                        // Client gone: backend keeps producing. Drain briefly
                                        // and drop. Active-close cost on this path is acceptable
                                        // (rare).
                                        let _ = tokio::time::timeout(
                                            Duration::from_millis(50),
                                            ws.close(None),
                                        ).await;
                                        drain_close(ws, drain_timeout).await;
                                        return ForwardOutcome::ClientGone;
                                    }
                                }
                            }
                            metrics
                                .chunk_forward_seconds
                                .observe(chunk_start.elapsed().as_secs_f64());
                        }
                        Message::Text(text_data) => {
                            let backend_msg = BackendMessage::from_text(&text_data);
                            match backend_msg {
                                BackendMessage::Queued => {}
                                BackendMessage::Done { audio_duration } => {
                                    let ttfc = first_chunk_time
                                        .map(|t| t.duration_since(request_start))
                                        .unwrap_or_default();
                                    drain_close(ws, drain_timeout).await;
                                    return ForwardOutcome::Success {
                                        ttfc,
                                        audio_duration,
                                    };
                                }
                                BackendMessage::Error { message } => {
                                    drain_close(ws, drain_timeout).await;
                                    if message.to_lowercase().contains("busy") {
                                        return ForwardOutcome::BackendBusy;
                                    }
                                    return ForwardOutcome::BackendError(message);
                                }
                                BackendMessage::Unknown(raw) => {
                                    drain_close(ws, drain_timeout).await;
                                    return ForwardOutcome::MalformedResponse(raw);
                                }
                            }
                        }
                        Message::Close(_) => {
                            drain_close(ws, drain_timeout).await;
                            return ForwardOutcome::BackendCrashed;
                        }
                        _ => {}
                    },
                }
            }
        }
    }
}

/// Non-blocking poll of the warm slot before sending start.
/// Returns Some(outcome) if the connection is dead or has buffered data,
/// None if Pending (slot is alive — proceed with start).
async fn check_warm_slot_liveness(ws: &mut BackendWs) -> Option<ForwardOutcome> {
    use futures_util::future::poll_immediate;
    match poll_immediate(ws.next()).await {
        None => None, // Pending: alive
        Some(None) => Some(ForwardOutcome::BackendCrashed),
        Some(Some(Err(_))) => Some(ForwardOutcome::BackendCrashed),
        Some(Some(Ok(Message::Close(_)))) => Some(ForwardOutcome::BackendCrashed),
        Some(Some(Ok(Message::Text(t)))) => {
            let m = BackendMessage::from_text(&t);
            Some(match m {
                BackendMessage::Error { message } => {
                    if message.to_lowercase().contains("busy") {
                        ForwardOutcome::BackendBusy
                    } else {
                        ForwardOutcome::BackendError(message)
                    }
                }
                BackendMessage::Unknown(raw) => ForwardOutcome::MalformedResponse(raw),
                BackendMessage::Done { .. } | BackendMessage::Queued => {
                    ForwardOutcome::MalformedResponse(format!("pre-start: {t}"))
                }
            })
        }
        Some(Some(Ok(_))) => Some(ForwardOutcome::MalformedResponse(
            "pre-start binary".to_string(),
        )),
    }
}

/// Drain reads to EOF (or timeout) so the peer's FIN arrives before our drop
/// sends ours — making us the passive TCP closer.
async fn drain_close(ws: &mut BackendWs, deadline: Duration) {
    let _ = tokio::time::timeout(deadline, async {
        while ws.next().await.is_some() {}
    })
    .await;
}
```

- [ ] **Step 7.2: Build.**

```bash
cargo build --release 2>&1 | tail -20
```

Expected: `Finished release`. There will be a build error in `src/dispatch/mod.rs::dispatch_to` because the call site still passes `backend_addr`, not a `ws`. We fix that in Task 8 — for now, **expect build failure** at the dispatch layer.

If the build error is *only* in `src/dispatch/mod.rs`, proceed. If forward.rs itself doesn't compile, fix the type errors before committing.

- [ ] **Step 7.3: Confirm forward.rs compiles in isolation by running its tests:**

```bash
cargo build --release --bin tts-multiplexer 2>&1 | grep -E "error\[|error:" | head -20
```

The errors should all be in `src/dispatch/mod.rs`. If forward.rs reports errors, fix them first.

- [ ] **Step 7.4: Commit (build is broken — note in commit message).**

```bash
git add src/forward.rs
git commit -m "$(cat <<'EOF'
forward: take ws by value; add liveness check and drain_close

- run_forwarding receives a warm BackendWs instead of opening one
- check_warm_slot_liveness handles REFUSE/dead slots before sending start
- drain_close runs on every terminal path (success/error/crash/malformed)
  to consume the peer's FIN before our drop sends ours — passive TCP closer
- HANG path skips drain (server is sleeping, no FIN coming)
- Cancel/ClientGone paths attempt best-effort WS close + brief drain

Build is broken at the dispatch layer until Task 8 wires the new
forward signature into dispatch_to.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Wire dispatcher to own connections, spawn connect_task, handle new events

**Files:**
- Modify: `src/dispatch/mod.rs`

This task fixes the build by wiring the new forward signature into `dispatch_to`, implements the real `handle_connection_established` / `handle_connection_failed`, kicks off connect tasks at startup and after every request completion, and removes the old 500ms recovery timer.

- [ ] **Step 8.1: Add a `spawn_connect` helper on `Dispatcher`.** Place it near the bottom of `impl Dispatcher`, before the closing brace (after `find_best_backend_filtered`):

```rust
    /// Spawn a connect task for the given backend with the current attempt
    /// counter. Caller is responsible for setting state to Connecting.
    fn spawn_connect(&self, backend_id: BackendId) {
        let addr = self.backends[backend_id.0].addr;
        let attempt = self.backends[backend_id.0].reconnect_attempt;
        let dispatch_tx = self.dispatch_tx.clone();
        let connect_timeout = self.config.connect_timeout;
        tokio::spawn(crate::dispatch::connect::connect_task(
            addr,
            backend_id,
            dispatch_tx,
            connect_timeout,
            attempt,
        ));
    }
```

- [ ] **Step 8.2: Kick off initial connects from `Dispatcher::run`.** At the very top of `run()`, before the `while let Some(event) = ...` loop, add:

```rust
        // Kick off initial connect for every backend.
        for backend in &mut self.backends {
            backend.state = BackendState::Connecting;
            self.metrics
                .set_backend_state(&backend.addr.to_string(), backend.state.name());
        }
        for i in 0..self.backends.len() {
            self.spawn_connect(BackendId(i));
        }
```

- [ ] **Step 8.3: Replace the stub `handle_connection_established`:**

```rust
    fn handle_connection_established(
        &mut self,
        backend_id: BackendId,
        ws: crate::backend::BackendWs,
    ) {
        let backend = &mut self.backends[backend_id.0];
        backend.conn = Some(ws);
        backend.state = BackendState::Ready;
        backend.reconnect_attempt = 0;
        self.metrics
            .set_backend_state(&backend.addr.to_string(), backend.state.name());
        tracing::debug!(backend = %backend_id, "connection ready (warm slot)");
        self.try_dispatch();
    }
```

- [ ] **Step 8.4: Replace the stub `handle_connection_failed`:**

```rust
    fn handle_connection_failed(&mut self, backend_id: BackendId, error: String) {
        {
            let backend = &mut self.backends[backend_id.0];
            backend.state = BackendState::Disconnected;
            backend.conn = None;
            backend.reconnect_attempt = backend.reconnect_attempt.saturating_add(1);
            self.metrics
                .set_backend_state(&backend.addr.to_string(), backend.state.name());
            tracing::warn!(
                backend = %backend_id,
                attempt = backend.reconnect_attempt,
                error = %error,
                "connect failed; will retry with backoff"
            );
        }

        // If circuit is open, do not respawn — wait for BackendRecovered.
        let circuit_open = !self.backends[backend_id.0].circuit.can_attempt();
        if circuit_open {
            tracing::debug!(backend = %backend_id, "circuit open; deferring reconnect");
            return;
        }

        self.backends[backend_id.0].state = BackendState::Connecting;
        self.metrics.set_backend_state(
            &self.backends[backend_id.0].addr.to_string(),
            self.backends[backend_id.0].state.name(),
        );
        self.spawn_connect(backend_id);
    }
```

- [ ] **Step 8.5: Update `dispatch_to` to take the warm `ws` from the slot.** Replace the existing `dispatch_to` method (around lines 458-494):

```rust
    fn dispatch_to(&mut self, backend_id: BackendId, req: PendingRequest) {
        let backend_addr_str = self.backends[backend_id.0].addr.to_string();
        let ws = match self.backends[backend_id.0].conn.take() {
            Some(ws) => ws,
            None => {
                // Defensive: try_dispatch should have filtered via is_available()
                // which checks conn.is_some(). If we're here, log and skip.
                tracing::error!(
                    backend = %backend_id,
                    "dispatch_to called but no warm slot — re-queueing"
                );
                let _ = self.queue.push_front(req);
                self.metrics.queue_depth.set(self.queue.len() as i64);
                return;
            }
        };

        self.backends[backend_id.0].state = BackendState::Busy;
        self.metrics
            .set_backend_state(&backend_addr_str, self.backends[backend_id.0].state.name());
        self.active_streams += 1;
        self.metrics.active_streams.set(self.active_streams as i64);

        let dispatch_tx = self.dispatch_tx.clone();
        let hang_timeout = self.config.hang_timeout;
        let drain_timeout = self.config.drain_timeout;
        let metrics = self.metrics.clone();

        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.in_flight.insert(req.stream_id.clone(), cancel_tx);

        tracing::debug!(
            stream = %req.stream_id,
            backend = %backend_id,
            "dispatching"
        );

        tokio::spawn(async move {
            let result = forward::run_forwarding(
                backend_id,
                ws,
                req,
                hang_timeout,
                drain_timeout,
                cancel_rx,
                metrics,
            )
            .await;
            let _ = dispatch_tx
                .send(DispatchEvent::StreamCompleted(result))
                .await;
        });
    }
```

- [ ] **Step 8.6: Update `handle_stream_completed` to spawn reconnect after every completion.** This is several edits:

  **(a)** In the `Success` arm (around line 192), replace `backend.state = BackendState::Ready;` with `backend.state = BackendState::Disconnected; backend.conn = None;` (the warm slot was consumed by the request). Then at the **end** of the success arm, add `self.spawn_reconnect_if_circuit_allows(result.backend_id);` (helper added in 8.7).

  **(b)** In the `BackendBusy` arm (around line 241), same change: `backend.state = BackendState::Disconnected; backend.conn = None;`, then at the end of the arm: `self.spawn_reconnect_if_circuit_allows(result.backend_id);`.

  **(c)** In the failure arm (`BackendCrashed | BackendHung | MalformedResponse | BackendError`, around lines 273-284):
   - Remove the existing `tokio::spawn(async move { tokio::time::sleep(500ms)... BackendRecovered })` block (lines 290-295).
   - Replace `backend.state = BackendState::Disconnected;` with the same plus `backend.conn = None;`.
   - On a circuit-just-opened, spawn the cooldown timer (Task 9 helper). For now, just call `self.spawn_reconnect_if_circuit_allows(result.backend_id);` at the end of the arm. The circuit cooldown timer is added in Task 9.

  **(d)** In the `ClientGone` and `Cancelled` arms (around lines 353-384), same substitution: `backend.state = BackendState::Disconnected; backend.conn = None;`, then `self.spawn_reconnect_if_circuit_allows(result.backend_id);`.

- [ ] **Step 8.7: Add the helper `spawn_reconnect_if_circuit_allows` on `Dispatcher`:**

```rust
    /// Transition backend to Connecting and spawn connect_task, but only if
    /// the circuit is closed/half-open. If circuit is open, leave the backend
    /// Disconnected — Task 9's cooldown timer will revive it via BackendRecovered.
    fn spawn_reconnect_if_circuit_allows(&mut self, backend_id: BackendId) {
        let circuit_open = !self.backends[backend_id.0].circuit.can_attempt();
        if circuit_open {
            tracing::debug!(backend = %backend_id, "circuit open; not reconnecting");
            return;
        }
        self.backends[backend_id.0].state = BackendState::Connecting;
        self.metrics.set_backend_state(
            &self.backends[backend_id.0].addr.to_string(),
            self.backends[backend_id.0].state.name(),
        );
        self.spawn_connect(backend_id);
    }
```

- [ ] **Step 8.8: Update `handle_backend_recovered` to spawn a connect task instead of going Ready directly.** Replace the existing implementation (around lines 390-399):

```rust
    fn handle_backend_recovered(&mut self, backend_id: BackendId) {
        let backend = &mut self.backends[backend_id.0];
        if matches!(backend.state, BackendState::Disconnected) {
            backend.state = BackendState::Connecting;
            self.metrics
                .set_backend_state(&backend.addr.to_string(), backend.state.name());
            tracing::debug!(backend = %backend_id, "backend recovered; reconnecting");
            drop(backend); // release borrow before calling spawn_connect (which borrows self)
            self.spawn_connect(backend_id);
        }
    }
```

- [ ] **Step 8.9: Build.**

```bash
cargo build --release 2>&1 | tail -25
```

Expected: `Finished release`. Fix any remaining errors (most likely: missing `BackendState` imports, or the `dispatch_to` references need adjustment).

- [ ] **Step 8.10: Run unit tests.**

```bash
cargo test --release
```

Expected: `34 passed; 0 failed`.

- [ ] **Step 8.11: Smoke test against calm backend.**

In a separate terminal:
```bash
source .venv/bin/activate
python assignment/mock_backend.py --workers 8 --base-port 9100 --calm > /tmp/mock_calm.log 2>&1 &
```

Then:
```bash
RUST_LOG=info ./target/release/tts-multiplexer \
  --backends 127.0.0.1:9100,127.0.0.1:9101,127.0.0.1:9102,127.0.0.1:9103,127.0.0.1:9104,127.0.0.1:9105,127.0.0.1:9106,127.0.0.1:9107 \
  > /tmp/mux.log 2>&1 &
sleep 2
curl -s http://localhost:9001/health | python3 -m json.tool | head -50
```

Expected: All 8 backends in `state: "ready"`. (Warm-pool startup is working.)

```bash
source .venv/bin/activate
python assignment/test_client.py ws://localhost:9000/v1/ws/speech 2>&1 | tail -20
```

Expected: Correctness suite passes (or substantially passes) on calm backends.

Cleanup:
```bash
pkill -f mock_backend; pkill -f tts-multiplexer; sleep 1
```

- [ ] **Step 8.12: Commit.**

```bash
git add src/dispatch/mod.rs
git commit -m "$(cat <<'EOF'
dispatch: own backend connections; spawn connect_task lifecycle

- spawn_connect helper kicks off connect_task with current attempt counter
- Initial connect spawned for every backend at Dispatcher::run startup
- handle_connection_established: state=Ready, conn=Some(ws), reset attempt
- handle_connection_failed: backoff-respecting retry (subject to circuit)
- dispatch_to takes warm ws via conn.take() and hands to forward
- handle_stream_completed: every terminal arm consumes the slot
  (state=Disconnected, conn=None) and spawns reconnect if circuit allows
- Removed the 500ms BackendRecovered timer that fired on every failure
- handle_backend_recovered now spawns a connect_task (Disconnected→Connecting)
  rather than going directly to Ready

Build is functional again. Smoke-tested against calm backends:
all 8 land in /health state=ready, correctness suite passes.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Add circuit-cooldown timer to drive `BackendRecovered`

**Files:**
- Modify: `src/dispatch/mod.rs`

After the changes in Task 8, the only emitter of `BackendRecovered` we removed (the 500ms-on-every-failure timer) is gone. Now we need an emitter for the circuit-cooldown case so that an open-circuit backend can come back.

- [ ] **Step 9.1: Add `spawn_circuit_cooldown_timer` helper on `Dispatcher`:**

```rust
    /// One-shot timer: when a backend's circuit just opened, schedule a
    /// BackendRecovered event after the cooldown elapses so the dispatcher
    /// can transition it back to Connecting.
    fn spawn_circuit_cooldown_timer(&self, backend_id: BackendId) {
        let dispatch_tx = self.dispatch_tx.clone();
        let cooldown = self.config.circuit_cooldown;
        tokio::spawn(async move {
            tokio::time::sleep(cooldown).await;
            let _ = dispatch_tx
                .send(DispatchEvent::BackendRecovered(backend_id))
                .await;
        });
    }
```

- [ ] **Step 9.2: Detect circuit-opened transition in the failure arm of `handle_stream_completed`.** In the `BackendCrashed | BackendHung | MalformedResponse | BackendError` arm (after the `backend.circuit.record_failure()` call), capture whether the circuit just transitioned to Open and spawn the timer.

Update that arm to look like:

```rust
            ForwardOutcome::BackendCrashed
            | ForwardOutcome::BackendHung
            | ForwardOutcome::MalformedResponse(_)
            | ForwardOutcome::BackendError(_) => {
                let backend = &mut self.backends[backend_idx];
                backend.scoring.record_result(false);
                let penalty_ms = self.config.hang_timeout.as_secs_f64() * 1000.0;
                backend.scoring.record_ttfc(penalty_ms);
                let was_closed_or_half_open = backend.circuit.can_attempt();
                backend.circuit.record_failure();
                let circuit_just_opened = was_closed_or_half_open
                    && matches!(
                        backend.circuit.state(),
                        crate::backend::circuit::CircuitState::Open,
                    );
                backend.state = BackendState::Disconnected;
                backend.conn = None;
                self.metrics
                    .set_backend_state(&backend.addr.to_string(), backend.state.name());
                self.active_streams = self.active_streams.saturating_sub(1);
                self.metrics.active_streams.set(self.active_streams as i64);

                let reason = match &result.outcome {
                    ForwardOutcome::BackendCrashed => "crashed".to_string(),
                    ForwardOutcome::BackendHung => "hung".to_string(),
                    ForwardOutcome::MalformedResponse(s) => format!("malformed: {s}"),
                    ForwardOutcome::BackendError(s) => format!("error: {s}"),
                    _ => "unknown".to_string(),
                };

                tracing::warn!(
                    stream = %result.stream_id,
                    backend = %result.backend_id,
                    retries_left = result.retries_remaining,
                    reason = %reason,
                    "forwarding failed"
                );

                if result.retries_remaining > 0 {
                    self.metrics
                        .requests_total
                        .with_label_values(&["retry"])
                        .inc();
                    let mut tried = result.tried_backends;
                    if !tried.contains(&result.backend_id) {
                        tried.push(result.backend_id);
                    }
                    let retry = PendingRequest {
                        stream_id: result.stream_id,
                        text: result.text,
                        speaker_id: result.speaker_id,
                        client_tx: result.client_tx,
                        retries_remaining: result.retries_remaining - 1,
                        created_at: result.created_at,
                        active_count: result.active_count,
                        tried_backends: tried,
                    };
                    let _ = self.queue.push_front(retry);
                    self.metrics.queue_depth.set(self.queue.len() as i64);
                } else {
                    self.metrics
                        .requests_total
                        .with_label_values(&["error"])
                        .inc();
                    result.active_count.fetch_sub(1, Ordering::Relaxed);
                    let err = ServerMessage::Error {
                        stream_id: result.stream_id.0.clone(),
                        message: format!(
                            "backend unavailable after {} retries",
                            self.config.max_retries
                        ),
                    };
                    let _ = result
                        .client_tx
                        .try_send(ClientEvent::Text(err.to_json()));
                }

                if circuit_just_opened {
                    tracing::info!(
                        backend = %result.backend_id,
                        cooldown = ?self.config.circuit_cooldown,
                        "circuit opened; scheduling recovery timer"
                    );
                    self.spawn_circuit_cooldown_timer(result.backend_id);
                } else {
                    self.spawn_reconnect_if_circuit_allows(result.backend_id);
                }
            }
```

(Note: `was_closed_or_half_open` calls `can_attempt()` which can transition Open→HalfOpen as a side-effect. That's safe here because we then call `record_failure()` which would re-Open it; the `circuit_just_opened` check still works because `state()` after `record_failure()` is what we care about.)

- [ ] **Step 9.3: Build.**

```bash
cargo build --release 2>&1 | tail -10
```

Expected: `Finished release`.

- [ ] **Step 9.4: Run unit tests.**

```bash
cargo test --release
```

Expected: `34 passed; 0 failed`.

- [ ] **Step 9.5: Commit.**

```bash
git add src/dispatch/mod.rs
git commit -m "$(cat <<'EOF'
dispatch: schedule circuit-cooldown timer when circuit opens

Adds spawn_circuit_cooldown_timer — a one-shot tokio task that fires
BackendRecovered after circuit_cooldown elapses. Replaces the previous
500ms-on-every-failure timer (removed in Task 8).

Failure arm now distinguishes "circuit just opened" (schedule cooldown
timer, do NOT reconnect) from "still closed/half-open" (immediate
reconnect with backoff).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: End-to-end verification — chaos suite + TIME_WAIT sampling

**Files:** None modified. This is the empirical verification of the fix.

- [ ] **Step 10.1: Reset any stale processes.**

```bash
pkill -f mock_backend; pkill -f tts-multiplexer; pkill -f sampler_loop; sleep 1
echo "cleared"
```

- [ ] **Step 10.2: Start mock backend in chaos mode.**

```bash
source .venv/bin/activate
python assignment/mock_backend.py --workers 8 --base-port 9100 > /tmp/mock_backend.log 2>&1 &
sleep 2
```

- [ ] **Step 10.3: Start the multiplexer.**

```bash
RUST_LOG=info ./target/release/tts-multiplexer \
  --backends 127.0.0.1:9100,127.0.0.1:9101,127.0.0.1:9102,127.0.0.1:9103,127.0.0.1:9104,127.0.0.1:9105,127.0.0.1:9106,127.0.0.1:9107 \
  > /tmp/mux.log 2>&1 &
sleep 2
```

- [ ] **Step 10.4: Verify all 8 backends warm up.**

```bash
curl -s http://localhost:9001/health | python3 -c "import json,sys; d=json.load(sys.stdin); print({b['addr']: b['state'] for b in d['backends']})"
```

Expected: All 8 in `ready`. If any are `connecting` after 2s, wait another second and re-check.

- [ ] **Step 10.5: Start the socket sampler.**

Recreate the sampler script:

```bash
cat > /tmp/sample_sockets.sh <<'EOF'
#!/bin/bash
ts=$(date +%H:%M:%S)
backend_side=$(netstat -an -p tcp | awk '$4 ~ /\.91(0[0-7])$/ {print $6}' | sort | uniq -c | tr '\n' ',')
mux_side=$(netstat -an -p tcp | awk '$5 ~ /\.91(0[0-7])$/ {print $6}' | sort | uniq -c | tr '\n' ',')
echo "$ts | backend_side[$backend_side] mux_side[$mux_side]"
EOF
chmod +x /tmp/sample_sockets.sh

cat > /tmp/sampler_loop.sh <<'EOF'
#!/bin/bash
while true; do
  /tmp/sample_sockets.sh
  sleep 1
done
EOF
chmod +x /tmp/sampler_loop.sh
/tmp/sampler_loop.sh > /tmp/samples.log 2>&1 &
```

- [ ] **Step 10.6: Run the chaos suite.**

```bash
source .venv/bin/activate
python assignment/test_client.py ws://localhost:9000/v1/ws/speech --chaos --requests 500 --concurrency 32 2>&1 | tail -15
```

- [ ] **Step 10.7: Stop the sampler and inspect results.**

```bash
pkill -f sampler_loop
echo "=== chaos suite outcome (above) ==="
echo "=== peak mux-side TIME_WAIT ==="
grep -oE "mux_side\[[^]]+\]" /tmp/samples.log | grep -oE "[0-9]+ TIME_WAIT" | sort -n | tail -5
echo "=== connect failures in mux log ==="
grep -c "connect failed" /tmp/mux.log
grep -c "os error 49" /tmp/mux.log
```

**Pass criteria:**
- Chaos success rate ≥ 95% (target from spec; previously 7.6%).
- `connect failed` count: 0 (previously 56). Note: spurious connects from mock-backend REFUSE may show as `BackendError` in the mux log but should NOT show as `connect failed` (which is the EADDRNOTAVAIL signature).
- Peak mux-side TIME_WAIT: < 200 (previously 16,267).

If any criterion fails, **stop and diagnose** — do not declare done. Re-run with `RUST_LOG=debug` to inspect state transitions.

- [ ] **Step 10.8: Run other suites for regression.**

```bash
source .venv/bin/activate
python assignment/test_client.py ws://localhost:9000/v1/ws/speech --multiplex 2>&1 | tail -10
python assignment/test_client.py ws://localhost:9000/v1/ws/speech 2>&1 | tail -10  # correctness
```

Expected: Both pass. If multiplex isolation regresses, investigate before proceeding.

- [ ] **Step 10.9: Cleanup.**

```bash
pkill -f mock_backend; pkill -f tts-multiplexer; pkill -f sampler_loop; sleep 1
echo "cleared"
```

- [ ] **Step 10.10: Commit a verification note (optional but useful for future readers).**

```bash
mkdir -p docs/superpowers/results
cat > docs/superpowers/results/2026-05-02-chaos-fix-verification.md <<'EOF'
# Chaos Fix Verification — 2026-05-02

## Setup
- macOS, ephemeral 49152-65535, MSL 15s.
- 8 mock backends on 127.0.0.1:9100..9107, default chaos rates (~36% failure).
- 500 requests, concurrency 32.

## Before fix (commit 049a549)
- Chaos success rate: 7.6%
- `connect failed: ... os error 49` log lines: 56
- Peak mux-side TIME_WAIT: 16,267 (entire macOS ephemeral range)
- Peak backend-side TIME_WAIT: 0 (multiplexer was active closer in 100% of cases)

## After fix (commit <NEW_COMMIT>)
- Chaos success rate: <FILL_IN>%
- `connect failed: ... os error 49` log lines: <FILL_IN>
- Peak mux-side TIME_WAIT: <FILL_IN>
- Peak backend-side TIME_WAIT: <FILL_IN>

## Conclusion
<one paragraph>
EOF
```

Fill in the actual measured values, then commit:

```bash
git add docs/superpowers/results/2026-05-02-chaos-fix-verification.md
git commit -m "$(cat <<'EOF'
docs: record chaos-fix verification numbers

Before/after measurements showing the connection-pool refactor
eliminates EADDRNOTAVAIL under chaos load.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review checklist (run by the implementing engineer)

- [ ] After Task 10 passes, run `cargo build --release && cargo test --release` once more from a clean state.
- [ ] Inspect `/health` during a chaos run — backends should cycle through `connecting` / `ready` / `busy` / `draining` (briefly) / `disconnected` (briefly).
- [ ] Run `--soak 600` if time permits (RSS growth < 5MB target from spec §8.3).
- [ ] If any of the spec's pass criteria are missed, do not paper over — return to design discussion.

---

## Risk reference

See spec §9 for the four documented risks. Most relevant for execution:
1. `futures_util::future::poll_immediate` behavior with WebSocketStream — if this misbehaves, fall back to `tokio::time::timeout(Duration::ZERO, ws.next())` in `check_warm_slot_liveness`.
2. Drain timeout: configurable via `--drain-timeout-ms`. Default 100ms is safe for the rig; if a real network deployment hits the timeout often, raise it.
