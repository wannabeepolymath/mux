# TTS Multiplexer — Design

This document explains the multiplexer's concurrency model, the state machines it
maintains, and the tradeoffs behind those choices. The implementation lives in
`src/`; cross-references below are to specific files and line ranges.

## 1. Goals and non-goals

The multiplexer sits between WebSocket clients and a pool of single-slot GPU TTS
workers. It must:

- Carry **multiple concurrent streams per client connection** (binary frames
  tagged with stream IDs).
- Survive backends that crash mid-stream, hang, send garbage, refuse
  connections, or run slowly.
- Reconnect to backends eagerly so a warm WebSocket is ready when the next
  dispatch happens.
- Route adaptively: prefer faster, healthier backends.
- Stay bounded in memory under hostile load — never OOM.

Out of scope (per the assignment): TLS, auth, horizontal scaling, persistence,
modifying the backend protocol.

## 2. Architecture at a glance

```
                 ┌─────────────────────────────────────────────────┐
                 │                  client TCP                     │
                 │            (one task per accepted conn)         │
                 └───┬───────────────────────────────┬─────────────┘
                     │ reader: parse client JSON     │ writer: drain ClientEvent
                     │                               │ (bounded mpsc, cap 4096)
                     │                               ▲
                     ▼                               │ Text / Binary
                ┌──────────────────────────────────────┐
                │           Dispatcher actor           │
                │  (single task, owns all mutable      │
                │   backend + queue state)             │
                └───┬───────────────────────────────┬──┘
                    │ spawn forward task             │ spawn connect task
                    ▼                                ▼
            ┌──────────────────┐            ┌──────────────────┐
            │ per-stream       │            │ per-backend      │
            │ forward task     │            │ connect task     │
            │ (owns one warm   │            │ (TCP + WS open,  │
            │  ws while busy)  │            │  reports back)   │
            └────────┬─────────┘            └────────┬─────────┘
                     │ ConnectionEstablished | StreamCompleted | ConnectionFailed | …
                     └────────────► back to Dispatcher via mpsc
```

The whole thing is **one tokio runtime with a single-actor dispatcher**. Hot
state — the queue, all `BackendConn` records, the in-flight cancellation
handles — is owned by `Dispatcher` and never shared. Every other task talks to
it via the `DispatchEvent` mpsc channel.

### Why an actor, not `Arc<Mutex<…>>`

Two reasons. First, the dispatcher's job is inherently sequential: pick a
backend, transition its state from `Ready` to `Busy`, take the warm
`WebSocketStream` out of the slot, spawn the forward task. A mutex would
serialise the same decisions through a lock anyway, with the added risk of
holding it across `.await` points. Second, the events the dispatcher consumes
(`NewRequest`, `StreamCompleted`, `ConnectionEstablished`, `CancelStream`,
`QueryHealth`) all want to mutate the same state atomically. Treating them as
messages on one queue made the invariants — especially the
`conn.is_some() ⇔ state == Ready` one (see §3.1) — trivial to enforce, because
nobody else can ever observe an intermediate state.

The cost is that the dispatcher is a single point of contention. In practice
it does very little per event (state transitions, channel sends, spawning
tasks); profiling shows it doesn't show up as a hotspot.

## 3. State machines

### 3.1 Backend connection state (`src/backend/state.rs`)

```
                ┌─ initial
                ▼
        ┌─ Disconnected ◄─────────────────────────┐
        │     │ (spawn_connect)                   │
        │     ▼                                   │
        │  Connecting ───► (connect failed) ──────┤
        │     │                                   │
        │     │ ConnectionEstablished             │
        │     ▼                                   │
        │   Ready ◄──── (forward task returns)    │
        │     │                                   │
        │     │ take_for_dispatch                 │
        │     ▼                                   │
        └─► Busy ────────────────────────────────►┘
                  (terminal outcome → Disconnected,
                   then spawn_reconnect_if_circuit_allows)
```

Invariant: **`conn.is_some()` iff `state == Ready`.** The warm
`WebSocketStream` is held in the `BackendConn::conn: Option<BackendWs>` slot
only while the backend is `Ready`. The transition from `Ready → Busy` and the
`.take()` of the slot happen atomically inside
`BackendConn::take_for_dispatch()` so this invariant is never transiently
violated. There's a `debug_assert!` (`backend/connection.rs:72-79`) that
catches violations in debug builds.

`Draining` is documented in `state.rs` and listed in the enum but is currently
`#[allow(dead_code)]` — the drain of the close handshake (`forward::drain_close`)
happens inside the spawned forward task, by which point ownership of the
connection has already moved out of the dispatcher, so there's no `BackendConn`
slot to flip into `Draining`. Surfacing it would require a separate
`DrainStarted`/`DrainEnded` event from the forward task. The variant is left in
place so the state vocabulary in the codebase still matches the assignment
spec; this is the one place where the implementation diverges from the spec's
literal state set, and the divergence is intentional and contained.

