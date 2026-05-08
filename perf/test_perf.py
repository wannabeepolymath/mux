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
    assert row["chunk_count"] == 0


# ---------------------------------------------------------------------------
# Integration smoke test: stress.py end-to-end against mock_backend
# ---------------------------------------------------------------------------

import json
import socket
import subprocess
import sys
import time as _time
from pathlib import Path


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


def _find_python_with_deps(repo_root: Path, *modules: str) -> str:
    """Return a Python interpreter that has all requested modules installed.

    Preference order:
      1. sys.executable (the interpreter running pytest) if all modules are available.
      2. <repo_root>/.venv313/bin/python if it exists and has all modules.
      3. <repo_root>/.venv/bin/python if it exists and has all modules.
      4. Fall back to sys.executable (subprocess will fail, skip will catch it).

    Parameters
    ----------
    repo_root:
        Root of the repository.
    *modules:
        Module names that must be importable (e.g. ``"websockets"``,
        ``"matplotlib"``, ``"numpy"``).
    """
    import importlib.util

    def _has_all(py: str) -> bool:
        check = "; ".join(f"import {m}" for m in modules)
        ret = subprocess.call(
            [py, "-c", check],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        return ret == 0

    if all(importlib.util.find_spec(m) is not None for m in modules):
        return sys.executable

    for venv_name in (".venv313", ".venv"):
        venv_py = repo_root / venv_name / "bin" / "python"
        if venv_py.exists() and _has_all(str(venv_py)):
            return str(venv_py)

    return sys.executable


# Back-compat alias used by mock_backend fixture below.
def _find_python_with_websockets(repo_root: Path) -> str:
    return _find_python_with_deps(repo_root, "websockets")


@pytest.fixture
def mock_backend(tmp_path):
    """Start assignment/mock_backend.py with one calm worker on port 9200, tear down after."""
    repo_root = Path(__file__).resolve().parent.parent
    mb = repo_root / "assignment" / "mock_backend.py"
    if not mb.exists():
        pytest.skip("assignment/mock_backend.py not present")

    py = _find_python_with_websockets(repo_root)
    proc = subprocess.Popen(
        [
            py, str(mb),
            "--workers", "1", "--base-port", "9200", "--calm",
            # Fast response so a single worker can sustain 10 RPS:
            # 0.1 ms delays make service time ~5 ms (dominated by asyncio/TCP
            # round-trip overhead), leaving sufficient headroom for Poisson bursts.
            "--first-chunk-delay", "0.0001",
            "--chunk-interval", "0.0001",
            "--no-jitter",
        ],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        cwd=str(repo_root),
    )
    if not _wait_for_port("127.0.0.1", 9200, timeout_s=5.0):
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
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

    py = _find_python_with_websockets(repo_root)
    rc = subprocess.call([
        py, "-m", "perf.stress",
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


# ---------------------------------------------------------------------------
# plot_latency.py smoke test
# ---------------------------------------------------------------------------


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
    py = _find_python_with_deps(repo_root, "matplotlib", "numpy")
    rc = subprocess.call([
        py, "-m", "perf.plot_latency",
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
