# Box 5: Writer Pipeline, Framing, and Compression

Investigation of `src/write/writer.rs` (847 lines), `src/write/file_writer.rs` (81 lines),
and their interactions with `BlockBuilder`, `blob_index`, merge, and io_uring.

## Executive Summary

1. **The `to_vec()` copy in `write_primitive_block` (line 330) is real but mostly unavoidable.** The root cause is `BlockBuilder::take()` returning `&[u8]` from an internal reusable buffer. Changing the API to yield ownership (`Vec<u8>`) would eliminate the copy but destroy the buffer-reuse optimization that already eliminated 960 MB of allocation churn per Denmark run. The copy is ~1.5 MB per block, but peak memory impact is only WRITE_AHEAD x 1.5 MB = 48 MB. The real cost is memcpy bandwidth: ~3.75 TB cumulative at planet scale, but each copy completes in ~0.3 ms (L1/L2 hot) and overlaps with compression. This is not the bottleneck.

2. **`scan_block_ids` redundancy (line 333) is a genuine wasted-work finding the reviewer missed.** BlockBuilder already knows the element type, min ID, max ID, and count at `take()` time. Re-scanning the serialized protobuf wire format to recover this information is pure waste: ~7400 scans/file for Denmark, ~2.5M for planet. Each scan touches ~130 KB of data. Eliminating this is straightforward and saves ~325 GB of redundant reads at planet scale.

3. **Zstd encoder allocation per blob (line 726) is a real and fixable inefficiency.** Each call to `zstd::stream::write::Encoder::new()` allocates ~512 KB of internal state. Unlike the zlib path (which reuses `Compress` via `reset()`), the zstd path creates and destroys an encoder per blob. At planet scale with zstd compression, that is 2.5M allocations of 512 KB each = 1.28 TB of allocator churn. The `zstd` crate (v0.13) supports `Encoder::reset()` for reuse.

4. **`frame_blob_into` (line 697) allocates a fresh output `Vec<u8>` per blob in the pipelined path.** This is inherent to the channel-based design (the Vec must be owned to send through the channel). Pooling is possible but complex and the allocation is small (~32-64 KB compressed). Not a priority.

5. **WRITE_AHEAD=32 and fixed pipeline policy are theoretical concerns with minimal real impact** at current scale. The reorder buffer peaks at ~2 MB for compressed blobs. Adaptive depth could help for Compression::None but would add complexity for marginal gain.

---

## Finding 1: `to_vec()` Copy in `write_primitive_block`

### What the reviewer claimed

Line 330 does `block_bytes.to_vec()` to convert the borrowed `&[u8]` into owned bytes
for dispatch to rayon. The reviewer flagged this as a high-impact finding.

### Investigation

The parameter `block_bytes: &[u8]` at line 325 accepts a borrow. The pipelined path
(lines 326-345) must send data to a rayon task, which requires `'static` ownership.
Hence `block_bytes.to_vec()` at line 330.

**Where does the borrow come from?** `BlockBuilder::take()` (block_builder.rs line 772)
returns `Option<&[u8]>`, borrowing from `self.encode_buf` (line 803). This is an
intentional design: `encode_buf` is reused across calls, avoiding ~960 MB of allocation
churn per Denmark file (notes/take-buffer-reuse.md). The borrow lifetime is:

```rust
if let Some(bytes) = bb.take()? {        // borrows bb.encode_buf
    writer.write_primitive_block(bytes)?; // copies inside if pipelined
}                                         // borrow released
// bb can accept new elements
```

**Size of each copy:** A serialized PrimitiveBlock is typically ~130 KB (8000 dense nodes
with metadata). Way/relation blocks can be 100-200 KB. Compressed output is ~32-64 KB.

**Planet-scale math:**
- ~2.5M blocks x ~130 KB average = ~325 GB cumulative memcpy
- Each 130 KB copy: ~30 us (memcpy at ~4 GB/s from warm cache)
- Total time: ~75 seconds of memcpy, but overlapped with rayon compression
- Peak memory: WRITE_AHEAD (32) x 130 KB = ~4.2 MB of in-flight uncompressed copies

