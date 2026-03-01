# P4: Bounded Buffer Pool Retention Policy

## Problem Statement

The `DecompressPool` in `src/read/blob.rs` recycles decompression buffers via
`Bytes::from_owner` / `PooledBuffer::drop` but has no cap on retained buffer
count or total capacity. After processing unusually large blobs (up to the
spec limit of 32 MB decompressed), oversized buffers remain in the pool
indefinitely because `put()` unconditionally clears and pushes every returned
buffer. At planet scale (2.5M blobs, 64 GB RAM), this inflates tail RSS when
outlier blobs leave multi-megabyte buffers in a pool that typically needs only
~1.4 MB buffers.

**Goal:** Add size-classed caps for pooled decode buffers, drop oversized
returned buffers beyond cap. Target: improved RSS recovery after outlier blobs
with <=5% throughput regression on the pipelined read path.

---

## Current State

### DecompressPool implementation (`src/read/blob.rs`, lines 33-63)

```rust
pub(crate) struct DecompressPool {
    buffers: Mutex<Vec<Vec<u8>>>,
}
```

- **`new()`** (line 39): creates `Arc<Self>` with an empty `Vec<Vec<u8>>`.
- **`get()`** (line 48): locks mutex, pops a buffer (any buffer), returns empty
  Vec if pool is empty. Handles mutex poisoning gracefully via `.ok()`.
- **`put()`** (line 57): clears the buffer (`buf.clear()`), locks mutex, pushes
  unconditionally. No size check, no count check, no capacity trimming.

### Buffer lifecycle

1. **Acquire** (`pool_get`, line 86): pops from pool, calls `buf.reserve()` to
   ensure sufficient capacity for the expected decompressed size. If the pool is
   empty, allocates a fresh `Vec::with_capacity(capacity)`.

2. **Decompress** (`decompress_blob`, lines 1114-1153): decompresses into the
   buffer via `zlib_decompress_into()` or `zstd::stream::copy_decode()`. The
   buffer grows to fit the actual decompressed size.

3. **Wrap** (`pool_wrap`, line 98): wraps the `Vec<u8>` in a `PooledBuffer`
   struct, then `Bytes::from_owner(PooledBuffer { vec, pool })`. The `Bytes`
   handle is now the owner.

4. **Use** (`PrimitiveBlock::new`, block.rs line 351): the `Bytes` is stored in
   `PrimitiveBlock.buffer`. The `WireBlock<'static>` borrows from it via unsafe
   lifetime erasure. The buffer is held for the entire duration of element
   iteration.

5. **Return** (`PooledBuffer::drop`, line 78): when the `PrimitiveBlock` is
   dropped (after `block_fn` returns on the main thread), `Bytes` drops, which
   drops `PooledBuffer`, which calls `pool.put(std::mem::take(&mut self.vec))`.

### Cross-thread flow

- **get()** happens on rayon worker threads (decode pool, `pipeline.rs` line 130)
- **put()** happens on the main thread (consumer drops `PrimitiveBlock`)
- This cross-thread pattern is why `thread_local` pooling is impossible --
  buffers would accumulate on the main thread and starve workers.

### What makes this unbounded

1. **No count limit**: the `Vec<Vec<u8>>` grows without bound. Every returned
   buffer is retained. In steady state the pool stabilizes at the pipeline's
   concurrency level (DECODE_AHEAD=32 + rayon threads), but there is no explicit
   cap.

2. **No capacity check on return**: a buffer that grew to 32 MB (the
   `MAX_BLOB_MESSAGE_SIZE` limit) to hold an outlier blob is returned to the
   pool with 32 MB of reserved capacity. The next `get()` will return this
   oversized buffer even if only 1.4 MB is needed. The excess capacity is never
   released.

3. **No shrink-to-fit**: `put()` calls `buf.clear()` (sets `len = 0`) but does
   not reduce `capacity`. The `Vec` retains its allocation.

### Bounded memory in practice

In typical operation the pool is naturally bounded:

- Maximum in-flight buffers: `DECODE_AHEAD` (32) decoded blocks in the reorder
  channel + `BLOCK_QUEUE` (8) in the `into_blocks_pipelined` channel + 1 being
  processed by the consumer = ~41 buffers.
- Typical buffer size: ~1.4 MB decompressed.
- Steady-state pool memory: ~41 * 1.4 MB = ~57 MB.

The problem is not steady-state but transient: after an outlier blob inflates a
buffer to multi-megabyte sizes, that buffer stays in the pool at full capacity
forever.

---

## Blob Size Distribution Analysis

### Typical sizes

From code comments and profiling data across 7 regions:

| Metric | Value | Source |
|--------|-------|--------|
| Compressed blob (avg) | ~32 KB | reader.rs line 336, blob.rs line 866 |
| Decompressed blob (avg) | ~1.4 MB | blob.rs line 26, reader.rs line 266 |
| Max per spec | 32 MB decompressed | `MAX_BLOB_MESSAGE_SIZE`, blob.rs line 310 |
| Elements per block | 8000 (fixed) | `MAX_ENTITIES_PER_BLOCK`, block_builder.rs line 26 |
| Denmark blobs | 7,396 | region-profiles.md |
| Planet blobs | ~2.5M | box2-blob-decode.md |

### Why outliers exist

OSM PBF blocks contain 8000 entities each, but entity size varies dramatically:

- **Tagless nodes** (common in coastlines): ~20 bytes decompressed per entity.
  Block: ~160 KB. These are the majority of planet node blocks.
- **Dense tagged nodes** (Japan addressing, urban POIs): ~100-200 bytes per
  entity. Block: ~0.8-1.6 MB. Typical.
- **Ways with many refs** (Norway coastlines, 200-500 nodes): ~2-4 KB per way.
  Block: ~16-32 MB could theoretically happen but is rare because the 8000
  entity limit bounds it.
- **Relations with many members** (London TfL routes, 30-100 stops): ~0.5-8 KB
  per relation. Block: ~4-64 MB could happen in extreme cases.

In practice, the largest blocks are way blocks with long node reference lists
(Norway profile: 900 bytes/way) and relation blocks with many members (London
profile: 2.1 KB/relation). These can produce decompressed blocks of 7-17 MB --
significantly above the 1.4 MB average.

### Size distribution shape

The distribution is heavily right-skewed:
- P50: ~1.0-1.4 MB (typical node blocks)
- P90: ~2.0-3.0 MB (tagged node blocks, typical way blocks)
- P99: ~4-8 MB (dense way blocks with many refs, relation blocks)
- P99.9: ~8-16 MB (extreme way/relation blocks)
- Max: 32 MB (spec limit, vanishingly rare)

### Memory impact of outliers

Without eviction, a single 16 MB outlier buffer persists in the pool for the
rest of the file. At planet scale with ~2,500 outlier blobs (0.1%), the pool
could retain 40+ GB of excess capacity in extreme cases. More realistically,
since pool size is bounded by concurrency (~41 slots), the worst case is
~41 * 16 MB = ~656 MB of retained capacity where ~41 * 1.4 MB = 57 MB would
suffice. The excess 600 MB is pure waste.

---

## Design: Bounded Retention Policy

### Design principles

1. **Throughput first**: the pipelined read path is the hot path. Any retention
   policy must not measurably increase contention or allocation rates for typical
   (non-outlier) blobs.

2. **Simple is better**: avoid complex size-class hierarchies. The distribution
   has a clear mode (~1.4 MB) and a long tail. A single threshold separating
   "normal" from "oversized" captures 95% of the value.

3. **Cap retained capacity, not count**: the pool count is naturally bounded by
   pipeline concurrency. The problem is retained *capacity* per buffer, not
   buffer count.

4. **No preallocation**: the pool starts empty and fills organically. The
   retention policy only limits what happens on `put()`.

### Approach: capacity threshold on return

Add a single constant `MAX_RETAINED_CAPACITY` to `DecompressPool`. On `put()`,
if the buffer's capacity exceeds this threshold, drop it instead of returning
it to the pool.

```rust
/// Maximum capacity (in bytes) of a buffer retained in the pool.
/// Buffers larger than this are dropped on return instead of being recycled.
/// This prevents outlier blobs (up to 32 MB decompressed) from permanently
/// inflating the pool's retained memory.
///
/// Set to 4 MB: covers >99% of real-world PBF blocks (8000 elements at
/// typical sizes) while dropping the long tail of outlier blocks.
const MAX_RETAINED_CAPACITY: usize = 4 * 1024 * 1024;
```

#### Why 4 MB

- Covers the P99 of the decompressed size distribution (~4-8 MB boundary).
- A 1.4 MB buffer (typical) is always retained. A 2 MB buffer (tagged nodes,
  ways with moderate refs) is always retained. A 3.5 MB buffer (dense ways)
  is retained.
- An 8 MB buffer (outlier ways/relations) is dropped. A 16 MB buffer
  (extreme outlier) is dropped.
- The threshold is generous enough that normal variation does not cause churn.

#### Why not size classes

A size-class approach (e.g., 1 MB / 4 MB / 16 MB buckets with per-class caps)
adds complexity for marginal benefit:

- The pool is accessed under a `Mutex`. Adding bucket selection logic inside the
  critical section increases hold time.
- The access pattern (pop-any on get, push-one on put) does not benefit from
  class-aware selection. The caller always calls `reserve()` to grow the buffer
  to the needed capacity anyway.
- The decompressed size distribution has a single mode. There is no bimodal
  pattern that would benefit from separate class pools.

A single threshold captures the design intent: keep typical buffers, drop
outliers.

### Alternative considered: shrink_to on return

Instead of dropping oversized buffers, `shrink_to(MAX_RETAINED_CAPACITY)` could
reduce their capacity while keeping them in the pool. This preserves the buffer
count but requires a `realloc` (potential `memcpy` of up to
`MAX_RETAINED_CAPACITY` bytes) inside the critical section.

**Rejected**: the `realloc` cost is 1-10 us (for 4 MB copy), which is 100-1000x
longer than the current critical section (~10 ns). This would measurably
increase contention. Simply dropping the buffer is O(1) for the `free()` call
and the pool will replenish from the next `get()` that finds it empty.

### Alternative considered: pool count cap

Cap the number of buffers in the pool (e.g., max 48). When the pool is full,
drop the returned buffer.

**Rejected as primary mechanism**: the pool count is already naturally bounded
by pipeline concurrency (~41 in-flight). A count cap would only trigger during
shutdown when all in-flight blocks are returned simultaneously. The real problem
is per-buffer capacity, not buffer count.

**Accepted as secondary guard**: a count cap is cheap to check and provides
defense-in-depth. Add `const MAX_POOL_SIZE: usize = 64` as a secondary cap.

---

## Implementation Plan

### Step 1: Add constants

In `src/read/blob.rs`, add two constants near the `DecompressPool` struct:

```rust
/// Maximum capacity (bytes) of a buffer retained in the pool.
/// Buffers larger than this are dropped on return instead of recycled.
const MAX_RETAINED_CAPACITY: usize = 4 * 1024 * 1024;

/// Maximum number of buffers retained in the pool.
/// Defense-in-depth: prevents unbounded pool growth if pipeline topology changes.
const MAX_POOL_SIZE: usize = 64;
```

### Step 2: Modify `DecompressPool::put()`

Change from:

```rust
fn put(&self, mut buf: Vec<u8>) {
    buf.clear();
    if let Ok(mut v) = self.buffers.lock() {
        v.push(buf);
    }
}
```

To:

```rust
fn put(&self, mut buf: Vec<u8>) {
    // Drop oversized buffers instead of retaining them.
    // This prevents outlier blobs (up to 32 MB) from permanently
    // inflating the pool's memory footprint.
    if buf.capacity() > MAX_RETAINED_CAPACITY {
        return; // buf dropped here, memory freed
    }
    buf.clear();
    if let Ok(mut v) = self.buffers.lock() {
        if v.len() < MAX_POOL_SIZE {
            v.push(buf);
        }
        // else: buf dropped here, pool is full
    }
}
```

The capacity check is *before* the lock, avoiding any critical section overhead
for oversized buffers. The count check is inside the lock (must read `v.len()`)
but is a single comparison.

### Step 3: No changes to `get()` or `pool_get()`

`get()` pops whatever buffer is available. `pool_get()` reserves additional
capacity if needed. No changes required.

### Step 4: No changes to `pool_wrap()` or `PooledBuffer`

The drop path calls `put()` which now has the eviction logic. No other changes
needed in the buffer lifecycle.

### Step 5: No changes to `pipeline.rs`

The pipeline creates the `DecompressPool` and shares it via `Arc`. The
retention policy is entirely internal to the pool.

### Total diff: ~10 lines changed, 2 constants added

---

## Impact Analysis

### Pipelined read path (hot path)

The typical case (buffer capacity <= 4 MB) adds zero overhead: the capacity
check (`buf.capacity() > MAX_RETAINED_CAPACITY`) is a single comparison that
is always-false-predicted by the branch predictor.

The outlier case (buffer capacity > 4 MB) drops the buffer instead of returning
it. The next `get()` on a rayon worker will find the pool empty (or pop a
normal-sized buffer) and allocate a fresh buffer. The allocation cost (~100 ns
for a 1.4 MB Vec) is negligible relative to the subsequent decompression
(~200-400 us).

**Expected throughput impact**: unmeasurable. The outlier case occurs <0.1% of
the time, and the allocation cost is <0.001% of the decompression cost.

### Mutex contention

No change. The critical section is still ~10 ns (Vec pop/push + one comparison).
For oversized buffers, the lock is not even acquired. Contention probability
remains <0.01% (from box2-blob-decode.md analysis).

