#!/usr/bin/env bash
# Run mock backend + tts-multiplexer + test client in one tmux session.
#
# Usage:
#   ./run-all.sh                 # default: chaos backends, basic correctness tests
#   ./run-all.sh --calm          # backends without chaos
#   ./run-all.sh --all           # full test suite
#   ./run-all.sh --chaos --requests 200
#   ./run-all.sh --no-tmux       # run inline (background procs + foreground tests)
#
# Anything after `--` is forwarded verbatim to test_client.py.
#   ./run-all.sh -- --soak 120
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
WORKERS=8
BASE_PORT=9100
MUX_PORT=9000
METRICS_PORT=9001
SESSION="tts-mux"
USE_TMUX=1
BACKEND_FLAGS=()
TEST_FLAGS=()

# Split args at `--`: before goes to backend/test heuristics, after goes only to test client.
forwarding=0
for arg in "$@"; do
    if [[ $forwarding -eq 1 ]]; then
        TEST_FLAGS+=("$arg")
        continue
    fi
    case "$arg" in
        --)         forwarding=1 ;;
        --no-tmux)  USE_TMUX=0 ;;
        --calm)     BACKEND_FLAGS+=("--calm") ;;
        --heterogeneous) BACKEND_FLAGS+=("--heterogeneous") ;;
        *)          TEST_FLAGS+=("$arg") ;;
    esac
done

BACKENDS=""
for ((i=0; i<WORKERS; i++)); do
    p=$((BASE_PORT + i))
    BACKENDS+="${BACKENDS:+,}127.0.0.1:${p}"
done

PY="${PYTHON:-python3}"
ASSIGN="$ROOT/assignment"
MUX_BIN="$ROOT/target/release/tts-multiplexer"
MUX_URL="ws://localhost:${MUX_PORT}/v1/ws/speech"

# --- Pre-flight ---
command -v cargo >/dev/null || { echo "cargo not found"; exit 1; }
command -v "$PY" >/dev/null || { echo "$PY not found"; exit 1; }
"$PY" -c "import websockets" 2>/dev/null || {
    echo "Installing websockets..."
    "$PY" -m pip install -q -r "$ASSIGN/requirements.txt"
}

echo ">> Building multiplexer (release)..."
(cd "$ROOT" && cargo build --release -p tts-multiplexer)

wait_port() {  # wait_port HOST PORT TIMEOUT_SEC
    local host=$1 port=$2 timeout=${3:-15} t=0
    until (echo >/dev/tcp/"$host"/"$port") 2>/dev/null; do
        sleep 0.2
        t=$((t+1))
        if (( t > timeout*5 )); then
            echo "timed out waiting for $host:$port"; return 1
        fi
    done
}

run_inline() {
    local logdir="$ROOT/.run-logs"
    mkdir -p "$logdir"
    echo ">> Starting backend (logs: $logdir/backend.log)"
    "$PY" "$ASSIGN/mock_backend.py" --workers "$WORKERS" --base-port "$BASE_PORT" \
        "${BACKEND_FLAGS[@]}" >"$logdir/backend.log" 2>&1 &
    BACK_PID=$!
    trap 'echo ">> Cleaning up"; kill $BACK_PID ${MUX_PID:-} 2>/dev/null || true' EXIT

    echo ">> Waiting for backends..."
    for ((i=0; i<WORKERS; i++)); do wait_port 127.0.0.1 $((BASE_PORT+i)) 30; done

    echo ">> Starting multiplexer (logs: $logdir/mux.log)"
    "$MUX_BIN" --port "$MUX_PORT" --metrics-port "$METRICS_PORT" \
        --backends "$BACKENDS" >"$logdir/mux.log" 2>&1 &
    MUX_PID=$!
    wait_port 127.0.0.1 "$MUX_PORT" 15

    echo ">> Running tests"
    "$PY" "$ASSIGN/test_client.py" "$MUX_URL" "${TEST_FLAGS[@]}"
}

run_tmux() {
    command -v tmux >/dev/null || { echo "tmux not found, retry with --no-tmux"; exit 1; }
    tmux has-session -t "$SESSION" 2>/dev/null && tmux kill-session -t "$SESSION"

    local back_cmd mux_cmd test_cmd
    back_cmd="$PY '$ASSIGN/mock_backend.py' --workers $WORKERS --base-port $BASE_PORT ${BACKEND_FLAGS[*]}"
    mux_cmd="bash -c \"for p in $(seq $BASE_PORT $((BASE_PORT+WORKERS-1))); do until (echo >/dev/tcp/127.0.0.1/\$p) 2>/dev/null; do sleep 0.2; done; done; '$MUX_BIN' --port $MUX_PORT --metrics-port $METRICS_PORT --backends $BACKENDS\""
    test_cmd="bash -c \"until (echo >/dev/tcp/127.0.0.1/$MUX_PORT) 2>/dev/null; do sleep 0.2; done; sleep 0.5; $PY '$ASSIGN/test_client.py' '$MUX_URL' ${TEST_FLAGS[*]}; echo; echo '[tests done — Ctrl-b d to detach, or: tmux kill-session -t $SESSION]'; exec bash\""

    tmux new-session  -d -s "$SESSION" -n backend "$back_cmd"
    tmux split-window -t "$SESSION:0" -v "$mux_cmd"
    tmux split-window -t "$SESSION:0" -h "$test_cmd"
    tmux select-layout -t "$SESSION:0" tiled
    tmux set-option   -t "$SESSION" mouse on >/dev/null

    echo ">> tmux session '$SESSION' started (backend / mux / tests)"
    echo ">> Attaching. Detach with Ctrl-b d. Kill with: tmux kill-session -t $SESSION"
    exec tmux attach -t "$SESSION"
}

if [[ $USE_TMUX -eq 1 ]]; then run_tmux; else run_inline; fi