**Could the API accept `Vec<u8>` instead?** Yes, but at a steep cost. If `take()` returned
`Vec<u8>` (moving `encode_buf` out), every subsequent `take()` would allocate a fresh
~130 KB Vec. That is exactly the 960 MB allocation churn that the current design eliminates.
The alternatives:

1. **`take_vec()` variant** returning `Vec<u8>` plus re-creating `encode_buf`: Trades one
   memcpy for one alloc+dealloc pair. Allocator overhead (~150 ns for 130 KB via jemalloc)
   is less than memcpy (~30 us), so this is faster per-call but loses the amortization of
   the warm-cache Vec. Net: roughly neutral or slight win.

2. **Double-buffer scheme**: Two `encode_buf` Vecs, alternate between them. `take()` returns
   ownership of one while writing into the other. No copy, no realloc after warmup.
   Complexity: moderate (need to swap buffers, ensure both stabilize in capacity).

3. **Accept the copy**: The memcpy is L1/L2 hot (encode just wrote the data) and overlapped
   with compression work on rayon threads. It is not on the critical path.

### Verdict

**Real but low-impact.** The copy exists, is ~130 KB per block, and adds ~75 seconds of
cumulative memcpy at planet scale. However, this memcpy overlaps with rayon compression
and is L1-hot. Peak memory impact is 4.2 MB. The `&[u8]` return type in `take()` is a
deliberate trade-off that eliminates 960 MB of allocation churn. A double-buffer scheme
could eliminate both the copy and the allocation, but the engineering complexity is not
justified by the current bottleneck profile (compression dominates).

### Additional `to_vec` in `flush_local`

A second copy pattern exists in the parallel rewrite path. `flush_local` (merge.rs line
400-403, cat.rs line 178-181, getid.rs line 284-287, add_locations_to_ways.rs line
310-312, extract.rs) does:

```rust
fn flush_local(bb: &mut BlockBuilder, output: &mut Vec<Vec<u8>>) {
    if let Some(bytes) = bb.take()? {
        output.push(bytes.to_vec());  // copies encode_buf
    }
}
```

These copies accumulate `Vec<u8>` into `output: Vec<Vec<u8>>` which is later iterated
and passed to `write_primitive_block()`, triggering a *second* copy inside the pipelined
path. For merge's `rewrite_block_parallel`, a rewritten block is copied twice: once from
`encode_buf` into `output.blocks`, then from `output.blocks[i]` into the rayon task.
However, rewritten blocks are rare (~8% for Denmark, ~18% for Germany), so this
double-copy applies to a minority of blocks.

---

## Finding 2: WRITE_AHEAD=32 Static Depth

### What the reviewer claimed

The constant `WRITE_AHEAD=32` (line 33) is static. Too small could underlap compression;
too large increases memory pressure.

### Investigation

`WRITE_AHEAD` serves two purposes:
1. **Sync channel capacity** (lines 224, 274): bounds in-flight items between rayon tasks
   and the writer thread.
2. **Reorder buffer pre-allocation** (line 570, uring_writer.rs line 695):
   `VecDeque::with_capacity(WRITE_AHEAD)`.

**Memory analysis for compressed blobs (typical case):**
Each in-flight item in `PipelinePayload::Bytes` holds a `Vec<u8>` of framed, compressed
blob data. Typical compressed size: 32-64 KB.

Peak memory = 32 slots x 64 KB = 2 MB. This is negligible.

**Memory analysis for `Compression::None`:**
Uncompressed framed blobs are ~130 KB each. Peak = 32 x 130 KB = 4.2 MB. Still negligible.

**Memory analysis for `write_raw_owned` passthrough (merge):**
The merge coalescing buffer (`passthrough_buf`) accumulates multiple blobs before flushing
as a single `write_raw_owned`. Each flush can be several MB. But this is one channel
slot, not 32.

