# P3: Stream Rewrite Outputs Instead of Full Phase Materialization

## Problem Statement

Planet-scale OSM PBF merge (75 GB input, ~92% rewrite ratio for daily diffs,
64 GB RAM). The merge pipeline collects ALL rewrite outputs in a materialized
`Vec<RewriteOutput>` during Phase 3, then drains them sequentially in Phase 4.
This means raw frames (batch), parsed `PrimitiveBlock`s (in `RewriteJob`),
AND rewritten `OwnedBlock` outputs all coexist in memory simultaneously
during Phase 3. At planet scale with 92% rewrite ratio, this creates a
high-water mark that is avoidable.

Goal: emit rewrite outputs incrementally to the pipelined writer channel as
they complete, reducing simultaneous ownership of raw + parsed + rewritten
payloads. Target: reduced rewrite-window RSS without ordering regressions.

---

## Current State: Exact Memory Timeline

### Per-batch lifecycle (BATCH_SIZE = 64 blobs)

Each blob is ~64 KB compressed in `RawBlobFrame`, ~1.4 MB decompressed in
`PrimitiveBlock`, and produces ~130 KB of rewritten `OwnedBlock` output.

```
Time ──────────────────────────────────────────────────────────────────────►

Batch receive:
  batch: Vec<RawBlobFrame>          ← 64 × ~64 KB = ~4 MB
  (reader thread fills via sync_channel)

Phase 1 (parallel classify):
  batch: 4 MB                       HELD
  classify_results: Vec<ClassifyResult>
    - Passthrough: no extra memory
    - NeedsRewrite: PrimitiveBlock ~1.4 MB each
  At 92% rewrite: ~59 blobs × 1.4 MB = ~83 MB in PrimitiveBlocks

Phase 2 (sequential assign):
  batch: 4 MB                       HELD
  classify_results → consumed → dropped
  rewrite_jobs: Vec<RewriteJob>     ← 59 jobs × (PrimitiveBlock 1.4 MB + inline_upserts)
                                      = ~83 MB (PrimitiveBlocks moved, not copied)
  slots: Vec<BatchSlot>             ← trivial

Phase 3 (parallel rewrite):
  batch: 4 MB                       HELD ← needed for Phase 4 passthrough
  rewrite_jobs: ~83 MB              HELD ← PrimitiveBlock needed by rewriter
  rewrite_results: Vec<RewriteOutput>  ← accumulating
    Each RewriteOutput.blocks: typically 1-2 OwnedBlock per job
    ~59 jobs × ~130 KB = ~7.7 MB of serialized blocks

  *** PEAK: batch (4 MB) + rewrite_jobs (83 MB) + rewrite_results (7.7 MB) ***
  ***       = ~95 MB per batch of 64 blobs                                  ***

Phase 4 (sequential output):
  batch: 4 MB                       HELD (passthrough frames consumed here)
  rewrite_jobs: dropped (consumed)
  rewrite_results: draining (blocks sent to writer, then dropped)
  → Memory falls back to ~4 MB
```

### Planet-scale extrapolation

Planet daily diff touches ~92% of blobs (4M changes spread across ~2.5M
blobs). With 64-blob batches:
- ~59 rewrites per batch is the common case
- Peak per-batch: ~95 MB

The issue is NOT that 95 MB per batch is large. The issue is that
**all 59 PrimitiveBlocks and all 59 RewriteOutputs coexist simultaneously**
during Phase 3. With larger batches (a P2 optimization may increase
BATCH_SIZE to 128-256 for better amortization), this doubles/quadruples.

More critically, at 92% rewrite, the `rewrite_jobs` Vec holds ~59
`PrimitiveBlock`s all at once. Each `PrimitiveBlock` owns a `Bytes` buffer
of ~1.4 MB (the decompressed protobuf). These cannot be freed until the
corresponding rewrite completes. With streaming, as each rewrite completes,
its `PrimitiveBlock` can be dropped immediately, and its `RewriteOutput`
can be sent to the writer channel rather than accumulated.

### Where rewrite outputs are collected

In `merge()` at line 1132-1147:

```rust
let rewrite_results: Vec<Result<RewriteOutput, String>> = rewrite_jobs
    .par_iter()
    .map_init(
        BlockBuilder::new,
        |thread_bb, job| {
            rewrite_block_parallel(&job.block, &diff, thread_bb, &job.inline_upserts, job.kind)
                .map_err(|e| e.to_string())
        },
    )
    .collect();
```

The `.collect()` materializes ALL results before Phase 4 can begin.

