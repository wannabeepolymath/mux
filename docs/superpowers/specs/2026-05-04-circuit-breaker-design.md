# Circuit Breaker — Exponential Cooldown with Connect-as-Probe

**Date:** 2026-05-04
**Status:** Approved (brainstorming → impl)
**Branch:** bottlenecks

## Problem

Today's circuit breaker (`src/backend/circuit.rs`) opens after 3 consecutive failures, waits a fixed cooldown (default `1s`, spec asks `30s`), then admits the next real client request as the half-open probe. Two failure modes observed in the chaos run (200 req × 16 concurrency, default chaos rates):

1. **Cooldown too short.** At 1s, the probe lands inside the same chaos window that opened the circuit. With per-request chaos rate ≈32%, the probe fails ~1/3 of the time, immediately reopening the circuit. Logged: 88 `circuit opened` events in a 4-minute test — the breaker oscillated rather than protected.
2. **No backoff on repeat opens.** A flaky backend gets the same 1s cooldown every cycle. Backends spent the bulk of the test in cool-down/probe oscillation rather than serving traffic.

Spec language (assignment/README.md, Part 1 ¶2): *"remove it from rotation for a configurable cooldown period (default 30s). Probe it with a health check before re-admitting."* Today we satisfy neither the 30s default nor the "health check probe" notion.

## Approach

**Connect-as-probe + exponential cooldown.** The connect handshake itself is the health check (rules out backend down + REFUSE-chaos). After connect succeeds, the next client request is admitted as the second-stage probe (proves the backend can actually serve). Cooldown grows on every consecutive open and resets on any successful close.

Approaches considered and rejected:
- **A: just bump cooldown to 30s.** Trades fast recovery from transient blips for stability. Doesn't satisfy "health check probe" spec language. First probe is still a real client request that visibly fails.
- **B: exponential cooldown only, no probe protocol.** Captures most of the throughput win but still uses a real client request as probe.
- **C (chosen): connect-as-probe + exponential cooldown.** Spec-compliant, composes with existing `connect_task` lifecycle, no new wire protocol.

## Design

### State machine

`CircuitState` stays `Closed | Open | HalfOpen`. Add `consecutive_opens: u32` to drive cooldown selection (separate from `consecutive_failures` which drives the open transition).

```rust
pub struct CircuitBreaker {
    state: CircuitState,
    consecutive_failures: u32,         // unchanged
    consecutive_opens: u32,             // NEW
    threshold: u32,
    cooldown_schedule: &'static [Duration],  // NEW
    opened_at: Option<Instant>,
}
```

Transitions (deltas vs today in **bold**):
- `Closed`, `record_failure`, counter ≥ threshold → **`consecutive_opens += 1`**, `opened_at = now`, → `Open`.
- `Open`, `can_attempt()` after cooldown elapsed → `HalfOpen`, returns true.
- `HalfOpen`, `record_failure` → **`consecutive_opens += 1`**, `opened_at = now` (with new cooldown index), → `Open`.
- Any state, `record_success` → `consecutive_failures = 0`, **`consecutive_opens = 0`**, → `Closed`.

Cooldown lookup: `cooldown_schedule[min(consecutive_opens.saturating_sub(1), len-1)]`.

### Cooldown schedule

```
[1×, 2.5×, 7.5×, 15×, 30×] of base
```

With base = 2s default, that's `[2s, 5s, 15s, 30s, 60s]`. Index by `consecutive_opens - 1`, saturating at the cap.

Rationale: first open is often a transient chaos roll → 2s is responsive. Repeat opens escalate to the spec's 30s default by the third, then to a 60s "this backend is genuinely in trouble" cap. Reset to base on first successful probe-close.

### Connect-as-probe wiring

`dispatch::handle_connection_failed`: if `circuit.state == Open`, treat as failed probe — call `circuit.record_open_failure()` (new helper that bumps `consecutive_opens` and resets `opened_at`), then schedule the next cooldown timer. Otherwise (circuit Closed), keep current behavior (state = Disconnected, retry with reconnect_attempt backoff).

`dispatch::handle_connection_established`: no change. The transition Open → HalfOpen continues to happen lazily inside `circuit.can_attempt()` at the next `try_dispatch` call.

REFUSE chaos: TCP+WS handshake completes (mock_backend accepts then sends an error frame), so connect "succeeds." The first probe request hits `forward::check_warm_slot_liveness`, gets `BackendCrashed/BackendError`, that becomes a failed probe via `handle_stream_completed`'s failure path → circuit reopens with a longer cooldown. No special-case code needed.

### Probe gating (no new dispatcher state)

The existing warm-slot model already serializes one-probe-per-backend: HalfOpen backend has one warm slot → `take_for_dispatch` consumes it → backend is `Busy` → no parallel probe can be dispatched there. We rely on this and add no new flag.

### Config surface

Keep `--circuit-cooldown-secs` as today; reinterpret as the **base** value (multiplier index 0). Default changes from `1` to `2`. Schedule multipliers are hard-coded in `circuit.rs`. Tunable enough for now; can graduate to a comma-separated schedule later if needed.

## Out of scope

- **TTFC scoring penalty (bottleneck #2).** EWMA pollution by the 5000ms hang penalty makes adaptive routing pick poorly even with a perfect breaker. Separate fix; composes with this one.
- **Drain-blocking (bottleneck #4)**, **hang_timeout reduction (#3)**, **per-stream backpressure (#7)**: separate fixes.

## Tests

Unit tests in `circuit.rs`:
- First open uses base cooldown (index 0).
- Second open (HalfOpen → Open after probe failure) uses index-1 cooldown.
- 5+ opens cap at the last schedule entry.
- `record_success` from HalfOpen resets `consecutive_opens` to 0; next open returns to base cooldown.

Integration acceptance:
- `cargo test` passes.
- Re-run `--chaos --requests 200 --concurrency 16`. Bar: `circuit opened` event count in mux.log drops materially (target: <30 vs the prior 88), and chaos success rate climbs (final number depends also on bottleneck #2, so don't expect 95%+ from this fix alone).