### 3.2 Circuit breaker (`src/backend/circuit.rs`)

```
        Closed ──(N consecutive failures)──► Open
          ▲                                    │
          │                                    │ cooldown elapsed → can_attempt() trips
          │ record_success                     ▼
          └──────────────── HalfOpen ─────────►
                              │
                              └── failure → Open
```

`can_attempt()` is the single read used by `BackendConn::is_available()` and
by `Dispatcher::spawn_reconnect_if_circuit_allows`. It has a side effect —
when called in `Open` after the cooldown has elapsed, it transitions to
`HalfOpen` — which makes it cheap to "ask whether we're allowed in" without
needing a separate clock check anywhere else.

The half-open path doesn't probe with a separate `GET /health` request to the
backend; it simply lets the next reconnect attempt go through, and the
WebSocket handshake itself serves as the probe. If the handshake fails or the
backend serves an immediate error, `record_failure()` re-opens the circuit and
the next cooldown timer is scheduled. This is a conscious shortcut — adding a
distinct probe protocol would mean defining one for a backend whose only
contract is `/v1/ws/speech`. The cost is that a backend whose WS handshake
succeeds but whose first real request fails will burn one circuit-recovery
attempt; the metrics make that visible.

When the circuit opens, the dispatcher additionally **schedules a
`BackendRecovered` timer** (`dispatch/mod.rs:748-757`) so a backend that's
been sitting in `Open` with no other traffic still gets a reconnect attempt
after the cooldown elapses. Without this, an idle backend whose circuit just
opened would stay `Disconnected` indefinitely.

### 3.3 Per-stream lifecycle on a client connection

```
        client sends `start` ─► active_count++ (cap 4)
        Dispatcher: queued → dispatched → forward task spawned
                                              │
                       cancel ◄───────────────┤
                                              ▼
                      done|error|cancelled → active_count--
```

The per-connection `active_count: Arc<AtomicUsize>` is the gate for the
"4 concurrent streams per connection" rule. The counter is decremented in the
dispatcher's `handle_stream_completed` for every terminal outcome, including
cancel and client-gone, so a misbehaving stream can't leak a slot.

## 4. Request lifecycle

1. **Client `start`** lands in `client::handle_client`. Active-count check
   passes, a `PendingRequest` is built (including `retries_remaining`,
   `created_at`, and an empty `tried_backends: Vec<BackendId>`) and forwarded
   to the dispatcher via `DispatchEvent::NewRequest`.
2. **Dispatcher enqueues** (`BoundedQueue::push_back`). If the queue is full
   (default 64), the request is rejected with an error to the client and
   `active_count` is decremented. The `queued` ack is sent regardless of the
   eventual dispatch outcome — clients can rely on receiving it.
3. **`try_dispatch`** walks the queue from head to tail. For each request it
   asks `find_best_backend(exclude = req.tried_backends)` — the exclusion list
   prevents bouncing the same request onto a backend that just failed it.
   Walking the queue (not just the head) ensures a request whose exclusion
   list rules out every idle backend doesn't block requests behind it.
4. **`dispatch_to`** atomically takes the warm slot via `take_for_dispatch`
   (which transitions `Ready → Busy`), wires up an in-flight cancel oneshot,
   and spawns the forward task.
5. **Forward task** (`forward::run_forwarding`) sends `start` to the backend,
   reads frames with a `hang_timeout`, forwards binary chunks tagged with the
   stream ID, and resolves into a `ForwardOutcome`. The terminal outcome is
   sent back as `DispatchEvent::StreamCompleted`.
6. **Dispatcher records outcome**: updates `BackendScoring`, the circuit
   breaker, and the backend state; on retryable outcomes (crash/hang/malformed/
   error) it pushes the request back to the **front** of the queue with
   `retries_remaining - 1` and the failing backend appended to `tried_backends`.
   After exhausting retries it sends an `error` text frame to the client
   ("backend unavailable after N retries") and decrements `active_count`.
7. **Eager reconnect**: regardless of outcome, the slot transitions to
   `Disconnected` and `spawn_reconnect_if_circuit_allows` immediately starts a
   new connect task so the next dispatch finds a warm slot.

## 5. Passive-close protocol

This is the most failure-mode-sensitive piece of the design.

The backend closes the connection after every successful response. If the
multiplexer is the **active TCP closer** (sends FIN first), the mux-side socket
sits in `TIME_WAIT` for ~30s, occupying an ephemeral port. Under sustained load
that's how you hit `EADDRNOTAVAIL`: with 8 backends and ~3-second cycles, you
churn ~10k ports/minute and exhaust the 16k-port ephemeral range within minutes.

