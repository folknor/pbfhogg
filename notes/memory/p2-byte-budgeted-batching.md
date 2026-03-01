# P2: Switch merge from fixed-count batching to byte-budgeted in-flight control

Action plan for replacing `BATCH_SIZE=64` (fixed blob count) with adaptive
byte-budgeted batch sizing in `src/commands/merge.rs`. Target: materially lower
peak RSS on planet-scale merges (75+ GB PBF, 64 GB RAM) with acceptable
throughput loss.

## Current state

### The 4-phase batch pipeline

The merge loop in `merge()` (line 1067) reads raw blob frames into batches
of up to `BATCH_SIZE=64`, then processes each batch through 4 phases:

```
Reader thread ──[sync_channel(64)]──> Main thread batch loop
                                        │
                                   Phase 1: par classify
                                   Phase 2: seq assign
                                   Phase 3: par rewrite
                                   Phase 4: seq output ──> PbfWriter pipeline
                                                           │
                                                      [sync_channel(32)]
                                                           │
                                                      Writer thread
```

**Phase 1 (Parallel classify):** Each `RawBlobFrame` in the batch is classified
on the rayon pool via `classify_only()`. This decompresses the blob data into a
per-thread `Vec<u8>` buffer, scans for ID range overlap with the diff, and if
the range overlaps, does a full parse into `PrimitiveBlock` and checks for
precise element overlap. Results: `ClassifyResult::Passthrough(BlobIndex, bool)`,
`ClassifyResult::FalsePositive(BlobIndex, bool)`, or
`ClassifyResult::NeedsRewrite(PrimitiveBlock, BlobIndex)`.

**Phase 2 (Sequential inline assign):** On the main thread, iterates classify
results to build `Vec<BatchSlot>` and `Vec<RewriteJob>`. For each
`NeedsRewrite`, binary-searches the diff's sorted upsert vectors to compute the
`inline_upserts: Vec<i64>` for that blob's ID range. The `PrimitiveBlock` is
moved into the `RewriteJob`.

**Phase 3 (Parallel rewrite):** Each `RewriteJob` is processed on the rayon pool
via `rewrite_block_parallel()`. This walks the parsed `PrimitiveBlock` element by
element, interleaving upserts, applying deletes, and flushing full blocks to a
local `Vec<OwnedBlock>`. Each `OwnedBlock` is `(Vec<u8>, BlobIndex,
Option<Vec<u8>>)` -- serialized block bytes, index metadata, and optional
tagdata. Output: `RewriteOutput { blocks: Vec<OwnedBlock>, stats: MergeStats }`.

**Phase 4 (Sequential output):** On the main thread, iterates `BatchSlot`s in
order. Passthrough blobs are coalesced into `passthrough_buf: Vec<u8>` and
flushed as single `write_raw_owned()` calls. Rewrite blobs drain their
`RewriteOutput.blocks` into
`writer.write_primitive_block_owned(block_bytes, index, tagdata)`. Gap creates
and type-transition flushes go through `emit_create_for_output()` which uses the
main-thread `BlockBuilder`.

### Data alive at each phase boundary

**Entering Phase 1 (batch collected):**
- `batch: Vec<RawBlobFrame>` -- up to 64 frames, each containing:
  - `frame_bytes: Vec<u8>` -- complete framed blob (4-byte len + BlobHeader + Blob)
  - `blob_type: BlobKind` -- enum, no heap alloc
  - `blob_offset: usize` -- scalar
  - `index: Option<BlobIndex>` -- 42 bytes if Some
  - `tagdata: Option<Box<[u8]>>` -- variable, typically 50-500 bytes
  - `file_offset: u64` -- scalar

**Phase 1 -> Phase 2 boundary:**
- `batch` still alive (frames needed for Phase 4 passthrough output)
- `classify_results: Vec<ClassifyResult>` -- for `NeedsRewrite` variants, this
  holds a `PrimitiveBlock` (owns a `Bytes` buffer of the decompressed data, plus
  a `WireBlock` with `Box<[(u32,u32)]>` group ranges and
  `WireStringTable { offsets: Vec<(u32,u32)>, validated: ... }`)
