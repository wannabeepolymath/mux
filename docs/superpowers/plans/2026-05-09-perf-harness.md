# Perf Harness — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Phase 1 of the perf harness spec — Python WS load gen with Poisson arrivals and contamination guards, four-chart latency renderer, samply profiling wrapper, smoke tests, README.

**Architecture:** Three independent CLI tools under `perf/`. `stress.py` drives load and writes per-request CSV + summary JSON. `plot_latency.py` renders four PNG charts from the CSV. `profile.sh` wraps `samply record`/`load` for CPU profiling. Pure histogram math (parse, diff, quantile) is isolated in `perf/metrics_diff.py` so it's cleanly unit-testable. Cargo.toml gets a new `[profile.release-with-debug]` profile for samply symbol resolution.

**Tech Stack:** Python 3.11+ (`websockets`, `matplotlib`, `numpy`, optional `uvloop`, optional `psutil`); Bash + `samply` (`cargo install samply`); Rust/Cargo for the profile addition. Tests via `pytest`.

**Spec:** `docs/superpowers/specs/2026-05-06-perf-harness-design.md`

**Branch:** `profilling` (current). Each task commits there.

---

## Pre-flight

Before starting, verify:

- [ ] On branch `profilling` with clean working tree: `git status` shows nothing.
- [ ] Repo root is `/Users/daksh/mySpace/code/multiplexer`. All paths below are relative to repo root.
- [ ] `assignment/test_client.py` exists and exposes `send_single_request` and `RequestResult` (verified during planning).
- [ ] `assignment/__init__.py` does NOT exist (the assignment dir is treated as read-only — we use `sys.path` injection to import from it).

---

## Task 1: Repo skeleton + Cargo profile

**Files:**
- Create: `perf/__init__.py` (empty)
- Create: `perf/runs/.gitkeep` (empty placeholder so the directory exists)
- Modify: `.gitignore` (add `perf/runs/` rule but allow `.gitkeep`)
- Modify: `Cargo.toml` (add `[profile.release-with-debug]`)

**Why this task:** establishes the directory structure, gitignore rules, and the Cargo profile that subsequent tasks rely on. No code yet.

- [ ] **Step 1: Create the perf/ skeleton**

```bash
mkdir -p perf/runs
touch perf/__init__.py
touch perf/runs/.gitkeep
```

- [ ] **Step 2: Update `.gitignore`**