`forward.rs` is structured to ensure the **backend** sends FIN first in the
happy path:

- After receiving `{"type":"done"}` we **don't drop the WS yet**; we call
  `drain_close(ws, drain_timeout)` which reads until EOF or the timeout (default
  100ms). Once the backend's FIN arrives, the next `ws.next().await` returns
  `None`, our drop sends FIN-ACK, and we're the passive closer.
- For paths where the peer never sends FIN (hang, cancel, client-gone) we take
  the active-close cost on purpose — `active_close` sends a WS Close frame
  (capped at 50ms) then drains. These paths are rare.

The verification doc (`docs/superpowers/results/2026-05-02-chaos-fix-verification.md`)
shows mux-side `TIME_WAIT` dropping from ~16k to ~6 under load after this was
implemented, and zero `EADDRNOTAVAIL` log lines in the 500-request chaos run.

## 6. Adaptive routing

`BackendScoring` (`src/backend/scoring.rs`) maintains:

- **TTFC EWMA**, α = 0.3. First observation seeds the EWMA directly (otherwise
  the initial value would be 70% of the first measurement, which biases low).
  Failed attempts also feed the EWMA with a `hang_timeout`-shaped penalty so a
  flaky backend's score reflects real cost.
- **Sliding-window error rate** over the last 20 requests (`VecDeque<bool>`).
- **Lifetime `total_requests`** counter (for /health).

`find_best_backend` runs two passes (`dispatch/mod.rs:650-705`):

1. Prefer the backend with the **lowest TTFC EWMA** among those with `error_rate < 30%`,
   a closed/half-open circuit, and a warm slot.
2. If every candidate fails the 30% gate (which happens under heavy chaos),
   re-run the same comparison without the error-rate filter so requests are
   never permanently undispatchable.

Round-robin tie-break via `next_backend_idx` prevents all traffic from hammering
backend 0 when scores are equal (e.g. on startup before any TTFC has been recorded).

## 7. Backpressure (drop-oldest, per-stream)

The egress path per client connection is a **per-stream byte-bounded buffer
with drop-oldest semantics**, implemented in `src/client/buffer.rs` and
`src/client/router.rs`.