- Per-thread decompression buffers (reusable, on rayon threads) -- NOT counted
  against the batch because they are recycled via `map_init`

**Phase 2 -> Phase 3 boundary:**
- `batch` still alive
- `slots: Vec<BatchSlot>` -- lightweight enum, references into `rewrite_jobs` by
  index
- `rewrite_jobs: Vec<RewriteJob>` -- each holds:
  - `block: PrimitiveBlock` (moved from classify result)
  - `kind: ElemKind`
  - `inline_upserts: Vec<i64>` -- typically small (10s-100s of IDs)

**Phase 3 -> Phase 4 boundary:**
- `batch` still alive (for passthrough frame_bytes)
- `slots` still alive (for dispatch logic)
- `rewrite_outputs: Vec<RewriteOutput>` -- each holds:
  - `blocks: Vec<OwnedBlock>` -- each `OwnedBlock` is
    `(Vec<u8>, BlobIndex, Option<Vec<u8>>)`:
    - `Vec<u8>` serialized PrimitiveBlock: typically 100-250 KB uncompressed
    - `BlobIndex`: 42 bytes inline
    - `Option<Vec<u8>>` tagdata: typically 50-500 bytes
  - `stats: MergeStats` -- scalars only
- `rewrite_jobs` -- the `PrimitiveBlock` and `inline_upserts` are still alive
  because `par_iter()` borrows, does not consume

**During Phase 4 output:**
- `passthrough_buf: Vec<u8>` -- coalesced passthrough frames, can grow to
  multiple MB during passthrough runs
- Each `write_primitive_block_owned()` call moves `block_bytes` into the writer
  pipeline (rayon task for compression -> sync_channel(32) -> writer thread)
- Each `write_raw_owned()` moves bytes into sync_channel(32)

**After Phase 4 (before next batch):**
- `batch.clear()` -- frame_bytes already moved/consumed, but the Vec is reused
- `slots`, `rewrite_jobs`, `classify_results`, `rewrite_outputs` are dropped
- Writer pipeline may still hold up to 32 in-flight compressed blobs

### The problem: peak RSS spike at planet scale

Planet PBF has ~43,000 blobs. Typical blob sizes:

| Metric | Compressed (wire) | Decompressed |
|--------|-------------------|--------------|
| Node blob | 30-65 KB | 600 KB - 2 MB |
| Way blob | 20-55 KB | 150 KB - 1.5 MB |
| Relation blob | 5-60 KB | 50 KB - 2 MB |
| Outlier (dense urban) | up to 130 KB | up to 4 MB |

With `BATCH_SIZE=64`, peak memory in a single batch iteration:

1. **Raw frames:** 64 blobs x ~65 KB avg = ~4.2 MB (compressed wire bytes)
2. **Classify decompressed (NeedsRewrite):** Up to 64 PrimitiveBlocks, each
   ~1.5 MB decompressed = ~96 MB worst case (100% rewrite ratio)
3. **Rewrite jobs:** Same PrimitiveBlocks (moved, not copied) + inline_upserts
4. **Rewrite outputs:** Each rewritten blob produces ~1-3 OwnedBlocks, each
   ~150-250 KB. 64 rewrites x 2 blocks x 200 KB = ~25 MB
5. **Writer pipeline in-flight:** 32 compressed blobs x ~50 KB = ~1.6 MB
6. **Passthrough coalescing:** variable, 0-4 MB during passthrough runs

**Worst case per batch:** ~130 MB when rewrite ratio is high and blobs are large.

The real problem: **blob size variance**. Planet PBF has both small relation
blobs (10 KB compressed, 50 KB decompressed) and large node blobs (130 KB
compressed, 4 MB decompressed). A batch of 64 large node blobs during a
high-rewrite window (e.g., a diff that touches many node ranges) holds:
- 64 x 130 KB raw frames = 8.3 MB
- 64 x 4 MB decompressed PrimitiveBlocks = 256 MB
- 64 x ~300 KB rewrite output = 19 MB
- Total: ~283 MB in a single batch

With the writer pipeline's 32 slots also full, and the reader thread's 64-frame
read-ahead channel, total in-flight memory can spike to **~400 MB** during
worst-case windows. On 64 GB RAM this is manageable, but the spikes are
unpredictable and can interact with OS page cache pressure during io_uring writes.

