#!/usr/bin/env python3
"""
Mock TTS backend with chaos engineering built in.

Each worker processes ONE request at a time, just like the real GPU workers.
By default, workers misbehave: crash mid-stream, hang, send garbage, refuse
connections, and run at variable speeds. This is the environment the
multiplexer must survive.

Usage:
    # Default: chaos mode (crashes, hangs, malformed data, slow backends)
    python mock_backend.py --workers 8 --base-port 9100

    # Calm mode for initial development
    python mock_backend.py --workers 8 --base-port 9100 --calm

    # Custom chaos rates
    python mock_backend.py --workers 8 --base-port 9100 --crash-rate 0.15 --hang-rate 0.10

    # Heterogeneous backends (some fast, some slow)
    python mock_backend.py --workers 8 --base-port 9100 --heterogeneous
"""

import argparse
import asyncio
import json
import logging
import os
import signal
import struct
import sys
import time
import random
from typing import Optional
from enum import Enum

try:
    import websockets
    from websockets.asyncio.server import serve
except ImportError:
    print("Install websockets: pip install websockets>=13.0")
    sys.exit(1)


def _configure_websocket_logging(*, log_handshake_failures: bool) -> None:
    """websockets logs ERROR + traceback when a peer drops during the HTTP upgrade.

    That is normal noise (HTTP probes, wrong client, IDE port checks). Suppress by default.
    """
    if log_handshake_failures:
        return

    class _DropOpeningHandshakeSpam(logging.Filter):
        def filter(self, record: logging.LogRecord) -> bool:
            return record.getMessage() != "opening handshake failed"

    logging.getLogger("websockets.server").addFilter(_DropOpeningHandshakeSpam())


SAMPLE_RATE = 24000
SAMPLE_WIDTH = 2  # int16
SAMPLES_PER_FRAME = 1920  # 24000 Hz / 12.5 fps
BYTES_PER_FRAME = SAMPLES_PER_FRAME * SAMPLE_WIDTH


class ChaosMode(Enum):
    NORMAL = "normal"
    CRASH_MID_STREAM = "crash"
    HANG = "hang"
    MALFORMED = "malformed"
    SLOW = "slow"
    REFUSE = "refuse"


class WorkerStats:
    def __init__(self, worker_id: int, port: int):
        self.worker_id = worker_id
        self.port = port
        self.total_requests = 0
        self.total_chaos_events = 0
        self.active = False
        self.speed_multiplier = 1.0  # for heterogeneous mode
        self.refusing = False  # for connection-refuse chaos


def generate_fake_pcm(num_frames: int) -> bytes:
    num_samples = num_frames * SAMPLES_PER_FRAME
    return os.urandom(num_samples * SAMPLE_WIDTH)


def pick_chaos(config: dict) -> ChaosMode:
    roll = random.random()
    thresholds = [
        (config["crash_rate"], ChaosMode.CRASH_MID_STREAM),
        (config["hang_rate"], ChaosMode.HANG),
        (config["malformed_rate"], ChaosMode.MALFORMED),
        (config["slow_rate"], ChaosMode.SLOW),
        (config["refuse_rate"], ChaosMode.REFUSE),
    ]
    cumulative = 0.0
    for threshold, mode in thresholds:
        cumulative += threshold
        if roll < cumulative:
            return mode
    return ChaosMode.NORMAL


