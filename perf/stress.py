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