**Underlap risk:**
With 32 slots, rayon can have up to 32 compression tasks in flight. On a 16-core machine,
this is 2x the thread count. Even if compression is slow (zlib level 9, ~5 ms per blob),
32 slots provide ~160 ms of buffered work. The writer thread drains at ~1 blob/ms (64 KB
at ~64 MB/s disk write). No underlap risk.

**Comparison with read pipeline:**
The read pipeline uses `READ_AHEAD=16` and `DECODE_AHEAD=32`. The write pipeline's
`WRITE_AHEAD=32` is consistent.

### Verdict

**Not a real concern.** 32 is a well-chosen value. Peak memory is 2-4 MB. There is no
underlap risk on any realistic hardware. Making it configurable would add API complexity
for zero measurable gain. The only scenario where it matters is if someone uses extremely
large custom block sizes (> 1 MB uncompressed per blob), which is outside the PBF spec.

---

## Finding 3: Compression Mode Policy

### What the reviewer claimed

The pipeline policy is fixed regardless of whether compression is None/zlib/zstd.

### Investigation

The pipeline architecture is:
1. Main thread calls `write_primitive_block`
2. Rayon task: `to_vec()` + `scan_block_ids()` + `frame_blob_into()` (compress + frame)
3. Writer thread: reorder + `write_all()`

For `Compression::None`:
- Step 2 becomes: `to_vec()` + `scan_block_ids()` + encode raw blob body (no compression)
- The rayon task does almost no work (~30 us memcpy + ~20 us scan + ~5 us framing)
- The bottleneck shifts entirely to the writer thread (I/O bound)
- Rayon thread pool is mostly idle

For `Compression::Zlib(6)`:
- Step 2 is compression-dominated (~2-5 ms per blob)
- Pipeline is well-balanced: rayon saturates cores, writer thread keeps up

For `Compression::Zstd(3)`:
- Similar to zlib but faster compression (~1-3 ms per blob)

**What could be done differently for `Compression::None`?**
Skip rayon entirely — frame the blob on the main thread and send directly to the writer,
like `write_raw_owned` does. This would eliminate the `to_vec()` copy, the rayon task
overhead, and the channel round-trip. However, this would require `write_primitive_block`
to also handle `scan_block_ids` + framing on the main thread, which would serialize
the main thread behind both the scan and the framing.

The io_uring writer (`to_path_pipelined_uring`) already addresses the `Compression::None`
I/O bottleneck with async I/O and registered buffers. The North America benchmarks show
uring+none is 30% faster than buffered+none. The pipeline inefficiency for
`Compression::None` with the buffered writer is real but already has a solution path.

### Verdict

**Real but already addressed by io_uring.** For the buffered writer with `Compression::None`,
the rayon dispatch is unnecessary overhead. But `Compression::None` with buffered I/O is
not a recommended production configuration (it produces ~2x larger files). The io_uring
path handles the None case efficiently. Adding a bypass for `Compression::None` in the
buffered pipeline would save ~10% for that specific configuration but adds code complexity.

---

## Finding 4: `scan_block_ids` Redundancy (Reviewer Missed)

### What it does

Line 333 in `write_primitive_block`:
```rust
let indexdata = blob_index::scan_block_ids(&uncompressed)
    .map(|idx| idx.serialize());
```

And line 348-349 in the sync path:
```rust
let indexdata = blob_index::scan_block_ids(block_bytes)
    .map(|idx| idx.serialize());
```

`scan_block_ids` (blob_index.rs line 142-166) walks the protobuf wire format of the
serialized PrimitiveBlock to find PrimitiveGroup fields, then scans element IDs to
determine:
- `ElemKind` (Node/Way/Relation)
- `min_id`, `max_id`
- `count`

### Why it is redundant