### Thread-local ZlibDecoder

The `ZLIB_DECOMPRESS` thread-local (blob.rs line 16) is unaffected. It pools
the `flate2::Decompress` inflate state (~32 KB), not the output buffer. The
Decompress is reset via `reset(true)` between blobs. Its memory footprint is
fixed at ~32 KB per thread regardless of blob size. **No bounding needed.**

### Memory savings

Worst case before: ~41 buffers * 16 MB = 656 MB pool capacity.
Worst case after: ~41 buffers * 4 MB = 164 MB pool capacity.
Typical case: unchanged (~57 MB).

The savings manifest as RSS recovery: after processing a sequence of outlier
blobs, the pool sheds its oversized buffers and RSS drops back to steady state
within seconds (as the consumer drops `PrimitiveBlock`s holding the oversized
`Bytes`).

---

## Measurement Strategy

### Before/after validation

1. **Throughput**: `brokkr bench read --modes pipelined` on Denmark and Japan.
   Expect <1% difference (within noise).

2. **Roundtrip correctness**: `brokkr check -- --ignored` to run the full
   Denmark roundtrip test. Must pass.

3. **RSS tracking**: add a hotpath measurement or external RSS sampler during
   `brokkr bench read --modes pipelined --dataset japan`. Compare peak RSS
   and tail RSS (after processing) between old and new.

### Observability (optional, for debugging)

If debugging is needed, temporarily add an atomic counter to track evictions:

```rust
pub(crate) struct DecompressPool {
    buffers: Mutex<Vec<Vec<u8>>>,
    #[cfg(debug_assertions)]
    evicted: std::sync::atomic::AtomicU64,
}
```

Increment on eviction, log at pool drop. Not for production.

### Planet-scale validation

Run `brokkr bench read --modes pipelined --dataset north-america` (18.8 GB)
and compare peak RSS vs steady-state RSS. The north-america dataset has the
highest likelihood of containing outlier blobs due to its size and diversity
(US road network has ways with many node refs).

---

## Risk Assessment

### Over-aggressive eviction

If `MAX_RETAINED_CAPACITY` is too low (e.g., 1 MB), many normal blobs will
produce buffers that exceed the threshold, causing constant pool misses and
fresh allocations. This would degrade throughput.

**Mitigation**: 4 MB covers >99% of real-world blocks. The threshold is 2.8x
the average decompressed size. Only genuinely unusual blocks trigger eviction.

**Safety net**: if profiling shows excessive pool misses, increase the threshold.
The constant is a single line to change.

### Too-generous retention

If `MAX_RETAINED_CAPACITY` is too high (e.g., 16 MB), it effectively disables
the policy and the RSS issue persists.

**Mitigation**: 4 MB is well below the P99.9 of the distribution. It captures
the design intent without being so generous as to be useless.

### Pool starvation after outlier sequence

If many outlier blobs arrive in a burst (e.g., a sequence of large relation
blocks), the pool drains as all returned buffers are evicted. Subsequent
`get()` calls allocate fresh buffers, which also exceed the threshold and are
evicted on return. This creates temporary allocation churn.

**Analysis**: this is the correct behavior. The alternative (retaining oversized
buffers) would consume more memory than the allocation churn costs. The churn
is bounded: as soon as normal-sized blobs resume, the pool refills with
appropriately-sized buffers. For a planet file, outlier sequences are rare
and short (tens of blobs, not thousands).

### Interaction with `reserve()` in `pool_get()`

`pool_get()` calls `buf.reserve(capacity.saturating_sub(buf.capacity()))` which
may grow a returned buffer beyond `MAX_RETAINED_CAPACITY` if the blob's
`raw_size` exceeds 4 MB. This is fine: the buffer serves its purpose for
decompression, and when returned via `put()`, the eviction policy drops it.
The cycle is: get small buffer -> grow for outlier -> decompress -> use -> drop
on return -> next get allocates fresh. This is exactly the desired behavior.

---

## Summary

| Property | Value |
|----------|-------|
| Lines changed | ~10 |
| New constants | `MAX_RETAINED_CAPACITY = 4 MB`, `MAX_POOL_SIZE = 64` |
| Files modified | `src/read/blob.rs` only |
| Hot path overhead | unmeasurable (single branch, always-predicted) |
| Worst-case RSS savings | ~500 MB (41 * 12 MB excess per buffer) |
| Typical RSS impact | none (typical buffers are below threshold) |
| Risk | low (generous threshold, easy to tune) |
| Dependencies | none |
| Test requirements | existing roundtrip + pipelined benchmarks |