### How long they are held

Rewrite outputs are held from Phase 3 `.collect()` (line 1147) until
Phase 4 drains them in `BatchSlot::Rewrite` handling (lines 1239-1257).
This is the full duration of Phase 4's sequential iteration over all 64
slots, which includes passthrough writes, gap create emission, type
transition handling, and passthrough buffer coalescing.

### When they are drained

In Phase 4, line 1241-1244:
```rust
let output = &mut rewrite_outputs[*job_index];
for (block_bytes, index, tagdata) in output.blocks.drain(..) {
    writer.write_primitive_block_owned(block_bytes, index, tagdata.as_deref())?;
}
```

Each `RewriteOutput` is drained when its slot is reached in file order.
The `drain(..)` frees the inner `Vec<OwnedBlock>` as blocks are sent to
the writer. But outputs whose file-order position is late in the batch
wait for all earlier slots to be processed first.

---

## The Ordering Contract

### Why ordering matters

The PBF spec requires `Sort.Type_then_ID`: all nodes before all ways
before all relations, and within each type, elements are sorted by ID.
The output PBF header declares `is_sorted()`, and downstream consumers
(nidhogg, elivagar, osm2pgsql, Planetiler) rely on this for merge-join
algorithms, binary search on ID ranges, and streaming processing.

### Ordering invariants in the current design

1. **Batch-level**: Batches are processed strictly sequentially. Batch N
   completes entirely before batch N+1 starts. No cross-batch reordering.

2. **Within-batch file order**: Phase 4 iterates `slots` in the same order
   as `batch` (file order). Passthrough blobs are written at their original
   position. Rewrite outputs are written at the position of the blob they
   replaced.

3. **Within-rewrite order**: `rewrite_block_parallel` preserves element
   order within a blob: base elements are iterated in file order, creates
   are interleaved at their sorted position via `upsert_cursor`, and
   `flush_local` collects blocks in the order they fill up.

4. **Gap creates**: Between blobs, `emit_gap_creates` and
   `flush_remaining_upserts` emit creates whose IDs fall in gaps between
   consecutive blobs. These are emitted at the correct file-order position
   in Phase 4.

5. **Type transitions**: When blob type changes (Node -> Way, Way ->
   Relation), remaining upserts of the previous type are flushed before
   any blob of the new type is written.

### What streaming must preserve

Any streaming design must maintain invariant (2): output blobs appear in
the same file order as input blobs. This means rewrite output for blob N
must be written before any data from blob N+1. Streaming cannot reorder
blobs, but it CAN release them earlier once all predecessors are written.

---

## Design for Incremental Emission

### Core idea: reorder buffer between Phase 3 and the writer

Instead of materializing `Vec<RewriteOutput>` and then draining in Phase 4,
introduce a reorder buffer that accepts rewrite results as they complete
from rayon and drains them to the writer in file order. Passthrough blobs
are injected directly into the same reorder buffer.

This is the **same pattern** used by the read-side pipeline
(`src/read/pipeline.rs`) and the write-side `writer_thread`: a
`VecDeque<Option<...>>` indexed by sequence number, draining from the
front when consecutive slots are filled.

### Proposed architecture

```
                     ┌──────────────────────────┐
  Reader thread ───► │  batch: Vec<RawBlobFrame> │
                     └──────────────────────────┘
                                │
                     Phase 1: par_iter classify
                                │
                     Phase 2: sequential assign
                                │
                 ┌──────────────┴──────────────┐
                 │                             │
           Passthrough blobs            Rewrite jobs
                 │                             │
                 │                    rayon::spawn each job
                 │                    (sends result to channel)
                 │                             │
                 ▼                             ▼
          ┌─────────────────────────────────────────┐
          │  Reorder buffer (main thread)           │
          │  VecDeque<Option<OutputSlot>>            │
          │                                         │
          │  slot 0: Passthrough(frame_bytes)        │
          │  slot 1: Rewrite(RewriteOutput)    ◄─── rayon result
          │  slot 2: Passthrough(frame_bytes)        │
          │  slot 3: None (not yet complete)         │
          │  ...                                     │
          │                                         │
          │  Drain front when filled:               │
          │    Passthrough → write_raw_owned         │
          │    Rewrite → write_primitive_block_owned │
          │    (with gap creates before each)        │
          └─────────────────────────────────────────┘
                 │
                 ▼
          PbfWriter (pipelined, owns writer thread)
```

### Detailed design

#### Step 1: Unified output slot enum

