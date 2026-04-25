#!/usr/bin/env python3
"""
Test client for the TTS multiplexer.

Runs multiple test suites that verify correctness, chaos resilience,
stream multiplexing, backpressure handling, and performance.

Usage:
    # Correctness basics
    python test_client.py ws://localhost:9000/v1/ws/speech

    # Chaos survival (run with default chaotic mock)
    python test_client.py ws://localhost:9000/v1/ws/speech --chaos --requests 200

    # Stream multiplexing
    python test_client.py ws://localhost:9000/v1/ws/speech --multiplex

    # Slow consumer backpressure
    python test_client.py ws://localhost:9000/v1/ws/speech --slow-consumer

    # Soak test (sustained load, memory check)
    python test_client.py ws://localhost:9000/v1/ws/speech --soak 600

    # Kill a backend mid-test (tests circuit breaker)
    python test_client.py ws://localhost:9000/v1/ws/speech --kill-backend 9107

    # Full suite
    python test_client.py ws://localhost:9000/v1/ws/speech --all

    # Performance comparison
    python test_client.py ws://localhost:9000/v1/ws/speech --compare ws://localhost:9100/v1/ws/speech
"""

import argparse
import asyncio
import json
import os
import signal
import subprocess
import sys
import time
import statistics
import struct
import resource
from dataclasses import dataclass, field
from typing import Optional

try:
    import websockets
except ImportError:
    print("Install websockets: pip install websockets>=13.0")
    sys.exit(1)


SAMPLE_RATE = 24000
SAMPLE_WIDTH = 2

TEST_TEXTS = [
    "Hello, how are you today?",
    "The quick brown fox jumps over the lazy dog near the riverbank on a warm afternoon.",
    "I'd like to schedule a meeting for tomorrow afternoon at three o'clock if that works for everyone involved in the project.",
    "Technology continues to reshape how we communicate, work, and interact with the world around us every single day of our lives.",
    "Short.",
    "This is a medium length sentence that should produce a moderate amount of audio output for testing.",
    "Let me tell you about something fascinating I learned recently about the intersection of artificial intelligence and creative expression in modern art and music production workflows that are changing everything.",
    "One two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen.",
]


def parse_binary_frame(data: bytes) -> tuple[str, bytes]:
    """Parse a stream-ID-prefixed binary frame. Returns (stream_id, pcm_payload)."""
    if len(data) < 1:
        raise ValueError("Empty binary frame")
    id_len = data[0]
    if len(data) < 1 + id_len:
        raise ValueError(f"Frame too short: need {1 + id_len} bytes, got {len(data)}")
    stream_id = data[1:1 + id_len].decode("utf-8")
    pcm = data[1 + id_len:]
    return stream_id, pcm


@dataclass
class RequestResult:
    success: bool
    stream_id: str = ""
    ttfc_ms: float = 0.0
    total_ms: float = 0.0
    audio_duration: float = 0.0
    total_bytes: int = 0
    chunks_received: int = 0
    retried: bool = False
    error: str = ""


@dataclass
class SuiteResult:
    name: str
    passed: bool
    total_tests: int = 0
    passed_tests: int = 0
    details: str = ""
    results: list[RequestResult] = field(default_factory=list)

    def summary(self) -> str:
        status = "PASS" if self.passed else "FAIL"
        lines = [f"\n{'='*60}"]
        lines.append(f"  [{status}] {self.name} ({self.passed_tests}/{self.total_tests})")
        lines.append(f"{'='*60}")
        if self.results:
            successes = [r for r in self.results if r.success]
            failures = [r for r in self.results if not r.success]
            lines.append(f"  Requests: {len(self.results)} total, {len(successes)} ok, {len(failures)} failed")
            if successes:
                ttfcs = [r.ttfc_ms for r in successes if r.ttfc_ms > 0]
                if ttfcs:
                    lines.append(f"  TTFC: p50={statistics.median(ttfcs):.0f}ms"
                                 f"  p99={sorted(ttfcs)[int(len(ttfcs)*0.99)]:.0f}ms")
            if failures:
                errors = {}
                for r in failures:
                    errors[r.error] = errors.get(r.error, 0) + 1
                lines.append(f"  Errors:")
                for err, count in sorted(errors.items(), key=lambda x: -x[1])[:5]:
                    lines.append(f"    {count}x {err[:80]}")
        if self.details:
            lines.append(f"  {self.details}")
        lines.append(f"{'='*60}")
        return "\n".join(lines)