**Components.**
- `StreamBuffer` — a `VecDeque<StreamEvent>` capped at `cap_bytes`
  (default 256KB, configurable via `--client-buffer-bytes`). `push_chunk`
  drops the oldest `Chunk` events until the new one fits (or until no Chunks
  remain, in which case the new chunk is admitted alone — we don't lose data
  we've already promised to deliver). `Terminal` events (`done`/`error` JSON)
  are never dropped and don't count against the byte cap.
- `BinaryRouter` — per-client: a `std::sync::Mutex<RouterState>` plus a
  `tokio::sync::Notify`. `RouterState` holds the per-stream `StreamBuffer`s,
  a `drain_order` VecDeque for fair round-robin across streams, and a
  generic `text` VecDeque for non-stream-scoped frames (`queued` ack,
  queue-full / stream-limit errors).
- `ClientChannel` — cheap-clone handle (`Arc<BinaryRouter> + Arc<Metrics>`).
  Plumbed through `PendingRequest` and `ForwardResult` so the dispatcher
  and forward tasks can push without holding a long-lived reference.

**Why `std::sync::Mutex`, not `tokio::sync::Mutex`.** No critical section
spans an `.await`; the mutex is held for microseconds (pop one event, drop
lock, do the I/O outside the lock). Tokio's own docs recommend `std::sync::Mutex`
in exactly this shape — it avoids the cooperative-yield overhead of the async
mutex on the hot path. An earlier iteration that used `tokio::sync::Mutex`
showed a measurable throughput regression under chaos; switching restored it.

**Ordering invariant: terminal text after final chunks.** Stream-scoped text
frames (`done`, `error`) are pushed into the same per-stream `StreamBuffer`
via `push_terminal`, not into the generic text queue. FIFO pop order
guarantees the client receives all buffered chunks for stream S before S's
terminal text. Non-stream text (`queued` ack, generic errors) bypasses the
per-stream buffer entirely.

**Producer/consumer.** Forward tasks call `channel.push_chunk(stream_id, bytes)`
— a non-blocking, drop-oldest push. They never park on the client. The
backend is read at line rate, which releases the backend slot back to the
pool as soon as the backend finishes producing. The dedicated writer task
loops on `router.next_work().await`: register interest via `Notify::notified()`,
acquire the lock, pop one event, drop the lock, write to the WS sink.

**Observability.** `mux_backpressure_drops_total` counts dropped chunks
lifetime. Each push that drops at least one chunk emits a `WARN`-level
tracing event with `stream_id` and `dropped` count.

**Tradeoff.** Drop-oldest sacrifices completeness under sustained slow
consumers (the client misses the front of the audio) in exchange for
*never parking the forward task*. That matters under chaos: a parked
forward task holds a `Busy` backend slot, so blocking backpressure caps
aggregate throughput. The bounded ring also caps memory at
`cap_bytes × max_streams_per_conn × active_connections` — for defaults
that's 256KB × 4 × N clients.

## 8. Concurrency model summary

| Task | Spawned when | Owns | Talks to |
|---|---|---|---|
| Dispatcher | startup | All `BackendConn` records, queue, in-flight cancel handles | All other tasks (via mpsc) |
| Client reader | client TCP accept | client WS reader half | Dispatcher (events), writer (close signal) |
| Client writer | client TCP accept | client WS writer half | Drains `BinaryRouter` via `next_work()` |
| Forward task | per dispatch | one warm `BackendWs` | Client writer (binary+text), Dispatcher (`StreamCompleted`) |
| Connect task | per (re)connect | nothing persistent | Dispatcher (`ConnectionEstablished` / `ConnectionFailed`) |
| Circuit cooldown timer | circuit just opened | nothing | Dispatcher (`BackendRecovered`) |
| HTTP server | startup | nothing | Dispatcher (`QueryHealth`) |

No `Arc<Mutex<…>>` exists in hot paths. Shared state is either immutable
configuration (`Arc<Config>`) or atomic counters (`active_count`,
prometheus metrics).

## 9. Operational surface

- **`/health`** — JSON snapshot with backend state, scoring, circuit, queue
  depth, active streams/connections.
- **`/metrics`** — Prometheus text format. Required metrics: `mux_requests_total{status}`,
  `mux_ttfc_seconds` (histogram, spec buckets), `mux_queue_depth`,
  `mux_active_streams`, `mux_backend_state{backend,state}`,
  `mux_chunk_forward_seconds`. Plus `mux_active_connections`.
- **Tracing** — `tracing_subscriber` with `RUST_LOG` env filter, default `info`.
  Per-stream debug logging keys on `stream_id` and `backend_id`.

## 10. Tradeoffs and known limitations

1. **Chaos survival rate.** The repo's most recent verification run
   (`docs/superpowers/results/2026-05-02-chaos-fix-verification.md`) measured
   **83.5%** end-to-end success at 200 reqs / 16 concurrency, against the
   spec's 95% target. The doc breaks down why: each backend cycle under chaos
   averages ~3.6s (1× hang-budget per 5% hang event, 3× normal duration per
   15% slow event), so 8 backends ceiling at ~2.2 req/s aggregate. At
   concurrency 16, the queue accumulates faster than it drains and tail
   requests time out in the test client's 30s window. Closing the gap would
   require relaxing the one-request-per-backend constraint (which the spec
   prohibits) or shortening the slow-chaos cycle, neither of which is in the
   multiplexer's hands. Worth re-measuring on the current `main`.

2. **`Draining` state unused.** See §3.1.

3. **No backend `/health` probe in half-open.** See §3.2. The WS handshake is
   the probe.

4. **No graceful shutdown.** `main.rs` exits on `Ctrl-C` immediately. In-flight
   streams are aborted (their forward tasks are killed when the runtime tears
   down) but the dispatcher doesn't drain. Adding `tokio::signal::ctrl_c()` to
   the main `select!` with a quiesce path is straightforward and is a logical
   next step.

5. **Default circuit cooldown.** `config.rs` defaults to **1s**, the spec
   says **30s**. Operationally a 1s cooldown is friendlier under transient
   network blips (faster recovery) but doesn't honor the spec literal. Easy
   to change at the CLI; default should be aligned to 30s.

6. **WebSocket path is not enforced.** The server accepts upgrades on any path,
   not strictly `/v1/ws/speech`. The mock backend the multiplexer talks to
   also doesn't enforce the path. Hardening would be a one-liner in `server.rs`
   to inspect the HTTP request URI before calling `accept_async`.

## 11. Testing

- 34 unit tests in `cargo test --release` cover: protocol parsing/encoding,
  bounded queue, circuit breaker transitions, EWMA convergence, sliding window
  eviction, reconnect backoff schedule and jitter bounds.
- Integration suites are the Python harness in `assignment/test_client.py`
  (correctness, multiplex, slow-consumer, soak, kill-backend, compare, chaos).
- `perf/` contains a soak/load harness with checked-in CSVs and charts from
  several runs (`perf/runs/2026-05-09-*`, `perf/runs/2026-05-10-phase1-vs-phase2`).