```rust
enum OutputSlot {
    /// Raw passthrough: frame bytes ready to write.
    Passthrough {
        frame: RawBlobFrame,
        index: BlobIndex,
        has_indexdata: bool,
    },
    /// Rewrite result from rayon.
    Rewrite(RewriteOutput),
}
```

#### Step 2: Channel for rayon rewrite results

Replace the materialized `Vec<RewriteOutput>` with a bounded channel:

```rust
let (rewrite_tx, rewrite_rx) = mpsc::sync_channel::<(usize, RewriteOutput)>(num_workers);
```

Each rayon task sends `(slot_index, RewriteOutput)` when done.

#### Step 3: Main thread interleaving loop

After Phase 2 builds `slots` and spawns rewrite jobs:

```rust
// Pre-fill reorder buffer with passthrough slots (immediately ready).
let mut reorder: VecDeque<Option<OutputSlot>> = VecDeque::with_capacity(batch.len());
for slot in &slots {
    match slot {
        BatchSlot::Passthrough { .. } | BatchSlot::FalsePositive { .. } => {
            reorder.push_back(Some(OutputSlot::Passthrough { ... }));
        }
        BatchSlot::Rewrite { .. } => {
            reorder.push_back(None); // placeholder, filled by rayon
        }
    }
}

// Spawn rewrite jobs into rayon with slot index.
for (job_idx, job) in rewrite_jobs.iter().enumerate() {
    let slot_idx = job_to_slot_map[job_idx]; // map from job index to batch slot index
    let tx = rewrite_tx.clone();
    rayon::spawn(move || {
        let result = rewrite_block_parallel(...);
        drop(tx.send((slot_idx, result)));
    });
}
drop(rewrite_tx); // close sender

// Drain loop: receive rewrite results and drain head.
let mut next_slot = 0;
let mut pending_rewrites = rewrite_count;

while next_slot < batch.len() {
    // Try to drain filled head slots.
    while next_slot < reorder.len() {
        if reorder.front().is_none_or(Option::is_none) { break; }
        let slot = reorder.pop_front().unwrap().unwrap();
        emit_slot(&slot, next_slot, ...);
        next_slot += 1;
    }

    // If we can't drain, wait for a rewrite result.
    if pending_rewrites > 0 {
        if let Ok((idx, output)) = rewrite_rx.recv() {
            let buf_idx = idx - next_slot;
            reorder[buf_idx] = Some(OutputSlot::Rewrite(output));
            pending_rewrites -= 1;
        }
    }
}
```

#### Step 4: Gap creates and type transitions

Gap creates and type transitions must still be handled sequentially as each
slot is drained. The `emit_slot` function runs the same logic as current
Phase 4: check for type transitions, emit gap creates, then write the
passthrough or rewrite data. This is unchanged from the current design --
the only difference is WHEN each slot's output is available.

### Memory benefit

With streaming, as each rewrite completes:
1. Its `RewriteOutput` is received and placed in the reorder buffer.
2. If it is at the head, it is immediately drained: the `OwnedBlock`s are
   sent to the writer, and the `RewriteOutput` is dropped.
3. Once drained, the corresponding `RewriteJob`'s `PrimitiveBlock` can be
   dropped (if the rayon closure has returned).

The key insight: **PrimitiveBlocks that finish rewriting early can be freed
before later PrimitiveBlocks finish**. In the current design, ALL
PrimitiveBlocks live until ALL rewrites complete. With streaming, only the
PrimitiveBlocks for in-flight (not yet completed) rewrites are alive.

At steady state with N rayon workers, at most N rewrites are in-flight.
With 12 cores and typical 5-8 active workers, that is 5-8 PrimitiveBlocks
(7-11 MB) instead of 59 PrimitiveBlocks (83 MB). The savings are ~72 MB
per batch, or ~76% of the rewrite-phase peak.

The `RewriteOutput` blocks (~130 KB each) are also freed earlier -- as soon
as they drain from the reorder buffer head rather than waiting for Phase 4
to iterate to their position.

---

## How Passthrough and Rewrite Blobs Interleave

### Current interleaving (Phase 4)

Phase 4 iterates `slots` in file order. For a typical batch at 92% rewrite:

```
slot 0: Passthrough  → write_raw_owned immediately
slot 1: Rewrite[0]   → drain rewrite_outputs[0]
slot 2: Rewrite[1]   → drain rewrite_outputs[1]
slot 3: Passthrough  → write_raw_owned
slot 4: Rewrite[2]   → drain rewrite_outputs[2]
...
```

