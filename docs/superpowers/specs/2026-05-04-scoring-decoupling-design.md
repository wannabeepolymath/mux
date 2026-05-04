# Scoring — Decouple TTFC EWMA from Failure Penalty

**Date:** 2026-05-04
**Status:** Approved (brainstorming → impl)
**Branch:** bottlenecks
**Companion to:** 2026-05-04-circuit-breaker-design.md

## Problem

`dispatch::handle_stream_completed` failure branch was feeding a synthetic
5000ms (= `hang_timeout`) observation into `BackendScoring::record_ttfc()` for
every failure. With α=0.3, after ~3 failures the EWMA converges to ~5000ms and
stays pinned there.

Observed (yesterday's chaos snapshot): all 8 backends had `ttfc_ewma_ms`
between 4296–4997ms. `find_best_backend_filtered` sorts by TTFC, so with all
scores ~equal the dispatcher degenerated to round-robin via `next_backend_idx`
tiebreaking. Adaptive routing didn't actually adapt.

Spec language (Part 3): *"dispatch to the one with the lowest TTFC EWMA among
those with error rate below 30%."* — TTFC and error_rate are kept separate;
the spec does not call for a synthetic TTFC penalty on failures.

## Fix

Remove the `record_ttfc(penalty_ms)` call on the failure branch in
`dispatch/mod.rs`. Keep `record_result(false)` — that's the right channel for
risk (drives the error_rate sliding window used by the 30% filter).

Two-line change (plus a comment explaining why).

## Why this is safe

- **Spec compliance.** The two metrics are explicitly separate in spec Part 3.
  TTFC is for ranking; error_rate is for filtering.
- **No-data handling.** `find_best_backend_filtered` already treats
  `ttfc_ewma_ms == 0.0` as "no data, prefer real data." A backend that's never
  succeeded simply has no TTFC observation; the existing logic still picks it
  via round-robin order when other candidates also lack data.
- **Stale EWMA.** A backend that succeeded once long ago and has only failed
  since will hold a stale TTFC. That's a known trade-off of EWMAs without
  decay; outside the scope of this fix. The error_rate filter keeps repeatedly
  failing backends out of rotation regardless.

## Test

New unit test in `scoring.rs`:
`record_result_does_not_touch_ttfc_ewma` — explicitly asserts that calling
`record_result(true)` or `record_result(false)` never changes `ttfc_ewma_ms`,
guarding against re-introducing the pollution.

## Verification (chaos run, 200 req × 16 concurrency)

- TTFC EWMAs are now realistic (377–687ms range), no longer pinned at 5000ms.
- Adaptive routing operates on real data.
- Circuit-open events further reduced (22 vs 31 in the previous run; both far
  below the 88 pre-circuit-fix baseline).
- Chaos success rate: in the 51–68.5% band across runs — within noise. The
  property fix is correct; the throughput plateau confirms the binding
  constraint has moved downstream (bottleneck #4: drain blocking; bottleneck
  #3: 5s hang_timeout; test-side 30s per-request timeout).

## Out of scope

- TTFC EWMA decay/aging.
- Tuning α (spec mandates 0.3).
- Tolerance band on TTFC ranking (would be load-balancing; not what this fix
  is about).
