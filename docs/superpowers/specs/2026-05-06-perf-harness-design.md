# Perf Harness — Design

**Date:** 2026-05-06
**Branch:** `bottlenecks`
**Author:** Daksh (with Claude)
**Status:** Draft, awaiting review

---

## 1. Problem

The multiplexer has correctness and chaos coverage, but no first-class way to:

1. **See where CPU time goes** under realistic load (no flame graph, no profiler integration).
2. **Drive sustained, controllable load** at a target rate. Existing `assignment/test_client.py` uses closed-loop concurrency (`asyncio.Semaphore`), which hides queuing latency: a slow response immediately throttles the offered load. We can't ask "what does p99 TTFC look like at 50 rps?" because we can't *offer* 50 rps.
3. **Visualise latency tails over time.** Prometheus exposes `mux_ttfc_seconds` as a histogram, and `test_client.py` prints a single p50/p99 line per suite. Neither shows how the tail evolves during a run, what the CDF shape looks like, or how p95 reacts to a backend kill mid-run.

We've finished the connection-pool refactor (`bottlenecks` branch) and need to validate the perf claims with charts and flame graphs that are reproducible and committable.

---

## 2. Goals & non-goals

**Goals**
1. A reproducible CPU profile + flame graph workflow on macOS (and Linux), with no permanent runtime changes to the multiplexer (a Cargo profile addition for symbol-resolution debug info is fine; the existing `release` build is untouched).
2. An open-loop WebSocket stress generator that drives a target RPS regardless of backend latency, plus a closed-loop mode for saturation tests.
3. Static charts (PNG/SVG) of p50/p95/p99 TTFC and total-duration over time, latency CDF, achieved-vs-offered RPS, and success rate over time — generated from a per-request CSV produced by the stress tool.
4. All deliverables live in version control (`perf/` directory) and run via documented commands. No external services required for v1.

**Non-goals**
1. **In-process pprof endpoint** (`pprof-rs` HTTP handler). Out of scope for v1; deferred to a follow-up spec because it requires a Rust dependency and a feature flag.
2. **Live dashboards** (Grafana, Prometheus over time). The existing `/metrics` endpoint already publishes `mux_ttfc_seconds` as a histogram, so anyone who wires up Grafana can render `histogram_quantile()` panels — but committing a Grafana stack to the repo is out of scope here.
3. **Distributed load gen.** Single-machine load only. macOS loopback ephemeral exhaustion is a known constraint (see connection-pool spec); we'll work around it with `--max-inflight` rather than scaling out.
4. **Comparing against alternatives** (k6, wrk, vegeta). These don't speak the binary WS protocol the mux requires; rolling our own keeps the tool aligned with the spec.

---

## 3. Architecture

### 3.1 File layout

```
perf/
  stress.py          # WS load generator (open-loop + closed-loop)
  plot_latency.py    # CSV → PNG chart renderer
  profile.sh         # samply wrapper (start/stop/render)
  README.md          # quick-start
  runs/              # gitignored — CSVs and PNGs land here
docs/
  superpowers/specs/2026-05-06-perf-harness-design.md  # this file
```

`assignment/` stays untouched — that directory is the evaluation harness from the original assignment and we treat it as read-only.

### 3.2 Component boundaries

Three independent pieces, each runnable on its own:

| Piece | Input | Output | Depends on |
|---|---|---|---|
| **stress.py** | `--url`, `--rps` or `--concurrency`, `--duration` | `perf/runs/<name>.csv` + `<name>.summary.json` | `websockets` (already a dep), `uvloop` (optional) |
| **plot_latency.py** | `perf/runs/<name>.csv` | `perf/runs/charts/<name>/*.png` | `matplotlib`, `numpy` |
| **profile.sh** | running `tts-multiplexer` PID | `perf/runs/<name>-profile.json.gz` (samply) + optional `flamegraph.svg` (inferno) | `samply` (`cargo install samply`), optional `inferno` |

All paths in this document are relative to the repo root.

The CSV is the only contract between stress and plot. Anything that can produce the CSV schema (§3.4) can feed the plotter.

### 3.3 Stress generator design

**Open-loop is the default.** Arrivals follow a Poisson process at the target RPS, so the offered rate is independent of how fast the system responds. Each new request is dispatched on its own task; if the system can't keep up, in-flight count grows and we see queuing latency for what it is.

**Closed-loop is the fallback** for saturation/soak tests where we want to *know* we're applying constant pressure rather than constant arrival rate. Selected by passing `--concurrency` instead of `--rps`.

**Shape:**

```
                    ┌──────────────────┐
  ticker(rps) ──────►│  arrival queue  │
                    └────────┬─────────┘
                             │  spawn task per arrival
                             ▼
                  ┌──────────────────────────┐      ┌──────────────────┐
                  │ run_request(url, req_id) ├─────►│  csv_writer task │
                  └──────────────────────────┘      └──────────────────┘
                             │
                             ▼
                  reuse send_single_request()
                  from test_client.py (imported)
```

