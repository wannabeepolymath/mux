# Forward — Detach `drain_close` from `StreamCompleted`

**Date:** 2026-05-04
**Status:** Approved (brainstorming → impl)
**Branch:** bottlenecks
**Companion to:** circuit-breaker, scoring-decoupling

## Problem

`forward::do_forward` ran `drain_close(ws, drain_timeout=100ms)` synchronously
on every terminal outcome before returning. Only after the drain finished did
the spawned task return its `ForwardResult` to the dispatcher; only then did
the dispatcher transition the backend to `Disconnected` and spawn a fresh
reconnect.

Net effect: every successful request added ≤100ms of "backend dark time" before
the warm slot could even start re-establishing. With 8 backends, this directly
caps per-backend throughput.

The drain has no semantic role in the request flow. It's purely TCP hygiene —
read peer's FIN before our drop sends ours, so we end as the passive closer.
That hygiene does not need to gate the dispatcher.

## Fix

Move all WS cleanup (drain + optional active-close) out of `do_forward` and into
a detached task spawned by `run_forwarding`. `do_forward` returns the outcome
immediately; `run_forwarding` decides cleanup style from the outcome and spawns
a tokio task that owns the `ws`, performs the cleanup, and drops it.

New `CleanupKind` enum encodes the three styles the previous code had inline:
- `Drain` — read to EOF or drain_timeout. Default for terminal success/most
  failures.
- `ActiveClose` — send a Close frame (capped 50ms), then drain. Used on
  `Cancelled` and `ClientGone` (peer doesn't support cancel; politeness).
- `Drop` — skip drain entirely. Used on `BackendHung`: server is sleeping and
  won't FIN, so a drain task would just sit idle for the whole timeout.

## Why this is safe

- **TCP hygiene preserved.** The detached task still drains; we still end as
  the passive closer in the typical case.
- **Concurrency cost is negligible.** ~3 in-flight drain tasks per second
  worst case (8 backends × ~3 req/s × 100ms). Tokio task overhead is
  ~microseconds; memory is bounded.
- **No outcome semantics moved.** Cancellation, hang detection, malformed
  parsing — all still happen on the request path before the task spawns.
- **`ForwardResult` interface unchanged.** Callers (dispatch::handle_stream_completed)
  see the outcome at the same point in the lifecycle, just sooner in
  wall-clock.

## Verification (chaos run, 200 req × 16 concurrency)

Measured with the test bench's per-request timeout temporarily bumped to 90s
to isolate true mux behavior from test-cap artifacts:

|              | Pre-#4 | Post-#4 |
|--------------|--------|---------|
| Mux errors   | 6      | **3**   |
| True mux success rate | 96% | **98%** |
| Test reports | 72%    | 74%     |

The throughput plateau under chaos (0.5 req/s in both runs) is bounded by the
chaos environment itself (slow chunks 9-15s, retries, queue waiting). Calm-mode
chaos suite hits 100% success at 3.5 req/s — confirming the multiplexer's
dispatch path is healthy.

## Out of scope

- Per-stream backpressure (#7).
- Single-dispatcher serialization (#5).
- Allocation-churn cleanup (#8).
