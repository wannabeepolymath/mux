# Connection-Pool Refactor — Empirical Verification

**Date:** 2026-05-02
**Branch:** `eph`
**Final commit:** `4a05a61` (Task 9), with the verification doc itself committed after.
**Spec:** `docs/superpowers/specs/2026-05-02-connection-pool-and-passive-close-design.md`
**Plan:** `docs/superpowers/plans/2026-05-02-connection-pool-and-passive-close.md`

## Setup

- macOS 25.2.0; ephemeral range `49152–65535` (~16k); `net.inet.tcp.msl = 15000` (TIME_WAIT ≈ 30s).
- 8 mock backends on `127.0.0.1:9100..9107` with default chaos rates (~36% failure: 8% crash, 5% hang, 3% malformed, 15% slow, 5% refuse).
- Multiplexer (release build) with default config (max_retries=3, hang_timeout=5s, drain_timeout=100ms, connect_timeout=3s, circuit_threshold=3, circuit_cooldown=1s).

## Results: EADDRNOTAVAIL fix

| Metric | Before (`049a549`) | After (`4a05a61`) | Target | Verdict |
|---|---|---|---|---|
| `connect failed: ... os error 49` log lines (500-req chaos) | **56** | **0** | 0 | ✅ |
| Peak mux-side TIME_WAIT (foreign port = backend) | **16,267** | **6** | <200 | ✅ |
| Peak backend-side TIME_WAIT (local port = backend) | 0 | 48,306 | n/a | (passive-close shifted cost off the multiplexer) |
| Active TCP closer | multiplexer (~100% of cases) | backend (~100% of cases) | passive-close | ✅ |

The ephemeral-port exhaustion bug is **eliminated**. The multiplexer is now the passive TCP closer in essentially all cases, exactly as the spec §4.2 / §4.3 specified.

## Results: spec compliance

- `/health` shows backends transitioning through `connecting → ready → busy → disconnected → connecting` over the run. ✅
- After a request completes, the backend's state returns to `ready` within ~10ms on loopback (warm-pool eager-reconnect working). ✅
- 5-state machine implemented per spec §3.2 (`Draining` is currently a documented `#[allow(dead_code)]` — see spec footnote and `state.rs` comment). ✅ (with documented gap)

## Results: regression suites (against calm backends)

| Suite | Result |
|---|---|
| Correctness (6 tests) | **PASS 6/6** |
| Multiplexing (5 tests) | **PASS 5/5** |

## Results: chaos throughput target

| Test config | Success rate | Throughput |
|---|---|---|
| 200 reqs, 16 concurrency (README spec, fresh process) | **83.5%** | 1.3 req/s |
| 500 reqs, 32 concurrency (stress) | 27.6% — bottlenecked by 30s client timeout |

The 95% chaos-survival target (assignment §"Performance Requirements") is **not met** at 83.5%. The 11× improvement over baseline (7.6% → 83.5%) confirms the EADDRNOTAVAIL fix was the dominant problem, but a residual throughput limit prevents hitting 95%.

### Why we don't hit 95%

The remaining gap is **throughput, not correctness**. Per-backend cycle under chaos:
- Normal request: ~3s (0.3s first-chunk + 11 × 0.25s)
- Hang chaos (5%): 5s blocked on `hang_timeout`
- Slow chaos (15%): 9s on inflated chunk intervals
- Plus: ~1ms reconnect, ~0.1ms liveness check, ~10ms drain (when applicable)

Aggregate: 8 backends × ~3.6s avg cycle = ~2.2 req/s ceiling.

With concurrency 16, the queue holds the excess; under the test client's 30s timeout per request, the tail of the queue times out before getting a Ready slot. At higher concurrency (32) the timeout eats more of the request stream — that's why 500/32 collapses harder than 200/16.

### What would close the gap (out of scope for this design)

1. **Per-backend connection pool size > 1.** The README explicitly mandates "one request at a time per backend" and our design honors that. Relaxing this would require backend-protocol changes.
2. **Increase `max_streams_per_conn` and reuse client connections more aggressively** — but the test client opens fresh connections per request, so this doesn't help here.
3. **More aggressive failover:** when one backend's circuit opens, route around it without waiting for cooldown. We already do this (`spawn_reconnect_after_failure`); the cooldown timer only delays the disconnected backend's *return*, not other backends' availability.
4. **Client-side retry tolerance:** the 30s timeout is in the test client (`assignment/test_client.py:139`), not the multiplexer. The multiplexer keeps retrying past the client deadline — those completions count as multiplexer successes but client failures.

## Conclusion

The connection-pool refactor delivers exactly what the spec promised: it eliminates EADDRNOTAVAIL, implements the assignment's required state machine, and shifts TCP-closer responsibility off the multiplexer. Correctness and multiplexing suites pass cleanly. Chaos survival improves by 11× but doesn't reach the assignment's 95% target due to a separate throughput limit not addressed by this work.