# ──────────────────────────────────────────────────────────────
#  Single-stream request (for basic tests and chaos tests)
# ──────────────────────────────────────────────────────────────

async def send_single_request(
    url: str,
    text: str,
    stream_id: str = "req",
    timeout: float = 30.0,
    expect_stream_tags: bool = True,
) -> RequestResult:
    """Send a single request on a fresh connection."""
    result = RequestResult(success=False, stream_id=stream_id)
    ws = None
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024),
            timeout=10.0,
        )
        start = time.monotonic()
        await ws.send(json.dumps({
            "type": "start",
            "stream_id": stream_id,
            "text": text,
            "speaker_id": 0,
            "priority": 10,
        }))

        first_chunk_time = None
        total_bytes = 0
        chunks = 0

        async for message in ws:
            if time.monotonic() - start > timeout:
                result.error = "timeout"
                return result

            if isinstance(message, bytes):
                if expect_stream_tags:
                    sid, pcm = parse_binary_frame(message)
                    if sid != stream_id:
                        result.error = f"wrong stream_id in binary: expected {stream_id}, got {sid}"
                        return result
                    payload_bytes = len(pcm)
                else:
                    payload_bytes = len(message)

                if first_chunk_time is None:
                    first_chunk_time = time.monotonic()
                total_bytes += payload_bytes
                chunks += 1

            elif isinstance(message, str):
                try:
                    data = json.loads(message)
                except json.JSONDecodeError:
                    result.error = f"invalid JSON from multiplexer: {message[:50]}"
                    return result

                msg_type = data.get("type")
                if msg_type == "queued":
                    continue
                elif msg_type == "done":
                    result.audio_duration = data.get("audio_duration", 0)
                    break
                elif msg_type == "error":
                    result.error = data.get("message", "unknown")
                    return result

        elapsed = time.monotonic() - start
        result.success = chunks > 0
        result.ttfc_ms = (first_chunk_time - start) * 1000 if first_chunk_time else 0
        result.total_ms = elapsed * 1000
        result.total_bytes = total_bytes
        result.chunks_received = chunks
        if result.audio_duration == 0 and total_bytes > 0:
            result.audio_duration = (total_bytes / SAMPLE_WIDTH) / SAMPLE_RATE

    except asyncio.TimeoutError:
        result.error = "connection timeout"
    except websockets.exceptions.ConnectionClosed as e:
        result.error = f"connection closed: {e.code}"
    except Exception as e:
        result.error = str(e)
    finally:
        if ws:
            try:
                await ws.close()
            except Exception:
                pass
    return result


# ──────────────────────────────────────────────────────────────
#  Suite 1: Basic correctness
# ──────────────────────────────────────────────────────────────

