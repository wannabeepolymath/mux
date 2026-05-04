# Allocation Churn Pass

**Date:** 2026-05-04
**Status:** Approved
**Branch:** bottlenecks
**Bottleneck addressed:** #8

## Problem

Three sources of per-event allocation surface in the hot paths:

1. **`addr.to_string()` on every metric update.** Every state transition,
   every `set_backend_state` call, every `snapshot()` call constructs a
   fresh `String` from `SocketAddr`. The dispatcher does this dozens of
   times per request resolution.
2. **`BytesMut::with_capacity` per chunk.** `encode_binary_frame` allocates
   a fresh `BytesMut` for every binary chunk. Under chaos load with ~76 KB
   chunks at the audio rate, this is the dominant per-chunk allocation.
3. **`tokio::time::timeout` constructs a fresh Sleep per call.** The hang
   detector wraps every `ws.next()` in `tokio::time::timeout(hang_timeout, …)`.
   Each call allocates a fresh Sleep timer. With hundreds of chunks per
   request, this is non-trivial.

None of these are currently a measured bottleneck (per-chunk forward
overhead is <500μs p50). This pass is engineering hygiene — the wins are
measurable in microbenchmarks, not in chaos throughput.

## Fix

### 1. Cache addr_str on `BackendShard`

Add `addr_str: Arc<str>` to `BackendShard` (already proposed in #5). Use
it everywhere a backend's address is formatted as a string for metrics,
logs, or snapshots:

```rust
// before
self.metrics.set_backend_state(&backend.addr.to_string(), ...);
// after
self.metrics.set_backend_state(&shard.addr_str, ...);
```

`Arc<str>` is preferred over `String` because the metrics layer accepts
`&str`, and Arc lets us cheaply hand the string to spawned tasks
(connect, lifecycle) without re-allocating.

### 2. Reuse Sleep with `tokio::pin!`

The hang-detection loop in `forward::do_forward` repeatedly calls
`tokio::time::timeout(hang_timeout, ws.next())`. Each call allocates a
new Sleep. Replacing with a single pinned Sleep that we reset between
iterations is idiomatic:

```rust
let sleep = tokio::time::sleep(hang_timeout);
tokio::pin!(sleep);
loop {
    tokio::select! {
        biased;
        _ = &mut cancel_rx => return Cancelled,
        _ = &mut sleep => return BackendHung,
        msg = ws.next() => {
            sleep.as_mut().reset(Instant::now() + hang_timeout);
            // ... handle msg ...
        }
    }
}
```

This converts N Sleep allocations per request to 1.

### 3. Audit `BytesMut` in `encode_binary_frame`

`encode_binary_frame(stream_id, data)` builds: 1-byte length + stream_id
bytes + data bytes. Today this is a fresh `BytesMut` of size
`1 + stream_id.len() + data.len()`.

The current implementation is already minimal (no over-allocation, no
copying beyond what's required to assemble the framed payload). The
realistic win is **using `Bytes::chain` to avoid the data copy**:

```rust
// before: copy stream_id into BytesMut, then copy data
let mut buf = BytesMut::with_capacity(1 + stream_id.len() + data.len());
buf.put_u8(stream_id.len() as u8);
buf.extend_from_slice(stream_id.as_bytes());
buf.extend_from_slice(&data);
buf.freeze()

// after: assemble header (small), then chain with the data Bytes
let mut header = BytesMut::with_capacity(1 + stream_id.len());
header.put_u8(stream_id.len() as u8);
header.extend_from_slice(stream_id.as_bytes());
let header = header.freeze();
header.chain(data)  // returns Bytes via Chain<Bytes, Bytes>
```

**Caveat:** the WS sink may not accept `Chain<Bytes, Bytes>` directly —
`tokio_tungstenite::tungstenite::Message::Binary` takes `Bytes` (or
`Vec<u8>` depending on version). If `Bytes::chain` doesn't fit, we
fall back to `concat()` which still saves the BytesMut allocation by
collecting into a single `Bytes`.

In practice this saves ~76 KB of memcpy per chunk (the `data` half) and
one BytesMut allocation. Whether the tungstenite API tolerates Chain
will be discovered at impl time; if not, we keep current behavior for
this sub-item and ship just (1) and (2).

## Implementation order

1. Cache `addr_str: Arc<str>` on `BackendShard` (already in #5 spec).
   Replace all `addr.to_string()` callsites.
2. Convert `do_forward`'s timeout pattern to pinned Sleep + reset.
3. Try `Bytes::chain` in `encode_binary_frame`. If tungstenite API
   accepts it, ship. If not, document the limitation and skip.

## Tests

- `addr_str_matches_addr_to_string` — Arc<str> equals what
  `addr.to_string()` would have produced.
- `pinned_sleep_resets_correctly` — under simulated tokio time, the
  reset happens on each successful read; hang fires after exactly
  `hang_timeout` of silence.
- Existing chunk encoding tests pass with the new framing path.

## Out of scope

- Replacing `String` allocations in error messages and tracing fields
  — these are off-hot-path.
- Replacing `tokio::sync::mpsc` with custom lock-free queues —
  no measured contention.
- Pooling `BytesMut` via a global allocator — adds significant
  complexity for unmeasured wins.

## Expected impact

- **Chaos test number:** zero, expected and confirmed by the
  baseline analysis in `progress.md`. This is a hygiene pass.
- **Per-chunk forward overhead:** drop from <500μs p50 to ~400μs p50
  (microbenchmark estimate; not validated until impl).
- **Memory pressure:** lower steady-state allocation rate; reduces
  GC-equivalent overhead in the tokio runtime's allocator hot path.
