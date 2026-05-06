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
1. A reproducible CPU profile + flame graph workflow on macOS (and Linux), with no permanent changes to the multiplexer binary.
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
| **stress.py** | `--url`, `--rps` or `--concurrency`, `--duration` | `perf/runs/<name>.csv` | `websockets` (already a dep) |
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
| `ts_unix_ms` | int | Wall-clock ms when the request was *issued* |
| `req_id` | int | Monotonic request index |
| `status` | str | `success` \| `error` \| `dropped_offered` \| `timeout` |
| `error_kind` | str | Empty unless status=error; e.g. `connect_failed`, `server_error`, `unexpected_close` |
| `ttfc_ms` | float | Empty if no first chunk arrived |
| `duration_ms` | float | Wall-clock from issue to terminal frame (or error) |
| `bytes_received` | int | Audio bytes received |
| `chunk_count` | int | Number of audio frames received |

Why these fields: TTFC is the user-visible latency we care most about; `duration_ms` lets us cross-check overall throughput; `status`/`error_kind` lets the chart split success vs error lines without re-running.

### 3.5 Chart renderer

`plot_latency.py perf/runs/X.csv --out-dir perf/runs/charts/X` produces four PNGs:

1. **`latency_over_time.png`** — TTFC and total-duration, each as three lines (p50/p95/p99) computed in 1s windows. Y-axis log scale (latency tails span orders of magnitude). X-axis is seconds since first request. Annotations for any window where success rate < 95%.
2. **`latency_cdf.png`** — CDF of TTFC for successful requests, log-x. Marker lines at p50/p95/p99/p99.9.
3. **`throughput.png`** — Achieved RPS (success) and offered RPS in 1s windows, stacked with error/dropped/timeout RPS.
4. **`success_rate.png`** — Rolling success rate (1s windows). Horizontal reference line at 95%.

Window size and quantiles are CLI-tweakable (`--window-ms`, `--quantiles 50,95,99,99.9`), but the defaults above are the canonical view checked in to the README.

Reading order if you've never run this before: open `success_rate.png` first (did the run hold up?), then `throughput.png` (did we actually offer what we asked for?), then `latency_over_time.png` and `latency_cdf.png`.

### 3.6 CPU profiling + flame graph

**Tool: `samply`.** Sampling profiler, no code change, works on macOS without disabling SIP, produces a Firefox-profiler format that includes an interactive flame graph and call tree. Install via `cargo install samply`.

**Why not `cargo flamegraph`:** it relies on DTrace on macOS, which is unreliable post-SIP and produces a static SVG only.

**Why not `pprof-rs`:** requires adding a dep and an HTTP endpoint. Useful for production-style triggering, but overkill for a local perf-validation pass. Deferred.

**Workflow** (`perf/profile.sh`):

```bash
# 1. Build with debug info kept (release + debuginfo for symbol resolution)
RUSTFLAGS="-C debuginfo=2" cargo build --release

# 2. Start the multiplexer (terminal A)
./target/release/tts-multiplexer --port 9000 --metrics-port 9001 \
  --backends 127.0.0.1:9100,...,127.0.0.1:9107

# 3. Start the mock backends (terminal B)
python assignment/mock_backend.py --workers 8 --base-port 9100 --calm

# 4. Attach samply for the profile window (terminal C)
PID=$(pgrep -f target/release/tts-multiplexer)
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

The `Cargo.toml` `[profile.release]` already has `lto = "thin"` and `opt-level = 3`. We add `debug = "line-tables-only"` to a new `[profile.release-with-debug]` profile so symbols resolve in samply without paying full-debug binary size — *but* this is a Cargo-only change with no runtime impact and the `release` build still works as-is. If we don't want even that, `RUSTFLAGS="-C debuginfo=2"` on a one-off build works fine.

### 3.7 What we deliberately do *not* change in the multiplexer

- No new dependency in `Cargo.toml`. (Possible later: `pprof-rs` behind a `profiling` feature.)
- No new HTTP endpoints. The existing `/metrics` already publishes `mux_ttfc_seconds`; if you want live histogram quantiles, scrape it with a tool of your choice.
- No new metric. The CSV from `stress.py` is the source of truth for chart rendering — keeps load-gen latency and SUT internal latency clearly separated.

---

## 4. Data flow

```
                 ┌──────────────────┐
                 │  perf/stress.py  │
                 │   (open-loop)    │
                 └────────┬─────────┘
                          │ WS frames over loopback
                          ▼
                 ┌──────────────────┐    samply --pid
                 │ tts-multiplexer  │◄──────────────────  perf/profile.sh
                 │   (release)      │                            │
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
- Fatal load-gen errors (Python exception in the runner itself) print to stderr and exit non-zero — never silently corrupt the CSV. The CSV is fsync'd on each row to survive a Ctrl-C mid-run.

**plot_latency.py:**
- Missing/empty CSV → exit non-zero with a clear message.
- A 1s window with zero successful requests → percentile lines have a gap (NaN), not a zero. Crashing because of an empty window is wrong; pretending p50 is 0 is misleading.

**profile.sh:**
- Multiplexer not running → exit non-zero with hint to start it.
- samply not on PATH → print install command (`cargo install samply`).

---

## 6. Testing

This is tooling, not product code. Test surface is small:

1. **stress.py smoke test**: a 5-second 10 rps run against the mock backend at calm mode, asserts CSV row count is between 40 and 60, asserts >95% success.
2. **plot_latency.py smoke test**: feed it a synthetic CSV (10 known rows), assert all four PNGs are produced and non-empty.
3. **profile.sh smoke test**: not automated — manual verification once.

Tests live in `perf/test_perf.py`. Not part of the existing `cargo test` or `assignment/test_client.py --all` runs; invoked as `python -m pytest perf/`. Goal is to catch tool bitrot, not to be a CI gate.

---

## 7. Phasing

**Phase 1 (this spec):** stress.py, plot_latency.py, profile.sh, README, smoke tests. No multiplexer code change.

**Phase 2 (separate spec, optional):** `pprof-rs` HTTP endpoint behind a `profiling` cargo feature, for in-process trigger during prod-shaped runs. Plus a checked-in Grafana dashboard JSON for `histogram_quantile()` views off `/metrics`.

Phase 1 is enough to answer the questions that motivated this work (where does CPU go, what does p99 look like at 50 rps, what shape is the CDF). Phase 2 is "nice to have when we want to profile a long-running process without restarting it."

---

## 8. Open questions

None blocking. Calibration choices to make once we run it:

- **Default RPS for the canonical "baseline" run.** Probably 50 rps × 60s for a quick check, 200 rps × 300s for a thorough one. Settled empirically once we see what the box can do.
- **Whether to include `assignment/mock_backend.py --calm` vs default chaos in the canonical baseline run.** Calm for clean CPU profile (no chaos noise), chaos for end-to-end realism. Both should be documented in `perf/README.md` as named recipes.

These are fine-tuning, not architecture.