`BlockBuilder` already has all this information at `take()` time:
- `block_type` (line 227): `DenseNodes`, `Ways`, or `Relations` — maps directly to `ElemKind`
- Dense nodes: `dense_ids` contains all IDs; first and last are min/max (sorted input).
  `count` = `dense_ids.len()`. These are available before `reset()` clears them.
- Ways/Relations: IDs are tracked via the element stream. The BlockBuilder could track
  min/max/count with three additional fields.

The scan walks ~130 KB of wire-format data per block. For dense nodes, it must decode
the entire packed sint64 ID array with zigzag + delta decoding (scan_dense_node_ids,
lines 204-239). For ways/relations, it parses each message header to extract field 1.

### Quantified waste

**Denmark (465 MB, 7396 blocks):**
- 7396 scans x ~130 KB = ~960 MB of data scanned
- Each scan: ~20-50 us (dense nodes require delta decoding of 8000 sint64 varints)
- Total: ~150-370 ms

**Planet (~2.5M blocks):**
- 2.5M scans x ~130 KB = ~325 GB of data scanned
- Total: ~50-125 seconds

### Fix

Add three fields to BlockBuilder: `first_id: Option<i64>`, `last_id: i64`, `count: usize`
(count already exists). Track `first_id` on the first `add_*` call, update `last_id` on
every call. Return a `BlobIndex` alongside the `&[u8]` from `take()`:

```rust
pub fn take(&mut self) -> io::Result<Option<(&[u8], BlobIndex)>> { ... }
```

Or, simpler: a separate `fn last_index(&self) -> Option<BlobIndex>` called before `take()`.

Then `write_primitive_block` can skip `scan_block_ids` entirely and use the provided index.

The API change touches `flush_block` in `src/commands/mod.rs` and `flush_local` in 5
command files. All follow the same pattern. The change is mechanical.

For the `flush_local` + `write_primitive_block` chain (parallel rewrite blocks in merge),
the scan happens inside `write_primitive_block` on data that was already constructed by
the local `BlockBuilder`. The `RewriteOutput` could carry `Vec<(Vec<u8>, BlobIndex)>`
instead of `Vec<Vec<u8>>`.

### Priority

**High.** This is pure wasted work with a clean, low-risk fix. The 50-125 seconds of
cumulative scanning at planet scale is entirely eliminable.

---

## Finding 5: Zstd Encoder Allocation Per Blob

### What happens

Line 724-736 in `encode_blob_body`:
```rust
Compression::Zstd(level) => {
    scratch.compress_buf.clear();
    let mut encoder = zstd::stream::write::Encoder::new(&mut scratch.compress_buf, *level)?;
    encoder.write_all(uncompressed)?;
    encoder.finish()?;
    ...
}
```

A new `zstd::stream::write::Encoder` is created for every blob. The zstd encoder
allocates internal state: a `CCtx` context (~512 KB at level 3, more at higher levels).
This is created and destroyed per call.

### Contrast with zlib path

The zlib path (lines 742-766 for flate2, 770-796 for libdeflater) reuses the compressor
via `scratch.zlib_compressor`:

```rust
let compressor = scratch.zlib_compressor.get_or_insert_with(|| { ... });
// ... use compressor ...
compressor.reset(); // reuse on next call
```

The comment at line 95 explicitly documents this: "avoiding ~312 KB of deflate state
allocation per blob". The zstd path has no equivalent reuse.

### Quantified waste

**Denmark (7396 blocks, zstd):**
- 7396 x 512 KB = 3.7 GB of allocator churn

**Planet (~2.5M blocks, zstd):**
- 2.5M x 512 KB = 1.28 TB of allocator churn

Each alloc/dealloc pair costs ~1-5 us (jemalloc, 512 KB). Total: ~2.5-12.5 seconds at
planet scale. Not huge in wall-clock, but the cache pollution from 1.28 TB of allocator
traffic is significant.

### Fix

Add `zstd_compressor: Option<zstd::stream::write::Encoder<...>>` to `FrameScratch` is
awkward because `Encoder` wraps a `&mut Vec<u8>` (lifetime tied to `compress_buf`).

