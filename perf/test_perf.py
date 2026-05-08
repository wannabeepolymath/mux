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