Append to `.gitignore` (create the file if it doesn't track perf/runs already):

```
# Perf harness outputs
perf/runs/*
!perf/runs/.gitkeep
```

The `!perf/runs/.gitkeep` exception keeps the directory present in git so `perf/runs/<name>.csv` paths "just work" without needing `mkdir -p` everywhere.

- [ ] **Step 3: Add the Cargo profile**

Open `Cargo.toml`. Find the existing `[profile.release]` section (or the last `[profile.*]` section). Append:

```toml
[profile.release-with-debug]
inherits = "release"
debug = "line-tables-only"
```

This produces `target/release-with-debug/tts-multiplexer` with line-table debug info that lets samply resolve symbols. The existing `release` profile is untouched.

- [ ] **Step 4: Verify the Cargo profile builds**

Run:

```bash
cargo build --profile release-with-debug
```

Expected: builds successfully; produces `target/release-with-debug/tts-multiplexer`.

- [ ] **Step 5: Verify gitignore**

Run:

```bash
echo "test" > perf/runs/sample.csv
git status --short perf/
```

Expected: only `perf/runs/.gitkeep` and `perf/__init__.py` show as untracked (as new files); `sample.csv` is ignored.

Then clean up:

```bash
rm perf/runs/sample.csv
```

- [ ] **Step 6: Commit**

```bash
git add perf/__init__.py perf/runs/.gitkeep .gitignore Cargo.toml
git commit -m "perf: skeleton + release-with-debug Cargo profile"
```

---

## Task 2: `metrics_diff.py` — Prometheus histogram parser

**Files:**
- Create: `perf/metrics_diff.py`
- Create: `perf/test_perf.py` (this is the central test file for the whole harness)

**Why this task:** the Prometheus text format is small but specific. Parsing it correctly — including ignoring labels we don't care about and recognising `+Inf` — is a precondition for the bucket-diff and quantile work in Tasks 3-4.

- [ ] **Step 1: Write the failing tests**

Create `perf/test_perf.py`:

```python
"""Tests for the perf harness tooling."""
from __future__ import annotations

import math

import pytest

from perf.metrics_diff import Histogram, parse_histogram


SAMPLE_METRICS = """\
# HELP mux_ttfc_seconds Time to first chunk
# TYPE mux_ttfc_seconds histogram
mux_ttfc_seconds_bucket{le="0.005"} 10
mux_ttfc_seconds_bucket{le="0.01"} 25
mux_ttfc_seconds_bucket{le="0.025"} 60
mux_ttfc_seconds_bucket{le="0.05"} 90
mux_ttfc_seconds_bucket{le="0.1"} 100
mux_ttfc_seconds_bucket{le="+Inf"} 100
mux_ttfc_seconds_count 100
mux_ttfc_seconds_sum 2.5
# HELP unrelated_metric Something else
# TYPE unrelated_metric counter
unrelated_metric 42
"""


def test_parse_histogram_basic():
    hist = parse_histogram(SAMPLE_METRICS, "mux_ttfc_seconds")
    assert hist.count == 100
    assert hist.sum == pytest.approx(2.5)
    assert len(hist.buckets) == 6
    assert hist.buckets[0] == (0.005, 10)
    assert hist.buckets[-1][0] == math.inf
    assert hist.buckets[-1][1] == 100


def test_parse_histogram_buckets_are_sorted_ascending():
    hist = parse_histogram(SAMPLE_METRICS, "mux_ttfc_seconds")
    boundaries = [le for le, _ in hist.buckets]
    assert boundaries == sorted(boundaries)


def test_parse_histogram_missing_raises():
    with pytest.raises(KeyError):
        parse_histogram(SAMPLE_METRICS, "nonexistent_metric")


def test_parse_histogram_with_extra_labels():
    text = """\
mux_ttfc_seconds_bucket{backend="a",le="0.01"} 5
mux_ttfc_seconds_bucket{backend="a",le="+Inf"} 10
mux_ttfc_seconds_count{backend="a"} 10
mux_ttfc_seconds_sum{backend="a"} 0.05
"""
    hist = parse_histogram(text, "mux_ttfc_seconds")
    assert hist.count == 10
    assert hist.buckets == [(0.01, 5), (math.inf, 10)]
```

- [ ] **Step 2: Run tests, confirm they fail**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: ImportError or ModuleNotFoundError on `perf.metrics_diff` — fails to even collect.

- [ ] **Step 3: Implement the parser**

Create `perf/metrics_diff.py`:

```python
"""Pure functions for Prometheus histogram parsing, diffing, and quantile interpolation.

These functions are extracted from stress.py so they can be unit-tested without
spinning up HTTP/WS infrastructure. They are the load-bearing math behind the
SUT-vs-CSV cross-check (see spec §3.3).
"""
from __future__ import annotations

import math
import re
from dataclasses import dataclass


@dataclass(frozen=True)
class Histogram:
    """A snapshot of a Prometheus histogram metric.

    `buckets` is a list of (le_boundary, cumulative_count) sorted ascending by le.
    The final bucket's le is math.inf (the +Inf bucket).
    `count` is the total observation count; `sum` is the sum of observed values.
    """
    buckets: list[tuple[float, int]]
    count: int
    sum: float


_BUCKET_RE = re.compile(
    r'^([a-zA-Z_:][a-zA-Z0-9_:]*)_bucket\{([^}]*)\}\s+([0-9eE.+\-]+)\s*$'
)
_COUNT_RE = re.compile(
    r'^([a-zA-Z_:][a-zA-Z0-9_:]*)_count(?:\{[^}]*\})?\s+([0-9eE.+\-]+)\s*$'
)
_SUM_RE = re.compile(
    r'^([a-zA-Z_:][a-zA-Z0-9_:]*)_sum(?:\{[^}]*\})?\s+([0-9eE.+\-]+)\s*$'
)
_LE_RE = re.compile(r'le="([^"]+)"')


def _parse_le(le_str: str) -> float:
    if le_str == "+Inf":
        return math.inf
    return float(le_str)


def parse_histogram(metrics_text: str, metric_name: str) -> Histogram:
    """Parse a Prometheus text-format histogram for `metric_name` from `metrics_text`.

    Ignores other labels on the metric (sums their values across labels). For the
    multiplexer's `mux_ttfc_seconds` this is fine — it's a single histogram per
    process. If your histogram is partitioned by labels, this parser will collapse
    those partitions; revisit if that ever matters.

    Raises KeyError if no buckets for `metric_name` are present.
    """
    bucket_totals: dict[float, int] = {}
    total_count: int | None = None
    total_sum: float | None = None

    for line in metrics_text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue

        m = _BUCKET_RE.match(line)
        if m and m.group(1) == metric_name:
            labels_blob = m.group(2)
            le_match = _LE_RE.search(labels_blob)
            if not le_match:
                continue
            le = _parse_le(le_match.group(1))
            bucket_totals[le] = bucket_totals.get(le, 0) + int(float(m.group(3)))
            continue

        m = _COUNT_RE.match(line)
        if m and m.group(1) == metric_name:
            v = int(float(m.group(2)))
            total_count = (total_count or 0) + v
            continue

        m = _SUM_RE.match(line)
        if m and m.group(1) == metric_name:
            v = float(m.group(2))
            total_sum = (total_sum or 0.0) + v
            continue

    if not bucket_totals:
        raise KeyError(f"histogram {metric_name!r} not found in metrics text")

    if total_count is None:
        total_count = bucket_totals.get(math.inf, max(bucket_totals.values()))
    if total_sum is None:
        total_sum = 0.0

    buckets = sorted(bucket_totals.items())
    return Histogram(buckets=buckets, count=total_count, sum=total_sum)
```

- [ ] **Step 4: Run tests, confirm they pass**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: all four tests in this task PASS.

- [ ] **Step 5: Commit**

```bash
git add perf/metrics_diff.py perf/test_perf.py
git commit -m "perf: metrics_diff — Prometheus histogram parser"
```

---

## Task 3: `metrics_diff.py` — Histogram diff with counter-reset detection

**Files:**
- Modify: `perf/metrics_diff.py`
- Modify: `perf/test_perf.py`

**Why this task:** Prometheus histograms are cumulative from process start. Per spec §3.3, this-run-only quantiles require diffing start/end snapshots. Counter resets (multiplexer restart mid-run) must be detected and reported as `skipped`, not silently produce garbage.

- [ ] **Step 1: Write the failing tests**

Append to `perf/test_perf.py`:

```python
from perf.metrics_diff import CounterResetError, diff_histograms


def _h(buckets: list[tuple[float, int]], count: int, total_sum: float) -> Histogram:
    return Histogram(buckets=buckets, count=count, sum=total_sum)


def test_diff_histograms_normal():
    start = _h([(0.01, 5), (0.1, 8), (math.inf, 10)], count=10, total_sum=0.5)
    end = _h([(0.01, 12), (0.1, 18), (math.inf, 20)], count=20, total_sum=1.0)
    diff = diff_histograms(start, end)
    assert diff.buckets == [(0.01, 7), (0.1, 10), (math.inf, 10)]
    assert diff.count == 10
    assert diff.sum == pytest.approx(0.5)


def test_diff_histograms_zero_observations():
    start = _h([(0.01, 5), (math.inf, 10)], count=10, total_sum=0.5)
    end = _h([(0.01, 5), (math.inf, 10)], count=10, total_sum=0.5)
    diff = diff_histograms(start, end)
    assert diff.count == 0
    assert diff.sum == 0.0


def test_diff_histograms_counter_reset_via_count():
    start = _h([(math.inf, 10)], count=10, total_sum=1.0)
    end = _h([(math.inf, 5)], count=5, total_sum=0.5)
    with pytest.raises(CounterResetError):
        diff_histograms(start, end)


def test_diff_histograms_counter_reset_via_bucket():
    start = _h([(0.01, 5), (math.inf, 10)], count=10, total_sum=1.0)
    end = _h([(0.01, 4), (math.inf, 12)], count=12, total_sum=1.2)
    with pytest.raises(CounterResetError):
        diff_histograms(start, end)


def test_diff_histograms_counter_reset_via_sum():
    start = _h([(math.inf, 10)], count=10, total_sum=1.0)
    end = _h([(math.inf, 11)], count=11, total_sum=0.5)  # sum decreased
    with pytest.raises(CounterResetError):
        diff_histograms(start, end)


def test_diff_histograms_bucket_boundary_mismatch():
    start = _h([(0.01, 5), (math.inf, 10)], count=10, total_sum=0.5)
    end = _h([(0.05, 5), (math.inf, 10)], count=10, total_sum=0.5)
    with pytest.raises(ValueError, match="bucket boundaries"):
        diff_histograms(start, end)
```

- [ ] **Step 2: Run tests, confirm they fail**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: the new tests fail on import of `CounterResetError` and `diff_histograms`.

- [ ] **Step 3: Implement diff and CounterResetError**

Append to `perf/metrics_diff.py`:

```python
class CounterResetError(Exception):
    """Raised when a histogram diff detects a counter reset (start > end somewhere).

    Typical cause: the multiplexer was restarted mid-run, or `/metrics` was
    scraped from a different process between snapshots. Per spec §3.3, the run
    still completes but SUT-side quantiles are marked `skipped` with
    `reason="counter_reset_or_restart"`.
    """


def diff_histograms(start: Histogram, end: Histogram) -> Histogram:
    """Compute end − start, returning a histogram of observations in the interval.

    Raises CounterResetError if any bucket count, the total count, or the sum
    decreased between start and end (a sign the underlying counter was reset).
    Raises ValueError if bucket boundaries differ between snapshots.
    """
    if end.count < start.count:
        raise CounterResetError(
            f"_count decreased: start={start.count} end={end.count}"
        )
    if end.sum < start.sum:
        raise CounterResetError(
            f"_sum decreased: start={start.sum} end={end.sum}"
        )
    if len(start.buckets) != len(end.buckets):
        raise ValueError(
            f"bucket count mismatch: start={len(start.buckets)} end={len(end.buckets)}"
        )

    diff_buckets: list[tuple[float, int]] = []
    for (le_s, c_s), (le_e, c_e) in zip(start.buckets, end.buckets):
        if le_s != le_e:
            raise ValueError(
                f"bucket boundaries differ: start le={le_s} end le={le_e}"
            )
        if c_e < c_s:
            raise CounterResetError(
                f"bucket le={le_e} decreased: start={c_s} end={c_e}"
            )
        diff_buckets.append((le_e, c_e - c_s))

    return Histogram(
        buckets=diff_buckets,
        count=end.count - start.count,
        sum=end.sum - start.sum,
    )
```

- [ ] **Step 4: Run tests, confirm they pass**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: all tests so far PASS (parser tests + 6 diff tests).

- [ ] **Step 5: Commit**

```bash
git add perf/metrics_diff.py perf/test_perf.py
git commit -m "perf: metrics_diff — histogram diff with counter-reset detection"
```

---

## Task 4: `metrics_diff.py` — Quantile interpolation

**Files:**
- Modify: `perf/metrics_diff.py`
- Modify: `perf/test_perf.py`

**Why this task:** computes p50/p95/p99 from a (diffed) histogram using the same linear-interpolation-within-bucket approach Prometheus's `histogram_quantile()` uses. Required by the cross-check (spec §3.3) for SUT-side quantiles.

- [ ] **Step 1: Write the failing tests**

Append to `perf/test_perf.py`:

```python
from perf.metrics_diff import quantile_from_buckets


def test_quantile_basic_interpolation():
    # 100 samples: 0 in [0, 0.01], 50 in [0.01, 0.05], 50 in [0.05, 0.1], 0 above.
    hist = _h([(0.01, 0), (0.05, 50), (0.1, 100), (math.inf, 100)], count=100, total_sum=5.5)
    # p50 → target = 50, falls right at the 0.05 boundary
    assert quantile_from_buckets(hist, 0.5) == pytest.approx(0.05)
    # p25 → target = 25, halfway through the (0.01, 0.05] bucket
    assert quantile_from_buckets(hist, 0.25) == pytest.approx(0.03)
    # p75 → target = 75, halfway through the (0.05, 0.1] bucket
    assert quantile_from_buckets(hist, 0.75) == pytest.approx(0.075)


def test_quantile_falls_in_inf_bucket():
    # 100 samples, but 5 are in the +Inf bucket (i.e., > 0.1)
    hist = _h([(0.01, 50), (0.1, 95), (math.inf, 100)], count=100, total_sum=10.0)
    # p99 → target = 99, falls in (0.1, +Inf) — return previous boundary (0.1) since
    # we cannot interpolate past +Inf
    assert quantile_from_buckets(hist, 0.99) == pytest.approx(0.1)


def test_quantile_empty_histogram_returns_none():
    hist = _h([(0.01, 0), (math.inf, 0)], count=0, total_sum=0.0)
    assert quantile_from_buckets(hist, 0.5) is None


def test_quantile_at_zero_lower_boundary():
    # All 10 samples in [0, 0.01]
    hist = _h([(0.01, 10), (math.inf, 10)], count=10, total_sum=0.05)
    # p50 → target = 5, halfway through (0, 0.01]
    assert quantile_from_buckets(hist, 0.5) == pytest.approx(0.005)


def test_quantile_p99_with_50_samples_documents_noise():
    # Documents the chart-design rationale: at low sample counts, p99 is
    # essentially the upper boundary of the highest bucket containing samples.
    # 50 samples, all in [0, 0.5]
    hist = _h([(0.5, 50), (math.inf, 50)], count=50, total_sum=5.0)
    # p99 → target = 49.5, very near the top of the (0, 0.5] bucket
    assert 0.49 < quantile_from_buckets(hist, 0.99) <= 0.5
```

- [ ] **Step 2: Run tests, confirm they fail**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: 5 new tests fail on import of `quantile_from_buckets`.

- [ ] **Step 3: Implement the quantile function**

Append to `perf/metrics_diff.py`:

```python
def quantile_from_buckets(hist: Histogram, q: float) -> float | None:
    """Approximate quantile q ∈ [0, 1] from a Prometheus-style cumulative histogram.

    Uses linear interpolation within the bucket containing the target rank, matching
    Prometheus's `histogram_quantile()` convention. Returns None for an empty
    histogram (count == 0). When the target falls in the +Inf bucket, returns the
    previous (finite) boundary — we cannot extrapolate past +Inf, so this is the
    best we can do.

    The lower edge of the lowest finite bucket is treated as 0.
    """
    if hist.count == 0:
        return None
    if not 0.0 <= q <= 1.0:
        raise ValueError(f"q must be in [0, 1], got {q}")

    target = q * hist.count
    prev_le = 0.0
    prev_cumulative = 0
    for le, cumulative in hist.buckets:
        if cumulative >= target:
            if math.isinf(le):
                return prev_le
            bucket_count = cumulative - prev_cumulative
            if bucket_count == 0:
                return prev_le
            frac = (target - prev_cumulative) / bucket_count
            return prev_le + frac * (le - prev_le)
        prev_le = le
        prev_cumulative = cumulative

    return prev_le
```

- [ ] **Step 4: Run tests, confirm they pass**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: all metrics_diff tests pass (parser + diff + quantile).

- [ ] **Step 5: Commit**

```bash
git add perf/metrics_diff.py perf/test_perf.py
git commit -m "perf: metrics_diff — Prometheus-style quantile interpolation"
```

---

## Task 5: `stress.py` — CSV writer with line buffering and signal handling

**Files:**
- Create: `perf/stress.py` (just the CSV writer for now; later tasks fill out the rest)
- Modify: `perf/test_perf.py`

**Why this task:** lands the durability story for per-request rows (spec §5). Line-buffering plus a SIGINT/SIGTERM handler that flushes is sufficient to survive Ctrl-C without the latency cost of fsync-per-row.

- [ ] **Step 1: Write the failing tests**

Append to `perf/test_perf.py`:

```python
import csv as _csv

from perf.stress import CsvRowWriter


def test_csv_writer_writes_header_and_rows(tmp_path):
    out = tmp_path / "rows.csv"
    w = CsvRowWriter(out)
    w.write_row({
        "ts_unix_ms": 1000, "req_id": 0, "status": "success", "error_kind": "",
        "ttfc_ms": 12.3, "duration_ms": 100.4, "bytes_received": 4096, "chunk_count": 32,
    })
    w.write_row({
        "ts_unix_ms": 1100, "req_id": 1, "status": "error", "error_kind": "connect_failed",
        "ttfc_ms": "", "duration_ms": 5.0, "bytes_received": 0, "chunk_count": 0,
    })
    w.close()

    with out.open() as f:
        reader = _csv.DictReader(f)
        rows = list(reader)
    assert reader.fieldnames == list(CsvRowWriter.HEADER)
    assert len(rows) == 2
    assert rows[0]["status"] == "success"
    assert rows[1]["error_kind"] == "connect_failed"


def test_csv_writer_is_line_buffered(tmp_path):
    """After write_row returns, the row is durable on disk without an explicit flush."""
    out = tmp_path / "rows.csv"
    w = CsvRowWriter(out)
    w.write_row({
        "ts_unix_ms": 1, "req_id": 0, "status": "success", "error_kind": "",
        "ttfc_ms": 1.0, "duration_ms": 2.0, "bytes_received": 1, "chunk_count": 1,
    })
    # Read back immediately (without close()) — should see header + row.
    text = out.read_text()
    assert text.count("\n") >= 2
    assert "success" in text
    w.close()
```

- [ ] **Step 2: Run tests, confirm they fail**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: ImportError on `perf.stress`.

- [ ] **Step 3: Implement the CSV writer**

Create `perf/stress.py`:

```python
"""Open-loop WebSocket stress generator with per-run contamination guards.

See `docs/superpowers/specs/2026-05-06-perf-harness-design.md` for design.
This file is the CLI entrypoint; pure histogram math lives in `metrics_diff.py`.
"""
from __future__ import annotations

import csv
import signal
import threading
from pathlib import Path
from typing import Any


class CsvRowWriter:
    """Line-buffered CSV writer with a SIGINT/SIGTERM handler that flushes cleanly.

    See spec §3.4 for the row schema and §5 for the durability rationale (fsync
    per row would itself become a bottleneck at the rates we target; line-
    buffered writes plus a signal handler are enough to survive Ctrl-C).
    """

    HEADER = (
        "ts_unix_ms",
        "req_id",
        "status",
        "error_kind",
        "ttfc_ms",
        "duration_ms",
        "bytes_received",
        "chunk_count",
    )

    def __init__(self, path: Path):
        self._path = Path(path)
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._f = self._path.open("w", buffering=1, newline="")
        self._writer = csv.writer(self._f)
        self._writer.writerow(self.HEADER)
        self._lock = threading.Lock()
        self._closed = False

    def write_row(self, row: dict[str, Any]) -> None:
        with self._lock:
            if self._closed:
                return
            self._writer.writerow([row.get(k, "") for k in self.HEADER])

    def install_signal_handler(self) -> None:
        """Install SIGINT/SIGTERM handlers that flush+close the CSV before re-raising.

        Call once from the main thread. The handlers raise KeyboardInterrupt
        after flushing so the asyncio loop unwinds cleanly.
        """
        def _handler(signum, _frame):
            self.close()
            # Re-raise so the caller's main() unwinds cleanly.
            raise KeyboardInterrupt(f"received signal {signum}")

        signal.signal(signal.SIGINT, _handler)
        signal.signal(signal.SIGTERM, _handler)

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
            try:
                self._f.flush()
            finally:
                self._f.close()
```

- [ ] **Step 4: Run tests, confirm they pass**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: all tests pass including the two new CSV writer tests.

- [ ] **Step 5: Commit**

```bash
git add perf/stress.py perf/test_perf.py
git commit -m "perf: stress.py — line-buffered CSV writer with signal handler"
```

---

## Task 6: `stress.py` — Poisson arrivals + per-request runner

**Files:**
- Modify: `perf/stress.py`
- Modify: `perf/test_perf.py`

**Why this task:** the two primary load-driving primitives. `poisson_arrivals` enforces the spec's Poisson requirement (§3.3 "Poisson means exponential inter-arrivals"). `run_request` reuses `send_single_request` from `assignment/test_client.py` and maps its `RequestResult` to a CSV row dict.

- [ ] **Step 1: Write the failing tests**

Append to `perf/test_perf.py`:

```python
import asyncio
import statistics
from unittest.mock import patch

from perf.stress import build_csv_row, poisson_arrival_delays


def test_poisson_arrival_delays_long_run_mean_close_to_target():
    """With many samples, the mean inter-arrival time should be ~1/rps."""
    rps = 50
    n = 5000
    delays = list(poisson_arrival_delays(rps=rps, count=n, seed=42))
    assert len(delays) == n
    expected_mean = 1.0 / rps
    actual_mean = statistics.mean(delays)
    # Exponential SD = mean; with n=5000, SE ≈ mean/sqrt(n) ≈ 0.014 * mean.
    # Allow ±15% tolerance to keep CI flake-free.
    assert 0.85 * expected_mean < actual_mean < 1.15 * expected_mean


def test_poisson_arrival_delays_are_not_constant():
    """Guard against accidental fixed-interval implementation."""
    delays = list(poisson_arrival_delays(rps=10, count=50, seed=7))
    assert statistics.stdev(delays) > 0.01  # exponential SD = mean = 0.1


# ---- build_csv_row ----

class _FakeResult:
    def __init__(self, success=False, error="", ttfc_ms=0.0, total_ms=0.0,
                 total_bytes=0, chunks_received=0):
        self.success = success
        self.error = error
        self.ttfc_ms = ttfc_ms
        self.total_ms = total_ms
        self.total_bytes = total_bytes
        self.chunks_received = chunks_received


def test_build_csv_row_success():
    r = _FakeResult(success=True, ttfc_ms=12.3, total_ms=200.0,
                    total_bytes=4096, chunks_received=32)
    row = build_csv_row(req_id=7, ts_unix_ms=1000, result=r)
    assert row["req_id"] == 7
    assert row["status"] == "success"
    assert row["error_kind"] == ""
    assert row["ttfc_ms"] == 12.3
    assert row["duration_ms"] == 200.0
    assert row["bytes_received"] == 4096
    assert row["chunk_count"] == 32


def test_build_csv_row_timeout():
    r = _FakeResult(success=False, error="timeout", total_ms=30000.0)
    row = build_csv_row(req_id=1, ts_unix_ms=2000, result=r)
    assert row["status"] == "timeout"
    assert row["error_kind"] == ""


def test_build_csv_row_connect_failure():
    r = _FakeResult(success=False, error="connection timeout")
    row = build_csv_row(req_id=2, ts_unix_ms=3000, result=r)
    assert row["status"] == "error"
    assert row["error_kind"] == "connect_failed"


def test_build_csv_row_unexpected_close():
    r = _FakeResult(success=False, error="connection closed: 1006",
                    chunks_received=2, ttfc_ms=15.0, total_ms=200.0)
    row = build_csv_row(req_id=3, ts_unix_ms=4000, result=r)
    assert row["status"] == "error"
    assert row["error_kind"] == "unexpected_close"


def test_build_csv_row_server_error():
    r = _FakeResult(success=False, error="backend rejected request")
    row = build_csv_row(req_id=4, ts_unix_ms=5000, result=r)
    assert row["status"] == "error"
    assert row["error_kind"] == "server_error"


def test_build_csv_row_dropped():
    """A dropped_offered row is built without a RequestResult."""
    from perf.stress import build_dropped_row
    row = build_dropped_row(req_id=99, ts_unix_ms=6000)
    assert row["status"] == "dropped_offered"
    assert row["ttfc_ms"] == ""
    assert row["duration_ms"] == ""
    assert row["bytes_received"] == 0
```

- [ ] **Step 2: Run tests, confirm they fail**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: ImportError on the new symbols.

- [ ] **Step 3: Implement the arrivals generator and row builders**

Append to `perf/stress.py`:

```python
import random
import sys
import time
from typing import Iterator

# Path injection: assignment/ has no __init__.py and is treated as read-only
# per the spec. Inject it onto sys.path so we can import test_client cleanly.
_REPO_ROOT = Path(__file__).resolve().parent.parent
_ASSIGN_DIR = _REPO_ROOT / "assignment"
if str(_ASSIGN_DIR) not in sys.path:
    sys.path.insert(0, str(_ASSIGN_DIR))

# Imported lazily inside run_request to keep import cheap and to make unit
# tests of build_csv_row independent of the assignment package.


def poisson_arrival_delays(
    rps: float, count: int, seed: int | None = None
) -> Iterator[float]:
    """Yield exponential inter-arrival times (mean = 1/rps) for `count` arrivals.

    This is the spec-mandated Poisson process. Constant-spacing schedulers are
    explicitly forbidden because they under-stress queueing and produce
    optimistic latency tails. See spec §3.3.
    """
    rng = random.Random(seed)
    for _ in range(count):
        yield rng.expovariate(rps)


def _classify_error(error_msg: str, chunks_received: int) -> str:
    """Map a `RequestResult.error` string to a CSV `error_kind` token.

    See spec §5. Connection-level errors → `connect_failed` or
    `unexpected_close`; server-side error frames → `server_error`. Unknown
    errors fall through to `server_error` rather than introducing a new token.
    """
    if "connection" in error_msg.lower() or "websocket" in error_msg.lower():
        # Distinguish "got some chunks then dropped" from "never connected".
        if chunks_received > 0:
            return "unexpected_close"
        return "connect_failed"
    return "server_error"


def build_csv_row(req_id: int, ts_unix_ms: int, result) -> dict:
    """Build a CSV row dict from a test_client.RequestResult."""
    if result.success:
        return {
            "ts_unix_ms": ts_unix_ms,
            "req_id": req_id,
            "status": "success",
            "error_kind": "",
            "ttfc_ms": result.ttfc_ms,
            "duration_ms": result.total_ms,
            "bytes_received": result.total_bytes,
            "chunk_count": result.chunks_received,
        }

    if result.error == "timeout":
        return {
            "ts_unix_ms": ts_unix_ms,
            "req_id": req_id,
            "status": "timeout",
            "error_kind": "",
            "ttfc_ms": result.ttfc_ms if result.ttfc_ms else "",
            "duration_ms": result.total_ms if result.total_ms else "",
            "bytes_received": result.total_bytes,
            "chunk_count": result.chunks_received,
        }

    return {
        "ts_unix_ms": ts_unix_ms,
        "req_id": req_id,
        "status": "error",
        "error_kind": _classify_error(result.error, result.chunks_received),
        "ttfc_ms": result.ttfc_ms if result.ttfc_ms else "",
        "duration_ms": result.total_ms if result.total_ms else "",
        "bytes_received": result.total_bytes,
        "chunk_count": result.chunks_received,
    }


def build_dropped_row(req_id: int, ts_unix_ms: int) -> dict:
    """Build a row for an arrival that hit the --max-inflight cap and was dropped."""
    return {
        "ts_unix_ms": ts_unix_ms,
        "req_id": req_id,
        "status": "dropped_offered",
        "error_kind": "",
        "ttfc_ms": "",
        "duration_ms": "",
        "bytes_received": 0,
        "chunk_count": 0,
    }


async def run_request(url: str, req_id: int, csv_writer: CsvRowWriter,
                      timeout: float = 30.0) -> None:
    """Issue one WS request via send_single_request and write a CSV row."""
    from test_client import send_single_request  # noqa: E402  (lazy import)

    ts = int(time.time() * 1000)
    result = await send_single_request(
        url, text=f"perf stress req {req_id}", stream_id=f"perf-{req_id}",
        timeout=timeout,
    )
    csv_writer.write_row(build_csv_row(req_id, ts, result))
```

- [ ] **Step 4: Run tests, confirm they pass**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: all tests pass including the new arrivals + row builder tests.

- [ ] **Step 5: Commit**

```bash
git add perf/stress.py perf/test_perf.py
git commit -m "perf: stress.py — Poisson arrivals + RequestResult→CSV mapping"
```

---

## Task 7: `stress.py` — main CLI orchestration

**Files:**
- Modify: `perf/stress.py`
- (no test changes; integration smoke test is Task 8)

**Why this task:** glues the pieces from Tasks 5-6 plus `metrics_diff` from Tasks 2-4 into a runnable CLI. Implements every flag in spec §3.3 and the contamination guards. No new unit tests — orchestration is exercised end-to-end by Task 8's smoke test.

- [ ] **Step 1: Add the orchestration code**

Append to `perf/stress.py`:

```python
import argparse
import asyncio
import json
import os
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass

from perf.metrics_diff import (
    CounterResetError,
    Histogram,
    diff_histograms,
    parse_histogram,
    quantile_from_buckets,
)


_METRIC_NAME = "mux_ttfc_seconds"
_QUANTILES = (0.50, 0.95, 0.99)


def _setup_uvloop_if_available() -> str:
    try:
        import uvloop  # type: ignore
    except ImportError:
        return "stdlib_asyncio"
    uvloop.install()
    return "uvloop"


def _default_metrics_url(ws_url: str) -> str:
    """Convention: metrics on port 9001 of the same host as the WS URL."""
    parsed = urllib.parse.urlparse(ws_url)
    host = parsed.hostname or "127.0.0.1"
    return f"http://{host}:9001/metrics"


def _scrape_metrics(metrics_url: str, timeout: float = 5.0) -> Histogram | None:
    """Fetch and parse mux_ttfc_seconds. Returns None on any error (logged to stderr)."""
    try:
        with urllib.request.urlopen(metrics_url, timeout=timeout) as resp:
            text = resp.read().decode("utf-8", errors="replace")
    except (urllib.error.URLError, OSError, TimeoutError) as exc:
        print(f"WARN: /metrics scrape failed ({metrics_url}): {exc}", file=sys.stderr)
        return None
    try:
        return parse_histogram(text, _METRIC_NAME)
    except KeyError:
        print(
            f"WARN: /metrics fetched but {_METRIC_NAME} not present "
            f"(this is fine if SUT is mock_backend; cross-check will be skipped)",
            file=sys.stderr,
        )
        return None


def _csv_quantile(values: list[float], q: float) -> float | None:
    """Exact quantile from a sorted list of floats (linear interp between elements)."""
    if not values:
        return None
    s = sorted(values)
    pos = q * (len(s) - 1)
    lo = int(pos)
    hi = min(lo + 1, len(s) - 1)
    frac = pos - lo
    return s[lo] + frac * (s[hi] - s[lo])


@dataclass
class _RunCounts:
    attempted: int = 0
    success: int = 0
    error: int = 0
    timeout: int = 0
    dropped_offered: int = 0
    peak_inflight: int = 0
    max_inflight_reached_count: int = 0


async def _open_loop(
    url: str,
    target_rps: float,
    duration_s: float,
    ramp_up_s: float,
    max_inflight: int,
    csv_writer: CsvRowWriter,
    counts: _RunCounts,
    timeout: float,
) -> None:
    """Drive Poisson arrivals at target_rps with linear ramp-up.

    During the ramp-up window, the effective rps scales from 0 to target_rps.
    During steady state, arrivals follow expovariate(target_rps).
    """
    inflight = 0
    inflight_lock = asyncio.Lock()
    next_req_id = 0
    start_mono = time.monotonic()
    deadline = start_mono + duration_s
    rng = random.Random()

    async def _runner(req_id: int) -> None:
        nonlocal inflight
        try:
            await run_request(url, req_id, csv_writer, timeout=timeout)
        finally:
            async with inflight_lock:
                inflight -= 1

    pending: list[asyncio.Task] = []
    while True:
        now = time.monotonic()
        if now >= deadline:
            break

        elapsed = now - start_mono
        effective_rps = target_rps
        if ramp_up_s > 0 and elapsed < ramp_up_s:
            effective_rps = target_rps * (elapsed / ramp_up_s)
            if effective_rps < 1e-3:
                effective_rps = 1e-3  # avoid div-by-zero in expovariate

        delay = rng.expovariate(effective_rps)
        await asyncio.sleep(delay)

        if time.monotonic() >= deadline:
            break

        ts_unix_ms = int(time.time() * 1000)
        async with inflight_lock:
            counts.attempted += 1
            if inflight >= max_inflight:
                counts.dropped_offered += 1
                counts.max_inflight_reached_count += 1
                csv_writer.write_row(build_dropped_row(next_req_id, ts_unix_ms))
                next_req_id += 1
                continue
            inflight += 1
            if inflight > counts.peak_inflight:
                counts.peak_inflight = inflight

        task = asyncio.create_task(_runner(next_req_id))
        pending.append(task)
        next_req_id += 1

        # Lightly reap completed tasks so `pending` doesn't grow unbounded.
        if len(pending) > 1024:
            pending = [t for t in pending if not t.done()]

    if pending:
        await asyncio.gather(*pending, return_exceptions=True)


async def _closed_loop(
    url: str,
    concurrency: int,
    duration_s: float,
    csv_writer: CsvRowWriter,
    counts: _RunCounts,
    timeout: float,
) -> None:
    """Drive `concurrency` workers that loop issuing requests until the deadline."""
    deadline = time.monotonic() + duration_s
    next_req_id = 0
    next_req_id_lock = asyncio.Lock()
    inflight = 0
    inflight_lock = asyncio.Lock()

    async def worker():
        nonlocal next_req_id, inflight
        while time.monotonic() < deadline:
            async with next_req_id_lock:
                req_id = next_req_id
                next_req_id += 1
            counts.attempted += 1
            async with inflight_lock:
                inflight += 1
                if inflight > counts.peak_inflight:
                    counts.peak_inflight = inflight
            try:
                await run_request(url, req_id, csv_writer, timeout=timeout)
            finally:
                async with inflight_lock:
                    inflight -= 1

    workers = [asyncio.create_task(worker()) for _ in range(concurrency)]
    await asyncio.gather(*workers, return_exceptions=True)


def _aggregate_counts_from_csv(csv_path: Path, counts: _RunCounts) -> list[float]:
    """Read the CSV back to update success/error/timeout counts and collect TTFCs.

    The runner-side counts (`attempted`, `dropped_offered`, `peak_inflight`,
    `max_inflight_reached_count`) are authoritative; success/error/timeout we
    compute from the CSV to avoid double-bookkeeping.
    """
    ttfcs: list[float] = []
    with csv_path.open() as f:
        for row in csv.DictReader(f):
            status = row["status"]
            if status == "success":
                counts.success += 1
                if row["ttfc_ms"]:
                    ttfcs.append(float(row["ttfc_ms"]))
            elif status == "error":
                counts.error += 1
            elif status == "timeout":
                counts.timeout += 1
    return ttfcs


def _build_summary(
    args, counts: _RunCounts, ttfcs: list[float],
    sut_start: Histogram | None, sut_end: Histogram | None,
    cpu_user: float, cpu_sys: float, wall: float,
    run_start_unix_ms: int, run_end_unix_ms: int,
) -> dict:
    csv_quants = {
        "p50": _csv_quantile(ttfcs, 0.50),
        "p95": _csv_quantile(ttfcs, 0.95),
        "p99": _csv_quantile(ttfcs, 0.99),
    }
    csv_quants_ms = {
        k: (round(v, 3) if v is not None else None) for k, v in csv_quants.items()
    }

    sut_block: dict
    delta_block: dict | None
    if sut_start is None or sut_end is None:
        sut_block = {"p50": None, "p95": None, "p99": None,
                     "source": "skipped", "reason": "metrics_unreachable"}
        delta_block = None
    else:
        try:
            diff = diff_histograms(sut_start, sut_end)
            sut_q = {
                "p50": quantile_from_buckets(diff, 0.50),
                "p95": quantile_from_buckets(diff, 0.95),
                "p99": quantile_from_buckets(diff, 0.99),
            }
            # Prometheus values are in seconds; convert to ms for symmetry with CSV.
            sut_q_ms = {k: (round(v * 1000, 3) if v is not None else None)
                        for k, v in sut_q.items()}
            sut_block = {**sut_q_ms, "source": "histogram_diff"}
            delta_block = {
                k: (round(csv_quants_ms[k] - sut_q_ms[k], 3)
                    if csv_quants_ms[k] is not None and sut_q_ms[k] is not None
                    else None)
                for k in ("p50", "p95", "p99")
            }
        except CounterResetError as exc:
            sut_block = {"p50": None, "p95": None, "p99": None,
                         "source": "skipped", "reason": "counter_reset_or_restart",
                         "detail": str(exc)}
            delta_block = None

    cpu_fraction = (cpu_user + cpu_sys) / wall if wall > 0 else 0.0
    return {
        "run": {
            "start_unix_ms": run_start_unix_ms,
            "end_unix_ms": run_end_unix_ms,
            "duration_s": args.duration,
            "ramp_up_s": getattr(args, "ramp_up", 0.0),
            "mode": "open_loop" if args.rps is not None else "closed_loop",
            "target_rps": args.rps,
            "concurrency": args.concurrency,
            "url": args.url,
            "metrics_url": args.metrics_url,
        },
        "counts": {
            "attempted": counts.attempted,
            "success": counts.success,
            "error": counts.error,
            "timeout": counts.timeout,
            "dropped_offered": counts.dropped_offered,
            "peak_inflight": counts.peak_inflight,
            "max_inflight_reached_count": counts.max_inflight_reached_count,
        },
        "load_gen": {
            "cpu_user_s": round(cpu_user, 3),
            "cpu_system_s": round(cpu_sys, 3),
            "wall_s": round(wall, 3),
            "cpu_fraction_one_core": round(cpu_fraction, 4),
            "warning_high_cpu": cpu_fraction > 0.25,
        },
        "quantiles_ms": {
            "csv": csv_quants_ms,
            "sut": sut_block,
            "delta": delta_block,
        },
    }


def _print_summary(summary: dict) -> None:
    cm = summary["quantiles_ms"]["csv"]
    sm = summary["quantiles_ms"]["sut"]
    dm = summary["quantiles_ms"]["delta"]
    counts = summary["counts"]
    lg = summary["load_gen"]

    print()
    print("=== run summary ===")
    print(f"attempted={counts['attempted']} success={counts['success']} "
          f"error={counts['error']} timeout={counts['timeout']} "
          f"dropped_offered={counts['dropped_offered']}")
    print(f"peak_inflight={counts['peak_inflight']}")
    print(f"load-gen CPU: {lg['cpu_fraction_one_core']*100:.1f}% of one core "
          f"({lg['cpu_user_s']}s user + {lg['cpu_system_s']}s sys / "
          f"{lg['wall_s']}s wall)"
          + ("  [WARNING: high]" if lg['warning_high_cpu'] else ""))

    def _fmt(v): return f"{v:.2f}ms" if v is not None else "n/a"

    print()
    print(f"{'quantile':<10}{'CSV':>12}{'SUT':>12}{'delta':>12}")
    for q in ("p50", "p95", "p99"):
        c = cm.get(q)
        s = sm.get(q) if isinstance(sm, dict) else None
        d = dm.get(q) if isinstance(dm, dict) else None
        print(f"{q:<10}{_fmt(c):>12}{_fmt(s):>12}{_fmt(d):>12}")
    if isinstance(sm, dict) and sm.get("source") == "skipped":
        print(f"  (SUT cross-check skipped: {sm.get('reason')})")
    print()


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Open-loop WS stress generator (perf harness)")
    p.add_argument("--url", required=True, help="WS URL, e.g. ws://127.0.0.1:9000/v1/ws/speech")
    p.add_argument("--out", required=True, type=Path, help="Output CSV path")
    p.add_argument("--duration", type=float, required=True, help="Run duration in seconds")

    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--rps", type=float, help="Open-loop target RPS (Poisson arrivals)")
    g.add_argument("--concurrency", type=int, help="Closed-loop worker count")

    p.add_argument("--ramp-up", type=float, default=0.0,
                   help="Linear ramp-up duration in seconds (open-loop only)")
    p.add_argument("--max-inflight", type=int, default=500,
                   help="Drop new arrivals beyond this in-flight count (open-loop only)")
    p.add_argument("--timeout", type=float, default=30.0,
                   help="Per-request timeout in seconds")
    p.add_argument("--metrics-url", default=None,
                   help="Multiplexer /metrics URL (default: http://<host>:9001/metrics)")
    return p.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    if args.metrics_url is None:
        args.metrics_url = _default_metrics_url(args.url)

    loop_kind = _setup_uvloop_if_available()
    print(f"event loop: {loop_kind}", file=sys.stderr)

    csv_writer = CsvRowWriter(args.out)
    csv_writer.install_signal_handler()
    counts = _RunCounts()

    sut_start = _scrape_metrics(args.metrics_url)
    cpu_t0 = os.times()
    wall_t0 = time.monotonic()
    run_start_unix_ms = int(time.time() * 1000)

    try:
        if args.rps is not None:
            asyncio.run(_open_loop(
                args.url, args.rps, args.duration, args.ramp_up, args.max_inflight,
                csv_writer, counts, args.timeout,
            ))
        else:
            asyncio.run(_closed_loop(
                args.url, args.concurrency, args.duration,
                csv_writer, counts, args.timeout,
            ))
    finally:
        csv_writer.close()

    wall_elapsed = time.monotonic() - wall_t0
    cpu_t1 = os.times()
    sut_end = _scrape_metrics(args.metrics_url)
    run_end_unix_ms = int(time.time() * 1000)

    ttfcs = _aggregate_counts_from_csv(args.out, counts)
    summary = _build_summary(
        args, counts, ttfcs, sut_start, sut_end,
        cpu_user=cpu_t1.user - cpu_t0.user,
        cpu_sys=cpu_t1.system - cpu_t0.system,
        wall=wall_elapsed,
        run_start_unix_ms=run_start_unix_ms,
        run_end_unix_ms=run_end_unix_ms,
    )

    summary_path = args.out.with_suffix(".summary.json")
    summary_path.write_text(json.dumps(summary, indent=2))
    _print_summary(summary)

    if summary["load_gen"]["warning_high_cpu"]:
        print("WARNING: load-gen CPU >25% of one core, measurements may be biased",
              file=sys.stderr)

    return 0


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 2: Smoke-check that the file imports and the unit tests still pass**

```bash
python -c "import perf.stress; print('ok')"
python -m pytest perf/test_perf.py -v
```

Expected: imports cleanly; all existing tests still pass.

- [ ] **Step 3: Commit**

```bash
git add perf/stress.py
git commit -m "perf: stress.py — main CLI with open/closed-loop, cross-check, summary.json"
```

---

## Task 8: stress.py smoke test (integration)

**Files:**
- Modify: `perf/test_perf.py`

**Why this task:** the orchestration in Task 7 has many moving parts (asyncio, subprocess, CSV writing, JSON output) that aren't usefully unit-tested. A 5-second run against a real `mock_backend.py` exercises the whole tool end-to-end and is the lowest-cost way to catch tool bitrot (spec §6).

- [ ] **Step 1: Write the integration test**

Append to `perf/test_perf.py`:

```python
import json
import shutil
import socket
import subprocess
import sys
import time as _time
from pathlib import Path

import pytest


def _wait_for_port(host: str, port: int, timeout_s: float = 5.0) -> bool:
    """Poll until a TCP connect to (host, port) succeeds or timeout elapses."""
    deadline = _time.monotonic() + timeout_s
    while _time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return True
        except OSError:
            _time.sleep(0.1)
    return False


@pytest.fixture
def mock_backend(tmp_path):
    """Start assignment/mock_backend.py with one calm worker on port 9200, tear down after."""
    repo_root = Path(__file__).resolve().parent.parent
    mb = repo_root / "assignment" / "mock_backend.py"
    if not mb.exists():
        pytest.skip("assignment/mock_backend.py not present")

    proc = subprocess.Popen(
        [sys.executable, str(mb), "--workers", "1", "--base-port", "9200", "--calm"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        cwd=str(repo_root),
    )
    if not _wait_for_port("127.0.0.1", 9200, timeout_s=5.0):
        proc.terminate()
        proc.wait(timeout=5)
        pytest.skip("mock_backend did not bind 127.0.0.1:9200 in time")

    yield "ws://127.0.0.1:9200/v1/ws/speech"

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def test_stress_smoke_5s_at_10rps(mock_backend, tmp_path):
    """End-to-end: stress.py for 5s at 10rps against a calm mock backend.

    Asserts:
      - CSV row count is in [30, 70] (Poisson-loose; spec §6)
      - success rate > 95%
      - <name>.summary.json exists with required structure
      - SUT cross-check is `skipped` (mock_backend has no /metrics) — this
        verifies the skip path rather than the histogram_diff path
    """
    repo_root = Path(__file__).resolve().parent.parent
    out_csv = tmp_path / "smoke.csv"
    summary_path = out_csv.with_suffix(".summary.json")

    rc = subprocess.call([
        sys.executable, "-m", "perf.stress",
        "--url", mock_backend,
        "--rps", "10",
        "--duration", "5",
        "--out", str(out_csv),
    ], cwd=str(repo_root))
    assert rc == 0, "stress.py exited non-zero"

    # CSV row count
    lines = out_csv.read_text().splitlines()
    data_rows = lines[1:]  # skip header
    assert 30 <= len(data_rows) <= 70, f"got {len(data_rows)} rows (expected 30..70 for 5s@10rps Poisson)"

    # Success rate
    successes = sum(1 for line in data_rows if ",success," in line)
    assert successes / max(1, len(data_rows)) > 0.95, "success rate ≤ 95%"

    # Summary JSON
    assert summary_path.exists(), "summary.json was not written"
    summary = json.loads(summary_path.read_text())
    assert summary["run"]["mode"] == "open_loop"
    assert summary["run"]["target_rps"] == 10
    assert summary["counts"]["success"] == successes
    assert summary["quantiles_ms"]["csv"]["p50"] is not None
    assert summary["quantiles_ms"]["sut"]["source"] == "skipped"
    assert summary["quantiles_ms"]["sut"]["reason"] == "metrics_unreachable"
    assert summary["quantiles_ms"]["delta"] is None
```

- [ ] **Step 2: Run the test**

```bash
python -m pytest perf/test_perf.py::test_stress_smoke_5s_at_10rps -v -s
```

Expected: PASS. Takes ~7 seconds (5s run + ~2s for backend startup/teardown).

If `mock_backend.py`'s actual CLI flags differ from `--workers/--base-port/--calm`, adjust the fixture's `subprocess.Popen` argv to match. Read `assignment/mock_backend.py` first to confirm the flag names.

- [ ] **Step 3: Run the full test file to confirm nothing regressed**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add perf/test_perf.py
git commit -m "perf: stress.py smoke test (5s × 10rps against mock_backend)"
```

---

## Task 9: `plot_latency.py` — chart renderer

**Files:**
- Create: `perf/plot_latency.py`
- Modify: `perf/test_perf.py`

**Why this task:** turns the CSV into the four PNGs specified in §3.5: `latency_over_time.png` (5s windows), `latency_cdf.png`, `throughput.png` (1s windows), `success_rate.png` (1s windows). The smoke test feeds it a synthetic CSV and asserts the PNGs exist and are non-empty.

- [ ] **Step 1: Write the failing test**

Append to `perf/test_perf.py`:

```python
def test_plot_latency_smoke(tmp_path):
    """Synthetic 60-row CSV → 4 non-empty PNGs in out_dir."""
    csv_path = tmp_path / "synthetic.csv"
    out_dir = tmp_path / "charts"

    # Build a synthetic CSV: 60 success rows over 6 simulated seconds at ~10 rps.
    header = "ts_unix_ms,req_id,status,error_kind,ttfc_ms,duration_ms,bytes_received,chunk_count\n"
    rows = []
    base = 1_700_000_000_000
    for i in range(60):
        ts = base + i * 100  # 100 ms apart → 10 rps
        ttfc = 50 + (i % 10) * 5  # 50..95 ms
        rows.append(f"{ts},{i},success,,{ttfc:.1f},{ttfc + 100:.1f},4096,32\n")
    csv_path.write_text(header + "".join(rows))

    repo_root = Path(__file__).resolve().parent.parent
    rc = subprocess.call([
        sys.executable, "-m", "perf.plot_latency",
        str(csv_path), "--out-dir", str(out_dir),
    ], cwd=str(repo_root))
    assert rc == 0

    expected = [
        "latency_over_time.png",
        "latency_cdf.png",
        "throughput.png",
        "success_rate.png",
    ]
    for name in expected:
        p = out_dir / name
        assert p.exists(), f"{name} was not produced"
        assert p.stat().st_size > 1000, f"{name} is suspiciously small"
```

- [ ] **Step 2: Run, confirm it fails**

```bash
python -m pytest perf/test_perf.py::test_plot_latency_smoke -v
```

Expected: ImportError or ModuleNotFoundError on `perf.plot_latency`.

- [ ] **Step 3: Implement `plot_latency.py`**

Create `perf/plot_latency.py`:

```python
"""Render four PNG charts from a stress.py CSV.

Charts (see spec §3.5):
  1. latency_over_time.png — TTFC and duration p50/p95/p99 in 5s windows
  2. latency_cdf.png       — TTFC CDF for successful requests, log-x
  3. throughput.png        — achieved/offered/error/dropped/timeout RPS, 1s windows
  4. success_rate.png      — success rate in 1s windows, with 95% reference line
"""
from __future__ import annotations

import argparse
import csv
from pathlib import Path

import matplotlib

matplotlib.use("Agg")  # headless; required for PNG output without a display
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402


_DEFAULT_QUANTILES = (0.50, 0.95, 0.99)
_LATENCY_WINDOW_MS_DEFAULT = 5000
_RATE_WINDOW_MS_DEFAULT = 1000


def _load_csv(path: Path) -> dict[str, np.ndarray]:
    """Load a stress.py CSV into column arrays. Empty floats become NaN."""
    ts, req_id, status, ttfc, dur = [], [], [], [], []
    with path.open() as f:
        for row in csv.DictReader(f):
            ts.append(int(row["ts_unix_ms"]))
            req_id.append(int(row["req_id"]))
            status.append(row["status"])
            ttfc.append(float(row["ttfc_ms"]) if row["ttfc_ms"] else float("nan"))
            dur.append(float(row["duration_ms"]) if row["duration_ms"] else float("nan"))
    if not ts:
        raise ValueError(f"CSV is empty: {path}")
    return {
        "ts": np.array(ts, dtype=np.int64),
        "req_id": np.array(req_id, dtype=np.int64),
        "status": np.array(status, dtype=object),
        "ttfc": np.array(ttfc, dtype=np.float64),
        "dur": np.array(dur, dtype=np.float64),
    }


def _windowed_quantiles(
    ts_ms: np.ndarray, values: np.ndarray, *, window_ms: int, quantiles: tuple[float, ...]
) -> tuple[np.ndarray, dict[float, np.ndarray]]:
    """For each window of size `window_ms`, compute each requested quantile.

    Returns (window_centers_seconds_since_start, {q: array_of_quantile_values}).
    Empty windows yield NaN per spec §5 ("percentile lines have a gap, not a zero").
    """
    if ts_ms.size == 0:
        return np.array([]), {q: np.array([]) for q in quantiles}
    t0 = int(ts_ms.min())
    elapsed = ts_ms - t0
    n_windows = int(elapsed.max() // window_ms) + 1
    centers_s = (np.arange(n_windows) * window_ms + window_ms / 2) / 1000.0
    out: dict[float, np.ndarray] = {q: np.full(n_windows, np.nan) for q in quantiles}
    for i in range(n_windows):
        lo = i * window_ms
        hi = (i + 1) * window_ms
        mask = (elapsed >= lo) & (elapsed < hi) & ~np.isnan(values)
        v = values[mask]
        if v.size == 0:
            continue
        for q in quantiles:
            out[q][i] = float(np.quantile(v, q))
    return centers_s, out


def _windowed_counts_by_status(
    ts_ms: np.ndarray, status: np.ndarray, *, window_ms: int
) -> tuple[np.ndarray, dict[str, np.ndarray]]:
    """Returns (window_centers_s, {status: counts_per_window})."""
    if ts_ms.size == 0:
        return np.array([]), {}
    t0 = int(ts_ms.min())
    elapsed = ts_ms - t0
    n_windows = int(elapsed.max() // window_ms) + 1
    centers_s = (np.arange(n_windows) * window_ms + window_ms / 2) / 1000.0
    counts: dict[str, np.ndarray] = {}
    for st in ("success", "error", "timeout", "dropped_offered"):
        mask = status == st
        win = (elapsed[mask] // window_ms).astype(int)
        counts[st] = np.bincount(win, minlength=n_windows)[:n_windows]
    return centers_s, counts


def plot_latency_over_time(data, out_path: Path, window_ms: int, quantiles):
    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(10, 6), sharex=True)
    success_mask = data["status"] == "success"
    ts_ok = data["ts"][success_mask]
    ttfc_ok = data["ttfc"][success_mask]
    dur_ok = data["dur"][success_mask]

    centers, ttfc_q = _windowed_quantiles(ts_ok, ttfc_ok, window_ms=window_ms, quantiles=quantiles)
    _, dur_q = _windowed_quantiles(ts_ok, dur_ok, window_ms=window_ms, quantiles=quantiles)

    for q in quantiles:
        ax1.plot(centers, ttfc_q[q], label=f"p{int(q*100)}", marker=".", linewidth=1)
        ax2.plot(centers, dur_q[q], label=f"p{int(q*100)}", marker=".", linewidth=1)

    ax1.set_yscale("log")
    ax2.set_yscale("log")
    ax1.set_ylabel("TTFC (ms)")
    ax2.set_ylabel("duration (ms)")
    ax2.set_xlabel("seconds since first request")
    ax1.set_title(f"Latency over time ({window_ms}ms windows)")
    ax1.grid(True, which="both", alpha=0.3)
    ax2.grid(True, which="both", alpha=0.3)
    ax1.legend(loc="upper right")
    ax2.legend(loc="upper right")
    fig.tight_layout()
    fig.savefig(out_path, dpi=120)
    plt.close(fig)


def plot_latency_cdf(data, out_path: Path):
    success_mask = data["status"] == "success"
    ttfc = data["ttfc"][success_mask]
    ttfc = ttfc[~np.isnan(ttfc)]
    fig, ax = plt.subplots(figsize=(8, 5))
    if ttfc.size == 0:
        ax.text(0.5, 0.5, "no successful requests", ha="center", transform=ax.transAxes)
    else:
        x = np.sort(ttfc)
        y = np.arange(1, x.size + 1) / x.size
        ax.plot(x, y, linewidth=1.5)
        ax.set_xscale("log")
        for q in (0.50, 0.95, 0.99, 0.999):
            v = float(np.quantile(ttfc, q))
            ax.axvline(v, linestyle="--", alpha=0.4)
            ax.text(v, q, f" p{q*100:g}={v:.1f}ms", fontsize=8, va="bottom")
    ax.set_xlabel("TTFC (ms, log scale)")
    ax.set_ylabel("cumulative fraction")
    ax.set_title("TTFC CDF (successful requests)")
    ax.grid(True, which="both", alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_path, dpi=120)
    plt.close(fig)


def plot_throughput(data, out_path: Path, window_ms: int):
    centers, counts = _windowed_counts_by_status(data["ts"], data["status"], window_ms=window_ms)
    rate = lambda c: c / (window_ms / 1000.0)
    fig, ax = plt.subplots(figsize=(10, 4.5))
    ax.fill_between(centers, 0, rate(counts.get("success", np.zeros_like(centers))),
                    alpha=0.6, label="success")
    ax.fill_between(centers,
                    rate(counts.get("success", np.zeros_like(centers))),
                    rate(counts.get("success", np.zeros_like(centers))
                         + counts.get("error", np.zeros_like(centers))),
                    alpha=0.6, label="error")
    ax.fill_between(centers,
                    rate(counts.get("success", np.zeros_like(centers))
                         + counts.get("error", np.zeros_like(centers))),
                    rate(counts.get("success", np.zeros_like(centers))
                         + counts.get("error", np.zeros_like(centers))
                         + counts.get("timeout", np.zeros_like(centers))),
                    alpha=0.6, label="timeout")
    ax.fill_between(centers,
                    rate(counts.get("success", np.zeros_like(centers))
                         + counts.get("error", np.zeros_like(centers))
                         + counts.get("timeout", np.zeros_like(centers))),
                    rate(counts.get("success", np.zeros_like(centers))
                         + counts.get("error", np.zeros_like(centers))
                         + counts.get("timeout", np.zeros_like(centers))
                         + counts.get("dropped_offered", np.zeros_like(centers))),
                    alpha=0.6, label="dropped_offered")
    ax.set_xlabel("seconds since first request")
    ax.set_ylabel("RPS")
    ax.set_title(f"Throughput ({window_ms}ms windows)")
    ax.legend(loc="upper right")
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_path, dpi=120)
    plt.close(fig)


def plot_success_rate(data, out_path: Path, window_ms: int):
    centers, counts = _windowed_counts_by_status(data["ts"], data["status"], window_ms=window_ms)
    succ = counts.get("success", np.zeros_like(centers))
    total = sum(counts.get(s, np.zeros_like(centers))
                for s in ("success", "error", "timeout", "dropped_offered"))
    rate = np.full(centers.shape, np.nan, dtype=np.float64)
    nonzero = total > 0
    rate[nonzero] = succ[nonzero] / total[nonzero]
    fig, ax = plt.subplots(figsize=(10, 4))
    ax.plot(centers, rate, linewidth=1.5)
    ax.axhline(0.95, linestyle="--", alpha=0.5, color="red", label="95% reference")
    ax.set_ylim(0, 1.05)
    ax.set_xlabel("seconds since first request")
    ax.set_ylabel("success rate")
    ax.set_title(f"Success rate ({window_ms}ms windows)")
    ax.legend(loc="lower right")
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_path, dpi=120)
    plt.close(fig)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description="Render perf charts from a stress.py CSV")
    p.add_argument("csv", type=Path)
    p.add_argument("--out-dir", type=Path, required=True)
    p.add_argument("--window-ms", type=int, default=_LATENCY_WINDOW_MS_DEFAULT,
                   help="Window size for latency_over_time (default 5000ms)")
    p.add_argument("--rate-window-ms", type=int, default=_RATE_WINDOW_MS_DEFAULT,
                   help="Window size for throughput/success_rate (default 1000ms)")
    p.add_argument("--quantiles", default="50,95,99",
                   help="Comma-separated quantiles for latency_over_time (default 50,95,99)")
    args = p.parse_args(argv)
    args.out_dir.mkdir(parents=True, exist_ok=True)

    data = _load_csv(args.csv)
    quantiles = tuple(int(x) / 100.0 for x in args.quantiles.split(","))

    plot_latency_over_time(data, args.out_dir / "latency_over_time.png",
                           window_ms=args.window_ms, quantiles=quantiles)
    plot_latency_cdf(data, args.out_dir / "latency_cdf.png")
    plot_throughput(data, args.out_dir / "throughput.png", window_ms=args.rate_window_ms)
    plot_success_rate(data, args.out_dir / "success_rate.png", window_ms=args.rate_window_ms)
    print(f"wrote 4 charts to {args.out_dir}")
    return 0


if __name__ == "__main__":
    import sys as _sys
    _sys.exit(main())
```

- [ ] **Step 4: Run the smoke test**

```bash
python -m pytest perf/test_perf.py::test_plot_latency_smoke -v
```

Expected: PASS.

- [ ] **Step 5: Run the full test file**

```bash
python -m pytest perf/test_perf.py -v
```

Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add perf/plot_latency.py perf/test_perf.py
git commit -m "perf: plot_latency.py — four PNG charts (latency, CDF, throughput, success rate)"
```

---

## Task 10: `profile.sh` — samply wrapper

**Files:**
- Create: `perf/profile.sh`

**Why this task:** the convenience wrapper around `samply record`/`load` that the spec §3.6 defines. Manual smoke test only (per spec §6 task list).

- [ ] **Step 1: Create the script**

Create `perf/profile.sh`:

```bash
#!/usr/bin/env bash
# profile.sh — samply wrapper for the perf harness (see spec §3.6)
#
# Usage:
#   ./perf/profile.sh record <duration_s> <name>
#   ./perf/profile.sh view <name>
#
# `record` attaches samply to the running tts-multiplexer and records for
# <duration_s> seconds. `view` opens a previously recorded profile in Firefox
# profiler.

set -euo pipefail

cmd="${1:-}"
shift || true

case "$cmd" in
  record)
    duration="${1:?usage: profile.sh record <duration_s> <name>}"
    name="${2:?usage: profile.sh record <duration_s> <name>}"

    if ! command -v samply >/dev/null 2>&1; then
      echo "samply not on PATH. Install with: cargo install samply" >&2
      exit 2
    fi

    pid=$(pgrep -f target/release-with-debug/tts-multiplexer || true)
    if [ -z "$pid" ]; then
      echo "tts-multiplexer not running (release-with-debug build)." >&2
      echo "Start it with:" >&2
      echo "  cargo build --profile release-with-debug" >&2
      echo "  ./target/release-with-debug/tts-multiplexer --port 9000 --metrics-port 9001 --backends 127.0.0.1:9100,...,127.0.0.1:9107" >&2
      exit 1
    fi
    if [ "$(echo "$pid" | wc -l | tr -d ' ')" != "1" ]; then
      echo "Multiple tts-multiplexer PIDs found:" >&2
      echo "$pid" >&2
      echo "Kill the spurious ones and retry, or pass --pid manually." >&2
      exit 1
    fi

    mkdir -p perf/runs
    out="perf/runs/${name}-profile.json.gz"
    echo "Recording PID $pid for ${duration}s into ${out}..."
    samply record --pid "$pid" --duration "$duration" -o "$out" --no-open
    echo "Done. View with: $0 view $name"
    ;;

  view)
    name="${1:?usage: profile.sh view <name>}"
    out="perf/runs/${name}-profile.json.gz"
    if [ ! -f "$out" ]; then
      echo "No profile at $out" >&2
      exit 1
    fi
    samply load "$out"
    ;;

  *)
    echo "Usage: $0 {record <duration_s> <name>|view <name>}" >&2
    exit 2
    ;;