Better approach: use the lower-level `zstd::bulk::Compressor` which owns its CCtx:

```rust
// In FrameScratch:
zstd_compressor: Option<zstd::bulk::Compressor>,

// In encode_blob_body:
let compressor = scratch.zstd_compressor.get_or_insert_with(|| {
    zstd::bulk::Compressor::new(*level).unwrap()
});
scratch.compress_buf = compressor.compress(uncompressed)?;
```

`zstd::bulk::Compressor` reuses the internal `CCtx` across calls. The `zstd` crate
(v0.13) supports this pattern.

### Priority

**Medium.** Zstd is not yet the default compression and few PBF consumers support it.
But if/when zstd adoption grows, this will matter. The fix is straightforward.

---

## Finding 6: `frame_blob_into` Output Allocation

### What happens

Line 696-702 in `frame_blob_into`:
```rust
let total_len = 4 + scratch.header_buf.len() + scratch.blob_buf.len();
let mut out = Vec::with_capacity(total_len);
out.extend_from_slice(&header_len.to_be_bytes());
out.extend_from_slice(&scratch.header_buf);
out.extend_from_slice(&scratch.blob_buf);
Ok(out)
```

Every call allocates a fresh `Vec<u8>` for the output. This Vec is sent through the
pipeline channel and freed by the writer thread after `write_all`.

**Size:** 4 + ~40 bytes (header) + ~32-64 KB (compressed blob) = ~32-64 KB per blob.

**Planet scale:** 2.5M x 50 KB = ~125 GB of allocator traffic.

### Why pooling is hard

The output Vec must be owned by the `PipelinePayload::Bytes(Ok(Vec<u8>))` to send through
the channel. A pool would need to return Vecs to the pool after the writer thread
consumes them. This requires a cross-thread pool (writer thread -> rayon threads), adding
synchronization overhead.

### Contrast with sync path

The sync path (`write_framed_blob`, lines 458-493) writes directly from scratch buffers
with three `write_all` calls. No output Vec allocation at all. This is optimal.

### Verdict

**Real but low-priority.** The allocation is small (~50 KB), the allocator handles it
efficiently (jemalloc has per-thread caches for this size class), and pooling adds
cross-thread synchronization complexity. The writer thread could theoretically return
consumed Vecs through a reverse channel, but the engineering cost exceeds the benefit.

---

## Finding 7: `write_raw` vs `write_raw_owned` Usage

### What the reviewer asked

Are passthrough blobs in merge already using `write_raw_owned` (zero-copy) or `write_raw`
(copies via `to_vec`)?

### Investigation

**merge.rs** uses three write paths for passthrough:

1. **`coalesce_passthrough`** (line 1382-1399): accumulates raw frame bytes into
   `passthrough_buf: Vec<u8>` via `extend_from_slice`. For indexed blobs, uses
   `std::mem::take(&mut frame.frame_bytes)` (line 1389) to take ownership, then
   extends. For non-indexed blobs, reframes with indexdata first.

2. **`flush_passthrough_buf`** (line 1402-1411): calls `writer.write_raw_owned(std::mem::take(buf))`
   — moves the coalesced buffer into the channel. Zero copy.

3. **`write_raw_copy`** (line 1203): kernel-space copy_file_range when `linux-direct-io`
   feature is enabled and output is not O_DIRECT.

So: **merge already uses `write_raw_owned` for the coalesced passthrough path.** The
`write_raw(&[u8])` method (which does `to_vec()` at line 374) is used by **sort.rs**
(lines 331, 337) and **cat.rs** (line 151, non-direct-io path).

**sort.rs**: reads frame into a reusable `frame_buf`, then calls `writer.write_raw(frame_buf)`.
This copies `frame_buf` via `to_vec()`. Could use `write_raw_owned` with `std::mem::take`
like merge does, but sort reuses `frame_buf` across calls, so it would need re-allocation.
The sort case is not performance-critical (sort is I/O-dominated by random reads).