Backpressure: a `--max-inflight` cap (default 500) drops new arrivals if exceeded, recording them as `status=dropped_offered`. This prevents the load gen itself from melting when the SUT (system under test) collapses, while still recording that we tried.

**Load-gen contamination guards.** Because Phase 1 deliberately runs load gen on the same box as the SUT, three guards are mandatory in stress.py — without them we cannot tell load-gen-induced latency apart from SUT-induced latency:

1. **`uvloop` by default.** Try-import `uvloop`; install as the asyncio policy if available, else fall back to the stdlib loop. On macOS and Linux this gives ~2–3× throughput headroom on socket-heavy workloads with no API change. (Phase 2's Rust load gen sidesteps Python's per-task overhead entirely.)

2. **SUT-vs-CSV quantile cross-check.** At end of every run, scrape the multiplexer's `/metrics` endpoint, parse `mux_ttfc_seconds` histogram, compute server-side p50/p95/p99, and print alongside the same quantiles computed from the CSV. The delta is almost entirely load-gen + loopback + framing overhead — i.e., the contamination we're worried about. Save both sets of quantiles plus the per-quantile delta to `<name>.summary.json` next to the CSV. New CLI flag `--metrics-url` defaults to `http://<host_from_url>:9001/metrics`; if the scrape fails the run still completes and the cross-check is reported as skipped (with a warning).

3. **Self-CPU report.** Sample `os.times()` and `time.monotonic()` at start/end. At end of run, print `(utime + stime) / wall_time` as a fraction of one core, both for the load-gen process and (best-effort, via `psutil` if available, else skipped) for the SUT process. Emit a stderr `WARNING: load-gen CPU >25% of one core, measurements may be biased` when the load-gen value exceeds the threshold. Backup signal in case `/metrics` scraping is unavailable.

The cross-check is the load-bearing one; the self-CPU number is a sanity check. The Phase 2 trigger in §7 is defined against the cross-check delta, not the CPU number.

**CLI:**

```bash
# Open-loop, 50 rps for 60s with 10s linear ramp-up
python perf/stress.py \
  --url ws://127.0.0.1:9000/v1/ws/speech \
  --rps 50 --duration 60 --ramp-up 10 \
  --max-inflight 500 \
  --out perf/runs/2026-05-06-baseline.csv

# Closed-loop saturation test
python perf/stress.py \
  --url ws://127.0.0.1:9000/v1/ws/speech \
  --concurrency 64 --duration 60 \
  --out perf/runs/2026-05-06-saturation.csv
```

### 3.4 CSV schema

One row per attempted request. Fields:

| Field | Type | Notes |
|---|---|---|
| `ts_unix_ms` | int | Wall-clock ms when the request was *issued*. Human-readable timestamp; not used for latency math. |
| `req_id` | int | Monotonic request index |
| `status` | str | `success` \| `error` \| `dropped_offered` \| `timeout` |
| `error_kind` | str | Empty unless status=error; e.g. `connect_failed`, `server_error`, `unexpected_close` |
| `ttfc_ms` | float | `time.monotonic()` delta from issue to first chunk. Empty if no first chunk arrived. |
| `duration_ms` | float | `time.monotonic()` delta from issue to terminal frame (or error). |
| `bytes_received` | int | Audio bytes received |
| `chunk_count` | int | Number of audio frames received |

**Time-source rule.** `ts_unix_ms` uses wall-clock so rows can be aligned to external events (e.g., a backend kill recorded in another log). Both latency fields use `time.monotonic()` deltas captured at the same call site as the request issue, so NTP slews and DST transitions cannot poison the math.

Why these fields: TTFC is the user-visible latency we care most about; `duration_ms` lets us cross-check overall throughput; `status`/`error_kind` lets the chart split success vs error lines without re-running.

### 3.5 Chart renderer

`plot_latency.py perf/runs/X.csv --out-dir perf/runs/charts/X` produces four PNGs:

1. **`latency_over_time.png`** — TTFC and total-duration, each as three lines (p50/p95/p99) computed in 5s windows. Y-axis log scale (latency tails span orders of magnitude). X-axis is seconds since first request. Annotations for any window where success rate < 95%.
2. **`latency_cdf.png`** — CDF of TTFC for successful requests, log-x. Marker lines at p50/p95/p99/p99.9.
3. **`throughput.png`** — Achieved RPS (success) and offered RPS in 1s windows, stacked with error/dropped/timeout RPS.
4. **`success_rate.png`** — Rolling success rate (1s windows). Horizontal reference line at 95%.

