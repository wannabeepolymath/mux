# Connection Pool with Passive-Close Discipline — Design

**Date:** 2026-05-02
**Branch:** `eph`
**Author:** Daksh (with Claude)
**Status:** Draft, awaiting review

---

## 1. Problem

Under chaos load, the multiplexer fails its own chaos suite (7.6% success on a 500-request run) because `connect(2)` to backends starts returning `EADDRNOTAVAIL` (macOS `os error 49`). The kernel has no free `(src_ip, src_port, dst_ip, dst_port)` tuple in the ephemeral range to use for new outbound connections.

### Empirical evidence

500-request chaos run, concurrency 32, 8 backends on `127.0.0.1:9100..9107`, sampling `netstat` every 1s during the run:

| Sample (peak) | backend-side states (local port = 9100..9107) | mux-side states (foreign port = 9100..9107) | total ephemerals in use |
|---|---|---|---|
| 15:27:18 | 1 ESTABLISHED, 8 LISTEN | 1 ESTABLISHED, **16,226 TIME_WAIT** | 16,385 |
| 15:28:09 | 8 LISTEN | **16,267 TIME_WAIT** | 16,385 |

macOS ephemeral range is 49152–65535 (~16,384 ports); `net.inet.tcp.msl = 15000` ms → TIME_WAIT ≈ 30s.

Critical finding: **0 backend-side TIME_WAIT entries** at any sample. Backend-side LAST_ACK appears in some samples. This means the **multiplexer is the active TCP closer in essentially 100% of cases**, not just on HANG/cancellation as initially hypothesized.

### Root cause

In `src/forward.rs`, every terminal-frame path (`Done`, server-sent `error`, `BackendCrashed`, `MalformedResponse`) returns immediately after reading the terminal frame and drops the `WebSocketStream`. The drop issues `close(2)` on the underlying TcpStream, sending FIN. Because the multiplexer is faster than the Python `websockets` library's close handshake, the multiplexer's FIN almost always wins the race — so the multiplexer becomes the active closer and pays ~30s of TIME_WAIT per ephemeral.

Compounding factors (real but secondary): no connection pooling (one TCP connection per request), retries amplify connection count under chaos (max_retries=3 → up to 4 connections per logical request), and loopback narrows the destination 4-tuple space to 8 ports on one IP.

### Spec misalignment

`assignment/README.md` §"Required behaviors" #5 mandates an explicit connection-pool state machine: `Connecting → Ready → Busy → Draining → Disconnected → Connecting`. The current code (`src/backend/state.rs:1-8`) explicitly opts out of `Connecting` and `Draining` as distinct states — and that opt-out is what makes the close-handshake discipline get skipped.

The spec's `Draining` state, when implemented correctly, **is** the passive-close fix.

---

## 2. Goals & non-goals

**Goals**
1. Eliminate `EADDRNOTAVAIL` under chaos load by becoming the passive TCP closer in ≥95% of backend connections.
2. Implement the spec's full state machine (`Connecting → Ready → Busy → Draining → Disconnected → Connecting`) with eager reconnect into a warm slot.
3. Preserve all existing behavior: retries, circuit breaker, scoring, cancellation, multi-stream isolation, health/metrics endpoints.
4. Keep the dispatcher's single-actor concurrency model.

**Non-goals**
1. Multi-connection pool per backend (backends are single-flight per spec).
2. WebSocket-level keepalive / ping (not in protocol).
3. TLS (excluded by README non-goals).
4. Tuning macOS sysctls or relying on `SO_LINGER 0` workarounds.

---

## 3. Architecture

### 3.1 Ownership model

The `Dispatcher` actor (single-threaded, message-driven) continues to own all backend state. `BackendConn` gains an owned connection slot:

```rust
pub struct BackendConn {
    pub id: BackendId,
    pub addr: SocketAddr,
    pub state: BackendState,
    pub circuit: CircuitBreaker,
    pub scoring: BackendScoring,
    pub conn: Option<BackendWs>,        // populated iff state == Ready
    pub reconnect_attempt: u32,         // for backoff calculation
}

pub type BackendWs = WebSocketStream<MaybeTlsStream<TcpStream>>;
```

**Invariant:** `conn.is_some() ⇔ state == Ready`. This is asserted in debug builds and respected by the dispatcher's transition logic.

`BackendWs` is `Send`, so it can be moved across the channel into a forwarding task.

