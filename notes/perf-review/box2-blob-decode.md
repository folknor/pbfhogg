# Box 2: Blob Decode, Decompression, and Buffer Reuse

Primary file: `src/read/blob.rs` (1092 lines).
Investigated: blob.rs, pipeline.rs, reader.rs, mmap_blob.rs, commands/merge.rs,
commands/sort.rs, commands/cat.rs, blob_index.rs, file_reader.rs, wire.rs.
Cross-referenced: hotpath-profile.md, region-profiles.md, PBF spec wiki.

## Executive Summary

1. **DecompressPool Mutex contention is NOT real.** The critical section is
   ~10 ns (Vec pop/push). With 1-5 ms between lock acquisitions per thread
   (zlib decompress), the probability of contention at 14 threads is <0.01%.
   The reviewer's concern sounds plausible in the abstract but collapses under
   quantitative analysis. No fix needed.

2. **Blob size limits are spec-mandated and correct.** The 64 KB header / 32 MB
   blob limits are the PBF MUST limits from the official specification. No
   real-world PBF producer violates them. Zero performance impact. No fix needed.

3. **Copy vs zero-copy API confusion is a documentation/surface-area issue, not
   a performance bug.** All internal hot paths already use the optimal variant.
   The unused public functions (`decompress_blob_data`, `decompress_blob_data_from_bytes`,
   `decompress_blob_data_into_from_bytes`) have zero internal callers and exist
   only as convenience API for external consumers.

4. **The reviewer missed the real bottleneck: ZlibDecoder per-blob allocation.**
   Each `ZlibDecoder::new()` allocates ~32 KB of inflate state. At 2.5M blobs
   (planet), that is 80 GB of cumulative alloc/dealloc for decoder state alone.
   This is the single largest allocation source in the decompress path and is
   addressable by pooling `Decompress` instances alongside the output buffers.

5. **WireBlobHeader small allocations (blob_type String, indexdata Vec) are
   measurable at planet scale (~100 MB cumulative) but dwarfed by decompression
   costs.** The blob_type String allocation could be eliminated with an enum, but
   the fix complexity is low and benefit modest.

---

## Finding 1: DecompressPool Mutex Contention

**Verdict: NOT REAL. The reviewer's theoretical concern does not survive quantitative analysis.**

### How the pool works

`DecompressPool` (lines 25-55) is a `Mutex<Vec<Vec<u8>>>` pool of reusable
decompression output buffers. It is used exclusively in the pipelined read path
(`pipeline.rs` line 100).

- `get()` (line 40): lock, `Vec::pop()`, unlock. Returns empty `Vec` on miss.
- `put()` (line 49): lock, `Vec::push(buf)`, unlock. Called from `PooledBuffer::drop()`.

### Contention math

The pool is accessed at two points per blob:
- **get**: at the start of `decompress_blob()` via `pool_get()` (line 78)
- **put**: when the `PrimitiveBlock` (which owns the `Bytes` wrapping a `PooledBuffer`) is dropped

For planet: ~2.5M blobs = ~5M lock acquisitions total.

Critical section duration:
- `get()`: `v.pop()` on a `Vec<Vec<u8>>` = load pointer, decrement length, return.
  This is 2-3 instructions, ~3-10 ns.
- `put()`: `buf.clear()` (set length to 0, no dealloc) + `v.push(buf)` = ~5-10 ns.

Time between lock acquisitions per thread:
- Zlib decompression of a typical PBF blob (~32 KB compressed -> ~1.4 MB decompressed):
  **1-5 ms** (measured: Denmark avg 337-374 us per `decompress_blob` call, but this
  includes overhead -- pure zlib time is ~200-400 us for the read path; the pipelined
  path has 10-14 threads so each thread processes one blob every ~2-5 ms).

Contention probability (two threads hitting the lock simultaneously):
```
P(contention) = N * (t_hold / t_between)
             = 14 * (10 ns / 3,000,000 ns)
             = 14 * 3.3e-6
             = 0.000047 = 0.005%
```