Window size and quantiles are CLI-tweakable (`--window-ms`, `--quantiles 50,95,99,99.9`); defaults are 5000 ms for `latency_over_time` and 1000 ms for `throughput`/`success_rate`. The latency chart needs a wider window because high quantiles from small samples are statistically noisy: at 50 rps × 1 s = 50 samples, p99 is essentially "max of 50" and the line shakes. 5 s × 50 rps = 250 samples gives a clean line; for very low offered rates (<20 rps) consider passing `--window-ms 10000`. The throughput and success-rate charts stay at 1 s because they are counts, not quantiles, and their precision is unaffected by sample size.

Reading order if you've never run this before: open `success_rate.png` first (did the run hold up?), then `throughput.png` (did we actually offer what we asked for?), then `latency_over_time.png` and `latency_cdf.png`.

### 3.6 CPU profiling + flame graph

**Tool: `samply`.** Sampling profiler, no code change, works on macOS without disabling SIP, produces a Firefox-profiler format that includes an interactive flame graph and call tree. Install via `cargo install samply`.

**Why not `cargo flamegraph`:** it relies on DTrace on macOS, which is unreliable post-SIP and produces a static SVG only.

**Why not `pprof-rs`:** requires adding a dep and an HTTP endpoint. Useful for production-style triggering, but overkill for a local perf-validation pass. Deferred.

**Cargo profile.** Add a new `[profile.release-with-debug]` to `Cargo.toml` that inherits from `release` and sets `debug = "line-tables-only"`. The `release` profile is unchanged; the new profile produces a binary with line-table-only debug info (~few MB extra) that lets samply resolve symbols cleanly without the cost of full debug builds. Profile builds are invoked as `cargo build --profile release-with-debug` and produce `target/release-with-debug/tts-multiplexer`. This is the canonical profile for the perf harness; all examples below assume it.

**Workflow** (`perf/profile.sh`):

```bash
# 1. Build with line-table debug info for samply symbol resolution
cargo build --profile release-with-debug

# 2. Start the multiplexer (terminal A)
./target/release-with-debug/tts-multiplexer --port 9000 --metrics-port 9001 \
  --backends 127.0.0.1:9100,...,127.0.0.1:9107

# 3. Start the mock backends (terminal B)
python assignment/mock_backend.py --workers 8 --base-port 9100 --calm

# 4. Attach samply for the profile window (terminal C)
PID=$(pgrep -f target/release-with-debug/tts-multiplexer)
samply record --pid "$PID" --duration 60 -o perf/runs/profile.json.gz --no-open

# 5. Drive load while samply records (terminal D)
python perf/stress.py --url ws://127.0.0.1:9000/v1/ws/speech \
  --rps 50 --duration 60 --out perf/runs/baseline.csv

# 6. View flame graph
samply load perf/runs/profile.json.gz   # opens Firefox profiler
```

`perf/profile.sh` wraps steps 4 and 6 so you typically run:

```bash
./perf/profile.sh record 60 baseline   # records 60s into perf/runs/baseline-profile.json.gz
./perf/profile.sh view baseline        # opens the recorded profile
```

### 3.7 What we deliberately do *not* change in the multiplexer

- No new dependency in `Cargo.toml`. We do add a `[profile.release-with-debug]` profile section — metadata only, no runtime impact, and the existing `release` profile is unchanged. (Possible future: `pprof-rs` behind a `profiling` feature, separate spec.)
- No new HTTP endpoints. The existing `/metrics` already publishes `mux_ttfc_seconds`; `stress.py` scrapes it for the cross-check (§3.3) using the existing surface.
- No new metric. The CSV from `stress.py` is the source of truth for chart rendering — keeps load-gen-observed latency and SUT-observed latency clearly separated, which is exactly the property the cross-check relies on.

---

## 4. Data flow

```
                 ┌──────────────────┐
                 │  perf/stress.py  │
                 │   (open-loop)    │
                 └────────┬─────────┘
                          │ WS frames over loopback
                          ▼
                 ┌──────────────────────┐ samply --pid
                 │   tts-multiplexer    │◄──────────────  perf/profile.sh
                 │ (release-with-debug) │                       │
                 └────────┬─────────┘                            │
                          │ WS frames                            ▼
                          ▼                          perf/runs/<name>.json.gz
                 ┌──────────────────┐                         │
                 │ mock_backend.py  │                         ▼
                 └──────────────────┘                  Firefox profiler
                          │                            (flame graph)
                          │
   per-request ts/ttfc/   │
   duration/status        │
                          ▼
                 perf/runs/<name>.csv
                          │
                          ▼
                 ┌──────────────────┐
                 │ plot_latency.py  │
                 └────────┬─────────┘
                          ▼
                 perf/runs/charts/<name>/*.png
```

---

## 5. Error handling