Passthrough blobs can be written immediately (their raw bytes are in
`batch[i].frame_bytes`). Rewrite blobs must wait for their
`RewriteOutput`. In the current design, all rewrites are done before any
Phase 4 processing begins, so there is never any waiting.

### Streaming interleaving

With the reorder buffer, passthrough blobs at the head of the buffer can
be drained immediately without waiting for any rewrite. If the batch
starts with 3 passthrough blobs followed by a rewrite blob, all 3
passthroughs are drained to the writer instantly. The main thread only
blocks when the head slot is a not-yet-completed rewrite.

This means passthrough runs at the head of a batch have ZERO latency --
they flow to the writer without waiting for any rayon work. This is a
throughput improvement beyond just the memory savings: the writer thread
gets data sooner, reducing writer thread idle time.

### Passthrough coalescing interaction

The current passthrough coalescing buffer (`passthrough_buf: Vec<u8>`)
accumulates consecutive passthrough frames and flushes them as a single
`write_raw_owned`. This optimization composes naturally with streaming:
consecutive passthrough slots at the head of the reorder buffer are
coalesced exactly as before. The flush happens either when a rewrite slot
is reached or at the end of the drained run.

### copy_file_range interaction

The `linux-direct-io` copy_file_range path sends individual blobs via
`write_raw_copy`. This also composes naturally: each passthrough slot
drained from the head issues its copy_file_range call immediately.

---

## Interaction with P2 (Byte-Budgeted Batching)

P2 proposes adaptive batch sizing based on a byte budget instead of a
fixed blob count. The two optimizations compose cleanly:

### Why they compose

- **P2 controls batch formation**: how many blobs enter a batch (variable,
  based on byte budget). Larger batches improve classify/rewrite
  amortization.
- **P3 controls batch draining**: how rewrite outputs flow out of the batch
  (streaming via reorder buffer). Earlier draining reduces peak memory.

With P2 alone, larger batches increase peak memory (more PrimitiveBlocks
held simultaneously). P3 **counteracts** this: streaming limits the
simultaneous PrimitiveBlock count to the rayon parallelism, regardless of
batch size.

### Composition

With both P2 and P3:
- P2 sets BATCH_SIZE to, say, 256 blobs (tuned by byte budget).
- Phase 1 classifies 256 blobs in parallel.
- Phase 2 assigns inline upserts for ~236 rewrite blobs (92%).
- Phase 3 spawns 236 rayon tasks, but only ~8 run concurrently.
- The reorder buffer is 256 slots wide, but at most ~8 are pending (the
  rest are filled passthrough slots or already-drained rewrite slots).
- Peak PrimitiveBlock count: ~8 (not ~236).

Without P3, the same P2 configuration would hold 236 PrimitiveBlocks
simultaneously (~330 MB). P3 reduces this to ~11 MB -- a 30x reduction.

### Implementation note

If P2 is implemented first, the streaming design slots in as a drop-in
replacement for the `rewrite_results: Vec<RewriteOutput>` materialization.
If P3 is implemented first, P2's variable batch sizing just changes the
reorder buffer width.

---

## Memory Savings Analysis

### Per-batch savings (64-blob batch, 92% rewrite)

| Component | Current peak | Streaming peak | Savings |
|-----------|-------------|---------------|---------|
| PrimitiveBlocks | 59 x 1.4 MB = 83 MB | 8 x 1.4 MB = 11 MB | 72 MB (87%) |
| RewriteOutputs | 59 x 130 KB = 7.7 MB | 8 x 130 KB = 1.0 MB | 6.7 MB (87%) |
| Reorder buffer overhead | 0 | ~2 KB (VecDeque of Options) | -2 KB |
| Channel (sync_channel) | 0 | ~bounded, trivial | ~0 |
| **Total** | **~91 MB** | **~12 MB** | **~79 MB (87%)** |

### Why 8 and not 59

Rayon's thread pool defaults to `available_parallelism()` threads. On a
typical 12-core server, the pool has 12 threads, but merge also uses rayon
for Phase 1 classify and the pipelined writer's compression. In practice,
5-8 rewrite tasks run concurrently (measured at Germany scale). Even at
full parallelism, 12 concurrent tasks hold 12 PrimitiveBlocks (17 MB),
still far less than the 83 MB materialized peak.

### Planet-scale RSS impact

At planet scale with 64-blob batches, per-batch savings of ~79 MB are
meaningful but not transformative because only one batch is in flight at a
time. The bigger win is enabling LARGER batches (P2) without RSS blowup:

| Batch size | Current peak (92% rewrite) | Streaming peak | Savings |
|-----------|---------------------------|---------------|---------|
| 64        | 91 MB                     | 12 MB         | 79 MB   |
| 128       | 182 MB                    | 12 MB         | 170 MB  |
| 256       | 364 MB                    | 12 MB         | 352 MB  |
| 512       | 728 MB                    | 12 MB         | 716 MB  |

Streaming decouples batch size from memory, allowing P2 to increase
batch size freely for better amortization without memory consequences.

---

## Concurrency Design

### Can rewrite outputs be sent to the writer from rayon threads directly?

**No -- and here is why.** The writer must receive blobs in file order.
Rewrite outputs must be interleaved with passthrough blobs and gap creates.
If rayon threads sent directly to `PbfWriter`, they would need to:

1. Wait for all predecessor slots (passthrough and earlier rewrites) to be
   written first -- this requires cross-task coordination.
2. Emit gap creates before their output -- this requires sequential state
   (upsert cursors, type tracking).
3. Handle passthrough coalescing -- sequential by nature.

This coordination would turn rayon tasks into sequential bottlenecks,
defeating the purpose. The correct design keeps the main thread as the
sequencing authority: it receives results from rayon via a channel and
drains them through the reorder buffer in file order.

### Channel design

```rust
let (rewrite_tx, rewrite_rx) = mpsc::sync_channel::<(usize, Result<RewriteOutput, String>)>(
    rayon::current_num_threads().min(BATCH_SIZE)
);
```

The channel capacity is bounded by the rayon thread count (not batch size).
This provides backpressure: if the main thread is slow to drain, rayon
tasks block on send, limiting memory growth. A capacity of 8-12 means
at most 8-12 completed-but-unread results buffer in the channel.

### Thread interaction diagram

```
Reader thread                    Main thread                      Rayon pool              Writer thread
    │                                │                                │                       │
    │──frame──►                      │                                │                       │
    │          batch.push(frame)     │                                │                       │
    │          ...                   │                                │                       │
    │                          Phase 1: par_iter classify ──────────► │                       │
    │                                │ ◄──── classify_results ────── │                       │
    │                          Phase 2: sequential assign             │                       │
    │                                │                                │                       │
    │                          Pre-fill passthrough slots              │                       │
    │                          Spawn rewrite jobs ──────────────────► │                       │
    │                                │                                │──rewrite job 0──►     │
    │                                │                                │──rewrite job 1──►     │
    │                                │◄─(slot 0 is PT, drain)──       │                       │
    │                                │──write_raw_owned──────────────────────────────────────►│
    │                                │◄─(slot 1 recv)──────────────── │ (rewrite 0 done)     │
    │                                │──write_primitive_block_owned──────────────────────────►│
    │                                │◄─(slot 2 is PT, drain)──       │                       │
    │                                │──write_raw_owned──────────────────────────────────────►│
    │                                │                                │                       │
    │                                │ (blocks on recv for slot 3)    │                       │
    │                                │◄──────────────────────────────  │ (rewrite 1 done)     │
    │                                │──write_primitive_block_owned──────────────────────────►│
```

### Rewrite job ownership model

Current: `rewrite_jobs: Vec<RewriteJob>` holds ownership of all
`PrimitiveBlock`s. `par_iter()` borrows them.

Streaming: `rewrite_jobs` must be consumed (not borrowed) so rayon tasks
can take ownership. Use `into_par_iter()` or `rayon::spawn` with moved
jobs. After spawning, the `RewriteJob` (and its `PrimitiveBlock`) is owned
by the rayon task and dropped when the task completes.

```rust
for (job_idx, job) in rewrite_jobs.into_iter().enumerate() {
    let slot_idx = job_to_slot[job_idx];
    let tx = rewrite_tx.clone();
    let diff_ref = &diff; // shared reference
    rayon::scope(|s| {
        s.spawn(move |_| {
            let mut thread_bb = BlockBuilder::new(); // or use thread-local
            let result = rewrite_block_parallel(&job.block, diff_ref, &mut thread_bb, &job.inline_upserts, job.kind);
            drop(tx.send((slot_idx, result.map_err(|e| e.to_string()))));
            // job (and its PrimitiveBlock) dropped here
        });
    });
}
```

**Important**: Using `rayon::scope` (not `rayon::spawn`) because `diff`
is borrowed. `rayon::spawn` requires `'static`. Two alternatives:

1. **`rayon::scope`**: Spawns all tasks within a scope that borrows `diff`.
   But scope blocks until all tasks complete -- this defeats streaming.