esac
```

- [ ] **Step 2: Make it executable and verify it parses**

```bash
chmod +x perf/profile.sh
bash -n perf/profile.sh
./perf/profile.sh
```

Expected: `bash -n` exits 0 (script is syntactically valid). Bare invocation prints the usage line to stderr and exits 2.

- [ ] **Step 3: Commit**

```bash
git add perf/profile.sh
git commit -m "perf: profile.sh — samply record/view wrapper"
```

---

## Task 11: `perf/README.md` — quick-start

**Files:**
- Create: `perf/README.md`

**Why this task:** documents the canonical recipes (calm vs chaos baselines per spec §8 open question) and the order-of-operations for a profile-while-stressing run. The spec is the design; the README is the runbook.

- [ ] **Step 1: Write the README**

Create `perf/README.md`:

```markdown
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
  pip install psutil  # optional, enables SUT-CPU reporting
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
```

- [ ] **Step 2: Commit**

```bash
git add perf/README.md
git commit -m "perf: README quick-start and recipes"
```

---

## Final verification

- [ ] **Step 1: Run the full test suite**

```bash
python -m pytest perf/ -v
```

Expected: all unit tests + the latency-plot smoke test + the stress.py smoke test pass.

- [ ] **Step 2: End-to-end manual run**

In one shell, start mock_backend:
```bash
python assignment/mock_backend.py --workers 1 --base-port 9200 --calm
```

In another, run stress.py against it:
```bash
python -m perf.stress \
  --url ws://127.0.0.1:9200/v1/ws/speech \
  --rps 20 --duration 10 \
  --out perf/runs/manual-test.csv
