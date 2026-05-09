#!/usr/bin/env bash
# profile.sh — samply wrapper for the perf harness (see spec §3.6)
#
# Usage:
#   ./perf/profile.sh record <duration_s> <name>
#   ./perf/profile.sh view <name>
#
# `record` attaches samply to the running tts-multiplexer and records for
# <duration_s> seconds. `view` opens a previously recorded profile in Firefox
# profiler.

set -euo pipefail

cmd="${1:-}"
shift || true

case "$cmd" in
  record)
    duration="${1:?usage: profile.sh record <duration_s> <name>}"
    name="${2:?usage: profile.sh record <duration_s> <name>}"

    if ! command -v samply >/dev/null 2>&1; then
      echo "samply not on PATH. Install with: cargo install samply" >&2
      exit 2
    fi

    pid=$(pgrep -f target/release-with-debug/tts-multiplexer || true)
    if [ -z "$pid" ]; then
      echo "tts-multiplexer not running (release-with-debug build)." >&2
      echo "Start it with:" >&2
      echo "  cargo build --profile release-with-debug" >&2
      echo "  ./target/release-with-debug/tts-multiplexer --port 9000 --metrics-port 9001 --backends 127.0.0.1:9100,...,127.0.0.1:9107" >&2
      exit 1
    fi
    if [ "$(echo "$pid" | wc -l | tr -d ' ')" != "1" ]; then
      echo "Multiple tts-multiplexer PIDs found:" >&2
      echo "$pid" >&2
      echo "Kill the spurious ones and retry, or pass --pid manually." >&2
      exit 1
    fi

    mkdir -p perf/runs
    out="perf/runs/${name}-profile.json.gz"
    echo "Recording PID $pid for ${duration}s into ${out}..."
    samply record --pid "$pid" --duration "$duration" -o "$out" --no-open
    echo "Done. View with: $0 view $name"
    ;;

  view)
    name="${1:?usage: profile.sh view <name>}"
    out="perf/runs/${name}-profile.json.gz"
    if [ ! -f "$out" ]; then
      echo "No profile at $out" >&2
      exit 1
    fi
    samply load "$out"
    ;;

  *)
    echo "Usage: $0 {record <duration_s> <name>|view <name>}" >&2
    exit 2
    ;;
esac
