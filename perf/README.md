# Perf Harness

Tools for measuring multiplexer performance: an open-loop WebSocket stress
generator (`stress.py`), a chart renderer (`plot_latency.py`), and a samply
profiling wrapper (`profile.sh`).

Design spec: `docs/superpowers/specs/2026-05-06-perf-harness-design.md`.

## Prerequisites

- **Rust + Cargo.** Build the multiplexer with the dedicated profile so samply
  can resolve symbols:
  ```bash
  cargo build --profile release-with-debug
  ```
  Produces `target/release-with-debug/tts-multiplexer`.
- **samply** for CPU profiling:
  ```bash
  cargo install samply
  ```
- **Python 3.11+** with these packages:
  ```bash
  pip install websockets matplotlib numpy
  pip install uvloop  # optional but recommended (Linux/macOS, ~2-3x speedup)
  pip install psutil  # reserved for future SUT-process CPU reporting (not yet wired up)
  ```

## Quick start: comparison run

Three terminals.

**Terminal A — multiplexer (release-with-debug, calm chaos config):**
```bash
./target/release-with-debug/tts-multiplexer \
  --port 9000 --metrics-port 9001 \
  --backends 127.0.0.1:9100,127.0.0.1:9101,127.0.0.1:9102,127.0.0.1:9103,127.0.0.1:9104,127.0.0.1:9105,127.0.0.1:9106,127.0.0.1:9107
```

**Terminal B — mock backends (calm: clean profile, no chaos noise):**
```bash
python assignment/mock_backend.py --workers 8 --base-port 9100 --calm
```

**Terminal C — drive load and render charts:**
```bash
python -m perf.stress \
  --url ws://127.0.0.1:9000/v1/ws/speech \
  --rps 50 --duration 60 --ramp-up 10 \
  --out perf/runs/$(date +%Y-%m-%d)-baseline.csv

python -m perf.plot_latency \
  perf/runs/$(date +%Y-%m-%d)-baseline.csv \
  --out-dir perf/runs/charts/$(date +%Y-%m-%d)-baseline
```

The stress run prints a summary table at the end (CSV-side and SUT-side
quantiles plus the per-quantile delta) and writes `<name>.summary.json` next
to the CSV.

**Reading the charts** (open in this order):
1. `success_rate.png` — did the run hold up?
2. `throughput.png` — did the offered RPS match the request?
3. `latency_over_time.png` — how did the tail evolve?
4. `latency_cdf.png` — what's the tail shape?

## Recipe: profile-while-stressing

Add a fourth terminal alongside the quick-start setup.

**Terminal D — record samply profile:**
```bash
./perf/profile.sh record 60 baseline
```

This attaches samply to the running multiplexer for 60 s. Drive load in
Terminal C **at the same time** so the profile sees real work. When done:

```bash
./perf/profile.sh view baseline
```

Opens the profile in Firefox profiler with an interactive flame graph and
call tree.

## Recipe: end-to-end realism (chaos backends)

Same as the quick start but drop `--calm` from Terminal B:

```bash
python assignment/mock_backend.py --workers 8 --base-port 9100
```

This activates the mock backend's chaos behaviour (slow workers, occasional
errors, etc.) — better for end-to-end realism, worse for clean CPU profiles.

## Closed-loop saturation test

When you want **constant pressure** rather than constant arrival rate (e.g.,
to find the saturation point):

```bash
python -m perf.stress \
  --url ws://127.0.0.1:9000/v1/ws/speech \
  --concurrency 64 --duration 60 \
  --out perf/runs/saturation.csv
```

Closed-loop hides queueing — use it deliberately, not as the default.

## Interpreting the SUT-vs-CSV cross-check

The summary printed at end of run looks like:

```
quantile         CSV         SUT       delta
p50           48.20ms     42.50ms      5.70ms
p95          120.40ms    115.00ms      5.40ms
p99          180.10ms    160.00ms     20.10ms
```

The delta is **not** purely Python overhead. It includes:
- Kernel TCP / loopback overhead
- WebSocket frame parse/serialise on both sides
- asyncio task scheduling and GIL contention
- SUT-side histogram bucket quantisation (a few ms is normal)

The Phase 2 trigger (spec §7) is on **growth** in the delta with offered RPS,
not absolute delta. A baseline 10 rps run with delta 5 ms is healthy; the same
delta growing to 60 ms at 200 rps is the signal that Python has hit its ceiling.

## Tests

```bash
python -m pytest perf/ -v
```

Tests are smoke checks against tool bitrot — not a CI gate.

If your default `pytest` interpreter doesn't have `websockets` or `matplotlib`
installed (e.g., Homebrew's pytest with a different Python), the test fixtures
will fall back to `.venv/bin/python` or `.venv313/bin/python` if either has the
required modules. To avoid this fallback, install the deps in pytest's
interpreter or run pytest as `.venv/bin/python -m pytest`.