## Memory model: bytes per blob at each phase

### Phase 1 (classify)

Each blob's memory cost depends on its classification path:

| Path | Cost per blob |
|------|---------------|
| Index hit (passthrough, no decompress) | `frame_bytes.len()` only (~50 KB) |
| Scan-only (decompress + scan, passthrough) | `frame_bytes.len()` + thread-local buf (recycled) |
| False positive (decompress + full parse, passthrough) | `frame_bytes.len()` + `PrimitiveBlock` (~1.5 MB, temporary during classify) |
| NeedsRewrite | `frame_bytes.len()` + `PrimitiveBlock` (~1.5 MB, kept) |

The critical distinction: **passthrough blobs release their decompressed data**
after classify (the thread-local buf is recycled). Only **NeedsRewrite blobs
retain the PrimitiveBlock** across phases 2-4.

### Phase 2 (assign)

Adds `inline_upserts: Vec<i64>` per rewrite job. Typically 10-100 IDs x 8 bytes
= 80-800 bytes. Negligible.

### Phase 3 (rewrite)

Each rewrite job produces `RewriteOutput`:
- `Vec<OwnedBlock>`: 1-3 blocks per input blob (due to block splitting at 8000
  entity limit and type boundaries)
- Each `OwnedBlock` `(Vec<u8>, BlobIndex, Option<Vec<u8>>)`:
  - Serialized block bytes: 100-250 KB typical, up to 500 KB for dense node blocks
  - Inline BlobIndex: 42 bytes
  - Tagdata: 50-500 bytes

Average rewrite output per blob: ~350 KB (1.5 blocks x 230 KB).

### Phase 4 (output)

Passthrough path: moves `frame_bytes` into `passthrough_buf` (zero-copy via
`std::mem::take`). Cost: the frame_bytes Vec is now in passthrough_buf.

Rewrite path: drains `OwnedBlock` bytes into `write_primitive_block_owned()`,
which immediately moves them into a rayon task (pipeline closure). The closure
compresses and sends to writer thread. Cost: block_bytes live in rayon task until
compression completes and channel send succeeds.

### Summary: per-blob cost formula

```
cost_passthrough(blob) = frame_bytes.len()
                          // Only the raw frame, decompressed data is recycled

cost_rewrite(blob) = frame_bytes.len()           // raw frame (until Phase 4)
                   + decompressed_size            // PrimitiveBlock (until Phase 4)
                   + rewrite_output_size           // OwnedBlock bytes (Phase 3-4)
                   // PrimitiveBlock and raw frame freed after Phase 4 drains rewrite

cost_writer_inflight = compressed_blob_size       // in writer channel, up to 32 slots
```

Typical ratios at planet scale:
- ~92% passthrough (daily diff): cost_passthrough dominates
- ~8% rewrite: cost_rewrite spikes are the RSS concern
- Rewrite ratio varies by blob type: node blobs ~5%, way blobs ~15%, relation
  blobs ~20% (way/relation diffs are denser per-blob)

## Byte-budget algorithm

### Core idea

Replace the fixed `BATCH_SIZE = 64` (blob count) with a byte budget that limits
the total estimated in-flight memory for a single batch. The batch loop collects
frames until either the byte budget is reached or no more frames are available.

### Budget parameters

```rust
/// Maximum estimated in-flight bytes for a single batch iteration.
/// Includes raw frames + potential decompressed data + rewrite outputs.
///
/// 128 MB is conservative for 64 GB RAM: leaves headroom for the writer
/// pipeline (32 x 50 KB = 1.6 MB), OS page cache, rayon thread stacks,
/// and the diff overlay (~100-300 MB for planet daily diffs).
const BATCH_BYTE_BUDGET: usize = 128 * 1024 * 1024; // 128 MB

/// Minimum batch size (blob count) to avoid degenerate single-blob batches
/// that underutilize parallel classify/rewrite.
const BATCH_MIN_BLOBS: usize = 8;

/// Maximum batch size (blob count) to bound the slots/rewrite_jobs vectors
/// and avoid excessive phase 2/4 sequential work per batch.
const BATCH_MAX_BLOBS: usize = 128;
```

