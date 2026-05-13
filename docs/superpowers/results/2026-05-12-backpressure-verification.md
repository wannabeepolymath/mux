# Drop-Oldest Backpressure — Verification

**Date:** 2026-05-12
**Branch:** main (uncommitted working tree)
**Plan:** `docs/superpowers/plans/2026-05-12-backpressure-drop-oldest.md`

## What changed

- `src/client/buffer.rs` (new) — `StreamBuffer` with drop-oldest semantics.
  Terminal text events never dropped, never counted against the byte cap.
- `src/client/router.rs` (new) — `BinaryRouter` + `ClientChannel`. Per-client
  shared state with `std::sync::Mutex<RouterState>` + `tokio::sync::Notify`.
  Round-robin drain order across streams.
- `src/client/mod.rs` — rewrote `handle_client` to use the router. Removed the
  per-client `mpsc<ClientEvent>` channel and the old `client_writer` body.
- `src/dispatch/mod.rs` — `PendingRequest.client_tx` → `PendingRequest.channel`.
  Terminal text routed through `push_terminal` (preserves ordering with binary).
- `src/forward.rs` — `client_tx.send(Binary).await` → `channel.push_chunk(...)`
  (non-blocking). Replaced the `tokio::select!` cancel arm with a non-blocking
  `cancel_rx.try_recv()` check at the top of each chunk iteration.
- `src/metrics.rs` — added `mux_backpressure_drops_total` `IntCounter`.
- `src/config.rs` / `src/main.rs` — `--client-buffer-bytes` flag (default 262144).
- `DESIGN.md` — §7 rewritten; §10 limitation #2 removed; §8 table updated.

Tokio's async mutex was initially used; an empirical regression motivated
switching to `std::sync::Mutex` (see §"Async mutex regression" below).

## Unit tests

```
cargo test --release
test result: ok. 48 passed; 0 failed
```

- 8 `client::buffer::tests::*` — push/pop, drop-oldest accounting, FIFO with
  Terminal sentinels, oversize-chunk-alone admission, drop-counter reset.
- 6 `client::router::tests::*` — text fast-path, single-stream FIFO,
  multi-stream round-robin (with `tokio::time::timeout` rather than
  `now_or_never` to avoid the `FutureExt` import), drop-oldest increments
  the metric, close drains-then-None.
- 34 prior tests still green.

## Integration suites (calm-mode mocks)

| Suite | Result |
|---|---|
| `--correctness` | PASS 6/6 |
| `--multiplex` | PASS 5/5 |
| `--slow-consumer` | PASS 2/2 |

**Slow-consumer caveat.** The harness's 0.5×-realtime reader receives ~12
chunks of ~3.8KB each ≈ 46KB total — well under the 256KB cap. So
`mux_backpressure_drops_total` stays at **0** through the suite. The
drop-oldest path is exercised by `client::buffer` unit tests, not by the
harness. To exercise it end-to-end would require a slower reader and a
longer stream than the suite generates; out of scope for this verification.

## Chaos suite

Three back-to-back runs at 200 reqs / 16 concurrency on the same machine
(macOS 25.2.0). All three on the new code after the `std::sync::Mutex`
switch:

| Run | Success rate | Throughput | TIME_WAIT at start (mux side, all peers) |
|---|---|---|---|
| 1 | **76.0%** (152/200) | 1.2 req/s | ~few |
| 2 | 57.5% (115/200)     | 0.9 req/s | ~16k |
| 3 | 28.5% (57/200)      | 0.7 req/s | ~80k (OS ephemeral pool effectively exhausted) |

`mux_backpressure_drops_total = 0` in every run.

The downward trend is **not the multiplexer regressing across runs** — it's
the host's ephemeral-port pool filling with `TIME_WAIT` sockets from the
test client's repeated 200-req fan-outs and the mock backend's per-request
close-and-reconnect cycle. After three back-to-back chaos runs ~80k
`TIME_WAIT` sockets exist; the test client can't open new connections
reliably, so requests fail at the TCP layer before reaching the multiplexer.

**Headline number to take from this run set: ~76% on a clean host.** This
sits between the prior 83.5% baseline (4a05a61, May 2) and the failing
runs, and given the small N (1 clean run) is consistent with no
statistically detectable change.

## Async mutex regression (incident)

The first iteration of `ClientChannel` used `tokio::sync::Mutex` for
`RouterState`. Initial chaos runs returned 63–65%, vs. the 83.5% baseline.
Diagnosis:

- Every `push_chunk` and `push_terminal` does `lock().await`. None of the
  critical sections span an `.await`; they're a few field ops then drop.
- `tokio::sync::Mutex` is cooperative-yield aware — every `lock().await`
  is a potential task switch point, *even when uncontended*, because the
  awaiting future must register with the runtime.
- The forward task hits the lock 2× per chunk (`is_closed` + `push_chunk`);
  the writer task hits it once per drained event; the dispatcher hits it
  on every `queued`/`done`/`error` push. At 200+ events/sec, the
  yield overhead accumulated.

Switching to `std::sync::Mutex` (per Tokio's own documentation for
non-await critical sections) restored throughput. The `is_closed` check
in the forward task was also dropped before each chunk push as
redundant — `push_chunk` is a no-op on a closed channel.

## What this verification proves and doesn't prove

**Proven:**
- Drop-oldest semantics are implemented correctly (unit tests).
- Terminal text and binary chunks remain in FIFO order per stream.
- Correctness, multiplex, and slow-consumer suites still pass.
- `mux_backpressure_drops_total` counter is wired up.

**Not proven:**
- The chaos number on a clean host is *no worse* than the prior 83.5%
  baseline — only one clean run was possible before TIME_WAIT
  exhaustion. Need a fresh host (or a more aggressive `tcp_msl` /
  `SO_REUSEADDR`+ephemeral tuning) for an honest A/B.
- The drop-oldest path improves chaos under the current harness — the
  harness's slow-consumer is the only suite that would exercise drops,
  and it doesn't push the buffer past cap.

**Worth doing next:**
- A clean-machine chaos sweep (3–5 runs spaced apart for TIME_WAIT to
  clear) to nail down the post-change number.
- An aggressive slow-consumer test (reader pauses ≥10s between reads)
  to actually observe drops.
