# Per-Backend Lifecycle Task (C-lite, absorbs #6)

**Date:** 2026-05-04
**Status:** Approved
**Branch:** bottlenecks
**Bottlenecks addressed:** #5 (single dispatcher actor — partial), #6 (reconnect on dispatcher's critical path — fully)

## Problem

Today's `Dispatcher::run` is a single tokio task that owns:
- the request queue,
- *and* every backend's `state`, `conn` (warm slot), `circuit`, `scoring`,
- and processes every connection-lifecycle event (`ConnectionEstablished`,
  `ConnectionFailed`, `BackendRecovered`) on the same event loop as request
  events.

Two consequences:

1. **Multi-writer races.** The dispatcher mutates `backend.state` / `backend.conn`
   in response to events, while the connect task races to publish a new
   connection. We've already added two patches for this class:
   - `take_for_dispatch` atomic transition (commit `e6566b4`)
   - "defensive ConnectionEstablished" handler that drains stray ws (commit `4dd94dd`)
   These patches paper over a structural problem: more than one task writes
   to the same backend's state.

2. **Connection events compete for the actor.** Every successful or failed
   connect attempt costs an actor turn. `try_dispatch` cannot run until the
   actor finishes processing whatever connection event arrived first. This
   is bottleneck #6 ("reconnect on dispatcher's critical path").

## Fix

Move all *per-backend* mutable state out of the dispatcher and into a shared
`Arc<BackendShard>`. Run one **lifecycle task per backend** that is the
**only writer** to that shard's `state`, `slot`, `circuit`, and `scoring`.
Remove `ConnectionEstablished`, `ConnectionFailed`, and `BackendRecovered`
from the dispatcher's event enum entirely — they are now internal to the
lifecycle task.

The dispatcher actor stays single-task and keeps its current shape:
- owns the queue,
- owns `in_flight: HashMap<StreamId, oneshot::Sender<()>>`,
- on each event, scans shards and tries to take a warm slot.

What changes for the dispatcher:
- It reads backend state through `Arc<BackendShard>` instead of `&mut self.backends[i]`.
- It learns about a backend going Ready via a new `DispatchEvent::BackendReady(BackendId)`
  message sent by the lifecycle task. (This replaces both `ConnectionEstablished`
  and `BackendRecovered`.)
- It no longer spawns connect tasks. The lifecycle task owns that.
- On forward completion, the forward task fans out two messages:
  one to the lifecycle task (`LifecycleEvent::ForwardDone(outcome)`),
  one to the dispatcher (`DispatchEvent::TerminalOutcome { req_data, outcome }`).
  Lifecycle handles backend health; dispatcher handles request fate
  (retry / done frame / error frame).

## Architecture

```
┌────────────────────────────────────────────────────────────────┐
│ Dispatcher actor (1 task)                                       │
│   queue: BoundedQueue<PendingRequest>                           │
│   shards: Vec<Arc<BackendShard>>     ← read-only sharing        │
│   in_flight: HashMap<StreamId, oneshot::Sender<()>>             │
│   active_streams: usize                                          │
│   Events: NewRequest, TerminalOutcome, BackendReady,            │
│           CancelStream, QueryHealth                              │
└────────────────────────────────────────────────────────────────┘
              │ try_take_slot()           ▲ BackendReady
              ▼                            │
┌────────────────────────────────────────────────────────────────┐
│ BackendShard (Arc — 1 per backend)                              │
│   id, addr, addr_str: Arc<str>                                  │
│   state: AtomicU8                       ← cheap dispatch read   │
│   slot:  Mutex<Option<BackendWs>>       ← std::sync::Mutex      │
│   circuit: Mutex<CircuitBreaker>                                │
│   scoring: Mutex<BackendScoring>                                │
│   lifecycle_tx: mpsc::Sender<LifecycleEvent>                    │
│   reconnect_attempt: AtomicU32                                  │
└────────────────────────────────────────────────────────────────┘
              ▲ LifecycleEvent::ForwardDone(outcome)
              │ from each forward task
┌────────────────────────────────────────────────────────────────┐
│ BackendLifecycle task (1 per backend, single writer)            │
│   Loop on lifecycle_rx receiving:                               │
│     ForwardDone(outcome)  → update scoring/circuit; transition   │
│                             state; spawn reconnect or cooldown   │
│     CooldownExpired       → spawn connect (= recovery probe)    │
│   Internally spawns connect_task; on its result:                │
│     Ok(ws)  → slot.lock().insert(ws); state→Ready;              │
│               send BackendReady to dispatcher                   │
│     Err(e)  → state→Disconnected; bump reconnect_attempt;       │
│               either re-spawn connect (closed circuit) or       │
│               record_open_failure + spawn cooldown timer.       │
└────────────────────────────────────────────────────────────────┘
```

