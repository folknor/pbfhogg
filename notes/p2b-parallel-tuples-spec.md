# P2b: Parallel tuples for external join stage 2

## Goal

Parallelize zlib decompression in external join stage 2 (node join).
Currently single-threaded at 301s Europe, zlib-bound. The merge-join
consumer is inherently sequential (global cursor through sorted bucket
pairs), so only decompression + wire parsing can be parallelized.

## Realistic expectations

Stage 2 breaks down as ~160s decompression + ~141s merge-join.
Parallel decompression with 6 threads: ~27s. Merge-join stays ~141s.
Tuple overhead from double iteration: ~11s.

**Expected: 301s → ~179s (-40%).** Not 4-6x — merge-join is 47% of
stage 2 and cannot be parallelized without fusing it into workers
(much more complex, future work).

At planet scale: ~1000s → ~600s. Saves ~400s.

## Architecture

```
IO thread: BlobReader reads raw compressed blobs
  │ skip non-OsmData, skip non-node via indexdata
  │ fadvise(DONTNEED) behind read head (existing)
  │ assign sequence numbers ONLY to blobs that pass filter
  ▼
sync_channel(16) — (usize, Blob) objects
  │
  ▼
Dispatch thread: receives raw blobs, spawns rayon tasks
  │
  ▼
Rayon workers (via rayon::spawn + thread_local!):
  │ thread-local: decompress_buf: Vec<u8> (reused via Blob::decompress_into)
  │ per-block: tuples: Vec<NodeTuple> (fresh allocation, ~128 KB)
  │ blob.decompress_into(&mut decompress_buf)
  │ extract_node_tuples(&decompress_buf, &mut tuples)
  │ send (seq, Ok(tuples)) through channel
  │ on error: send (seq, Err(e)) through channel
  ▼
sync_channel(32) — (usize, Result<Vec<NodeTuple>>)
  │
  ▼
Consumer (main thread):
  │ ReorderBuffer delivers tuples in file order
  │ propagates first error from any worker
  │ for each block's tuples:
  │   for each NodeTuple { id, lat, lon }:
  │     merge-join against sorted_pairs (same logic as current)
  │     emit resolved entries to slot buckets
  │ early exit when all buckets exhausted (drops receiver,
  │   rayon tasks finish silently — bounded wasted work)
  ▼
Slot bucket files (same as current)
```

**Note:** 4 threads total: IO, dispatch, rayon workers, consumer. Same
pattern as pipeline.rs.

## Key design decisions

### Buffer ownership

Workers own the decompress buffer (thread-local, reused via `map_init`).
Workers create a fresh `Vec<NodeTuple>` per block (~128 KB for ~8000 nodes).
This Vec crosses the thread boundary to the consumer. Consumer drops it
after merge-join.

This is cross-thread alloc/free of ~128 KB Vecs. At 500K Europe blocks,
that's 64 GB cumulative churn. Based on the OOM investigation:

- PrimitiveBlock retention was from `Box<[(u32,u32)]>` at ~10 KB with
  varying sizes (stringtable entries). The allocator couldn't reuse slots.
- Uniform 128 KB `Vec<NodeTuple>` allocations should reuse size-class slots
  efficiently. The allocator sees the same size every time.

**Start simple.** Validate with sidecar (`--sidecar`, check anon RSS stays
flat). If retention shows up, add an `ArrayQueue<Vec<NodeTuple>>` object pool
shared between workers and consumer.

### Blob filter

The IO thread skips non-node blobs using indexdata (same as current):
```rust
if let Some(idx) = blob.index() {
    if !matches!(idx.kind, ElemKind::Node) { continue; }
}
```
This halves the blobs sent through the raw channel (~50% of Europe's PBF
is way/relation blobs).

### Reorder buffer

Same `ReorderBuffer` from `pipeline.rs`. Workers may complete out of order
(rayon scheduling). The reorder buffer delivers tuple Vecs in file order.
Capacity matches the decoded channel (32).

Since the PBF is sorted and dense nodes are delta-encoded in ascending
order within each block, the tuples within each Vec are already sorted
by node ID. File-order delivery guarantees global ascending order for
the merge-join.

### Merge-join integration

The consumer's merge-join loop is identical to the current code, except
it iterates `&tuples` instead of inline-decoded nodes:

```rust
for result in reorder_rx {
    let tuples = result?;
    for &NodeTuple { id, lat, lon } in &tuples {
        // ... exact same bucket advance + cursor + emit logic ...
    }
}
```

The +11s overhead from the tuple intermediary (measured on sequential path)
is the cost of this extra iteration. In the parallel version, this cost
is amortized against the ~120s saved from parallel decompression.

## Rayon integration

Use `rayon::spawn` with a dedicated thread pool (same pattern as pipeline.rs
stage 2 dispatch):

```rust
let decode_pool = rayon::ThreadPoolBuilder::new()
    .num_threads(decode_threads)
    .build()?;

for (seq, blob) in raw_rx {
    let tx = decoded_tx.clone();
    decode_pool.spawn(move || {
        // Thread-local init via thread_local! or manual tracking
        thread_local! {
            static DBUF: RefCell<Vec<u8>> = RefCell::new(Vec::new());
        }
        DBUF.with_borrow_mut(|dbuf| {
            dbuf.clear();
            // decompress blob into dbuf
            // extract_node_tuples(dbuf, &mut tuples)
            // send (seq, tuples)
        });
    });
}
```

Alternative: use `std::thread::scope` with explicit worker threads (per
perf-Codex suggestion). Simpler lifetime management, no rayon dependency
for the decode pool. But rayon is already used for stage 1's pipelined
reader and stage 4's batch assembly — adding another pool is consistent.

## Prerequisites

### Blob::decompress_into

New method needed on `Blob`:
```rust
pub(crate) fn decompress_into(&self, buf: &mut Vec<u8>) -> Result<()>
```

Decompresses the blob's data into a caller-owned Vec (clear + refill).
Enables thread-local buffer reuse — without it, each decompress allocates
a fresh ~2 MB Vec (500K × 2 MB = 1 TB allocator churn at Europe scale).

Delegates to the existing `decompress_blob_data_into(&self.blob, buf)`
internal function. ~3 lines of code.

## What changes from current code

### Stage 2 function signature — unchanged
`stage2_node_join(input, direct_io, node_buckets, slot_buckets, total_slots)`

### Inside stage2_node_join:

**Remove:**
- Sequential BlobReader loop
- Inline wire parsing (granularity, lat_offset, lon_offset, group_starts)
- `use crate::read::wire::...` imports

**Add:**
- IO thread (reads blobs, filters by indexdata)
- Rayon decode pool (decompresses, calls `extract_node_tuples`)
- Decoded channel with reorder buffer
- Consumer loop iterating `Vec<NodeTuple>` instead of inline decode

**Keep:**
- `load_next_bucket` / `load_coo_bucket_into` (bucket loading, unchanged)
- Merge-join logic (bucket advance, cursor, slot bucket writes)
- All debug_log! and emit_marker calls

### Shared code used:
- `commands::node_scanner::extract_node_tuples` (already shared)
- `commands::node_scanner::NodeTuple` (already shared)
- `reorder_buffer::ReorderBuffer` (already shared)
- `blob::BlobReader`, `blob::BlobType`, `blob_index::ElemKind`

## Validation plan

1. **Denmark correctness**: diff output against current sequential result (0 diffs expected)
2. **Denmark sidecar**: `--bench --sidecar`, check anon RSS flat, no retention growth
3. **Japan bench**: compare wall time to sequential baseline (42s for dense, ~6s for Denmark external)
4. **Europe sidecar**: `--bench --sidecar` on stages 1-2 (early exit after stage 2), check:
   - anon RSS stays bounded (~500 MB, not growing linearly)
   - Wall time ~179s (down from 301s)
   - Resolved count matches (4.69B)
5. If anon RSS grows: add `ArrayQueue` object pool, re-validate

## Future: fusing merge-join into workers

The merge-join is 47% of stage 2. If P2b shows this is the new bottleneck,
the next step is fusing merge-join into workers: each worker decompresses +
parses + merge-joins against the relevant portion of sorted_pairs for its
blob's node ID range. This requires:

- Sharing `sorted_pairs` across workers (read-only after sort)
- Knowing each blob's min/max node ID (from indexdata) to determine which
  bucket range it falls into
- Per-worker slot bucket writes (or a shared channel to the consumer)

This is significantly more complex and should only be attempted after P2b
proves the parallel decompress pattern works.