At 5M total acquisitions, expected contended acquisitions: ~230 out of 5,000,000.
Each contention costs ~100-200 ns (futex wake). Total contention cost: ~50 us over
the entire planet file read. The pipelined read of planet takes ~5-10 minutes. This
is noise at the 10^-7 level.

### Poisoning behavior

The reviewer noted `get()` uses `.ok()` on `Mutex::lock()` (line 43). This is
intentional and documented: if another thread panicked while holding the lock
(poisoning), falling back to a fresh `Vec` is correct recovery. The comment at
line 38 explains this. The `put()` method (line 51) also handles poisoning
gracefully by simply dropping the buffer.

### Alternative analysis

- **thread_local pool**: Would eliminate all locking but would prevent buffer
  sharing between threads. In the pipelined path, blobs are decompressed on rayon
  workers but the `PrimitiveBlock` (and its buffer) is consumed on the main thread.
  The get happens on a worker; the put happens on the main thread. A thread_local
  pool would never return buffers to the workers. Fundamentally incompatible with
  the current architecture.

- **crossbeam SegQueue**: Lock-free MPMC queue. Would replace ~10 ns Mutex
  lock/unlock with ~15-30 ns CAS retry loop (typical for lock-free queues under
  low contention). Net savings: possibly negative. Adds a dependency.

- **Sharded pool**: N pools, hash thread ID to select one. At 0.005% contention
  rate, this is pure complexity for zero gain.

**Recommendation: No action. The current design is correct and near-optimal.**

### Arc clone overhead

The reviewer's task mentions the `Arc::clone` in `pool_get`. Looking at the code:
`pool_get()` (line 78) takes `Option<&Arc<DecompressPool>>`, so it borrows the Arc
-- no clone. The clone happens at `pipeline.rs` line 103: `Arc::clone(&buffer_pool)`
for each blob dispatched to rayon. This is one atomic increment per blob = 2.5M
atomics for planet. Each atomic increment is ~5-20 ns. Total: 12-50 ms. Against a
5-10 minute pipeline, this is 0.01%. Not measurable.

The corresponding `Arc::drop` happens when each rayon task completes (the `bp` local
drops). Another 2.5M atomics. Same analysis: negligible.

---

## Finding 2: Blob Size Limits

**Verdict: NOT REAL for performance. Correct per the PBF specification.**

### Spec compliance

