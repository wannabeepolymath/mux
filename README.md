# TTS Multiplexer (Rust)

Chaos-resilient WebSocket multiplexer for streaming TTS.

This repo contains:
- `src/`: Rust multiplexer (client WebSocket server + backend dispatcher + forwarding + metrics/health)
- `assignment/`: `mock_backend.py` and `test_client.py` used for evaluation

## Prereqs

- Rust (latest stable) + Cargo
- Python 3.10+ (3.11+ recommended)

Python dependency:

```bash
python -m pip install --upgrade websockets>=13.0
```

## Build

```bash
cargo build --release
```

The binary will be at:

```bash
./target/release/tts-multiplexer
```

## Run: mock backends

From repo root:

```bash
# Default: chaos mode ON
python assignment/mock_backend.py --workers 8 --base-port 9100

# Calm mode (no chaos) for initial development
python assignment/mock_backend.py --workers 8 --base-port 9100 --calm

# Custom chaos rates
python assignment/mock_backend.py --workers 8 --base-port 9100 --crash-rate 0.15 --hang-rate 0.10
```

This starts workers on `9100..9107` and a backend health endpoint on `9099` (mock backend’s own endpoint).

## Run: multiplexer

In a separate terminal:

```bash
./target/release/tts-multiplexer \
  --port 9000 \
  --metrics-port 9001 \
  --backends 127.0.0.1:9100,127.0.0.1:9101,127.0.0.1:9102,127.0.0.1:9103,127.0.0.1:9104,127.0.0.1:9105,127.0.0.1:9106,127.0.0.1:9107
```

Endpoints:
- WebSocket: `ws://127.0.0.1:9000/v1/ws/speech`
- Health: `http://127.0.0.1:9001/health`
- Prometheus: `http://127.0.0.1:9001/metrics`

Quick checks:

```bash
curl -s http://127.0.0.1:9001/health | python -m json.tool
curl -s http://127.0.0.1:9001/metrics | head
```

## Run tests (Python harness)

All suites target the multiplexer WebSocket URL:

```bash
URL="ws://127.0.0.1:9000/v1/ws/speech"
```

### Correctness

```bash
python assignment/test_client.py "$URL"
# or explicitly:
python assignment/test_client.py "$URL" --correctness
```

### Chaos survival (>95% required)

```bash
python assignment/test_client.py "$URL" --chaos --requests 200 --concurrency 16
```

### Stream multiplexing

```bash
python assignment/test_client.py "$URL" --multiplex
```

### Slow consumer / backpressure

```bash
python assignment/test_client.py "$URL" --slow-consumer
```

### Soak test (memory stability)

```bash
# 10 minutes:
python assignment/test_client.py "$URL" --soak 600 --concurrency 8
```

### Backend removal mid-test (circuit-breaker behavior)

```bash
python assignment/test_client.py "$URL" --kill-backend 9107
```

### Performance overhead comparison

Compares TTFC through the multiplexer vs direct-to-backend:

```bash
python assignment/test_client.py "$URL" --compare ws://127.0.0.1:9100/v1/ws/speech
```

### Full suite

```bash
python assignment/test_client.py "$URL" --all
```

## Run Rust unit tests

```bash
cargo test
```

