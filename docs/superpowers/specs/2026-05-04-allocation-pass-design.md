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

### 3. Audit `BytesMut` in `encode_binary_frame` — investigated, skipped

Investigation result: **the current implementation is already at the
floor** given the tungstenite API surface.

`tokio_tungstenite::tungstenite::Message::Binary` (v0.26) takes a single
contiguous `Bytes`. Any non-contiguous representation
(`bytes::Buf::Chain<Bytes, Bytes>`, `Vec<Bytes>`, etc.) must be flattened
back into one `Bytes` via `Buf::copy_to_bytes(len)` before the WS sink
accepts it — which copies the payload anyway, defeating the optimization.

The realistic minimum is exactly what we already do:

```rust
let mut buf = BytesMut::with_capacity(1 + id_len + payload.len()); // 1 alloc
buf.put_u8(id_len as u8);                                           // ~1 byte memcpy
buf.put_slice(id_bytes);                                            // ~10 byte memcpy
buf.put_slice(payload);                                             // payload memcpy
buf.freeze()                                                        // 0 copy
```

To eliminate the payload memcpy we'd need either a tungstenite API that
accepts a `Buf` of multiple chunks, or a custom raw-frame send path.
Both are bigger refactors than the wins justify (per-chunk forward
overhead is already <500μs p50). Skip this sub-item.

Sub-items (1) addr_str caching and (2) pinned Sleep reuse were the
actual improvements in this pass.

## Implementation order

1. Cache `addr_str: Arc<str>` on `BackendShard` (already in #5 spec).
   Replace all `addr.to_string()` callsites.
2. Convert `do_forward`'s timeout pattern to pinned Sleep + reset.
3. (Skipped — see §3 above.) Investigated `Bytes::chain` for
   `encode_binary_frame`; the tungstenite API requires contiguous
   `Bytes`, so the win evaporates after re-collection. Documented
   and skipped.

## Tests

- `addr_str_matches_addr_to_string` (in `backend::shard::tests`) — Arc<str>
  equals what `addr.to_string()` would have produced. Landed.
- The pinned-Sleep change is exercised by every chaos test since hang
  detection runs on every chunk read; behavior is verified via the
  existing chaos suites (no regression).
- No new test for the skipped `Bytes::chain` sub-item.

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