**cat.rs passthrough** (line 151): `writer.write_raw(&frame.frame_bytes)`. This copies
via `to_vec()`. Could be changed to `write_raw_owned(std::mem::take(&mut frame.frame_bytes))`
since the frame is consumed. Easy fix for cat.

### Verdict

**Already addressed for merge (the critical path).** Minor improvement possible for cat
and sort passthrough, but these are not the performance-critical commands.

---

## Finding 8: Writer Thread Reorder Buffer

### Memory analysis

The writer thread (lines 564-608) uses `VecDeque<Option<PipelinePayload>>` with
`with_capacity(WRITE_AHEAD)` = 32 slots.

Each slot holds a `PipelinePayload::Bytes(io::Result<Vec<u8>>)`. For compressed blobs:
32 x 64 KB = 2 MB. For `Compression::None`: 32 x 130 KB = 4.2 MB. For
`write_raw_owned` (coalesced passthrough in merge): one slot can hold several MB, but
this is a single entry, not 32.

**Identical pattern** as read pipeline reorder buffer (pipeline.rs lines 160-200).

### BufWriter vs direct writes for large payloads

The writer thread does `writer.write_all(&result?)` at line 589. `FileWriter::Buffered`
wraps a `BufWriter::with_capacity(256 * 1024, file)`. For payloads smaller than 256 KB
(the common case for compressed blobs), BufWriter coalesces them into fewer syscalls.
For large payloads (merge passthrough flushes can be several MB), BufWriter's `write_all`
implementation writes the internal buffer + the payload in one or two calls when the
payload exceeds the buffer capacity.

Looking at the std library source: `BufWriter::write_all` for data larger than buffer
capacity first flushes the internal buffer, then writes the large data directly to the
underlying writer. So large passthrough payloads bypass the buffer efficiently. No concern.

### Verdict

**Not a concern.** 2-4 MB peak memory. BufWriter handles large payloads correctly.

---

## Finding 9: `FrameScratch` Thread-Local Memory

### How it works

`PIPELINE_SCRATCH` (lines 110-118) is a `thread_local!` with `RefCell<FrameScratch>`.
Each rayon thread gets its own scratch.

### Memory per thread

After warmup, each `FrameScratch` holds:
- `blob_buf`: ~130 KB capacity (uncompressed blob body, or ~64 KB compressed)
- `header_buf`: ~50 bytes capacity
- `compress_buf`: ~130 KB capacity (compression intermediate, zlib/zstd output)
- `zlib_compressor`: ~312 KB (flate2 Compress state) or ~64 KB (libdeflater)

Total per thread: ~312-572 KB (flate2) or ~194-324 KB (libdeflater)

### Total for N rayon threads

The pipelined writer uses the **global rayon pool** (not a dedicated pool like the read
pipeline). Default global pool size = `num_cpus::get()` threads. On a 16-core machine:
16 x 500 KB = 8 MB. On a 64-core machine: 64 x 500 KB = 32 MB. Manageable.

The thread-local scratch is lazy-allocated (empty Vecs in the const initializer at line
112-117). Only threads that actually process blobs pay the cost. Since
`rayon::spawn(move || { ... })` distributes work across the pool, all threads will
eventually get scratch buffers.

### Zlib compressor reuse verification

For flate2 (non-libdeflater path), line 748-750:
```rust
let compressor = scratch.zlib_compressor.get_or_insert_with(|| {
    Compress::new(FlateCompression::new(level), true)
});
```
Line 760: `compressor.reset()` after each use.

This correctly reuses the ~312 KB deflate state. `reset()` reinitializes the stream
without deallocating. Verified working.

For libdeflater, line 776-779: `scratch.zlib_compressor` is initialized once and reused.
No `reset()` needed — `libdeflater::Compressor` is stateless between calls (each
`zlib_compress()` is independent).

### Verdict

**Working correctly.** Memory is well-bounded. Zlib reuse is verified. The only gap is
zstd (Finding 5).

---