### 3.2 State machine

```
                    ┌─────────────┐
       startup ────►│ Disconnected│
                    └─────┬───────┘
                          │ spawn connect_task
                          ▼
                    ┌─────────────┐  connect failed
                    │ Connecting  ├───────────────┐
                    └─────┬───────┘               │
                          │ ConnectionEstablished │
                          ▼                       │
                    ┌─────────────┐               │
                  ┌►│   Ready     │               │
                  │ └─────┬───────┘               │
                  │       │ dispatch              │
                  │       ▼                       │
                  │ ┌─────────────┐               │
                  │ │   Busy      │               │
                  │ └─────┬───────┘               │
                  │       │ terminal frame seen   │
                  │       │ (in forwarding task)  │
                  │       ▼                       │
                  │ ┌─────────────┐               │
                  │ │  Draining   │ (in-task)     │
                  │ └─────┬───────┘               │
                  │       │ drain done / timeout  │
                  │       ▼                       │
                  │ ┌─────────────┐               │
                  │ │Disconnected │◄──────────────┘
                  │ └─────┬───────┘
                  │       │ spawn connect_task (with backoff if attempt>0)
                  │       │ (skip if circuit open — wait for BackendRecovered)
                  │       ▼
                  │ ┌─────────────┐
                  └─┤ Connecting  │
                    └─────────────┘
```

`Draining` is observed by the dispatcher only as a transient — the forwarding task does the drain inline before sending `StreamCompleted`. Exposed as a state value for `/health` observability when a forwarding task is in its drain window.

`Disconnected` is also transient: the dispatcher's invariant is *if circuit is closed/half-open and state is Disconnected, a connect task must be in flight or about to be spawned*.

### 3.3 Channel topology

Two new dispatcher events:

```rust
pub enum DispatchEvent {
    NewRequest(PendingRequest),
    StreamCompleted(ForwardResult),
    BackendRecovered(BackendId),
    CancelStream { stream_id: StreamId },
    QueryHealth(oneshot::Sender<HealthSnapshot>),
    ConnectionEstablished {                    // NEW
        backend_id: BackendId,
        ws: BackendWs,
    },
    ConnectionFailed {                         // NEW
        backend_id: BackendId,
        error: String,
    },
}
```

Channel capacity stays at 256 (current value). `BackendWs` does not impl `Clone` and the channel is `mpsc`, so ownership transfer is clean.

### 3.4 Hot-path flow

1. **Startup.** Dispatcher initialization sets each backend to `Disconnected` and immediately spawns `connect_task(addr, id, dispatch_tx, attempt=0)` for each. State transitions to `Connecting`.
2. **Connect succeeds.** Connect task sends `ConnectionEstablished { backend_id, ws }`. Dispatcher: `state = Ready`, `conn = Some(ws)`, `reconnect_attempt = 0`. Calls `try_dispatch()`.
3. **Connect fails.** Connect task sends `ConnectionFailed { backend_id, error }`. Dispatcher: `state = Disconnected`, `reconnect_attempt += 1`. If circuit closed/half-open, spawn another `connect_task` (with backoff); otherwise wait for `BackendRecovered`.
4. **Dispatch.** `find_best_backend` only considers backends where `state == Ready`. On match: `state = Busy`, `conn.take()`, hand `ws` into `forward::run_forwarding(backend_id, ws, request, …)`.
5. **Forwarding task does its work** (see §4). On terminal outcome, runs `drain_close(&mut ws, 100ms)` inline, drops `ws`, sends `StreamCompleted`.
6. **Stream completion.** Dispatcher: `state = Disconnected`. Apply outcome (success/failure → scoring, circuit, retry queue). If circuit is closed or half-open, spawn `connect_task` so the next dispatch finds a warm slot. If circuit just opened, skip the spawn — the cooldown timer (§5.3) will trigger reconnect later.

### 3.5 Concurrency invariants

- Dispatcher actor is single-threaded; all `BackendConn` mutations happen inside `Dispatcher::run`. No locking on `BackendConn`.
- `BackendWs` is owned by exactly one of: (a) `BackendConn::conn` (state Ready), or (b) a forwarding task (state Busy/Draining), or (c) nothing (state Connecting/Disconnected). Type system can't enforce this; assert in debug.
- Connect tasks and forwarding tasks both communicate back to the dispatcher only via `dispatch_tx`.

---

## 4. Forwarding task: in-task state and behavior

