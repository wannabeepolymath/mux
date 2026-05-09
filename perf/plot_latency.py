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

    # Spec §3.5: shade windows where success rate < 95% on both subplots.
    all_centers, status_counts = _windowed_counts_by_status(data["ts"], data["status"], window_ms=window_ms)
    if all_centers.size > 0:
        succ = status_counts.get("success", np.zeros_like(all_centers))
        total = sum(status_counts.get(s, np.zeros_like(all_centers))
                    for s in ("success", "error", "timeout", "dropped_offered"))
        half_w_s = (window_ms / 2) / 1000.0
        for i, c in enumerate(all_centers):
            if total[i] > 0 and (succ[i] / total[i]) < 0.95:
                ax1.axvspan(c - half_w_s, c + half_w_s, alpha=0.15, color="red")
                ax2.axvspan(c - half_w_s, c + half_w_s, alpha=0.15, color="red")

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