### Per-blob byte estimate

When collecting frames into a batch, estimate each blob's potential cost:

```rust
fn estimate_blob_cost(frame: &RawBlobFrame) -> usize {
    let raw_size = frame.frame_bytes.len();

    // If the blob has indexdata and the diff doesn't overlap its range,
    // it will be classified as passthrough without decompression.
    // But we don't know that at collection time — we must be conservative.
    //
    // Estimate: raw_size (always held) + decompressed_size (held if NeedsRewrite)
    // Decompressed size is typically 10-30x the compressed size for zlib PBFs.
    // Use 16x as a middle estimate; actual ratio varies:
    //   - Node blobs: ~20x (highly compressible dense coords)
    //   - Way blobs: ~12x (refs + tags)
    //   - Relation blobs: ~8x (tags + member lists)
    //
    // This is intentionally an overestimate: it's better to have smaller batches
    // (slightly lower parallelism) than to blow the memory budget.
    let estimated_decompressed = raw_size * 16;

    // Rewrite output is typically ~30% of decompressed size (serialized blocks
    // are slightly larger than the original due to different string table layout,
    // but compression is not counted here since that happens in the writer pipeline).
    let estimated_rewrite_output = estimated_decompressed / 3;

    raw_size + estimated_decompressed + estimated_rewrite_output
}
```

### Adaptive refinement: use blob index for tighter estimates

When a `RawBlobFrame` has indexdata (`frame.index.is_some()`), we can make a
much tighter estimate:

```rust
fn estimate_blob_cost_refined(
    frame: &RawBlobFrame,
    ranges: &DiffRanges,
) -> usize {
    let raw_size = frame.frame_bytes.len();

    // If we have an index and the diff doesn't overlap, this blob will be
    // pure passthrough — no decompression, no rewrite.
    if let Some(ref idx) = frame.index {
        if !ranges.range_overlaps(idx.kind, idx.min_id, idx.max_id) {
            // Pure passthrough: only raw frame bytes are held.
            return raw_size;
        }
    }

    // Range overlaps or no index — must assume potential rewrite.
    let estimated_decompressed = raw_size * 16;
    let estimated_rewrite_output = estimated_decompressed / 3;
    raw_size + estimated_decompressed + estimated_rewrite_output
}
```

This is a major improvement: at planet scale with daily diffs, ~92% of blobs
have indexdata and don't overlap the diff. These get a cost of just `raw_size`
(~50 KB), allowing much larger batch counts during passthrough-heavy windows
while still constraining memory during rewrite-heavy windows.

### Batch collection loop

Replace the current batch collection (lines 1068-1089):

```rust
loop {
    batch.clear();
    let mut batch_bytes: usize = 0;

    while batch.len() < BATCH_MAX_BLOBS {
        // Stop collecting if we've hit the byte budget (unless below minimum)
        if batch.len() >= BATCH_MIN_BLOBS && batch_bytes >= BATCH_BYTE_BUDGET {
            break;
        }

        let frame = match frame_rx.try_recv() {
            Ok(frame) => frame,
            Err(mpsc::TryRecvError::Empty) => {
                if batch.is_empty() {
                    // Block for first frame
                    match frame_rx.recv() {
                        Ok(frame) => frame,
                        Err(_) => break, // reader done
                    }
                } else {
                    break; // partial batch, proceed
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
        };

        batch_bytes += estimate_blob_cost_refined(&frame, &ranges);
        batch.push(frame);
    }
    if batch.is_empty() {
        break;
    }

    // ... phases 1-4 ...
}
```

### Why the estimate runs before classify (not after)

One might think: "classify tells us exactly which blobs need rewrite — why not
classify first, then decide batch size?" The answer is that classify IS the
expensive operation. Phase 1 decompresses blobs on the rayon pool, which is where
the memory spike happens. We need to limit the batch size BEFORE phase 1 runs,
not after.

The pre-classify estimate using indexdata is the key enabler: for indexed PBFs
(which is the target use case — all pbfhogg-produced files have indexdata), the
range overlap check is O(log n) on the diff's sorted ID vectors and tells us
with high confidence whether a blob will be passthrough or needs rewrite.