### 4.1 Liveness check (first action)

REFUSE chaos rolls per-connection inside `mock_backend.handle_connection`. A warm slot may already be `error+closed` before dispatch. To detect this without spending a full request roundtrip:

```rust
match futures_util::future::poll_immediate(ws.next()).await {
    None => { /* Pending: connection alive, proceed */ }
    Some(None) => return drained_outcome(ForwardOutcome::BackendCrashed),
    Some(Some(Err(_))) => return drained_outcome(ForwardOutcome::BackendCrashed),
    Some(Some(Ok(Message::Close(_)))) => return drained_outcome(ForwardOutcome::BackendCrashed),
    Some(Some(Ok(Message::Text(t)))) => {
        // Pre-start text: REFUSE error or similar. Parse and route.
        return handle_pre_start_text(t, &mut ws).await;
    }
    Some(Some(Ok(_))) => {
        // Pre-start binary: malformed warm slot.
        return drained_outcome(ForwardOutcome::MalformedResponse("pre-start binary".into()));
    }
}
```

Where `drained_outcome(o)` runs `drain_close` then returns `o`.

`handle_pre_start_text(t, ws)` parses `t` as a `BackendMessage`:
- If it's `Error { message }` containing "busy" → drain → return `BackendBusy`.
- If it's `Error { message }` (any other text, e.g., REFUSE's "Worker unavailable") → drain → return `BackendError(message)`.
- If it's `Done`, `Queued`, `Unknown(_)`, or any other variant pre-start → drain → return `MalformedResponse(raw)` (server is not following protocol — `start` hasn't been sent yet).

Cost: one non-blocking poll (~100ns).

### 4.2 Drain implementation

```rust
async fn drain_close(ws: &mut BackendWs, deadline: Duration) {
    let _ = tokio::time::timeout(deadline, async {
        while ws.next().await.is_some() {}
    }).await;
}
```

**Default deadline: 100ms.** Loopback close handshake completes in single-digit ms; real-network close handshake adds round-trip latency, still well under 100ms in practice. Timeout fall-through reverts to active close for that one connection — same as today's behavior, no regression.

**Where called:** every terminal path inside the forwarding loop, before returning the `ForwardOutcome`:
- `Done` text → drain → `Success`
- Server `error` text (incl. "busy") → drain → `BackendError` / `BackendBusy`
- `Message::Close` → drain (no-op; peer already closed) → `BackendCrashed`
- `Ok(None)` from `ws.next()` → drain (no-op) → `BackendCrashed`
- `Ok(Some(Err(_)))` → drain → `BackendCrashed`
- Unknown / malformed text or binary → drain → `MalformedResponse`

### 4.3 Paths where active close is unavoidable

- **HANG**: server is in `asyncio.sleep(300)` and will not close. After 5s `hang_timeout`, drop `ws` (no drain — would just sit idle for the full timeout). Multiplexer is active closer for this connection.
- **Cancel before any chunk**: backend doesn't know the request was cancelled at the multiplexer level. Best-effort: send `Message::Close(None)` via `ws.close(None).await` with 50ms timeout, then drain briefly. Likely lands as active closer.
- **Cancel mid-stream**: backend keeps sending (no cancel support — README §2 invariants). Same best-effort close+drain. Active closer in most cases.
- **ClientGone**: same shape as cancel mid-stream.

**Steady-state TIME_WAIT cost**: bounded by `(hang_rate + cancel_rate + client_gone_rate) × throughput × 30s`. At default chaos (5% hang) and ~30 req/s on the loopback rig, this is ~50 TIME_WAIT entries steady-state. Three orders of magnitude under the budget.

---

## 5. Reconnect

### 5.1 Connect task

```rust
async fn connect_task(
    addr: SocketAddr,
    backend_id: BackendId,
    dispatch_tx: mpsc::Sender<DispatchEvent>,
    attempt: u32,
) {
    if attempt > 0 {
        tokio::time::sleep(backoff_for_attempt(attempt)).await;
    }

    let url = format!("ws://{addr}/v1/ws/speech");
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(&url),
    ).await;

    match result {
        Ok(Ok((ws, _))) => {
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
```

### 5.2 Backoff schedule

```
attempt 0 → 0ms
attempt 1 → 100ms ± 25ms jitter
attempt 2 → 250ms ± 60ms
attempt 3 → 500ms ± 125ms
attempt 4 → 1000ms ± 250ms
attempt 5 → 2000ms ± 500ms
attempt 6+ → 5000ms ± 1250ms (capped)
```

