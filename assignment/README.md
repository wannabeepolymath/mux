# Backend Assignment: Streaming TTS Multiplexer

## Context

We run a real-time text-to-speech system: N GPU workers, each processing **one request at a time**, streaming PCM audio over WebSocket. Today nginx routes clients to workers with `max_conns=1`, connections are torn down after every response, and there's no intelligence in the routing layer.

Your job: **build a multiplexer in Rust** that replaces nginx. It sits between clients and GPU workers, owns connection lifecycle, and must survive a hostile environment — backends crash mid-stream, hang, send garbage, and disappear without warning. This is normal in production.

The multiplexer must also support **concurrent streams per client connection** — a single WebSocket carries multiple in-flight TTS requests simultaneously, tagged with stream IDs. This is how our mobile clients work: they pipeline requests to hide latency.

```
                    Clients
                       │
              ┌────────┴────────┐
              │   MULTIPLEXER   │  ◄── You build this
              │   (Rust)        │
              └────────┬────────┘
                       │
         ┌─────────────┼─────────────┐
         ▼             ▼             ▼
    ┌─────────┐   ┌─────────┐   ┌─────────┐
    │ Worker 1│   │ Worker 2│   │ Worker N│
    │ (mock)  │   │ (mock)  │   │ (mock)  │
    └─────────┘   └─────────┘   └─────────┘

    Each worker processes ONE request at a time.
    Workers crash, hang, and misbehave. Handle it.
```

---

## Part 1: Chaos-Resilient Core

The mock backend (`mock_backend.py`) runs in **chaos mode by default**. On any given request, a backend might:

| Failure mode | Default rate | What happens |
|-------------|-------------|--------------|
| Mid-stream crash | 8% | Backend sends 1-3 chunks then drops the TCP connection |
| Hang | 5% | Backend accepts connection, receives start message, then goes silent forever |
| Malformed response | 3% | Backend sends invalid JSON or unexpected binary data instead of proper protocol messages |
| Slow backend | 15% | Backend takes 3-5x longer than normal per chunk (simulates GPU thermal throttling) |
| Connection refused | 5% | Backend refuses the TCP connection entirely (simulates container restart) |

Your multiplexer must achieve **>95% end-to-end success rate** under these conditions with 8 backends and 16 concurrent clients.

### Required behaviors

1. **Request retry**: If a backend dies mid-stream, re-dispatch the request to another backend. The client receives audio from the retry — it's OK to restart from the beginning (the client handles the discontinuity).

2. **Circuit breaker**: If a backend fails 3 consecutive requests, remove it from rotation for a configurable cooldown period (default 30s). Probe it with a health check before re-admitting.

3. **Hang detection**: If a backend doesn't send any data within a configurable timeout (default 5s), treat it as dead. Kill the connection, retry the request.

4. **Backpressure**: If a client reads slowly (slower than the backend produces), the multiplexer must not buffer unboundedly. Implement per-stream flow control — if the buffer exceeds a configurable limit (default 256KB), drop the oldest chunks and log a warning. Do NOT let the multiplexer OOM.

5. **Connection pool**: Backends close connections after each request. The multiplexer must reconnect eagerly — when a backend finishes a request, start reconnecting immediately so a warm connection is ready for the next dispatch. Track connection lifecycle as an explicit state machine: `Connecting → Ready → Busy → Draining → Disconnected → Connecting`.

6. **Global queue**: Single FIFO queue across all backends. When a backend becomes ready, the next queued request is dispatched immediately. If the queue exceeds max depth (configurable, default 64), reject new requests with an error.

---

## Part 2: Stream Multiplexing

A single client WebSocket connection can carry **multiple concurrent TTS streams**. This is not optional.

### Client → Multiplexer

**Start a stream** (text frame):
```json
{
  "type": "start",
  "stream_id": "s1",
  "text": "Hello world.",
  "speaker_id": 0,
  "priority": 10
}
```

**Cancel a stream** (text frame):
```json
{"type": "cancel", "stream_id": "s1"}
```

**Close connection** (text frame):
```json
{"type": "close"}
```

A client may have up to **4 concurrent streams** on one connection. The multiplexer must reject a 5th with an error (per-connection limit, not global).

### Multiplexer → Client

**Acknowledgment** (text frame):
```json
{"type": "queued", "stream_id": "s1", "queue_depth": 3}
```

**Audio data** (binary frame): Binary frames carry a stream-ID header so the client can demultiplex:

```
┌──────────┬───────────────────────┬─────────────────────┐
│ id_len   │ stream_id             │ pcm_payload         │
│ (1 byte) │ (id_len bytes, UTF-8) │ (remaining bytes)   │
└──────────┴───────────────────────┘─────────────────────┘
```

- Byte 0: length of stream_id (u8)
- Bytes 1..1+id_len: stream_id as UTF-8
- Bytes 1+id_len..: raw PCM int16 24kHz mono audio

**Stream completion** (text frame):
```json
{
  "type": "done",
  "stream_id": "s1",
  "audio_duration": 5.2,
  "total_time": 4.8,
  "rtf": 1.08
}
```

**Stream error** (text frame):
```json
{"type": "error", "stream_id": "s1", "message": "backend unavailable after 2 retries"}
```

### Multiplexing invariants