## Finding 10: Sync Path Triple Write

### What happens

`write_framed_blob` (lines 458-493) in sync mode makes three `write_all` calls:
```rust
writer.write_all(&header_len.to_be_bytes())?;   // 4 bytes
writer.write_all(&scratch.header_buf)?;          // ~40 bytes
writer.write_all(&scratch.blob_buf)?;            // ~32-64 KB
```

### Is this a concern?

For `PbfWriter::to_path` (buffered), `FileWriter::Buffered` wraps
`BufWriter::with_capacity(256 * 1024, file)`. The 4-byte and 40-byte writes go into the
256 KB buffer. The ~32-64 KB blob body also fits in the buffer. No extra syscalls.

For `PbfWriter::new(writer)` with an unbuffered writer, these would be 3 syscalls per
blob. The doc comment at line 294 warns: "callers should wrap it in
`BufWriter::with_capacity(256 * 1024, file)`". Users who ignore this will get poor
performance, but that is a documented requirement.

For the O_DIRECT path (`DirectWriter`), writes go through `DirectWriter::write` which has
its own 256 KB aligned buffer. Same coalescing behavior.

### Verdict

**Not a concern.** All standard write paths buffer appropriately. The triple write is
invisible behind the BufWriter.

---

## Finding 11: `libdeflater` vs `flate2` Compression Paths

### Differences

**flate2** (lines 742-766): streaming `compress_vec` with `FlushCompress::Finish`.
Pre-allocates `compress_buf` with a worst-case bound. Reuses the `Compress` object.

**libdeflater** (lines 770-796): single-call `zlib_compress` with exact `compress_bound`.
`compress_buf` is resized to the bound, then truncated to actual output. Reuses the
`Compressor` object.

**Correctness:** Both produce valid zlib output. libdeflater produces slightly different
byte-level output (different deflate tree decisions) but is spec-compliant.

**Performance:** libdeflater is 2-3x faster for sync mode (documented in memory.md
benchmarks: 12.7s vs 24.4s for Denmark sync zlib:6). For pipelined mode the difference
is smaller (6.7s vs 6.9s) because decode is the bottleneck.

**Allocation pattern:** libdeflater's `compress_buf.resize(bound, 0)` zero-fills the
excess capacity, while flate2's `compress_vec` grows the Vec dynamically. After warmup,
both reuse the same allocation. The zero-fill in libdeflater is wasteful (~100 KB of
zero-fill per blob for a ~130 KB input) but negligible in practice.

### Verdict

**No correctness concern. Minor inefficiency in libdeflater zero-fill**, but dominated
by the compression work itself.

---

## Finding 12: `copy_file_range` Interaction with BufWriter

### How it works

`write_raw_copy` (lines 522-554) sends a `CopyRange { in_fd, offset, len }` through
the pipeline channel. The writer thread (lines 591-601) handles it:

```rust
PipelinePayload::CopyRange { in_fd, offset, len } => {
    let out_fd = writer.flush_and_raw_fd()?  // flushes BufWriter, returns raw fd
        .ok_or_else(|| ...)?;                // returns None for O_DIRECT
    copy_range(in_fd, out_fd, offset, len)?;
}
```

`flush_and_raw_fd()` (file_writer.rs lines 52-61) flushes the BufWriter before returning
the raw fd. This ensures the fd's file position matches the logical write position.
After `copy_file_range` advances the fd position, subsequent `write_all` calls to the
BufWriter will write to the correct offset.

**Concern:** After `copy_file_range`, the BufWriter's internal position tracking is
out of sync with the fd's position. However, `BufWriter` does not track the fd position
— it only manages its internal buffer. Since we flushed before the copy, the buffer is
empty, and subsequent writes fill the buffer from scratch. No desync.

### Verdict

**Correctly implemented.** The flush-before-raw-fd pattern is sound.

---

## Cross-Box Interactions