2. **`Arc<DiffOverlay>`**: Wrap diff in Arc, clone per task, use
   `rayon::spawn`. This allows the main thread to run the drain loop
   concurrently with rayon tasks. The Arc overhead is negligible (one
   clone per rewrite blob).

Option 2 is required for streaming. The diff is already immutable during
merge, so `Arc` adds no synchronization cost.

### BlockBuilder reuse with rayon::spawn

Current `par_iter().map_init(BlockBuilder::new, ...)` gives each rayon
thread a reusable BlockBuilder. With `rayon::spawn`, we lose `map_init`.

Options:
- **Thread-local BlockBuilder**: `thread_local!` static, borrowed via
  `with_borrow_mut`. Same pattern as `PIPELINE_SCRATCH` in writer.rs.
- **Pool of BlockBuilders**: Pre-create N builders, tasks take/return.
- **Allocate per task**: BlockBuilder is ~48 KB heap. At 8 tasks/batch,
  this is ~384 KB per batch -- negligible.

Thread-local is cleanest and matches existing patterns.

---

## Risk Assessment

### Ordering bugs

**Risk: Medium. Mitigation: strong.**

The reorder buffer pattern is proven in two existing locations:
- `src/read/pipeline.rs` (line 189-230): identical `VecDeque<Option<...>>`
  pattern for decoded block reordering.
- `writer_thread` in `src/write/writer.rs` (line 620-660): identical
  pattern for write pipeline reordering.

The streaming design adds no new ordering logic -- it reuses the same
pattern. The main risk is incorrect mapping between `job_index` and
`slot_index` (the indirection between rewrite job numbering and batch
slot numbering). This must be computed correctly in Phase 2.

**Mitigation**: Debug assertion that verifies monotonically increasing
blob IDs in the output stream (already exists for read path via
`PrimitiveBlock` sorted assertion). Add an equivalent assertion to the
drain loop.

### Deadlock potential

**Risk: Low.**

The only blocking operation on the main thread is `rewrite_rx.recv()`.
Deadlock requires all rayon threads to be blocked waiting for the main
thread, which would mean the channel is full (all sends block). Since the
channel capacity equals rayon thread count, and we only spawn that many
concurrent tasks, the channel has room for every in-flight task. The main
thread drains received results immediately (or after draining passthrough
slots at the head), so the channel never fills.

**Edge case**: If the main thread blocks on `rewrite_rx.recv()` for a
slot that has not been spawned yet. This cannot happen because all
rewrite jobs are spawned before the drain loop starts.

**Edge case**: If rayon's thread pool is saturated by other work (e.g.
pipelined writer compression tasks). This could delay rewrite tasks,
causing the main thread to block longer on `recv()`. This is not a
deadlock -- just slower throughput. The pipelined writer uses the global
rayon pool for compression, and rewrite tasks also use the global rayon
pool. Under high load, rewrite and compression tasks compete for threads.
This is the current behavior (Phase 3 and the writer's compression tasks
also compete) -- streaming does not worsen it.

### Throughput impact

**Risk: Low positive or neutral.**

Current: Phase 3 blocks the main thread entirely (`.collect()`). Phase 4
runs after Phase 3 completes. Total time = Phase 3 + Phase 4.

Streaming: Phase 3 (rayon tasks) and Phase 4 (main thread drain loop) run
concurrently. The main thread starts draining as soon as the first head
slot is ready. Total time = max(Phase 3, Phase 4) rather than Phase 3 +
Phase 4.

At 92% rewrite, Phase 4 is dominated by `write_primitive_block_owned`
calls (dispatches to rayon for compression). These are non-blocking sends
to the writer pipeline. So Phase 4 is fast relative to Phase 3. The
overlap provides modest throughput improvement.

At 8% rewrite (Denmark), Phase 4 is dominated by passthrough writes.
Streaming lets passthrough slots drain immediately without waiting for
rewrites. Neutral to slight improvement.

**No throughput regression is expected** because the streaming design
performs the same work as the current design, just with different
scheduling. The reorder buffer adds negligible overhead (index arithmetic
on a VecDeque).

---

## Testing Strategy

### Correctness: byte-identical output

The merge output must be byte-identical to the current implementation for
any given input. Test approach:

1. **Existing roundtrip tests**: `tests/roundtrip_real.rs::roundtrip_denmark`
   (`brokkr check -- --ignored`) exercises the full merge path. Run before
   and after the change; compare output files byte-for-byte.

2. **Cross-validation**: `brokkr verify merge` compares pbfhogg merge
   output against osmium/osmosis/osmconvert. This is the gold standard.
   Run with Denmark dataset.