Reset to 0 on every `ConnectionEstablished`.

### 5.3 Circuit-breaker interaction

- `ConnectionFailed` does **not** call `circuit.record_failure()`. Connect failures are dominated by transient pressure (port exhaustion during recovery, brief server flap). Circuit is driven by request-level outcomes.
- Request-level outcomes (`BackendCrashed`, `BackendHung`, `BackendError`, `MalformedResponse`) continue to call `circuit.record_failure()` exactly as today.
- When circuit transitions to `Open` (in `handle_stream_completed`), the dispatcher spawns a one-shot timer task:
    ```rust
    let dispatch_tx = self.dispatch_tx.clone();
    let bid = backend_id;
    let cooldown = self.config.circuit_cooldown;
    tokio::spawn(async move {
        tokio::time::sleep(cooldown).await;
        let _ = dispatch_tx.send(DispatchEvent::BackendRecovered(bid)).await;
    });
    ```
- This is the **only** emitter of `BackendRecovered`. It replaces the existing 500ms-on-every-failure timer.
- `handle_backend_recovered` semantics change: instead of going `Disconnected → Ready` directly, it transitions the circuit to `HalfOpen` (via `circuit.record_attempt()` or equivalent) and spawns a `connect_task`. The backend goes `Disconnected → Connecting`. If the connect succeeds, eventually a request dispatched to it will either succeed (closing the circuit) or fail (re-opening it via the normal stream-completed path).

### 5.4 Replaces existing recovery timer

`dispatch/mod.rs:282-294` currently always sleeps 500ms then sends `BackendRecovered` regardless of circuit state — meaning every backend failure triggered a recovery, not just circuit-open ones. With the new model:
- The 500ms-every-failure timer is removed.
- Reconnect after a normal failure is driven directly by spawning `connect_task` from `handle_stream_completed` (subject to circuit state — see §3.4 step 6).
- Circuit-cooldown recovery is driven by the dedicated one-shot timer spawned only when the circuit opens (§5.3).

---

## 6. File-by-file impact

| File | Change |
|---|---|
| `src/backend/state.rs` | Replace 3-state enum with 5-state enum (`Connecting`, `Ready`, `Busy`, `Draining`, `Disconnected`). Update `name()`, `is_ready()`, `Display`. Remove the comment opting out of Connecting/Draining. |
| `src/backend/connection.rs` | Add `conn: Option<BackendWs>` field. Add `reconnect_attempt: u32`. Update `is_available()` to require `Ready`. Add type alias `BackendWs`. |
| `src/forward.rs` | Refactor `run_forwarding` / `do_forward` to take `ws: BackendWs` by value instead of opening it. Add liveness check at the top of `do_forward`. Add `drain_close()`. Wire drain into every terminal-frame return path. Remove the URL/connect logic from this file. |
| `src/dispatch/mod.rs` | Add `ConnectionEstablished` and `ConnectionFailed` event variants and handlers. Add `spawn_connect(backend_id, attempt)` helper. Modify `dispatch_to` to take `ws` from `conn.take()`. Remove the 500ms-then-`BackendRecovered` timer in `handle_stream_completed`. Modify startup to spawn connect tasks for every backend. Update circuit-breaker recovery path (`handle_backend_recovered`) to spawn a connect task instead of going directly to `Ready`. |
| `src/health.rs` | Surface the new states in `/health` output (`connecting`, `draining` in addition to existing values). |
| `src/main.rs` | No change beyond passing through. |
| `src/config.rs` | Add `drain_timeout_ms` (default 100), `connect_timeout_secs` (default 3 — currently hard-coded). |

Estimated diff size: ~250–350 lines net additions, ~50 lines removed.

---

## 7. Failure modes and edge cases

