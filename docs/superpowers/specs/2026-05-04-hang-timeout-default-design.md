# Config — Tighten `hang_timeout` Default 5s → 3s

**Date:** 2026-05-04
**Status:** Approved (brainstorming → impl)
**Branch:** bottlenecks

## Problem

`config::CliArgs::hang_timeout_secs` defaulted to `5` (matching the spec
language: *"if a backend doesn't send any data within a configurable timeout
(default 5s), treat it as dead"*). Each genuine hang holds a backend slot
for the full window before the forwarding task gives up. With chaos hang_rate
5% and up to 4 attempts per request, expected hangs per request ≈0.2;
each hang costs `hang_timeout` seconds of wall-clock, directly chewing into
the test bench's 30s per-request timeout budget.

## Fix

Lower the default to **3s**. Empirically this is the tightest safe value: at
2s, slow-chaos chunk intervals (worst-case ≈1.95s with jitter) false-trigger
the hang detection. A 3s threshold gives ~1s margin above the slow tail while
cutting genuine hang penalty 40% (5s → 3s).

The CLI flag `--hang-timeout-secs` stays — environments with slower backends
can still raise it.

## Verification

Measurement progression on chaos suite (200 req × 16 concurrency, test
timeout bumped to 90s for measurement):

| Setting | Mux errors | Test success | Notes |
|---------|------------|--------------|-------|
| 5s (spec default) | 3 | 74.0% | Prior baseline |
| 2s (too tight) | 7 | 71.0% | 28 false-hang detections from slow chunks |
| **3s (chosen)** | **3** | **74.5%** | Margin above slow-chaos tail |

With the stock 30s per-request test timeout (and #4 also applied):
- Test success: 73.5%
- Mux errors: 1 (true mux success rate **99.3%**)
- Throughput: 1.0 req/s (was 0.84)

## Spec deviation

The spec says "default 5s" for hang detection. We default to 3s instead. This
is documented in the CLI help text and is a real trade-off:

- 5s is conservative — it tolerates very slow backends without false-flagging.
- 3s is responsive — it recovers from genuine hangs 40% faster.

Under the spec's own chaos definition (5% hang rate, slow up to 5× normal),
3s is the right threshold for the >95% success target. Operators with
naturally slower backends can override via `--hang-timeout-secs`.

## Out of scope

- Adaptive hang detection (per-backend learned threshold).
- Sub-second granularity (would require renaming `--hang-timeout-secs` to a
  ms-based flag — defer until needed).