## Components

### `BackendShard`

```rust
pub struct BackendShard {
    pub id: BackendId,
    pub addr: SocketAddr,
    pub addr_str: Arc<str>,                        // cached (#8 bonus)

    state: AtomicU8,                                // BackendState as u8
    slot: std::sync::Mutex<Option<BackendWs>>,
    circuit: std::sync::Mutex<CircuitBreaker>,
    scoring: std::sync::Mutex<BackendScoring>,

    lifecycle_tx: mpsc::Sender<LifecycleEvent>,
    reconnect_attempt: AtomicU32,
}
```

**Locking discipline:**
- `slot`, `circuit`, `scoring` use `std::sync::Mutex`. Locks are held for
  the duration of one method call only, never across `.await`.
- `state` is atomic — the dispatcher's scan reads it lock-free.
- The dispatcher takes `slot.lock()` briefly to `take()` the warm WS at
  dispatch time; it transitions `state` Ready→Busy *while holding the slot
  lock* so two concurrent dispatches on the same shard cannot both win.

**Single-writer invariant:** Only the lifecycle task writes to
`state` (with one exception below), `slot.insert()`, `circuit`, `scoring`,
`reconnect_attempt`. The dispatcher only writes `state` Ready→Busy
inside `slot.lock()` during dispatch and `slot.take()` at the same time —
this is a tightly-bounded handoff that is safe under the slot lock.

### `BackendLifecycle` task

One tokio task per backend, spawned at startup. Owns the consumer end of
`lifecycle_rx` and the `dispatcher_tx` for sending `BackendReady`.

```rust
enum LifecycleEvent {
    ForwardDone(ForwardOutcome),
    CooldownExpired,
}
```

Pseudocode:

```rust
async fn run(self) {
    self.attempt_connect().await;          // initial connect on startup
    while let Some(ev) = self.rx.recv().await {
        match ev {
            LifecycleEvent::ForwardDone(outcome) => {
                self.apply_outcome(outcome);
                if self.circuit_just_opened() {
                    self.schedule_cooldown_timer();
                } else {
                    self.attempt_connect().await;
                }
            }
            LifecycleEvent::CooldownExpired => {
                // Acts as the half-open probe via connect.
                self.attempt_connect().await;
            }
        }
    }
}

async fn attempt_connect(&self) {
    self.set_state(Connecting);
    match connect_with_timeout(self.shard.addr, self.config.connect_timeout).await {
        Ok(ws) => {
            self.shard.slot.lock().unwrap().replace(ws);
            self.set_state(Ready);
            self.shard.reconnect_attempt.store(0, Relaxed);
            let _ = self.dispatch_tx.send(DispatchEvent::BackendReady(self.id)).await;
        }
        Err(_) => {
            let was_probe = self.shard.circuit.lock().unwrap().cooldown_elapsed();
            self.set_state(Disconnected);
            if was_probe {
                self.shard.circuit.lock().unwrap().record_open_failure();
                self.schedule_cooldown_timer();
            } else {
                self.shard.reconnect_attempt.fetch_add(1, Relaxed);
                // immediate respawn — reconnect uses connect_task's internal backoff
                self.attempt_connect().await;
            }
        }
    }
}
```

The lifecycle task does **not** spawn detached connect tasks. It awaits the
connect result inline. This is fine because each lifecycle task is
independent — backend A's connect attempt never blocks backend B's lifecycle.

### Dispatcher actor (slimmed)