```

Expected: prints a summary table, writes `perf/runs/manual-test.csv` and `perf/runs/manual-test.summary.json`. SUT cross-check shows `skipped` (mock_backend has no /metrics).

Then render charts:
```bash
python -m perf.plot_latency perf/runs/manual-test.csv --out-dir perf/runs/charts/manual-test
ls perf/runs/charts/manual-test/
```

Expected: four PNGs.

- [ ] **Step 3: Confirm git log**

```bash
git log --oneline profilling ^main | head -20
```

Expected: 11 commits, one per task, in order. Working tree clean.

- [ ] **Step 4: (Optional) Phase 1 wrap-up**

The plan ends here. Phase 2 (Rust load gen for absolute-ceiling work) gets a separate spec when the cross-check delta growth crosses the §7 trigger.

---

## Plan self-review (writer's check, not user-facing)

**Spec coverage check:**
- §3.1 file layout — Task 1 (skeleton, .gitignore)
- §3.2 component boundaries — Tasks 5-7 (stress.py), 9 (plot), 10 (profile.sh)
- §3.3 stress generator + Poisson + contamination guards — Tasks 6 (Poisson, run_request), 7 (uvloop, /metrics scrape, self-CPU)
- §3.4 CSV schema — Task 5 (CsvRowWriter.HEADER)
- §3.4.1 summary JSON schema — Task 7 (_build_summary)
- §3.5 chart renderer + 5s/1s windows — Task 9
- §3.6 samply + Cargo profile — Tasks 1 (Cargo profile), 10 (profile.sh)
- §3.7 don't-change list — implicit (no tasks add deps or endpoints)
- §4 data flow — informational, no task
- §5 error handling — Task 5 (line-buffered + signal handler), Task 6 (_classify_error)
- §6 testing — Task 8 (stress smoke), Task 9 (plot smoke), Task 10 step 2 (profile.sh syntax)
- §7 phasing — informational; this plan IS Phase 1
- §8 open questions — README documents calm vs chaos recipes

**Type-consistency check:** `Histogram` is the same dataclass throughout Tasks 2-4 and Task 7. `RunCounts` field names match between Task 7's dataclass and Task 7's summary builder. `CsvRowWriter.HEADER` field names match `build_csv_row`/`build_dropped_row` keys.

**Placeholder scan:** every code block contains real code. No "TBD" / "implement later" markers.
