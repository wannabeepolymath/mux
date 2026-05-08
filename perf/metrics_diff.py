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

    # In rare cases _count line is missing; fall back to +Inf bucket
    # (always equals total count in Prometheus histograms).
    if total_count is None:
        total_count = bucket_totals.get(math.inf, max(bucket_totals.values()))
    if total_sum is None:
        total_sum = 0.0

    buckets = sorted(bucket_totals.items())
    return Histogram(buckets=buckets, count=total_count, sum=total_sum)


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
