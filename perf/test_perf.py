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
