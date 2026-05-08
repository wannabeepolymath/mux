"""Open-loop WebSocket stress generator with per-run contamination guards.

See `docs/superpowers/specs/2026-05-06-perf-harness-design.md` for design.
This file is the CLI entrypoint; pure histogram math lives in `metrics_diff.py`.
"""
from __future__ import annotations

import csv
import random
import signal
import sys
import threading
import time
from pathlib import Path
from typing import Any, Iterator


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
    )
    csv_writer.write_row(build_csv_row(req_id, ts, result))