**stress.py:**
- Connect failure → row with `status=error`, `error_kind=connect_failed`, `ttfc_ms` empty.
- WS close before `Done` frame → `status=error`, `error_kind=unexpected_close`.
- Per-request timeout (configurable, default 30s) → `status=timeout`.
- Load gen exceeds `--max-inflight` → row written immediately with `status=dropped_offered`, no WS attempt.
- Fatal load-gen errors (Python exception in the runner itself) print to stderr and exit non-zero — never silently corrupt the CSV.
- CSV durability: writes are line-buffered (`open(..., buffering=1)`); a SIGINT/SIGTERM handler flushes and closes the file before re-raising. `fsync` per row is *not* used — at the rates we target (up to several hundred rps) it would itself become a bottleneck and contaminate latency measurements. Line buffering plus the signal handler is sufficient to survive Ctrl-C; the only failure mode it doesn't cover is a kernel panic mid-run, which is acceptable for a perf-validation tool.

**plot_latency.py:**
- Missing/empty CSV → exit non-zero with a clear message.
- Any window (5 s for latency, 1 s for throughput/success-rate) with zero successful requests → percentile lines have a gap (NaN), not a zero. Crashing because of an empty window is wrong; pretending p50 is 0 is misleading.

**profile.sh:**
- Multiplexer not running → exit non-zero with hint to start it.
- samply not on PATH → print install command (`cargo install samply`).

---

## 6. Testing

This is tooling, not product code. Test surface is small:

1. **stress.py smoke test**: a 5-second 10 rps run against the mock backend at calm mode. Asserts CSV row count is between 40 and 60, success rate >95%, and that `<name>.summary.json` exists with both CSV-derived and SUT-derived quantiles populated. Skips the SUT-quantile assertion if `/metrics` is unreachable (logging a clear skip message rather than failing — keeps the test useful when run without a live multiplexer).
2. **plot_latency.py smoke test**: feed it a synthetic CSV (10 known rows), assert all four PNGs are produced and non-empty.
3. **profile.sh smoke test**: not automated — manual verification once.

Tests live in `perf/test_perf.py`. Not part of the existing `cargo test` or `assignment/test_client.py --all` runs; invoked as `python -m pytest perf/`. Goal is to catch tool bitrot, not to be a CI gate.

---

## 7. Phasing

**Phase 1 (this spec): comparison-grade Python load gen.** `stress.py` (with uvloop default + SUT-vs-CSV cross-check + self-CPU report), `plot_latency.py`, `profile.sh`, README, smoke tests. The only multiplexer-side change is the new `[profile.release-with-debug]` Cargo profile (no runtime impact).

Sufficient for: A/B comparisons (e.g., before/after the connection-pool refactor), CPU profiling, latency-tail visualisation. Quantitative claims from this phase are *relative*, not absolute — both builds get measured the same way, so load-gen overhead cancels in the comparison. This is enough for the work that motivates the `bottlenecks` branch.

**Phase 2 (separate spec, deferred): Rust load gen for absolute-ceiling measurements.** Tokio + tungstenite, ~300 lines, same CSV + summary-JSON schema as Phase 1's `stress.py` so `plot_latency.py` is unchanged. Replaces (does not augment) the Python tool for ceiling work.

**Phase 2 trigger.** If at any tested load the SUT-side `mux_ttfc_seconds` p99 (from `/metrics`) and the CSV-derived p99 from `stress.py` diverge by **more than 10 ms or 20% of SUT p99 (whichever is larger)**, Python has hit its ceiling and absolute-ceiling claims are no longer trustworthy from Phase 1. The `<name>.summary.json` sidecar from Phase 1's cross-check makes this trigger observable rather than guessed; we promise to act on it rather than rationalise past it.

**Future work, not phased** (out of scope for both phases above; each deserves its own design pass):
- `pprof-rs` HTTP endpoint behind a `profiling` cargo feature, for in-process trigger during long-running prod-shaped runs without restart.
- Checked-in Grafana dashboard JSON for `histogram_quantile()` views off `/metrics`.

Phase 1 alone is enough to answer the questions that motivated this work (where does CPU go, what does p99 look like at our target rates, what shape is the CDF, did the connection-pool refactor improve the tail). Phase 2 only matters once we want a defensible *absolute* number for the box's ceiling.

---

## 8. Open questions

None blocking. Calibration choices to make once we run it:

- **Default RPS for the canonical "baseline" run.** Probably 50 rps × 60s for a quick check, 200 rps × 300s for a thorough one. Settled empirically once we see what the box can do.
- **Whether to include `assignment/mock_backend.py --calm` vs default chaos in the canonical baseline run.** Calm for clean CPU profile (no chaos noise), chaos for end-to-end realism. Both should be documented in `perf/README.md` as named recipes.

These are fine-tuning, not architecture.
