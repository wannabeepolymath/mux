# Per-Stream Backpressure with Drop-Oldest

**Date:** 2026-05-04
**Status:** Approved (brainstorming → impl)
**Branch:** bottlenecks
**Spec:** assignment/README.md Part 1 ¶4

## Problem

The pre-existing client output path used a single `mpsc::channel<ClientEvent>(4096)`
per client connection — all streams on that connection shared one queue. Two
issues:

1. **Head-of-line blocking.** If the writer task was blocked sending stream A's
   chunks to a slow sink, stream B's `done` text frame sat in the channel
   waiting for A. Spec Part 2 explicitly forbids this: *"One stream failing
   must NOT affect other streams on the same connection."*
2. **Unbounded by bytes.** The channel was bounded by *count* (4096 messages),
   not bytes. With ~76KB chunks, a stream could buffer ~300MB before the cap
   kicked in. Spec Part 1 ¶4 requires per-stream cap (default 256KB) with
   *"drop the oldest chunks and log a warning"* — neither was implemented.

## Fix

New module `src/client/streams.rs` defines `ClientStreams`, a per-client actor
that owns:
- A `HashMap<StreamId, StreamQueue>` (per-stream queues with byte tracking).
- A `VecDeque<StreamId>` for round-robin fair scheduling.
- A `tokio::sync::Notify` for waking the writer.
- A `closed` flag for client-disconnect signaling.

Producers (forward task and dispatcher) push frames non-blockingly via
`enqueue(stream_id, event) -> Result<(), ClientGone>`. On binary overflow,
`enqueue` drops the *oldest binary frame* in the stream's queue (text control
frames are scanned past — never dropped). The writer task drains queues
round-robin into the WS sink.

### Why round-robin (not strict FIFO across streams)

Without fairness, a chatty stream could monopolize the writer and stall a
quieter stream's `done`. Round-robin guarantees each stream that has pending
work gets one event per cycle.

### Why text frames are never dropped

`queued`/`done`/`error` are control transitions. Dropping them would
silently break the protocol (client never sees `done` → believes the stream
is still in flight). The drop policy targets only binary audio chunks.

### Why non-blocking enqueue (vs await on full)

The pre-existing `client_tx.send().await` propagated backpressure all the way
back to the backend (TCP read buffer fills → TCP backpressure → backend
`ws.send().await` blocks → backend slows down). That's "wait" semantics. The
spec wants "drop oldest" semantics: backend produces full speed, mux discards
old chunks if client can't keep up. Forward task never blocks on enqueue.

### Client-gone signaling

Pre-existing code detected client disconnect via `client_tx.send().await`
returning Err (channel closed when receiver dropped). With non-blocking
enqueue, we preserve the signal: `enqueue` returns `Err(ClientGone)` when the
`closed` flag is set. Forward task translates that to `ForwardOutcome::ClientGone`,
identical to old behavior.

`handle_client` calls `client_streams.close()` on exit (graceful client close
or read error). The writer task also calls `close()` if `sink.send()` errors
(client TCP closed mid-write).

## Bonus fix discovered while testing

`forward::check_warm_slot_liveness` was flagging WebSocket Ping/Pong frames as
"pre-start binary" via the catch-all `Some(Some(Ok(_)))` arm. Pings can arrive
on idle warm slots (peer keepalive). With pre-#7 timing, pings rarely landed
inside a check window; with #7's faster turnaround, the false-positive rate
spiked and tanked correctness. Fix: explicitly match `Message::Ping(_)` and
`Message::Pong(_)` and treat them as alive (return None). Verified by reproducing
the issue, then watching all suites pass after the patch.

## Config

New CLI flag `--max-buffer-bytes-per-stream` (default `262144` = 256 KiB,
matching spec). Stored in `Config::max_buffer_bytes_per_stream`, threaded
through to `ClientStreams::new()` in `handle_client`.

## Tests

Unit tests in `client/streams.rs`:
- `push_under_budget_no_drop` — under-cap pushes accumulate without dropping.
- `overflow_drops_oldest_binary` — drops oldest binary on overflow.
- `text_frames_never_dropped` — text preserved across overflow.
- `drops_oldest_only_skipping_text` — drop scan walks past text to find oldest binary.
- `round_robin_dequeue_across_streams` — fairness.
- `round_robin_strict_alternation` — order is `a1, b1, a2, b2`, not `a1, a2, b1, b2`.
- `close_then_enqueue_returns_client_gone` — disconnect signaling.
- `writer_exits_on_close` — shutdown semantics.

Integration (calm mode, all PASS):
- `--correctness` — 6/6
- `--multiplex` — 5/5 (including isolation under error)
- `--slow-consumer` — 2/2 (including head-of-line test that was at risk pre-#7)

Chaos test variance is large (±20pp across runs both before and after this
change). No statistically-meaningful regression.

## Out of scope

- Per-byte backpressure to the backend (we explicitly drop instead).
- Per-stream queue cleanup on stream completion (entries grow up to
  `max_streams_per_conn` per client; freed when ClientStreams drops).
- Replacing `std::sync::Mutex` with `parking_lot` (would shave nanoseconds;
  not measurable).