### Post-classify budget check (optional refinement)

After Phase 1, we know the exact classify results. If the batch turns out to
have much higher rewrite ratio than estimated (e.g., all 64 blobs need rewrite),
we could split the batch and process it in two halves. This adds complexity for
diminishing returns — the pre-classify estimate is already quite good for indexed
PBFs. Recommend deferring this to a follow-up if profiling shows it's needed.

## Backpressure design

### Current backpressure mechanisms

1. **Reader thread -> batch loop:** `sync_channel(BATCH_SIZE=64)`. The reader
   thread blocks when 64 frames are buffered. This provides read-ahead without
   unbounded memory growth.

2. **Batch loop -> writer pipeline:** `sync_channel(WRITE_AHEAD=32)`. The main
   thread blocks on `write_primitive_block_owned()` or `write_raw_owned()` when
   32 framed blobs are in-flight in the writer channel.

3. **Within writer pipeline:** Rayon tasks for compression are spawned inline
   during `write_primitive_block_owned()`. The `SyncSender::send()` call blocks
   if the channel is full. This is implicit backpressure: if the writer thread
   can't drain fast enough, rayon tasks pile up and the channel fills, stalling
   the main thread.

### New backpressure: reader channel size tracks byte budget

With byte-budgeted batching, the reader channel size should adapt:

```rust
// Size the reader channel to hold ~2 batches worth of frames,
// so the reader thread stays ahead without over-buffering.
// At planet scale with daily diffs, most blobs are ~50 KB passthrough,
// so 2 * BATCH_BYTE_BUDGET / 50_000 ≈ 5120 frames. Cap at 256 to bound
// the channel's internal VecDeque.
let reader_channel_size = 256.min(
    2 * BATCH_BYTE_BUDGET / 50_000
);
let (frame_tx, frame_rx) = mpsc::sync_channel::<RawBlobFrame>(reader_channel_size);
```

Actually, this is over-engineering. The reader channel size does not need to
change. The current `sync_channel(BATCH_SIZE)` with `BATCH_SIZE=64` bounds the
reader thread to 64 frames ahead. With adaptive batch sizing, the batch loop
may consume fewer or more frames per iteration. The reader channel should be
sized for read-ahead efficiency, not batch size:

```rust
/// Reader channel capacity. Decouples I/O from processing without
/// excessive buffering. 128 frames x ~50 KB avg = ~6.4 MB read-ahead.
const READER_CHANNEL_SIZE: usize = 128;
```

### Writer pipeline backpressure remains unchanged

The `WRITE_AHEAD=32` in `writer.rs` already provides effective backpressure.
The byte-budgeted batch sizing is orthogonal: it controls how much data is
in-flight BEFORE the writer pipeline, while WRITE_AHEAD controls what's
in-flight INSIDE the writer pipeline.

If we wanted to also byte-budget the writer pipeline, that would be a separate
change (P3). The writer pipeline blobs are compressed (30-65 KB each), so 32
slots x 65 KB = ~2 MB — not a significant contributor to RSS.

### No explicit backpressure signal from writer to batch loop

The current architecture has implicit backpressure via `SyncSender::send()`
blocking. This is sufficient because:

1. The batch loop's Phase 4 calls `write_primitive_block_owned()` and
   `write_raw_owned()` which are the send points.
2. If the writer can't keep up, sends block, the batch loop stalls, and no new
   batches are collected.
3. The byte budget only controls Phase 1-3 in-flight data. Once Phase 4 drains
   data into the writer channel, that memory is freed.