3. **Edge cases to test explicitly**:
   - Batch where ALL blobs are passthrough (0% rewrite) -- reorder buffer
     drains all slots immediately with no rayon recv.
   - Batch where ALL blobs are rewrite (100% rewrite) -- reorder buffer
     blocks on recv for every slot.
   - Batch with alternating passthrough/rewrite -- exercises interleaving.
   - Type transition within a batch (last node blob, first way blob) --
     exercises flush_remaining_upserts within the drain loop.
   - Gap creates between blobs -- exercises emit_gap_creates within drain.
   - Single-blob batch (edge: batch size = 1).
   - Empty batch (reader thread done before any frames sent).

4. **Ordering assertion**: Add debug assertion in the drain loop that
   verifies: for rewrite outputs, the first element ID in the first
   OwnedBlock is >= the last element ID drained from the previous slot.
   This catches ordering regressions at runtime in debug builds.

### Performance measurement

1. **Memory**: `brokkr bench merge --dataset germany` + `/proc/self/status`
   VmHWM (peak RSS) before and after. Germany (18.4% rewrite, 4.5 GB)
   should show modest savings. Planet extrapolation from these numbers.

2. **Throughput**: `brokkr bench merge --dataset germany --runs 5` before
   and after. Expect neutral to slight improvement (concurrent Phase 3+4).

3. **Throughput at scale**: `brokkr bench merge --dataset north-america`
   (18.8 GB, higher rewrite fraction). Larger dataset amplifies any
   per-batch improvement.

4. **Profile**: `brokkr hotpath --dataset germany` to verify that the
   streaming does not introduce new hotspots (channel overhead, VecDeque
   operations).

---

## Implementation Plan

### Step 1: Add reorder buffer infrastructure (low risk)

Add a `StreamingReorderBuf` utility (or inline it) that accepts items
by sequence number and drains the head. This is a trivial struct wrapping
`VecDeque<Option<T>>` with `insert(seq, item)` and `drain_head(callback)`
methods. Can be unit-tested in isolation.

### Step 2: Change RewriteJob ownership model

Change `rewrite_jobs` from `Vec<RewriteJob>` (borrowed by `par_iter`) to
consumed by `into_iter()`. Each job is moved into a `rayon::spawn` closure.
The closure sends `(slot_index, Result<RewriteOutput, String>)` to a
channel.

Wrap `diff` in `Arc<DiffOverlay>` at the start of `merge()`. Clone the
Arc per rayon task.

Add thread-local `BlockBuilder` for rewrite tasks (matching the existing
`PIPELINE_SCRATCH` pattern in writer.rs).

### Step 3: Build the job_index-to-slot_index mapping

During Phase 2, when building `BatchSlot::Rewrite { job_index, .. }`,
also build the reverse mapping: `job_to_slot: Vec<usize>` where
`job_to_slot[job_index] = batch_slot_index`. This is needed so rayon
tasks can label their results with the correct slot index.

### Step 4: Replace Phase 3 + Phase 4 with streaming drain

Replace:
```rust
// Phase 3: parallel rewrite
let rewrite_results: Vec<...> = rewrite_jobs.par_iter().map_init(...).collect();
// Phase 4: sequential output
for (i, slot) in slots.iter().enumerate() { ... }
```

With:
```rust
// Phase 3+4 combined: spawn rewrites, then drain reorder buffer
let (rewrite_tx, rewrite_rx) = ...;
for (job_idx, job) in rewrite_jobs.into_iter().enumerate() {
    // spawn rayon task
}
drop(rewrite_tx);

// Pre-fill reorder buffer with passthrough/false-positive slots
// Drain loop: recv rewrite results, drain head, emit to writer
```

### Step 5: Verify and benchmark

Run the full test and verification suite:
- `brokkr check`
- `brokkr check -- --ignored` (Denmark roundtrip)
- `brokkr verify merge --dataset denmark`
- `brokkr bench merge --dataset germany --runs 5`
- `brokkr hotpath --dataset germany`

### Estimated effort: Medium

- Step 1: ~30 lines, trivial
- Step 2: ~40 lines, moderate (ownership changes, Arc wrapping)
- Step 3: ~10 lines, trivial
- Step 4: ~80 lines (replaces ~60 lines of Phase 3+4 code)
- Step 5: testing/benchmarking

Total: ~160 lines changed, ~1-2 sessions of focused work.

### Dependencies

- No dependency on P2 (byte-budgeted batching). Can be implemented
  independently in any order.