```rust
pub enum DispatchEvent {
    NewRequest(PendingRequest),
    TerminalOutcome {
        backend_id: BackendId,
        stream_id: StreamId,
        outcome: ForwardOutcome,
        // request payload for retry/error/done construction:
        text: String,
        speaker_id: u32,
        client_streams: Arc<ClientStreams>,
        retries_remaining: u32,
        created_at: Instant,
        active_count: Arc<AtomicUsize>,
        tried_backends: Vec<BackendId>,
    },
    BackendReady(BackendId),
    CancelStream { stream_id: StreamId },
    QueryHealth(oneshot::Sender<HealthSnapshot>),
}
```

Note absence of `ConnectionEstablished`, `ConnectionFailed`, `BackendRecovered`,
`StreamCompleted`. The first three are handled inside the lifecycle task;
`StreamCompleted` is replaced by `TerminalOutcome` (no `ForwardResult`-style
struct because the lifecycle handling is decoupled).

**`try_dispatch`** scans `Vec<Arc<BackendShard>>` instead of `&mut Vec<BackendConn>`.
Picks shard via existing TTFC EWMA + error-rate logic (reads through Arc).
On dispatch: takes `shard.slot.lock()`, swaps `Option<BackendWs>` to `None`,
state Ready→Busy, releases lock, spawns forward task. If two concurrent
dispatch calls race, the second sees `None` after locking and continues
the scan.

**On `TerminalOutcome`:**
- update `active_streams` counter (request-level concern, not backend-level)
- decide retry / done / error from outcome (same logic as today)
- emit user-visible frame
- run `try_dispatch` again

The dispatcher does NOT touch `circuit`, `scoring`, or `slot` on
`TerminalOutcome`. Those updates happen in the lifecycle task in parallel,
triggered by the same forward task fanning out a `ForwardDone(outcome)`
message to lifecycle.

### Forward task

Receives a `Sender<LifecycleEvent>` (cloned from `shard.lifecycle_tx`) and
the `Sender<DispatchEvent>` (existing). On completion:

```rust
let outcome = do_forward(...).await;
let _ = lifecycle_tx.send(LifecycleEvent::ForwardDone(outcome.clone_lite())).await;
let _ = dispatch_tx.send(DispatchEvent::TerminalOutcome { ... outcome ... }).await;
```

`ForwardOutcome` needs to be cloneable enough to send to both channels.
Since it's small (variants carry `Duration`, `f64`, `String`), implementing
`Clone` is trivial.

## Data flow

### Successful request

```
client → Dispatcher::NewRequest
    Dispatcher: enqueue, try_dispatch
    Dispatcher: lock shard.slot, take ws, state Ready→Busy
    Dispatcher: spawn forward task with (ws, lifecycle_tx, dispatch_tx)

forward task → backend → completes Success
    forward → Lifecycle::ForwardDone(Success)
        Lifecycle: scoring.record_ttfc + record_result(true);
                   circuit.record_success;
                   state→Disconnected;
                   attempt_connect (background)
                   ... slot fills ...
                   state→Ready
                   send BackendReady to dispatcher
    forward → Dispatcher::TerminalOutcome
        Dispatcher: emit done frame; try_dispatch
```

### Failed request with retries

```
forward → Lifecycle::ForwardDone(BackendCrashed)
    Lifecycle: scoring.record_result(false); circuit.record_failure;
               if circuit just opened: state→Disconnected, schedule cooldown;
               else: state→Disconnected, attempt_connect.
forward → Dispatcher::TerminalOutcome
    Dispatcher: push_front to queue with bumped tried_backends;
                try_dispatch (likely picks a different ready backend immediately).
```

The retry doesn't wait for the failed backend's reconnect — it dispatches to
any available backend immediately, exactly as today.

### Cancellation

