"""Open-loop WebSocket stress generator with per-run contamination guards.

See `docs/superpowers/specs/2026-05-06-perf-harness-design.md` for design.
This file is the CLI entrypoint; pure histogram math lives in `metrics_diff.py`.
"""
from __future__ import annotations

import argparse
import asyncio
import csv
import json
import os
import random
import signal
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterator

from perf.metrics_diff import (
    CounterResetError,
    Histogram,
    diff_histograms,
    parse_histogram,
    quantile_from_buckets,
)


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
        # The stress tool measures throughput/latency only — it does not validate
        # per-stream binary framing (a correctness concern).  Passing False here
        # lets stress.py target both the multiplexer and a raw mock_backend
        # without stream-ID-prefix parsing errors.
        expect_stream_tags=False,
    )
    csv_writer.write_row(build_csv_row(req_id, ts, result))


# ---------------------------------------------------------------------------
# Orchestration (Task 7)
# ---------------------------------------------------------------------------

_METRIC_NAME = "mux_ttfc_seconds"


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