- One stream failing must NOT affect other streams on the same connection. If stream "s1" hits a backend that crashes and exhausts retries, stream "s2" on the same connection must continue uninterrupted.
- Cancelling a stream must release the backend (if one is assigned) so it can serve other requests. The backend will continue generating (it doesn't support cancellation) — the multiplexer must drain and discard the backend's remaining output.
- Binary frames for different streams may be interleaved in any order. The client uses the stream ID header to demux.
- Text frames (done, error, queued) always include `stream_id`.

---

## Part 3: Adaptive Routing

Don't just pick a random idle backend. Score them.

### Backend scoring

Maintain per-backend metrics:
- **TTFC EWMA**: exponentially-weighted moving average of time-to-first-chunk (alpha=0.3)
- **Error rate**: errors in last 20 requests (sliding window)
- **Consecutive failures**: for circuit breaker

When multiple backends are idle, dispatch to the one with the **lowest TTFC EWMA** among those with error rate below 30%.

### Health endpoint

`GET /health` must return:
```json
{
  "status": "ok",
  "backends": [
    {
      "addr": "localhost:9100",
      "state": "ready",
      "ttfc_ewma_ms": 312.5,
      "error_rate": 0.05,
      "total_requests": 142,
      "consecutive_failures": 0,
      "circuit": "closed"
    }
  ],
  "queue_depth": 3,
  "active_streams": 12,
  "active_connections": 8
}
```

### Prometheus metrics

`GET /metrics` with at minimum:
- `mux_requests_total{status="success|error|retry"}` counter
- `mux_ttfc_seconds` histogram (buckets: 0.1, 0.25, 0.5, 1.0, 2.5, 5.0)
- `mux_queue_depth` gauge
- `mux_active_streams` gauge
- `mux_backend_state{backend="addr", state="ready|busy|circuit_open"}` gauge
- `mux_chunk_forward_seconds` histogram (measures the multiplexer's own overhead per chunk)

---

## Part 4: Performance Requirements

| Metric | Target | How we test |
|--------|--------|-------------|
| Chunk forwarding overhead | <500μs p50, <2ms p99 | `test_client.py --compare` |
| Throughput at saturation | >95% of theoretical (N backends × per-backend throughput) | Benchmark with --concurrency=2N |
| Memory under sustained load | RSS growth <5MB over 10 minutes | `test_client.py --soak 600` |
| Chaos survival rate | >95% success rate | `test_client.py --chaos` (default mock mode) |
| Stream isolation | 0 cross-contamination events | `test_client.py --multiplex` |

### Required deliverable

Include in your submission:
1. A `DESIGN.md` explaining your concurrency model, state machines, and the tradeoffs you made
2. Benchmark results from `test_client.py` (all test suites)
3. If you can produce a flamegraph (`cargo flamegraph` or `perf`), include it and annotate where your overhead comes from

---

## Multiplexer → Backend Protocol

The backend speaks a simple protocol. The multiplexer connects as a client:

1. Connect to `ws://worker:PORT/v1/ws/speech`
2. Send `{"type": "start", "text": "...", "speaker_id": 0}`
3. Receive text frame: `{"type": "queued", "queue_depth": 0}`
4. Receive binary frames: raw PCM chunks
5. Receive text frame: `{"type": "done", "audio_duration": ..., ...}`
6. Backend **closes the connection**

Backends enforce one-request-per-connection. After `done`, the connection is closed by the backend. Your connection pool must reconnect.

If the backend is already busy, it sends `{"type": "error", "message": "Worker busy"}` and closes.

---

## What You're Given

### `mock_backend.py`

```bash
pip install websockets

# Default: chaos mode ON (crashes, hangs, malformed data, slow backends)
python mock_backend.py --workers 8 --base-port 9100

# Calm mode for initial development (no chaos)
python mock_backend.py --workers 8 --base-port 9100 --calm

# Crank up the chaos
python mock_backend.py --workers 8 --base-port 9100 --crash-rate 0.15 --hang-rate 0.10
```

### `test_client.py`

```bash
# Correctness (basic protocol, connection reuse, error handling)
python test_client.py ws://localhost:9000/v1/ws/speech

# Chaos survival (must pass >95%)
python test_client.py ws://localhost:9000/v1/ws/speech --chaos --requests 200

# Stream multiplexing (concurrent streams per connection)
python test_client.py ws://localhost:9000/v1/ws/speech --multiplex

# Slow consumer (backpressure test)
python test_client.py ws://localhost:9000/v1/ws/speech --slow-consumer

# Soak test (10 minutes sustained load, memory check)
python test_client.py ws://localhost:9000/v1/ws/speech --soak 600

# Backend removal mid-test
python test_client.py ws://localhost:9000/v1/ws/speech --kill-backend 9107

# Full suite
python test_client.py ws://localhost:9000/v1/ws/speech --all

# Performance overhead comparison
python test_client.py ws://localhost:9000/v1/ws/speech --compare ws://localhost:9100/v1/ws/speech
```

---

## Evaluation Criteria

| Weight | Criteria |
|--------|----------|
| 25% | **Chaos resilience**: >95% success rate under default chaos, correct retry behavior, circuit breaker works, no panics, no leaks |
| 25% | **Stream multiplexing**: concurrent streams work, isolation is correct, cancellation releases resources, binary frame tagging is right |
| 20% | **Performance**: forwarding overhead, throughput at saturation, memory stability |
| 15% | **Code quality**: idiomatic Rust, state machines, error types, module boundaries, concurrency patterns |
| 15% | **Operational readiness**: health endpoint, Prometheus metrics, graceful shutdown, DESIGN.md quality |

---

## Constraints

- **Language**: Rust (latest stable)
- **Async runtime**: tokio
- **Time budget**: 3-4 days
- **What to submit**: Git repo with source, DESIGN.md, benchmark results, build + run instructions

## Non-Goals

- TLS / authentication
- Modifying the backend protocol
- Horizontal scaling of the multiplexer
- Persistent storage
- Frontend / UI

## Questions?

Make a decision and document it. We value judgment over clarification-seeking.