`CancelStream` arrives at dispatcher. Dispatcher fires `oneshot` to the
forward task. Forward task observes cancel, sends
`ForwardOutcome::Cancelled` to lifecycle (which clears slot + reconnects).
Forward task does NOT send `TerminalOutcome` to dispatcher because the
client request is already gone. (Identical to today's behavior.)

### Health snapshot

`QueryHealth` arrives at dispatcher. Dispatcher reads each shard's atomic
`state` lock-free, briefly locks `circuit` + `scoring` for stats. Snapshot
may show ephemerally-inconsistent state across shards; this was already true
before #5 (snapshot was assembled while connection events were in flight),
just less visible.

## Migration plan

Implementation order (each step independently testable):

1. Define `BackendShard` and move `state`/`conn`/`circuit`/`scoring` fields
   into it. Keep `Vec<Arc<BackendShard>>` on the dispatcher. Update
   `try_dispatch` and `find_best_backend` to read through Arc. Tests
   should still pass (no behavior change yet).

2. Add `LifecycleEvent` enum, `BackendLifecycle` struct, and spawn one
   lifecycle task per backend at startup. Lifecycle task currently does
   nothing — just receives and discards events.

3. Move connect logic out of `Dispatcher::spawn_connect` and
   `handle_connection_*` into the lifecycle task's `attempt_connect`.
   Remove `ConnectionEstablished` / `ConnectionFailed` from `DispatchEvent`
   enum and from the actor's match arm. Lifecycle task sends
   `BackendReady` instead.

4. Move circuit-cooldown timer out of `Dispatcher::spawn_circuit_cooldown_timer`
   into the lifecycle task. Remove `BackendRecovered` from `DispatchEvent`.

5. Add fan-out at forward task completion: send `LifecycleEvent::ForwardDone`
   in addition to (eventually replacing) `StreamCompleted`. Lifecycle task
   updates scoring/circuit/state. Dispatcher's `StreamCompleted` becomes
   `TerminalOutcome` (rename + drop the lifecycle-related fields).

6. Cleanup: remove `BackendConn` (replaced by `BackendShard`); remove
   `take_for_dispatch` (the slot-lock + state CAS is now the atomic swap);
   remove the "defensive ConnectionEstablished" drain handler (no longer
   reachable).

After step 6, the new structure is in place. Each step preserves all
existing tests.

## Tests

Add unit tests for `BackendShard`:
- `slot_take_under_lock_atomic_with_state` — concurrent take attempts
  result in exactly one `Some` and one `None`.
- `lifecycle_single_writer_invariant` — given mocked connect results,
  the lifecycle task's state transitions match expectations.
- `cooldown_expired_triggers_probe` — cooldown timer pushes
  `CooldownExpired` and the lifecycle task attempts connect.

Add unit tests for the slimmed dispatcher:
- `terminal_outcome_retry_repushes_with_tried_backends` — same logic as
  today but expressed against the new event shape.
- `backend_ready_event_triggers_try_dispatch` — sending `BackendReady`
  on an empty queue is a no-op; with a queued request it dispatches.

Integration:
- Existing chaos / multiplex / slow-consumer / correctness suites must
  pass calmly. Chaos number is expected unchanged (within ±20pp variance).
- New: a stress test that fires NewRequest while ConnectionEstablished
  events would normally be in flight, verifying the dispatcher handles
  load without the actor turn for connection events.

## Out of scope

- **Sharding the dispatcher actor** (the "C-full" design). Deferred
  pending evidence the actor is a real bottleneck.
- **Per-shard request queues.** Deferred with sharding.
- **Multi-slot backends** (`Vec<BackendWs>` per shard for batching).
  Pre-positioned by this design — slot becomes `slots: Mutex<SlotVec>` —
  but not implemented here.
- **Replacing `std::sync::Mutex` with `parking_lot`.** Nanoseconds, not
  measurable.

## Risk register

| Risk | Mitigation |
|---|---|
| Two concurrent dispatch calls race on same shard | Atomic state CAS *inside* slot lock makes the take exclusive; loser sees None and falls through. |
| Lifecycle task channel fills under high failure rate | Bounded mpsc with capacity = max_retries + 4; if it ever fills (shouldn't), forward task awaits — back-pressures the dispatcher loop, which is acceptable for terminal events. |
| Snapshot inconsistency in /health output | Acceptable — already true today; metrics are eventually-consistent by design. |
| Migration introduces regression | Stepwise migration plan keeps tests green at each step. |