Adding an explicit byte-tracking signal from the writer (e.g., tracking how many
bytes are in the writer's reorder buffer) would enable the batch loop to make
tighter estimates but adds complexity for minimal gain. The writer's in-flight
data is small (2 MB max) relative to the batch's in-flight data (128 MB budget).

## Interaction with passthrough vs rewrite paths

### Passthrough-dominated batches (typical for daily diffs)

With ~92% passthrough and indexed PBFs, `estimate_blob_cost_refined()` returns
`raw_size` (~50 KB) for most blobs. The byte budget allows:
```
128 MB / 50 KB = ~2,560 blobs per batch
```
Capped by `BATCH_MAX_BLOBS=128`, so batches will be 128 blobs of mostly
passthrough. This is fine — passthrough blobs in Phase 4 are cheap (move bytes
to passthrough_buf, single write_raw_owned).

The passthrough coalescing buffer (`passthrough_buf`) could grow large with 128
blobs: 128 x 50 KB = 6.4 MB. This is acceptable and will be flushed at batch
boundary or when a rewrite blob interrupts the run.

### Rewrite-dominated batches (large diffs or node-heavy regions)

When many blobs need rewriting, `estimate_blob_cost_refined()` returns the full
estimate (~21x raw_size). For 50 KB raw blobs:
```
128 MB / (50 KB * 21) = ~122 blobs per batch
```
But for large node blobs (130 KB compressed):
```
128 MB / (130 KB * 21) = ~47 blobs per batch
```
This is close to the current BATCH_SIZE=64 but adapts: smaller blobs get larger
batches, larger blobs get smaller batches. Exactly the desired behavior.

### Mixed batches (passthrough + rewrite)

The byte budget naturally handles mixed batches. A batch might collect 100
passthrough blobs (100 x 50 KB = 5 MB estimated) before hitting 20 rewrite blobs
(20 x 1.05 MB = 21 MB estimated), totaling 26 MB — well within budget. The
BATCH_MAX_BLOBS=128 cap prevents excessive sequential work in Phase 2/4.

### Degenerate case: single enormous blob

If a single blob's estimated cost exceeds `BATCH_BYTE_BUDGET`, the
`BATCH_MIN_BLOBS=8` floor ensures we still collect at least 8 blobs. This
prevents single-blob batches that waste rayon parallelism. The floor trades
memory overshoot for throughput: with 8 large blobs, peak memory could be
8 x (130 KB + 4 MB + 1.3 MB) = ~43 MB — well within tolerance.

## Edge cases

### Very large blobs (>2 MB compressed)

These are rare in practice but can occur in planet PBFs with dense urban areas.
A 2 MB compressed blob with 30x expansion ratio produces a 60 MB decompressed
PrimitiveBlock. With the 16x estimate multiplier, we'd estimate
2 MB x 21 = 42 MB per blob. The budget would allow ~3 such blobs per batch
(plus the minimum floor of 8). In practice, these large blobs are almost always
node blobs in passthrough regions, so the refined estimate correctly returns just
2 MB (raw_size).

If a large blob DOES need rewrite: the rewrite will produce many OwnedBlocks
(since block splitting at 8000 entities kicks in), but total serialized output
is bounded by the decompressed size. The 21x estimate accounts for this.

### Blob size variance within a batch

The current fixed-count approach treats a batch of 64 tiny relation blobs
(10 KB each, 640 KB total) the same as a batch of 64 large node blobs (130 KB
each, 8.3 MB total). With byte budgeting, the relation batch collects up to
BATCH_MAX_BLOBS=128 (since total estimate is low), while the node batch
collects fewer. This is strictly better: more parallelism when it's cheap, less
when it's expensive.

### Rewrite ratio changes mid-file

Planet PBFs are ordered: all nodes first, then all ways, then all relations.
Daily diffs tend to cluster changes:
- Node section: ~5% rewrite (geographic locality of edited nodes)
- Way section: ~15% rewrite (way edits touch more blobs)
- Relation section: ~20-40% rewrite (relation edits are sparse but dense per-blob)

The byte budget adapts automatically as the batch loop processes different
sections. Node blobs are larger but mostly passthrough → large batches. Relation
blobs are smaller but more often rewritten → medium batches. No explicit
adaptation logic is needed.

### Empty diffs or zero-overlap

If the diff has no changes for a given element type (e.g., no node changes),
all node blobs will be pure passthrough. `estimate_blob_cost_refined()` returns
`raw_size` for all of them, batches are BATCH_MAX_BLOBS=128, and Phase 1
classifies them all as passthrough without decompression (index hit path).
This is the optimal fast path and is unaffected by the byte budget.

### Non-indexed PBFs

For PBFs without indexdata (produced by other tools), `frame.index` is `None`,
so `estimate_blob_cost_refined()` always returns the full estimate (21x raw_size).
Batches will be smaller (comparable to current BATCH_SIZE=64). This is correct:
without indexdata, every blob must be decompressed during classify, so the
conservative estimate matches the actual cost.

## Migration path

### Step 1: Add the byte-budget constants and estimator (non-breaking)

Add `BATCH_BYTE_BUDGET`, `BATCH_MIN_BLOBS`, `BATCH_MAX_BLOBS`,
`READER_CHANNEL_SIZE`, and `estimate_blob_cost_refined()` as private constants
and functions in `merge.rs`. Keep `BATCH_SIZE` temporarily for reference.

### Step 2: Replace the batch collection loop

Replace the current `while batch.len() < BATCH_SIZE` loop with the
byte-budgeted collection loop. Update the reader channel size. Remove the
`BATCH_SIZE` constant.

### Step 3: Observability

Add batch-level metrics to the progress output:
```rust
if blob_count.is_multiple_of(500) {
    eprintln!(
        "  Blob {blob_count}: {} pass ({} idx) / {} rewrite, {} elements, batch={} ({:.1} MB est)",
        stats.blobs_passthrough, stats.blobs_index_hit,
        stats.blobs_rewritten, stats.total_elements(),
        batch.len(), batch_bytes as f64 / (1024.0 * 1024.0),
    );
}
```

This lets us monitor batch sizing behavior in benchmarks without additional
tooling.

### Step 4: Tune constants via benchmarking

Run `brokkr bench merge` on Denmark, Germany, and North America datasets.
Compare RSS (via `/proc/self/status` VmRSS sampling) and throughput. Adjust
`BATCH_BYTE_BUDGET`, `BATCH_MIN_BLOBS`, `BATCH_MAX_BLOBS`, and the 16x
expansion estimate as needed.

### Non-changes

- `PbfWriter` and its `WRITE_AHEAD=32` constant are NOT modified. Writer
  backpressure is orthogonal and already well-sized.
- `rewrite_block_parallel()` is NOT modified. It operates on a single
  PrimitiveBlock and is unaffected by batch sizing.
- `classify_only()` is NOT modified. It operates on a single RawBlobFrame.
- The reader thread is NOT modified beyond the channel size constant.
- `passthrough_buf` coalescing is NOT modified.

## Files to modify

1. **`src/commands/merge.rs`** -- the only file that needs changes:
   - Add `BATCH_BYTE_BUDGET`, `BATCH_MIN_BLOBS`, `BATCH_MAX_BLOBS` constants
   - Add `READER_CHANNEL_SIZE` constant (or just inline the value)
   - Add `estimate_blob_cost_refined()` function
   - Replace batch collection loop (lines 1060-1089)
   - Update progress logging to include batch size and estimated bytes
   - Remove `BATCH_SIZE` constant

No other files need modification.

## Testing and measurement strategy

### Unit tests

No new unit tests needed — the byte budget is a heuristic, not a correctness
property. Existing `brokkr verify merge` validates output correctness.

### Benchmark protocol

1. **Baseline (current BATCH_SIZE=64):**
   ```
   brokkr bench merge --dataset denmark
   brokkr bench merge --dataset germany
   ```
   Record throughput (ms) and peak VmRSS.

2. **After implementation:**
   Same benchmark commands. Compare:
   - Throughput: should be within 5% (acceptable regression: up to 10%)
   - Peak VmRSS: should decrease, especially on Germany (4.5 GB, 18.4% rewrite)

3. **RSS measurement:**
   Add a simple VmRSS sampler to the merge function that reads
   `/proc/self/status` at batch boundaries and records the high-water mark.
   This could be behind a `#[cfg(target_os = "linux")]` guard or printed to
   stderr alongside the progress line.

4. **Stress test (synthetic):**
   Create a test with a large diff that forces high rewrite ratio to exercise
   the worst-case path. Use `brokkr verify merge --dataset germany` which
   already applies a real daily diff.

### Verification

```
brokkr verify merge --dataset denmark
brokkr verify merge --dataset germany
```

Both must produce identical output to osmium. The byte budget affects batch
sizing only — it does not change the per-blob processing logic.

## Risk assessment

### Low risk: correctness

The batch sizing is pure scheduling — it controls how many blobs are processed
per iteration but does not change the Phase 1-4 logic within each iteration.
All existing correctness guarantees (sorted output, complete diff application,
passthrough fidelity) are preserved.

### Low risk: throughput regression

The byte budget allows larger batches during passthrough-heavy windows (up to
BATCH_MAX_BLOBS=128 vs current 64) and smaller batches during rewrite-heavy
windows. Net effect on throughput should be neutral or slightly positive:
- Passthrough: larger batches reduce loop overhead and improve coalescing
- Rewrite: smaller batches reduce memory pressure, potentially improving cache
  behavior and reducing allocation overhead

### Low risk: estimate accuracy

The 16x expansion estimate is intentionally conservative. If it's too aggressive
(underestimates), batches are too large and we spike RSS — but BATCH_MAX_BLOBS
caps the damage. If it's too conservative (overestimates), batches are
unnecessarily small — but BATCH_MIN_BLOBS provides a floor.

The refined estimator using indexdata is highly accurate for the passthrough case
(exact raw_size, no decompression) and only uses the heuristic for the
overlap-detected case. At planet scale with daily diffs, ~92% of blobs take the
accurate passthrough path.

### Medium risk: tuning sensitivity

The `BATCH_BYTE_BUDGET` constant may need adjustment for different hardware
(32 GB vs 64 GB vs 128 GB RAM). The current plan uses a fixed 128 MB budget.
A future enhancement could make this configurable via a `--memory-budget` CLI
flag, but this adds API surface. Start with 128 MB and adjust based on
benchmarking.

### No risk: API compatibility

All changes are internal to `merge.rs`. The public `merge()` function signature
and `MergeStats` return type are unchanged. No library API changes.

## Results (commit e1099c4)

Implementation: replaced `BATCH_SIZE=64` with `BATCH_BYTE_BUDGET=128MB`,
`BATCH_MIN_BLOBS=8`, `BATCH_MAX_BLOBS=128`, `READER_CHANNEL_SIZE=128`.
Added `estimate_blob_cost()` using `DiffRanges::range_overlaps()` for
index-based passthrough detection. Updated progress logging with batch
size and estimated bytes.

### vs E1.1 baseline (commit 1d291f1)

| Dataset | Variant | Time Δ | RSS Δ |
|---------|---------|--------|-------|
| Germany | buffered+zlib | 6381→5728 ms (**-10.2%**) | 652→532 MB (**-18.4%**) |
| Germany | buffered+none | 4465→3710 ms (**-16.9%**) | 601→388 MB (**-35.4%**) |
| Denmark | buffered+zlib | 474→395 ms (**-16.7%**) | 212→229 MB (+7.7%) |
| Denmark | buffered+none | 364→271 ms (**-25.5%**) | 171→178 MB (+3.9%) |

### vs original baseline (commit a3fc5ad, pre-E1.1)

| Dataset | Variant | Time Δ | RSS Δ |
|---------|---------|--------|-------|
| Germany | buffered+zlib | 6321→5728 ms (**-9.4%**) | 710→532 MB (**-25.1%**) |
| Germany | buffered+none | 4685→3710 ms (**-20.8%**) | 635→388 MB (**-38.9%**) |
| Denmark | buffered+zlib | 453→395 ms (-12.8%) | 220→229 MB (+3.9%) |
| Denmark | buffered+none | 328→271 ms (-17.4%) | 181→178 MB (-1.8%) |

### Adaptive behavior observed

- **Node section (blobs 1-53500):** Batches hit BATCH_MAX_BLOBS=128 with
  6-72 MB estimated — mostly passthrough, maximizing parallelism.
- **Way/relation section (blobs 54000+):** Batches drop to 18-93 blobs with
  ~128 MB estimated — byte budget actively constrains rewrite-heavy windows.
- Denmark: all-passthrough means batches always hit 128, small RSS uptick
  from more raw frames in memory (irrelevant at this scale).

### Decision: KEEP

Both throughput and memory improved on Germany. The throughput improvement
is a bonus from larger passthrough batches. Denmark's small RSS increase
is acceptable (228 MB vs 212 MB, all passthrough at small scale).