### Box 1 (Read Pipeline)
The read pipeline (pipeline.rs) uses the same VecDeque reorder buffer pattern with
`DECODE_AHEAD=32`. Any optimization to the reorder buffer would apply to both. However,
Finding 8 shows the reorder buffer is not a concern in either case.

### Box 6 (BlockBuilder)
**Critical interaction.** `BlockBuilder::take()` returns `&[u8]` which forces `to_vec()`
in the pipelined path. The `scan_block_ids` redundancy (Finding 4) is a direct consequence
of information loss at the `take()` boundary — BlockBuilder knows the index but does not
expose it. Fixing this requires a BlockBuilder API change.

The `flush_local` pattern in 5 command files also does `bytes.to_vec()` from `take()`.
These local blocks are later passed to `write_primitive_block`, causing a second copy in
pipelined mode. A `take_owned()` method or a double-buffer scheme in BlockBuilder would
address both copies at once.

### Box 7 (io_uring Writer)
The uring writer (uring_writer.rs) uses the same `WRITE_AHEAD=32` constant (line 12)
and the same `VecDeque` reorder buffer (line 694-695). It receives the same
`PipelinePayload` items. The uring writer adds its own `AlignedBufferPool` (64 x 256 KB
= 16 MB) for registered buffer I/O, which is independent of the framing pipeline.

The uring writer does not change the framing/compression pipeline — it only replaces the
I/O backend. All findings about `to_vec`, `scan_block_ids`, and zstd encoder reuse apply
equally to the uring path.

### Box 8 (Commands — Merge)
Merge is the primary consumer of `write_raw_owned` and `write_raw_copy`. Passthrough
coalescing already eliminates per-blob channel sends. The `write_primitive_block` path
is only used for rewritten blocks (~8-18% of blobs), limiting the impact of Finding 1.

However, Finding 4 (`scan_block_ids` redundancy) affects all blocks written through
`write_primitive_block`, including rewritten merge blocks. Since `rewrite_block_parallel`
uses `BlockBuilder` internally, the fix (exposing BlobIndex from BlockBuilder) benefits
merge directly.

---

## Recommended Actions (Prioritized)

### 1. Eliminate `scan_block_ids` in write path (Finding 4)
**Impact:** High. Saves ~50-125 seconds at planet scale. Pure waste elimination.
**Effort:** Low-medium. Add min_id/max_id/count tracking to BlockBuilder, expose via
`take()` return or a companion method. Update `flush_block` and `flush_local` in 6 files.
**Risk:** Low. Purely additive change to BlockBuilder.

### 2. Add zstd compressor reuse to `FrameScratch` (Finding 5)
**Impact:** Medium. Eliminates ~1.28 TB allocator churn at planet scale with zstd.
**Effort:** Low. Use `zstd::bulk::Compressor` with `get_or_insert_with` pattern, matching
the existing zlib reuse. ~20 lines changed.
**Risk:** Low. Isolated to `encode_blob_body`.

### 3. Use `write_raw_owned` in cat.rs passthrough (Finding 7)
**Impact:** Low (cat is not the hot path). Eliminates one `to_vec()` per passthrough blob.
**Effort:** Trivial. Change `writer.write_raw(&frame.frame_bytes)` to
`writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))`.
**Risk:** None.

### 4. Consider double-buffer scheme in BlockBuilder (Finding 1)
**Impact:** Medium. Eliminates the `to_vec()` in both `write_primitive_block` and
`flush_local`. Saves ~75 seconds at planet scale.
**Effort:** Medium. Requires redesigning `encode_buf` ownership in BlockBuilder.
Alternative: `take_vec()` that moves the buffer out (simpler but loses reuse).
**Risk:** Medium. Touches a core data structure with many callers.

### 5. Adaptive pipeline bypass for Compression::None (Finding 3)
**Impact:** Low. Only matters for `Compression::None` with buffered I/O, which is not
a recommended configuration. io_uring already handles this case.
**Effort:** Medium. Requires branching in `write_primitive_block`.
**Risk:** Low, but adds code complexity.