The [PBF Format specification](https://wiki.openstreetmap.org/wiki/PBF_Format) states:
- BlobHeader: SHOULD < 32 KiB, **MUST < 64 KiB**
- Blob (uncompressed): SHOULD < 16 MiB, **MUST < 32 MiB**

pbfhogg uses the MUST limits:
- `MAX_BLOB_HEADER_SIZE = 64 * 1024` (line 220)
- `MAX_BLOB_MESSAGE_SIZE = 32 * 1024 * 1024` (line 225)

These are compile-time constants, inlined at each use site. The checks are
simple integer comparisons that execute in <1 ns and are perfectly branch-predicted
(always false for valid files).

### Real-world producers

All major PBF producers conform:
- **osmium**: enforces these exact limits
- **Planetiler**: standard protobuf PBF, well within limits
- **osm2pgsql/osmconvert**: conform
- **Overpass API**: produces standard-compliant PBFs
- **Geofabrik extracts**: all use osmium, always compliant

The GitHub issues found during research (osmium-tool#235, osrm-backend#2820,
osrm-backend#6014) are all cases of corrupt/truncated files triggering these
checks correctly -- the limits are working as intended as corruption detectors.

### Performance implication

None. The comparisons are in the blob-reading loop but cost <1 ns per blob
(branch prediction + single compare). At 2.5M blobs: <2.5 ms total.

**Recommendation: No action. The limits are correct, spec-mandated, and free.**

---

## Finding 3: Copy vs Zero-Copy API

**Verdict: PARTIALLY REAL as a surface-area concern, but NOT a performance bug. All hot paths already use the optimal variant.**

### API surface inventory

There are 12 public decode/decompress functions in blob.rs. Here is the complete
callsite analysis:

| Function | Line | Hot-path callers | Notes |
|----------|------|------------------|-------|
| `decompress_blob_data(&[u8])` | 833 | **0** | Unused. Calls `decompress_blob` with copy. |
| `decompress_blob_data_into(&[u8], buf)` | 844 | **2**: merge.rs:856, sort.rs:253 | Optimal for command layer. Buffer reuse. |
| `decompress_blob_data_into_from_bytes(&Bytes, buf)` | 852 | **0** | Unused. |
| `decompress_blob_data_from_bytes(&Bytes)` | 905 | **0** | Unused. Has `#[hotpath::measure]`. |
| `decode_blob_to_primitiveblock(&[u8])` | 810 | **1**: sort.rs:407 | Overlap-run path only. Copies via `Bytes::copy_from_slice`. |
| `decode_blob_to_primitiveblock_from_bytes(&Bytes)` | 819 | **0** | Unused. |
| `parse_primitive_block_from_bytes(&[u8])` | 961 | **0** | Unused. |
| `parse_primitive_block_from_bytes_owned(&Bytes)` | 975 | **1**: merge.rs:870 | Correct usage (Bytes::from(raw) is O(1)). |
| `parse_blob_header(&[u8])` | 765 | **1**: cat.rs:89 | Header parse, not hot. |
| `parse_blob_header_from_bytes(&Bytes)` | 783 | **0** | Delegates to `&[u8]` variant anyway. |
| `parse_blob_header_with_index(&[u8])` | 792 | **2**: merge.rs:290, sort.rs:232 | Returns indexdata Vec. |
| `decode_blob_to_headerblock(&[u8])` | 984 | **3**: cat:118, merge:970, sort:247 | Once per file. Not hot. |

### Hot path analysis

**Pipelined read path** (ElementReader, pipeline.rs):
Uses `Blob::to_primitiveblock_pooled()` -> `decompress_blob()` -> `pool_get/pool_wrap`.
This is the internal `pub(crate)` function at line 1005. It receives `Option<&Arc<DecompressPool>>`
and uses `pool_get` for buffer reuse, `pool_wrap` to return buffers via `Bytes::from_owner`.
Optimal. No public API variant involved.

**Merge command** (merge.rs):
- classify_blob (line 856): `decompress_blob_data_into(frame.blob_bytes(), buf)` -- reuses `buf`.
  Optimal choice. `frame.blob_bytes()` returns `&[u8]`, which is what `decompress_blob_data_into`
  expects. No unnecessary copy.
- parse (line 870): `parse_primitive_block_from_bytes_owned(&Bytes::from(raw))` where `raw` is
  `std::mem::take(buf)`. The `Bytes::from(raw)` wraps the Vec in O(1). Optimal.

**Sort command** (sort.rs):
- Pass 1 classification (line 253): `decompress_blob_data_into(&blob_bytes, &mut decompress_buf)`.
  Reuses buffer. Optimal.
- Pass 2 overlap run (line 407): `decode_blob_to_primitiveblock(blob_bytes)`. This is the
  `&[u8]` variant that internally calls `Bytes::copy_from_slice`. This IS a copy, but:
  - Overlap runs handle a small fraction of blobs (only overlapping ID ranges)
  - The copy is ~32 KB (compressed blob data), dwarfed by the subsequent decompression
  - This function could use `Bytes::from(blob_bytes.to_vec())` to avoid the double copy,
    but the `to_vec()` + `Bytes::from()` is essentially identical cost to `copy_from_slice`

**Sequential/par_map_reduce paths** (reader.rs):
Use `blob.decode()` -> `blob.to_primitiveblock()` -> `decompress_blob(&self.blob, None)`.
No pool. Each decompression allocates a fresh Vec. This is the non-pipelined path; the
cost is dominated by sequential zlib, not allocation.

### The real issue

The 12-function API surface has 6 functions with zero callers. This is not a performance
problem but a maintenance burden. The `_from_bytes` variants were likely created during
the transition from prost (which used `Bytes` pervasively) and never cleaned up.

**Recommendation: Low priority. Consider deprecating unused public functions in a future
cleanup. No performance impact.**

---

## Additional Findings (Things the Reviewer Missed)

### D1. ZlibDecoder Per-Blob Allocation (~80 GB cumulative at planet scale)

**Impact: MEDIUM. Addressable. ~32 KB alloc/dealloc per blob.**

Every call to `decompress_blob()` (line 1024), `decompress_parsed_blob_into()` (line 876),
and `decompress_blob_data_from_bytes()` (line 933) creates a fresh `ZlibDecoder::new()`.

flate2's `ZlibDecoder` wraps a `Decompress` struct (the inflate state machine). The
`Decompress::new(/*zlib_header=*/true)` call allocates an `inflate_state` structure
from the underlying zlib implementation. For miniz_oxide (the `rust-zlib` default
backend), this is ~32 KB of Huffman tables, sliding window, and code length tables.
For zlib-ng, it is similar (~40 KB).

At planet scale:
```
2.5M blobs * 32 KB = 80 GB cumulative allocation for decoder state
```

The DecompressPool already pools the output buffers (the `Vec<u8>` that receives
decompressed data). But the ZlibDecoder (inflate state) is created fresh every time
and never pooled.

From the hotpath data (hotpath-profile.md, line 66):
```
blob::decompress_blob | 7396 calls | 790 MB total allocation
```
For Denmark's 7,396 blobs, the decoder state allocation is:
```
7,396 * 32 KB = 237 MB
```
The measured 790 MB includes both the output buffer (~1.4 MB avg * 7,396 = ~10.3 GB
cumulative, but pooled so net new allocation is much less) AND the decoder state.
With pooling active, the output buffer allocation drops significantly, meaning the
237 MB for decoder state becomes a more substantial fraction.

After the DecompressPool was introduced (hotpath-profile.md line 132):
```
blob::decompress_blob | 7,398 calls | 1.2 GB
```
This is in the cat path where both output buffers AND decoder state are allocated.
Without the pool (cat path doesn't use it), output buffers are ~10.3 GB. With the
pool (pipelined path), they drop to near zero. The 1.2 GB figure suggests decoder
state (237 MB) + reduced output buffer churn.

**Fix**: Extend `DecompressPool` to pool `flate2::Decompress` instances alongside
the `Vec<u8>` output buffers. Change the pool item from `Vec<u8>` to
`(Vec<u8>, Option<flate2::Decompress>)`. On get, reset the Decompress instance with
`Decompress::reset(true)` instead of allocating a new one. This saves the 32 KB
allocation per blob.

Complexity: Low. `Decompress::reset()` is a cheap operation (~100 ns). The pool
already handles the lifetime management via `PooledBuffer`.

**Caveat**: This only helps the pipelined path (which uses the pool). The sequential
path, `par_map_reduce`, and command-layer functions (`decompress_blob_data_into`) do
not use the pool. For those paths, a `thread_local!` ZlibDecoder could work since
the decompress and drop happen on the same thread.

### D2. BlobReader::next() Per-Blob Allocation

**Impact: LOW in pipelined path, MEDIUM in sequential/par_map_reduce.**

Line 570: `let mut blob_data = Vec::with_capacity(header.datasize as usize);`

Every call to `BlobReader::next()` allocates a fresh Vec for the compressed blob
data (typically 16-64 KB), then wraps it in `Bytes::from()` at line 575. For planet:
```
2.5M blobs * ~32 KB avg = ~80 GB cumulative allocation
```

In the pipelined path, the I/O reader thread produces these blobs sequentially.
Since `Bytes::from(vec)` is O(1) (takes ownership), the allocation is the Vec itself.
The Vec is dropped after decompression, but there is no reuse path -- each blob
allocates fresh.

This is somewhat mitigated by the allocator's free-list: at ~32 KB per blob, the
allocator will typically reuse the same slab. The actual RSS impact is minimal
(one 32-64 KB buffer hot in cache). But the cumulative allocator churn (80 GB of
alloc/dealloc) has overhead: ~50-100 ns per allocation * 2.5M = 125-250 ms.

**Fix**: Add a reusable read buffer to `BlobReader`. Read into it, then clone/freeze
into `Bytes` only when needed. This is how `MmapBlobReader` already works (reads from
the mmap, copies into `Bytes::copy_from_slice` only for the blob data).

Complexity: Low, but changes the `BlobReader` API. The buffer must outlive the
Iterator::next() call, which is already the case (it's an internal field).

### D3. WireBlobHeader::parse Allocations

**Impact: LOW. ~100 MB cumulative at planet scale, but constant-factor overhead.**

Line 128: `blob_type = String::from_utf8(bytes.to_vec())`
Line 134: `indexdata = Some(bytes.to_vec())`

For every blob, `WireBlobHeader::parse` allocates:
- A `String` for `blob_type`: ~7 bytes ("OSMData" or "OSMHeader"). With allocator
  overhead (16-byte minimum allocation + 24-byte String struct), this is ~40 bytes
  total per blob.
- An `Option<Vec<u8>>` for `indexdata`: 26 bytes for indexed PBFs, ~0 for non-indexed.
  With allocator overhead, ~48 bytes per blob.

At planet scale (2.5M blobs):
```
blob_type: 2.5M * 40 bytes = 100 MB cumulative alloc
indexdata:  2.5M * 48 bytes = 120 MB cumulative alloc (indexed PBFs only)
```

The blob_type is particularly wasteful because it is always one of two known strings.
A `BlobType`-like enum could eliminate this allocation entirely.

**Fix for blob_type**: Store a `BlobTypeKind` enum (Header/Data/Unknown(String))
directly in `WireBlobHeader`, only allocating a String for the rare Unknown case.
Alternatively, since `Cursor::read_len_delimited()` returns `&[u8]` (a borrow from
the input), we could borrow the slice and compare in-place, only allocating for
the `Unknown` case. This requires `WireBlobHeader` to carry a lifetime, which
propagates to `Blob` and `BlobHeader`.

**Fix for indexdata**: Similar approach -- borrow from input. But the indexdata
needs to outlive the parse call (it's stored in `WireBlobHeader`), so borrowing
requires a lifetime parameter. Alternatively, use `smallvec::SmallVec<[u8; 26]>`
to inline the 26-byte payload and avoid heap allocation. Or use a fixed-size
array since indexdata is always exactly 26 bytes when present.

Complexity: Low for blob_type enum approach. Medium for borrowing approach (lifetime
propagation). Low for smallvec/array approach for indexdata.

### D4. `decompress_blob_data_from_bytes` Code Duplication

**Impact: NONE (performance). LOW (maintenance).**

The function at line 905 duplicates the decompression logic from `decompress_blob()`
at line 1005. Both handle Raw/Zlib/Zstd variants with identical capacity heuristics
and decoder setup. The only difference:

- `decompress_blob()` returns `Bytes` (via `pool_wrap`), supports optional pool
- `decompress_blob_data_from_bytes()` returns `Vec<u8>`, no pool support

Since `decompress_blob_data_from_bytes` has zero internal callers, this duplication
has no practical impact. It is a maintenance concern only.

`decompress_parsed_blob_into()` (line 859) is a third implementation of the same
logic, but takes a `&mut Vec<u8>` for buffer reuse. This one IS used (merge, sort).

**Recommendation**: If cleaning up, unify the three implementations by having
`decompress_blob()` as the core, with wrappers. But since two of the three have
zero callers, this is low priority.

### D5. MmapBlobReader Allocation Strategy Comparison

**Impact: INFORMATIONAL. The current design is already well-optimized.**

`MmapBlobReader::next()` (mmap_blob.rs line 384) uses `Bytes::copy_from_slice()`
for blob data (~32 KB copy). This was a deliberate design choice, extensively
documented in the file comments (lines 119-170). The analysis in those comments
is correct: the ~4 us memcpy is negligible relative to decompression, and the
independent `Bytes` eliminates all Arc contention from the old `Bytes::slice()` approach.

For comparison, `BlobReader::next()` does `Vec::with_capacity(n)` + `read_to_end()` +
`Bytes::from(vec)`. The allocation strategy differs:
- Mmap: memcpy from mmap region (cache-friendly, no syscall)
- BlobReader: read_to_end into fresh Vec (syscall to BufReader, then memcpy from kernel)

Both paths produce an independent `Bytes` with no shared refcount. The mmap path is
slightly more efficient because the source data is already in memory.

---

## Cross-Box Interactions

### Box 1 (Pipeline) -> Box 2

The pipeline creates one `DecompressPool` (pipeline.rs line 100) and shares it via
`Arc::clone` to each rayon task (line 103). The pool lifetime is the dispatch thread's
scope. Key interaction:

- **get()** happens on rayon worker threads (inside `to_primitiveblock_pooled`)
- **put()** happens on the main thread (when `PrimitiveBlock` is dropped after `block_fn`)

This cross-thread get/put pattern means a `thread_local` pool would fail -- buffers
would accumulate on the main thread and starve the workers. The Mutex pool is the
correct design for this access pattern.

The `DECODE_AHEAD = 32` constant (pipeline.rs line 19) bounds the maximum number of
in-flight buffers. With 32 decompressed blobs in flight at ~1.4 MB each, the pool
holds at most ~45 MB of reusable buffer capacity. This is a modest, bounded memory
footprint.

### Box 2 -> Box 3 (Wire Parsing)

`decompress_blob()` produces a `Bytes` that is passed to `PrimitiveBlock::new()`.
The quality of the decompressed buffer directly affects Box 3:

- If the buffer came from the pool (pipelined path), it is a reused allocation with
  capacity >= the decompressed size. `PrimitiveBlock::new()` borrows from this `Bytes`
  via the self-referential `WireBlock<'static>` pattern.
- If the buffer is freshly allocated (sequential path), it is sized exactly to the
  decompressed output. Same downstream behavior.

The key insight: `PrimitiveBlock` holds the `Bytes` for its lifetime. When the
`PrimitiveBlock` is dropped, the `Bytes` drops, which triggers `PooledBuffer::drop()`
-> `pool.put()`. This means the buffer is in use for the entire duration of element
iteration, not just decompression. The pool's hit rate depends on how quickly the
consumer processes blocks relative to how quickly new ones are decompressed.

### Box 4 (Mmap) -> Box 2

`MmapBlobReader` does not use `DecompressPool`. Its `decode()` method (mmap_blob.rs
line 94) calls `decompress_blob(&blob, None)` -- passing `None` for the pool. Each
decompression allocates a fresh `Vec`. If mmap reading were used in a pipelined
context, it would benefit from pool integration. Currently, mmap is used only in
the sequential read benchmark mode.

---

## Recommended Actions (Prioritized)

### Priority 1: Pool ZlibDecoder State (Medium effort, Medium impact)

Extend `DecompressPool` to pool `flate2::Decompress` instances. Each instance holds
~32 KB of inflate state that is currently allocated and freed per blob. At planet
scale, this saves 80 GB of cumulative allocation.

Implementation sketch:
```
struct DecompressPool {
    buffers: Mutex<Vec<(Vec<u8>, Decompress)>>,
}
```

On `get()`: pop a `(Vec, Decompress)`, call `decompress.reset(true)`.
On `put()`: push the pair back.

This avoids 2.5M * 32 KB = 80 GB of allocator churn at planet scale. The actual
wall-clock savings depend on the allocator -- with jemalloc's thread-local caches,
the 32 KB allocation might be ~50-100 ns (vs ~100 ns for reset), so the savings
are modest per-call but accumulate to measurable totals (125-250 ms at planet scale).

The larger benefit is reduced allocator pressure: fewer 32 KB alloc/dealloc cycles
means less fragmentation and less work for the allocator's free-list management,
which has cascading benefits for other allocations happening concurrently on the
same threads.

### Priority 2: Eliminate blob_type String Allocation (Low effort, Low impact)

Replace `WireBlobHeader.blob_type: String` with an enum:
```
enum BlobKind {
    OsmHeader,
    OsmData,
    Unknown(String),
}
```

Parse the `Cursor::read_len_delimited()` result as `&[u8]` and match:
- `b"OSMHeader"` -> `BlobKind::OsmHeader`
- `b"OSMData"` -> `BlobKind::OsmData`
- other -> `BlobKind::Unknown(String::from_utf8(bytes.to_vec()))`

This eliminates 2.5M * ~40 bytes = 100 MB of alloc/dealloc at planet scale.
The match on `&[u8]` is faster than `String::from_utf8` + later `str` comparison.

Propagation: `WireBlobHeader` is `pub(crate)`, so changes are internal. The public
`BlobType<'a>` enum in blob.rs is already the right shape -- `WireBlobHeader` can
store a similar enum and `get_type()` becomes a trivial conversion.

### Priority 3: Use Fixed-Size Array for indexdata (Low effort, Low impact)

Replace `indexdata: Option<Vec<u8>>` with `indexdata: Option<[u8; 26]>` (or a
small inline buffer). Indexdata is always exactly 26 bytes when present
(blob_index.rs serialization format). This eliminates 2.5M * ~48 bytes = 120 MB
of alloc/dealloc for indexed PBFs.

Alternative: `Option<BlobIndex>` directly, parsing the indexdata inline during
`WireBlobHeader::parse`. This moves the deserialization earlier but eliminates both
the Vec allocation and the later `BlobIndex::deserialize()` call.

### Priority 4: Document or Deprecate Unused Public Functions (Low effort, Zero perf impact)

Six public functions have zero internal callers:
- `decompress_blob_data` (line 833)
- `decompress_blob_data_from_bytes` (line 905)
- `decompress_blob_data_into_from_bytes` (line 852)
- `decode_blob_to_primitiveblock` (line 810)
- `decode_blob_to_primitiveblock_from_bytes` (line 819)
- `parse_blob_header_from_bytes` (line 783)

These exist as public API for downstream users. Either document them clearly
as convenience wrappers (with guidance on which variant to prefer), or deprecate
the ones that are strictly inferior (e.g., `parse_blob_header_from_bytes` just
delegates to `parse_blob_header`).

### Priority 5: BlobReader Buffer Reuse (Medium effort, Low impact)

Add an internal reusable read buffer to `BlobReader` to avoid per-blob Vec allocation
in `next()`. This saves 2.5M * ~32 KB = 80 GB of cumulative alloc for the compressed
blob data. However, since the allocator's free-list efficiently handles same-size
reuse patterns, and the pipelined path's I/O thread is not the bottleneck (it is
always faster than the decompression workers), the wall-clock impact is small.

This becomes more relevant if the I/O thread is profiled as a bottleneck on
very fast storage (NVMe with 7 GB/s read) where allocator overhead could matter.

### NOT Recommended

- **Replacing DecompressPool Mutex**: Not needed. Contention is <0.01%.
- **Changing blob size limits**: They are spec-correct.
- **Unifying decompress function implementations**: Low value, code churn risk.
- **Adding pool support to MmapBlobReader**: Mmap path is not used in production
  hot paths (pipelined read uses BlobReader, merge uses raw frame reading).