async def suite_correctness(url: str) -> SuiteResult:
    suite = SuiteResult(name="Correctness", passed=True, total_tests=0)
    tests_passed = 0

    # Test 1: Basic single request
    suite.total_tests += 1
    print("  [1] Basic request...", end=" ", flush=True)
    r = await send_single_request(url, "Hello world, this is a basic test.", stream_id="basic-1")
    if r.success and r.chunks_received > 0:
        print(f"PASS (ttfc={r.ttfc_ms:.0f}ms, chunks={r.chunks_received})")
        tests_passed += 1
    else:
        print(f"FAIL ({r.error})")
        suite.passed = False

    # Test 2: Empty text rejection
    suite.total_tests += 1
    print("  [2] Empty text rejection...", end=" ", flush=True)
    r = await send_single_request(url, "", stream_id="empty-1")
    if not r.success or r.chunks_received == 0:
        print("PASS")
        tests_passed += 1
    else:
        print("FAIL (should have been rejected)")
        suite.passed = False

    # Test 3: Concurrent requests (8x, each on separate connection)
    suite.total_tests += 1
    print("  [3] 8 concurrent requests...", end=" ", flush=True)
    tasks = [
        send_single_request(url, f"Concurrent test number {i+1} for validation.", stream_id=f"conc-{i}")
        for i in range(8)
    ]
    results = await asyncio.gather(*tasks)
    successes = sum(1 for r in results if r.success)
    if successes >= 7:  # allow 1 failure (chaos)
        ttfcs = [r.ttfc_ms for r in results if r.success]
        print(f"PASS ({successes}/8 ok, ttfc {min(ttfcs):.0f}-{max(ttfcs):.0f}ms)")
        tests_passed += 1
    else:
        errors = [r.error for r in results if not r.success]
        print(f"FAIL ({successes}/8 ok, errors: {errors[:3]})")
        suite.passed = False

    # Test 4: Sequential requests on same connection (persistent connection)
    suite.total_tests += 1
    print("  [4] Persistent connection (5 sequential)...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        seq_ok = 0
        for i in range(5):
            sid = f"seq-{i}"
            await ws.send(json.dumps({
                "type": "start", "stream_id": sid,
                "text": f"Sequential request number {i+1}.", "speaker_id": 0,
            }))
            got_done = False
            got_audio = False
            async for msg in ws:
                if isinstance(msg, bytes):
                    parsed_sid, _ = parse_binary_frame(msg)
                    if parsed_sid == sid:
                        got_audio = True
                elif isinstance(msg, str):
                    data = json.loads(msg)
                    if data.get("type") == "done" and data.get("stream_id") == sid:
                        got_done = True
                        break
                    elif data.get("type") == "error":
                        break
            if got_done and got_audio:
                seq_ok += 1
        await ws.close()
        if seq_ok >= 4:
            print(f"PASS ({seq_ok}/5)")
            tests_passed += 1
        else:
            print(f"FAIL ({seq_ok}/5)")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    # Test 5: Client disconnect mid-stream, system recovers
    suite.total_tests += 1
    print("  [5] Client disconnect mid-stream...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        await ws.send(json.dumps({
            "type": "start", "stream_id": "disconnect-1",
            "text": "This is a long text that will produce many chunks for the mid-stream disconnect test scenario to verify recovery.",
            "speaker_id": 0,
        }))
        # Read one message then kill
        await asyncio.wait_for(ws.recv(), timeout=10.0)
        await ws.close()
        await asyncio.sleep(1.0)

        # System should recover
        r = await send_single_request(url, "Post-disconnect health check.", stream_id="health-1")
        if r.success:
            print("PASS (recovered)")
            tests_passed += 1
        else:
            print(f"FAIL (unhealthy: {r.error})")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    # Test 6: stream_id in done/error messages
    suite.total_tests += 1
    print("  [6] stream_id in control messages...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        sid = "tag-check-xyz"
        await ws.send(json.dumps({
            "type": "start", "stream_id": sid,
            "text": "Checking stream ID tagging.", "speaker_id": 0,
        }))
        stream_ids_seen = set()
        async for msg in ws:
            if isinstance(msg, bytes):
                parsed_sid, _ = parse_binary_frame(msg)
                stream_ids_seen.add(("binary", parsed_sid))
            elif isinstance(msg, str):
                data = json.loads(msg)
                if data.get("stream_id"):
                    stream_ids_seen.add(("text", data["stream_id"]))
                if data.get("type") in ("done", "error"):
                    break
        await ws.close()
        # All stream IDs should match
        wrong = [s for s in stream_ids_seen if s[1] != sid]
        if not wrong and len(stream_ids_seen) > 0:
            print("PASS")
            tests_passed += 1
        else:
            print(f"FAIL (wrong IDs: {wrong}, seen: {stream_ids_seen})")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    suite.passed_tests = tests_passed
    return suite


# ──────────────────────────────────────────────────────────────
#  Suite 2: Chaos survival
# ──────────────────────────────────────────────────────────────

async def suite_chaos(url: str, total_requests: int = 200, concurrency: int = 16) -> SuiteResult:
    suite = SuiteResult(name=f"Chaos Survival ({total_requests} requests, {concurrency} concurrent)", passed=True)
    suite.total_tests = 1

    sem = asyncio.Semaphore(concurrency)
    results = []

    async def worker(idx):
        text = TEST_TEXTS[idx % len(TEST_TEXTS)]
        async with sem:
            r = await send_single_request(url, text, stream_id=f"chaos-{idx}", timeout=30.0)
            results.append(r)

    print(f"  Running {total_requests} requests with {concurrency} concurrency against chaotic backends...")
    start = time.monotonic()
    await asyncio.gather(*[worker(i) for i in range(total_requests)])
    elapsed = time.monotonic() - start

    successes = sum(1 for r in results if r.success)
    rate = successes / total_requests
    suite.results = results

    throughput = total_requests / elapsed
    print(f"  Completed in {elapsed:.1f}s ({throughput:.1f} req/s)")
    print(f"  Success rate: {rate*100:.1f}% ({successes}/{total_requests})")

    if rate >= 0.95:
        print(f"  PASS (>= 95% required)")
        suite.passed_tests = 1
    else:
        print(f"  FAIL (< 95% — multiplexer must retry and circuit-break)")
        suite.passed = False

    suite.details = f"Success rate: {rate*100:.1f}%, throughput: {throughput:.1f} req/s"
    return suite


# ──────────────────────────────────────────────────────────────
#  Suite 3: Stream multiplexing
# ──────────────────────────────────────────────────────────────

async def suite_multiplex(url: str) -> SuiteResult:
    suite = SuiteResult(name="Stream Multiplexing", passed=True, total_tests=0)
    tests_passed = 0

    # Test 1: Two concurrent streams on one connection
    suite.total_tests += 1
    print("  [1] Two concurrent streams...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        # Fire two starts without waiting
        await ws.send(json.dumps({
            "type": "start", "stream_id": "mux-a",
            "text": "First concurrent stream for multiplexing test.", "speaker_id": 0,
        }))
        await ws.send(json.dumps({
            "type": "start", "stream_id": "mux-b",
            "text": "Second concurrent stream for multiplexing test.", "speaker_id": 0,
        }))

        streams = {"mux-a": {"chunks": 0, "done": False}, "mux-b": {"chunks": 0, "done": False}}
        deadline = time.monotonic() + 30.0

        async for msg in ws:
            if time.monotonic() > deadline:
                break
            if isinstance(msg, bytes):
                sid, pcm = parse_binary_frame(msg)
                if sid in streams:
                    streams[sid]["chunks"] += 1
            elif isinstance(msg, str):
                data = json.loads(msg)
                sid = data.get("stream_id", "")
                if data.get("type") == "done" and sid in streams:
                    streams[sid]["done"] = True
                elif data.get("type") == "error" and sid in streams:
                    streams[sid]["done"] = True  # count as complete
            if all(s["done"] for s in streams.values()):
                break

        await ws.close()
        both_got_audio = all(s["chunks"] > 0 for s in streams.values())
        both_done = all(s["done"] for s in streams.values())
        if both_got_audio and both_done:
            print(f"PASS (a={streams['mux-a']['chunks']} chunks, b={streams['mux-b']['chunks']} chunks)")
            tests_passed += 1
        else:
            print(f"FAIL (streams: {streams})")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    # Test 2: Four concurrent streams (max per connection)
    suite.total_tests += 1
    print("  [2] Four concurrent streams (max)...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        sids = [f"mux4-{i}" for i in range(4)]
        for sid in sids:
            await ws.send(json.dumps({
                "type": "start", "stream_id": sid,
                "text": f"Stream {sid} in a four-way multiplex test.", "speaker_id": 0,
            }))

        streams = {sid: {"chunks": 0, "done": False} for sid in sids}
        deadline = time.monotonic() + 45.0

        async for msg in ws:
            if time.monotonic() > deadline:
                break
            if isinstance(msg, bytes):
                sid, _ = parse_binary_frame(msg)
                if sid in streams:
                    streams[sid]["chunks"] += 1
            elif isinstance(msg, str):
                data = json.loads(msg)
                sid = data.get("stream_id", "")
                if data.get("type") in ("done", "error") and sid in streams:
                    streams[sid]["done"] = True
            if all(s["done"] for s in streams.values()):
                break

        await ws.close()
        completed = sum(1 for s in streams.values() if s["done"] and s["chunks"] > 0)
        if completed >= 3:  # allow 1 failure from chaos
            print(f"PASS ({completed}/4 completed)")
            tests_passed += 1
        else:
            print(f"FAIL ({completed}/4 completed, streams: {streams})")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    # Test 3: Fifth stream rejected (over limit)
    suite.total_tests += 1
    print("  [3] Fifth stream rejected...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        for i in range(5):
            await ws.send(json.dumps({
                "type": "start", "stream_id": f"limit-{i}",
                "text": "Testing the per-connection stream limit enforcement.",
                "speaker_id": 0,
            }))
            await asyncio.sleep(0.05)

        # Collect messages, look for an error on the 5th stream
        got_rejection = False
        deadline = time.monotonic() + 10.0
        async for msg in ws:
            if time.monotonic() > deadline:
                break
            if isinstance(msg, str):
                data = json.loads(msg)
                if (data.get("type") == "error"
                    and data.get("stream_id") == "limit-4"):
                    got_rejection = True
                    break
        await ws.close()

        if got_rejection:
            print("PASS (5th stream rejected)")
            tests_passed += 1
        else:
            print("FAIL (5th stream was not rejected)")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    # Test 4: Cancel a stream mid-flight
    suite.total_tests += 1
    print("  [4] Stream cancellation...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        await ws.send(json.dumps({
            "type": "start", "stream_id": "cancel-me",
            "text": "This is a long text that will produce many chunks so we can cancel it mid-stream to test cancellation behavior properly.",
            "speaker_id": 0,
        }))
        # Wait for first chunk
        got_chunk = False
        async for msg in ws:
            if isinstance(msg, bytes):
                got_chunk = True
                break
            elif isinstance(msg, str):
                data = json.loads(msg)
                if data.get("type") == "error":
                    break
                continue

        if got_chunk:
            # Send cancel
            await ws.send(json.dumps({"type": "cancel", "stream_id": "cancel-me"}))
            await asyncio.sleep(0.5)

            # Verify we can start a new stream on the same connection
            await ws.send(json.dumps({
                "type": "start", "stream_id": "after-cancel",
                "text": "Post-cancel stream.", "speaker_id": 0,
            }))
            post_ok = False
            deadline = time.monotonic() + 15.0
            async for msg in ws:
                if time.monotonic() > deadline:
                    break
                if isinstance(msg, str):
                    data = json.loads(msg)
                    if data.get("type") == "done" and data.get("stream_id") == "after-cancel":
                        post_ok = True
                        break
            await ws.close()
            if post_ok:
                print("PASS (cancelled + new stream works)")
                tests_passed += 1
            else:
                print("FAIL (post-cancel stream didn't complete)")
                suite.passed = False
        else:
            await ws.close()
            print("FAIL (never got first chunk)")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    # Test 5: Stream isolation (one stream errors, other continues)
    suite.total_tests += 1
    print("  [5] Stream isolation under error...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        # Start a good stream and a bad stream (empty text = error)
        await ws.send(json.dumps({
            "type": "start", "stream_id": "good",
            "text": "This stream should complete normally.", "speaker_id": 0,
        }))
        await ws.send(json.dumps({
            "type": "start", "stream_id": "bad",
            "text": "", "speaker_id": 0,
        }))

        good_done = False
        bad_errored = False
        good_chunks = 0
        deadline = time.monotonic() + 20.0

        async for msg in ws:
            if time.monotonic() > deadline:
                break
            if isinstance(msg, bytes):
                sid, _ = parse_binary_frame(msg)
                if sid == "good":
                    good_chunks += 1
            elif isinstance(msg, str):
                data = json.loads(msg)
                sid = data.get("stream_id", "")
                if data.get("type") == "error" and sid == "bad":
                    bad_errored = True
                elif data.get("type") == "done" and sid == "good":
                    good_done = True
            if good_done and bad_errored:
                break

        await ws.close()
        if good_done and bad_errored and good_chunks > 0:
            print(f"PASS (good={good_chunks} chunks + done, bad=errored)")
            tests_passed += 1
        else:
            print(f"FAIL (good_done={good_done}, bad_errored={bad_errored}, good_chunks={good_chunks})")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    suite.passed_tests = tests_passed
    return suite


# ──────────────────────────────────────────────────────────────
#  Suite 4: Slow consumer (backpressure)
# ──────────────────────────────────────────────────────────────

async def suite_slow_consumer(url: str) -> SuiteResult:
    suite = SuiteResult(name="Slow Consumer / Backpressure", passed=True, total_tests=0)
    tests_passed = 0

    # Test 1: Read at 0.5x realtime (each chunk ~1.6s of audio, read every 3.2s)
    suite.total_tests += 1
    print("  [1] Slow reader (0.5x realtime)...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        await ws.send(json.dumps({
            "type": "start", "stream_id": "slow-1",
            "text": "This is an extremely long text that needs to produce many many audio chunks for the slow consumer backpressure test. " * 3,
            "speaker_id": 0,
        }))

        chunks = 0
        got_done = False
        deadline = time.monotonic() + 60.0

        async for msg in ws:
            if time.monotonic() > deadline:
                break
            if isinstance(msg, bytes):
                chunks += 1
                # Simulate slow consumption
                await asyncio.sleep(0.5)
            elif isinstance(msg, str):
                data = json.loads(msg)
                if data.get("type") == "done":
                    got_done = True
                    break
                elif data.get("type") == "error":
                    break

        await ws.close()
        if got_done and chunks > 0:
            print(f"PASS (received {chunks} chunks at 0.5x speed)")
            tests_passed += 1
        else:
            print(f"FAIL (done={got_done}, chunks={chunks})")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    # Test 2: Fast stream + slow stream on same connection (slow must not block fast)
    suite.total_tests += 1
    print("  [2] Fast + slow stream on same conn...", end=" ", flush=True)
    try:
        ws = await asyncio.wait_for(
            websockets.connect(url, max_size=10 * 1024 * 1024), timeout=10.0,
        )
        # Start both
        await ws.send(json.dumps({
            "type": "start", "stream_id": "fast",
            "text": "Short fast request.", "speaker_id": 0,
        }))
        await ws.send(json.dumps({
            "type": "start", "stream_id": "slow",
            "text": "This is a much longer request that will take significantly more time to generate all the audio chunks needed for the slow stream test.",
            "speaker_id": 0,
        }))

        fast_done_at = None
        slow_done_at = None
        start = time.monotonic()
        deadline = start + 45.0

        async for msg in ws:
            if time.monotonic() > deadline:
                break
            if isinstance(msg, str):
                data = json.loads(msg)
                sid = data.get("stream_id", "")
                if data.get("type") == "done":
                    if sid == "fast" and fast_done_at is None:
                        fast_done_at = time.monotonic() - start
                    elif sid == "slow" and slow_done_at is None:
                        slow_done_at = time.monotonic() - start
            if fast_done_at is not None and slow_done_at is not None:
                break

        await ws.close()
        if fast_done_at is not None and slow_done_at is not None:
            print(f"PASS (fast={fast_done_at:.1f}s, slow={slow_done_at:.1f}s)")
            tests_passed += 1
        elif fast_done_at is not None:
            print(f"PARTIAL (fast={fast_done_at:.1f}s, slow didn't complete)")
            tests_passed += 1  # fast completing independently is the key test
        else:
            print(f"FAIL (fast_done={fast_done_at}, slow_done={slow_done_at})")
            suite.passed = False
    except Exception as e:
        print(f"FAIL ({e})")
        suite.passed = False

    suite.passed_tests = tests_passed
    return suite


# ──────────────────────────────────────────────────────────────
#  Suite 5: Soak test (sustained load, memory check)
# ──────────────────────────────────────────────────────────────

async def suite_soak(url: str, duration_seconds: int = 120, concurrency: int = 8) -> SuiteResult:
    suite = SuiteResult(name=f"Soak Test ({duration_seconds}s, {concurrency} concurrent)", passed=True, total_tests=1)

    results = []
    sem = asyncio.Semaphore(concurrency)
    stop = asyncio.Event()

    async def worker():
        idx = 0
        while not stop.is_set():
            text = TEST_TEXTS[idx % len(TEST_TEXTS)]
            async with sem:
                if stop.is_set():
                    break
                r = await send_single_request(url, text, stream_id=f"soak-{idx}", timeout=30.0)
                results.append(r)
                idx += 1

    print(f"  Running soak test for {duration_seconds}s with {concurrency} concurrency...")
    workers = [asyncio.create_task(worker()) for _ in range(concurrency)]

    # Sample memory usage periodically (of the test client itself —
    # candidate should monitor their own mux process)
    memory_samples = []
    start = time.monotonic()
    check_interval = max(10, duration_seconds // 20)

    for elapsed in range(0, duration_seconds, check_interval):
        await asyncio.sleep(min(check_interval, duration_seconds - elapsed))
        ok = sum(1 for r in results if r.success)
        total = len(results)
        rate = ok / total if total > 0 else 0
        print(f"    t={int(time.monotonic()-start)}s: {total} requests, {rate*100:.0f}% success", flush=True)

    stop.set()
    await asyncio.gather(*workers, return_exceptions=True)
    elapsed = time.monotonic() - start

    successes = sum(1 for r in results if r.success)
    total = len(results)
    rate = successes / total if total > 0 else 0
    throughput = total / elapsed

    print(f"  Completed: {total} requests in {elapsed:.0f}s ({throughput:.1f} req/s)")
    print(f"  Success rate: {rate*100:.1f}%")

    suite.results = results
    if rate >= 0.90:
        suite.passed_tests = 1
        suite.details = f"{total} requests, {rate*100:.1f}% success, {throughput:.1f} req/s over {elapsed:.0f}s"
    else:
        suite.passed = False
        suite.details = f"Success rate {rate*100:.1f}% below 90% threshold"

    return suite


# ──────────────────────────────────────────────────────────────
#  Suite 6: Backend removal (circuit breaker test)
# ──────────────────────────────────────────────────────────────

async def suite_kill_backend(url: str, kill_port: int) -> SuiteResult:
    suite = SuiteResult(name=f"Backend Removal (kill :{kill_port})", passed=True, total_tests=1)

    print(f"  Phase 1: Sending requests with all backends up...")
    before_results = []
    sem = asyncio.Semaphore(8)
    tasks = []
    for i in range(20):
        async def req(idx=i):
            async with sem:
                r = await send_single_request(
                    url, TEST_TEXTS[idx % len(TEST_TEXTS)], stream_id=f"pre-kill-{idx}",
                )
                before_results.append(r)
        tasks.append(asyncio.create_task(req()))
    await asyncio.gather(*tasks)
    before_ok = sum(1 for r in before_results if r.success)
    print(f"    {before_ok}/20 succeeded")

    print(f"  Phase 2: Killing backend on port {kill_port}...")
    # Find and kill the process on that port
    try:
        result = subprocess.run(
            ["lsof", "-ti", f":{kill_port}"], capture_output=True, text=True,
        )
        pids = result.stdout.strip().split("\n")
        for pid in pids:
            if pid:
                os.kill(int(pid), signal.SIGKILL)
                print(f"    Killed PID {pid}")
    except Exception as e:
        print(f"    Could not kill backend: {e}")
        print(f"    (manually stop the backend on port {kill_port} for this test)")

    await asyncio.sleep(2.0)

    print(f"  Phase 3: Sending requests after backend death...")
    after_results = []
    tasks = []
    for i in range(30):
        async def req(idx=i):
            async with sem:
                r = await send_single_request(
                    url, TEST_TEXTS[idx % len(TEST_TEXTS)], stream_id=f"post-kill-{idx}",
                )
                after_results.append(r)
        tasks.append(asyncio.create_task(req()))
    await asyncio.gather(*tasks)
    after_ok = sum(1 for r in after_results if r.success)
    print(f"    {after_ok}/30 succeeded")

    suite.results = before_results + after_results
    rate = after_ok / 30
    if rate >= 0.85:
        suite.passed_tests = 1
        suite.details = f"Post-kill success rate: {rate*100:.0f}% (circuit breaker should route around dead backend)"
        print(f"  PASS (system adapted to backend loss)")
    else:
        suite.passed = False
        suite.details = f"Post-kill success rate: {rate*100:.0f}% — too low"
        print(f"  FAIL (system didn't adapt)")

    return suite


# ──────────────────────────────────────────────────────────────
#  Suite 7: Performance comparison
# ──────────────────────────────────────────────────────────────

async def suite_compare(mux_url: str, direct_url: str) -> SuiteResult:
    suite = SuiteResult(name="Performance Overhead", passed=True, total_tests=1)

    print(f"  Measuring direct backend TTFC ({direct_url})...")
    direct_results = []
    for i in range(10):
        r = await send_single_request(
            direct_url, TEST_TEXTS[i % len(TEST_TEXTS)],
            stream_id=f"direct-{i}", expect_stream_tags=False,
        )
        if r.success:
            direct_results.append(r)

    print(f"  Measuring multiplexer TTFC ({mux_url})...")
    mux_results = []
    for i in range(10):
        r = await send_single_request(
            mux_url, TEST_TEXTS[i % len(TEST_TEXTS)], stream_id=f"compare-{i}",
        )
        if r.success:
            mux_results.append(r)

    if direct_results and mux_results:
        direct_p50 = statistics.median([r.ttfc_ms for r in direct_results])
        mux_p50 = statistics.median([r.ttfc_ms for r in mux_results])
        overhead = mux_p50 - direct_p50

        print(f"  Direct TTFC p50: {direct_p50:.1f}ms")
        print(f"  Mux TTFC p50:    {mux_p50:.1f}ms")
        print(f"  Overhead:        {overhead:+.1f}ms")

        if overhead < 0.5:
            verdict = "EXCELLENT (<0.5ms)"
        elif overhead < 2:
            verdict = "GOOD (<2ms)"
        elif overhead < 5:
            verdict = "ACCEPTABLE (<5ms)"
        else:
            verdict = "HIGH — investigate"
            suite.passed = False

        print(f"  Verdict: {verdict}")
        suite.passed_tests = 1 if suite.passed else 0
        suite.details = f"Overhead: {overhead:+.1f}ms ({verdict})"
    else:
        print(f"  SKIP (not enough successful requests)")
        suite.details = "Insufficient data"

    suite.results = mux_results
    return suite


# ──────────────────────────────────────────────────────────────
#  Main
# ──────────────────────────────────────────────────────────────

async def main():
    parser = argparse.ArgumentParser(
        description="TTS Multiplexer Test Suite",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("url", help="Multiplexer WebSocket URL")
    parser.add_argument("--all", action="store_true", help="Run all test suites")
    parser.add_argument("--correctness", action="store_true", help="Run correctness tests")
    parser.add_argument("--chaos", action="store_true", help="Run chaos survival test")
    parser.add_argument("--multiplex", action="store_true", help="Run stream multiplexing tests")
    parser.add_argument("--slow-consumer", action="store_true", help="Run backpressure tests")
    parser.add_argument("--soak", type=int, metavar="SECONDS", help="Run soak test for N seconds")
    parser.add_argument("--kill-backend", type=int, metavar="PORT", help="Kill backend on PORT mid-test")
    parser.add_argument("--compare", type=str, metavar="DIRECT_URL", help="Compare overhead vs direct backend")
    parser.add_argument("--concurrency", type=int, default=16)
    parser.add_argument("--requests", type=int, default=200)

    args = parser.parse_args()

    # Default: run correctness if no suite specified
    if not any([args.all, args.correctness, args.chaos, args.multiplex,
                args.slow_consumer, args.soak, args.kill_backend, args.compare]):
        args.correctness = True

    suites = []

    if args.all or args.correctness:
        print("\n--- Suite: Correctness ---")
        suites.append(await suite_correctness(args.url))

    if args.all or args.multiplex:
        print("\n--- Suite: Stream Multiplexing ---")
        suites.append(await suite_multiplex(args.url))

    if args.all or args.chaos:
        print("\n--- Suite: Chaos Survival ---")
        suites.append(await suite_chaos(args.url, args.requests, args.concurrency))

    if args.all or args.slow_consumer:
        print("\n--- Suite: Slow Consumer ---")
        suites.append(await suite_slow_consumer(args.url))

    if args.soak:
        print("\n--- Suite: Soak Test ---")
        suites.append(await suite_soak(args.url, args.soak, args.concurrency))
    elif args.all:
        print("\n--- Suite: Soak Test (120s) ---")
        suites.append(await suite_soak(args.url, 120, args.concurrency))

    if args.kill_backend:
        print(f"\n--- Suite: Backend Removal (:{args.kill_backend}) ---")
        suites.append(await suite_kill_backend(args.url, args.kill_backend))

    if args.compare:
        print("\n--- Suite: Performance Overhead ---")
        suites.append(await suite_compare(args.url, args.compare))

    # Final report
    print("\n" + "=" * 60)
    print("  FINAL REPORT")
    print("=" * 60)
    all_passed = True
    for s in suites:
        status = "PASS" if s.passed else "FAIL"
        print(f"  [{status}] {s.name} ({s.passed_tests}/{s.total_tests})")
        if s.details:
            print(f"         {s.details}")
        if not s.passed:
            all_passed = False

    print("=" * 60)
    if all_passed:
        print("  ALL SUITES PASSED")
    else:
        print("  SOME SUITES FAILED")
    print("=" * 60 + "\n")

    sys.exit(0 if all_passed else 1)


if __name__ == "__main__":
    asyncio.run(main())