| Scenario | Behavior |
|---|---|
| Server REFUSE on warm slot | Liveness check catches the pre-start error/close text; drain; record `BackendError`; transition to `Disconnected`; spawn reconnect. |
| Server crashes mid-stream | Forwarding task gets `Ok(None)` or `Close`; drain (no-op); return `BackendCrashed`. Multiplexer is passive closer (server FIN already received). |
| Server hangs | `hang_timeout` fires; drop `ws` (no drain). Multiplexer is active closer. Bounded TIME_WAIT cost. |
| Server sends malformed | Drain, return `MalformedResponse`. |
| Server "busy" error | Drain, return `BackendBusy`. Requeue (no penalty). |
| Connect fails (port pressure / server down) | `ConnectionFailed` → backoff → retry. Does not count toward circuit. |
| Circuit opens during request | Request-level failure recorded → circuit opens → on `Disconnected`, do NOT spawn reconnect. Wait for `BackendRecovered`. |
| Drain timeout (100ms exceeded) | Drop `ws`. Multiplexer becomes active closer for that connection. Same as today's behavior; rare. |
| Cancel during drain | Drain is a few-ms window; cancel arrives while in-flight gets handled normally on next dispatch. |
| Dispatcher channel full | Connect task `send().await` blocks (channel is `mpsc::Sender` with `await`). Acceptable backpressure; channel cap is 256 vs. 8 backends. |

---

## 8. Testing plan

### 8.1 Bug-fix verification

Reuse the existing reproducer:

1. Start backends in chaos mode (`python assignment/mock_backend.py --workers 8 --base-port 9100`).
2. Start multiplexer.
3. Sample socket states every 1s during the run (reuse `/tmp/sample_sockets.sh`).
4. Run `python assignment/test_client.py ws://localhost:9000/v1/ws/speech --chaos --requests 500 --concurrency 32`.

**Pass criteria:**
- Zero `connect failed.*os error 49` log lines.
- Peak mux-side TIME_WAIT < 200 (vs. 16,267 today).
- Chaos suite success rate ≥ 95% (vs. 7.6% today).

### 8.2 Spec compliance

- `/health` shows backends transitioning through `connecting`, `ready`, `busy`, `draining`, `disconnected` over the run (verified by repeated polls).
- After a request completes, the backend's state returns to `ready` (warm slot) within a small window (target: <50ms on loopback).

### 8.3 Performance / regression

- `--multiplex` suite: stream isolation holds.
- `--soak 600` suite: RSS growth <5MB.
- `--compare` suite: chunk forwarding overhead unchanged (drain is post-terminal, doesn't affect chunk hot path).
- TTFC EWMA: should *improve* slightly because connect handshake is now amortized into the warm-slot window rather than serialized into the request.

### 8.4 Failure-injection unit checks

If we add unit tests, target:
- Liveness check correctly classifies pre-start text/binary/close/None.
- `drain_close` returns within deadline when peer never closes.
- Reconnect backoff escalates and resets correctly.
- Circuit-open state suppresses reconnect attempts.

---

## 9. Risks & open questions

1. **`futures_util::future::poll_immediate` may exhibit edge behavior with `WebSocketStream`**: needs a smoke test that `Pending` is returned reliably for an idle stream and `Ready(Some(_))` for one with buffered data. Backup plan: use `tokio::time::timeout(Duration::from_millis(0), ws.next())`, which is heavier but well-defined.
2. **Drain timeout of 100ms** might be tight on real network with high RTT (e.g., cross-region). Mitigation: configurable via CLI; default 100ms is fine for the chaos rig and typical regional deployments. Document it.
3. **Warm slot held idle** has no keepalive. If a real load balancer GCs idle TCP after some interval, we'd discover it at dispatch time via the liveness check (which would catch a `None`). Acceptable cost: one wasted dispatch attempt + reconnect. Out of scope to add keepalive.
4. **Refactor blast radius**: `forward.rs` and `dispatch/mod.rs` both change substantially. Risk of regressing stream-multiplexing or cancellation behavior. Mitigation: run the full `--all` suite as part of acceptance, not just `--chaos`.

---

## 10. Out of scope / follow-ups

- Periodic keepalive ping for warm connections.
- Dynamic `drain_timeout` based on observed close-handshake latency.
- Observability: histogram of drain duration, gauge of warm-slot age.
- **Liveness fallback for stuck `Connecting` state**: if a `connect_task` panics or is dropped without sending either `ConnectionEstablished` or `ConnectionFailed`, the backend stays `Connecting` forever and is never dispatched to. Today no path produces this, but the dispatcher has no defense if `connect_task`'s contract is ever broken. Mitigations to consider: wrap the spawn body in `catch_unwind` and emit `ConnectionFailed` from the panic arm, OR add a per-backend connect-watchdog timer that emits a synthetic `ConnectionFailed` if the backend stays `Connecting` past `connect_timeout * 2`.