- No dependency on any other open perf-review item.
- Requires no new crate dependencies.

---

## Summary

Stream rewrite outputs via a reorder buffer between Phase 3 (parallel
rewrite) and the pipelined writer. Reuses the proven `VecDeque` reorder
pattern from `pipeline.rs` and `writer_thread`. Reduces rewrite-phase peak
memory by ~87% (83 MB -> 11 MB per 64-blob batch at 92% rewrite). More
importantly, decouples batch size from memory pressure, enabling P2's
larger batches without RSS blowup. No ordering regressions: the reorder
buffer preserves file order exactly as Phase 4 does today. Low deadlock
risk, neutral-to-positive throughput impact. Verified by existing
cross-validation suite.

---

## Results

### Implementation (commit 1e03e5b)

Simpler than the full VecDeque reorder buffer proposed above. Used a
`received: Vec<Option<RewriteOutput>>` indexed by `job_index` instead.
The main thread iterates slots in file order and when hitting a Rewrite
slot, receives from the channel in a loop until that job's result arrives.
Out-of-order arrivals are buffered; each entry is `take()`d exactly once.

Key changes to `src/commands/merge.rs` (63 insertions, 46 deletions):
- `Arc<CompactDiffOverlay>` wrapping (simpler than `rayon::scope`)
- `rayon::spawn` per job with `into_iter()` (moves PrimitiveBlock ownership)
- Bounded `sync_channel` (capacity = rayon thread count)
- Per-task `BlockBuilder::new()` (negligible ~48 KB per task)
- Combined Phase 3+4 hotpath timing (phases now overlap)

### Benchmark: E2.2 vs E2.1 (commit 1e03e5b vs e1099c4)

Host: folk-pc. Clean tree. Best of 3 runs stored in `.brokkr/results.db`.

| Dataset | Variant | E2.1 time | E2.2 time | Δ time | E2.1 RSS | E2.2 RSS | Δ RSS |
|---------|---------|-----------|-----------|--------|----------|----------|-------|
| Germany | zlib | 5728 ms | 5335 ms | **-6.9%** | 532 MB | 515 MB | **-3.2%** |
| Germany | none | 3710 ms | 3420 ms | **-7.8%** | 388 MB | 390 MB | +0.6% |
| Denmark | zlib | 395 ms | 363 ms | **-8.1%** | 229 MB | 226 MB | -1.1% |
| Denmark | none | 271 ms | 250 ms | **-7.7%** | 178 MB | 174 MB | -2.1% |

### Cumulative E1.1 + E2.1 + E2.2 vs original (commit 1e03e5b vs a3fc5ad)

| Dataset | Variant | Original | Current | Δ time | Orig RSS | Curr RSS | Δ RSS |
|---------|---------|----------|---------|--------|----------|----------|-------|
| Germany | zlib | 6321 ms | 5335 ms | **-15.6%** | 710 MB | 515 MB | **-27.5%** |
| Germany | none | 4685 ms | 3420 ms | **-27.0%** | 635 MB | 390 MB | **-38.6%** |
| Denmark | zlib | 453 ms | 363 ms | **-19.9%** | 220 MB | 226 MB | +2.8% |
| Denmark | none | 328 ms | 250 ms | **-23.8%** | 181 MB | 174 MB | -3.9% |

### Analysis

**Throughput improvement (~7-8%):** The overlap between Phase 3 (rayon rewrite)
and Phase 4 (main thread drain/write) means the pipelined writer gets data
sooner. Passthrough slots at the head of each batch drain immediately without
waiting for all rewrites to complete. Total time ≈ max(Phase 3, Phase 4) instead
of Phase 3 + Phase 4.

**RSS neutral at Germany scale:** Germany has only 18.4% rewrite ratio. The
streaming benefit is proportional to the rewrite fraction — at 18.4%, only ~24
PrimitiveBlocks are held simultaneously in the old design vs ~8 in the new
design. The ~22 MB savings is noise at this scale.

**Planet-scale projection (92% rewrite, 128-blob batches):** The old design holds
~118 PrimitiveBlocks × 1.4 MB = ~165 MB simultaneously. The new design holds
~8 × 1.4 MB = ~11 MB. Expected savings: ~154 MB per batch at steady state.
Combined with E2.1's byte budget, this makes batch size independent of rewrite
memory pressure.

### Decision

**KEEP.** 7-8% throughput improvement with no RSS regression at measured scale.
Architectural win for planet scale (decouples batch size from rewrite memory).
Correctness verified by `brokkr verify merge` (identical output to osmium).