async def handle_connection(ws, worker: WorkerStats, config: dict):
    if worker.active:
        await ws.send(json.dumps({
            "type": "error",
            "message": "Worker busy — one request at a time",
        }))
        await ws.close()
        return

    chaos = pick_chaos(config)

    # REFUSE: accept connection but immediately close with error
    if chaos == ChaosMode.REFUSE:
        worker.total_chaos_events += 1
        await ws.send(json.dumps({
            "type": "error",
            "message": "Worker unavailable (restarting)",
        }))
        await ws.close()
        return

    try:
        raw = await asyncio.wait_for(ws.recv(), timeout=30.0)
        msg = json.loads(raw)

        if msg.get("type") == "close":
            return

        if msg.get("type") != "start":
            await ws.send(json.dumps({
                "type": "error",
                "message": f"Expected 'start', got '{msg.get('type')}'",
            }))
            return

        text = msg.get("text", "")
        if not text:
            await ws.send(json.dumps({
                "type": "error",
                "message": "Missing 'text' field",
            }))
            return

        worker.active = True
        worker.total_requests += 1
        start_time = time.monotonic()

        num_chunks = max(2, min(12, len(text) // 10))
        frames_per_chunk = 20

        base_first_delay = config["first_chunk_delay"]
        base_interval = config["chunk_interval"]

        # Apply per-worker speed multiplier (heterogeneous mode)
        first_chunk_delay = base_first_delay * worker.speed_multiplier
        chunk_interval = base_interval * worker.speed_multiplier

        # Add jitter
        if config["jitter"]:
            first_chunk_delay *= random.uniform(0.8, 1.3)
            chunk_interval *= random.uniform(0.8, 1.3)

        # ── CHAOS: HANG ──────────────────────────────────────────────
        if chaos == ChaosMode.HANG:
            worker.total_chaos_events += 1
            # Send queued ack to look normal, then go silent
            await ws.send(json.dumps({"type": "queued", "queue_depth": 0}))
            # Hang for a very long time (multiplexer must timeout)
            await asyncio.sleep(300)
            return

        # ── CHAOS: MALFORMED ─────────────────────────────────────────
        if chaos == ChaosMode.MALFORMED:
            worker.total_chaos_events += 1
            malform_type = random.choice(["bad_json", "wrong_type", "binary_instead", "partial"])
            if malform_type == "bad_json":
                await ws.send("{invalid json [[[]")
            elif malform_type == "wrong_type":
                await ws.send(json.dumps({"type": "unexpected_galaxy", "data": 42}))
            elif malform_type == "binary_instead":
                # Send binary when text is expected
                await ws.send(os.urandom(64))
            elif malform_type == "partial":
                # Send queued, then send garbage binary, then close
                await ws.send(json.dumps({"type": "queued", "queue_depth": 0}))
                await asyncio.sleep(0.1)
                await ws.send(os.urandom(13))  # weird size, not valid PCM
                await ws.close()
            return

        # ── NORMAL / SLOW / CRASH path ───────────────────────────────
        await ws.send(json.dumps({"type": "queued", "queue_depth": 0}))

        # SLOW: multiply all delays by 3-5x
        if chaos == ChaosMode.SLOW:
            worker.total_chaos_events += 1
            slow_factor = random.uniform(3.0, 5.0)
            first_chunk_delay *= slow_factor
            chunk_interval *= slow_factor

        await asyncio.sleep(first_chunk_delay)

        total_bytes = 0
        # CRASH: pick a random chunk to die on
        crash_at = random.randint(1, max(1, num_chunks - 1)) if chaos == ChaosMode.CRASH_MID_STREAM else -1

        for i in range(num_chunks):
            if chaos == ChaosMode.CRASH_MID_STREAM and i == crash_at:
                worker.total_chaos_events += 1
                # Abruptly close — no done message, no warning
                await ws.close()
                return

            pcm_data = generate_fake_pcm(frames_per_chunk)
            await ws.send(pcm_data)
            total_bytes += len(pcm_data)

            if i < num_chunks - 1:
                await asyncio.sleep(chunk_interval)

        elapsed = time.monotonic() - start_time
        audio_duration = (total_bytes / SAMPLE_WIDTH) / SAMPLE_RATE

        await ws.send(json.dumps({
            "type": "done",
            "audio_duration": round(audio_duration, 2),
            "total_time": round(elapsed, 2),
            "rtf": round(audio_duration / elapsed if elapsed > 0 else 0, 2),
            "total_bytes": total_bytes,
        }))

    except asyncio.TimeoutError:
        pass
    except websockets.exceptions.ConnectionClosed:
        pass
    except Exception as e:
        try:
            await ws.send(json.dumps({"type": "error", "message": str(e)}))
        except Exception:
            pass
    finally:
        worker.active = False


async def run_worker(worker: WorkerStats, config: dict, shutdown_event: asyncio.Event):
    async def handler(ws):
        # Connection-level refuse: randomly reject at TCP accept time
        if worker.refusing:
            await ws.close()
            return
        await handle_connection(ws, worker, config)

    async with serve(handler, "0.0.0.0", worker.port):
        print(f"  Worker {worker.worker_id} listening on :{worker.port}"
              f" (speed={worker.speed_multiplier:.1f}x)")
        await shutdown_event.wait()


async def refuse_toggler(workers: list[WorkerStats], config: dict, shutdown_event: asyncio.Event):
    """Periodically toggle connection-refuse on random workers (simulates restarts)."""
    if config["refuse_rate"] <= 0:
        return
    while not shutdown_event.is_set():
        await asyncio.sleep(random.uniform(5.0, 15.0))
        if shutdown_event.is_set():
            break
        # Pick a random worker to toggle refuse
        w = random.choice(workers)
        if not w.refusing:
            w.refusing = True
            duration = random.uniform(1.0, 5.0)
            asyncio.get_event_loop().call_later(duration, setattr, w, "refusing", False)


async def health_server(workers: list[WorkerStats], port: int, shutdown_event: asyncio.Event):
    import http.server
    import threading

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            total_chaos = sum(w.total_chaos_events for w in workers)
            total_reqs = sum(w.total_requests for w in workers)
            status = {
                "workers": [
                    {
                        "id": w.worker_id,
                        "port": w.port,
                        "active": w.active,
                        "total_requests": w.total_requests,
                        "chaos_events": w.total_chaos_events,
                        "speed_multiplier": w.speed_multiplier,
                        "refusing": w.refusing,
                    }
                    for w in workers
                ],
                "totals": {
                    "requests": total_reqs,
                    "chaos_events": total_chaos,
                    "chaos_rate_actual": round(total_chaos / total_reqs, 3) if total_reqs > 0 else 0,
                },
            }
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(status, indent=2).encode())

        def log_message(self, *args):
            pass

    server = http.server.HTTPServer(("0.0.0.0", port), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    print(f"  Health endpoint on :{port}")
    await shutdown_event.wait()
    server.shutdown()


async def main():
    parser = argparse.ArgumentParser(
        description="Mock TTS backend with chaos engineering",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Chaos modes (all active by default):
  crash     Backend drops TCP mid-stream (no done message)
  hang      Backend goes silent after accepting request
  malformed Backend sends invalid JSON or unexpected data
  slow      Backend takes 3-5x longer per chunk
  refuse    Backend refuses connections (simulates restart)

Use --calm to disable all chaos for initial development.
        """,
    )
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--base-port", type=int, default=9100)
    parser.add_argument("--health-port", type=int, default=9099)
    parser.add_argument(
        "--log-ws-handshakes",
        action="store_true",
        help="Log failed WebSocket opening handshakes (verbose; usually peer aborted upgrade)",
    )
    parser.add_argument("--first-chunk-delay", type=float, default=0.3)
    parser.add_argument("--chunk-interval", type=float, default=0.25)

    # Chaos rates (sum should be < 1.0, remainder is normal)
    parser.add_argument("--crash-rate", type=float, default=0.08)
    parser.add_argument("--hang-rate", type=float, default=0.05)
    parser.add_argument("--malformed-rate", type=float, default=0.03)
    parser.add_argument("--slow-rate", type=float, default=0.15)
    parser.add_argument("--refuse-rate", type=float, default=0.05)

    # Convenience flags
    parser.add_argument("--calm", action="store_true",
                        help="Disable all chaos (clean backends)")
    parser.add_argument("--heterogeneous", action="store_true",
                        help="Give each backend a different speed (1x-3x)")
    parser.add_argument("--jitter", action="store_true", default=True,
                        help="Add timing jitter (default: on)")
    parser.add_argument("--no-jitter", action="store_true",
                        help="Disable timing jitter")

    args = parser.parse_args()

    if args.calm:
        args.crash_rate = 0.0
        args.hang_rate = 0.0
        args.malformed_rate = 0.0
        args.slow_rate = 0.0
        args.refuse_rate = 0.0

    if args.no_jitter:
        args.jitter = False

    config = {
        "first_chunk_delay": args.first_chunk_delay,
        "chunk_interval": args.chunk_interval,
        "jitter": args.jitter,
        "crash_rate": args.crash_rate,
        "hang_rate": args.hang_rate,
        "malformed_rate": args.malformed_rate,
        "slow_rate": args.slow_rate,
        "refuse_rate": args.refuse_rate,
    }

    workers = [
        WorkerStats(worker_id=i, port=args.base_port + i)
        for i in range(args.workers)
    ]

    # Heterogeneous mode: some workers are naturally slower
    if args.heterogeneous:
        for w in workers:
            w.speed_multiplier = random.uniform(1.0, 3.0)

    shutdown_event = asyncio.Event()

    _configure_websocket_logging(log_handshake_failures=args.log_ws_handshakes)

    def signal_handler():
        print("\nShutting down...")
        shutdown_event.set()

    loop = asyncio.get_event_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, signal_handler)

    chaos_total = sum([
        args.crash_rate, args.hang_rate, args.malformed_rate,
        args.slow_rate, args.refuse_rate,
    ])

    print(f"Starting {args.workers} mock TTS workers:")
    print(f"  Ports: {args.base_port}-{args.base_port + args.workers - 1}")
    print(f"  First chunk delay: {args.first_chunk_delay}s")
    print(f"  Chunk interval: {args.chunk_interval}s")
    if args.calm:
        print(f"  Chaos: OFF (calm mode)")
    else:
        print(f"  Chaos: {chaos_total*100:.0f}% of requests will fail")
        print(f"    crash={args.crash_rate*100:.0f}%  hang={args.hang_rate*100:.0f}%"
              f"  malformed={args.malformed_rate*100:.0f}%  slow={args.slow_rate*100:.0f}%"
              f"  refuse={args.refuse_rate*100:.0f}%")
    if args.heterogeneous:
        print(f"  Heterogeneous: ON (speed varies 1-3x per worker)")
    print()

    tasks = [asyncio.create_task(run_worker(w, config, shutdown_event)) for w in workers]
    tasks.append(asyncio.create_task(health_server(workers, args.health_port, shutdown_event)))
    tasks.append(asyncio.create_task(refuse_toggler(workers, config, shutdown_event)))

    await asyncio.gather(*tasks)
    print("All workers stopped.")


if __name__ == "__main__":
    asyncio.run(main())
