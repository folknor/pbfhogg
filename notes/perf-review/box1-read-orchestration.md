# Box 1: Read-Side Orchestration and API Modes

Performance review of `src/read/reader.rs` and `src/read/pipeline.rs`.

## 1. Executive Summary

- **Finding 1 (par_map_reduce full-collect) is real but overstated.** The reviewer's
  claim that it stores "~80GB in a single Vec" is correct on the surface, but the
  existing code comments (lines 322-345 in reader.rs) already acknowledge this
  explicitly and explain the design tradeoff. The real concern is not that it is
  unknown, but that `par_map_reduce` is not used by any production command -- only by
  benchmarks and the CLI `bench-read` mode. Impact: low for this codebase. Fix
  complexity: small (chunked variant), but not urgent.

- **Finding 2 (fixed queue depths) is real but inconsequential at current scale.** The
  `READ_AHEAD=16` and `DECODE_AHEAD=32` constants produce ~51 MB peak pipeline
  overhead (the TODO.md at line 32 already computes this). This is negligible even on
  planet-scale (80GB) files where the process will use far more memory for application
  state. The pipeline is already balanced per hotpath profiling -- the main thread is
  the bottleneck, not the channels.

- **Finding 3 (static decode thread heuristic) is already solved.** The builder method
  `.decode_threads(n)` exists at reader.rs:76-78, giving full user control. The default
  `available_parallelism() - 2` is well-reasoned and documented (pipeline.rs:63-77).

- **The reviewer missed several more interesting issues:** rayon task panics in the
  decode pool can silently deadlock the pipeline; `into_blocks_pipelined` creates a
  pipeline-inside-pipeline with 3 distinct bounded channels totaling up to
  `16 + 32 + 8 = 56` blocks in flight; and the rayon `ThreadPool` is created fresh on
  every pipelined read call.

- **No code-level changes are needed urgently.** The highest-value improvement would be
  adding panic recovery to the decode pool (medium complexity, prevents silent hangs).


## 2. Finding 1: par_map_reduce Full-Collect

### A. Is this finding real?

**Yes, the behavior is as described.** `collect_osm_data_blobs()` (reader.rs:454-463)
collects all OsmData blobs into a `Vec<Blob>` before any parallel processing begins.

However, the reviewer's framing overstates the severity:

**The code already acknowledges this.** Lines 322-345 in reader.rs contain an extensive
comment block titled "Memory safety analysis" that explicitly computes the planet-scale
cost:

> The full planet (~80GB PBF) has ~2.5M blobs at ~32KB avg = ~80GB. This is the same
> order as the file size, and any system processing the planet file already needs
> substantial RAM for the decoded data.

And under "Alternatives considered":

> Chunked collection (collect N blobs, process, repeat): Would cap memory but adds
> complexity and reintroduces synchronization between chunks. For typical PBF sizes the
> full collect is fine.

**The reviewer's math is correct but misleading.** Let me verify:

Each `Blob` struct contains:
- `WireBlobHeader`: `String` ("OSMData", 7 bytes + 24 byte String overhead), `i32`
  (datasize), `Option<Vec<u8>>` (indexdata, typically `None` = 24 bytes for the Option
  discriminant + pointer + len + cap, or ~50 bytes if present)
- `WireBlob`: `Option<BlobData>` where `BlobData::Zlib(Bytes)` -- the `Bytes` is a
  slice into the original allocation via `input.slice()` (blob.rs:195), keeping the
  full blob envelope alive via Arc refcount. Plus `Option<i32>` (raw_size).
- `Option<ByteOffset>`: 16 bytes

The dominant cost is the heap allocation backing each `Bytes`. In `BlobReader::next()`
(blob.rs:570-575):
```rust
let mut blob_data = Vec::with_capacity(header.datasize as usize);
reader.read_to_end(&mut blob_data)?;
let blob_bytes = Bytes::from(blob_data);
```

Each blob allocation is `header.datasize` bytes. For a planet file:
- ~2.5M blobs at ~32KB average compressed = **~80 GB** in heap allocations
- Plus the `Vec<Blob>` metadata: 2.5M * ~120 bytes (struct overhead) = ~300 MB
- Total: **~80 GB** peak RSS for the collect phase alone

This is indeed problematic for planet-scale. But the key insight is: **who uses this?**

### B. What's the real-world impact?

**Low.** `par_map_reduce` is not used by any command in `src/commands/`:

```
# Commands using for_each_pipelined (2):
src/commands/node_stats.rs:181
src/commands/check_refs.rs:124

# Commands using into_blocks_pipelined (13 callsites across 6 commands):
cat, tags_count, tags_filter, add_locations_to_ways, extract, getid

# Commands using par_map_reduce (0):
(none)
```

The only callsites are:
1. `cli/src/main.rs:757` -- the `bench-read` CLI subcommand's "parallel" mode
2. `dev/src/bench_read.rs:166` -- the dev benchmark harness
3. `bench/osmpbf-baseline/src/main.rs:45` -- the osmpbf comparison benchmark
4. `tests/read_paths.rs:288,319` -- unit tests

No downstream project (elivagar, nidhogg) is documented as using `par_map_reduce`. The
MEMORY.md states they use `for_each_pipelined` and `BlockBuilder+PbfWriter`.

**Impact assessment:** This is a library API concern, not an operational concern. A
third-party user could call `par_map_reduce` on a planet file and OOM. But pbfhogg's
own commands never do this.

### C. What did the reviewer miss?

1. **The code is self-documenting about this tradeoff.** The 50-line comment block
   (reader.rs:288-349) explains the rationale, the memory implications, and the
   alternatives considered. This is not a hidden defect -- it is an explicit design
   decision.

2. **The `par_bridge()` predecessor was worse.** The comment explains that `par_bridge()`
   caused Mutex contention at 8+ cores. The full-collect approach was chosen
   specifically to eliminate that bottleneck. A chunked approach would reintroduce
   synchronization overhead between chunks.

3. **Compressed blobs are individually allocated.** Each `Blob` holds a `Bytes` that
   keeps one allocation alive (the blob envelope). These are 2.5M separate heap
   allocations, not one contiguous 80GB buffer. This matters for the allocator -- 2.5M
   allocations of ~32KB each are normal for `malloc`. The `Vec<Blob>` itself is ~300MB
   (2.5M * 120 bytes), which is a single contiguous allocation that could cause virtual
   memory pressure on 32-bit systems but is fine on 64-bit.

### D. What's the concrete fix?

**Option 1: Chunked par_map_reduce** (small-medium complexity)

```rust
pub fn par_map_reduce_chunked<MP, RD, ID, T>(
    self,
    chunk_size: usize,   // e.g., 1024 blobs = ~32 MB
    map_op: MP,
    identity: ID,
    reduce_op: RD,
) -> Result<T>
```

Collect `chunk_size` blobs, process in parallel, reduce the chunk result into the
accumulator, repeat. Memory cap: `chunk_size * avg_blob_size`. The chunk boundary
introduces a synchronization point (rayon must finish the chunk before the next is
collected), but this is a one-time barrier per chunk, not per-blob contention like
`par_bridge()`.

Complexity: **small** -- ~30 lines of new code. The tricky part is API design: does it
go on `ElementReader` (adding another method), or should `par_map_reduce` just grow a
`max_memory` parameter?

**Option 2: Document the limitation** (trivial)

Add a doc comment to `par_map_reduce` warning about planet-scale memory. Since no
internal command uses it, this may be sufficient.

**Recommendation: Option 2 now, Option 1 if a user reports an issue.** The existing
50-line comment block is thorough but it is a code comment, not a doc comment. Adding
`/// # Memory` to the public API docs would cost 5 lines and prevent surprise OOMs.


## 3. Finding 2: Fixed Queue Depths

### A. Is this finding real?

**Yes, the constants are fixed.** `READ_AHEAD=16` and `DECODE_AHEAD=32` are defined at
pipeline.rs:16-19. They are not configurable.

**No, the memory impact is not a problem.** The TODO.md (line 32) already computes this:

```
Memory cost: ~16 * 32KB (compressed) + 32 * 1.4MB (decoded) = ~51 MB peak
pipeline overhead, independent of file size.
```

Let me verify:
- READ_AHEAD channel: 16 items, each is `(usize, Result<Blob>)`. The `Blob` holds
  compressed data (~32KB average). Total: 16 * 32KB = **~512 KB**.
- DECODE_AHEAD channel: 32 items, each is `(usize, Option<Result<PrimitiveBlock>>)`.
  Each `PrimitiveBlock` owns a `Bytes` buffer of decompressed data (~1.4 MB average for
  Denmark, from the DecompressPool doc comment at blob.rs:18-19). Total:
  32 * 1.4MB = **~45 MB**.
- Reorder VecDeque: pre-allocated to `DECODE_AHEAD` capacity = 32 slots. Each slot is
  `Option<Option<Result<PrimitiveBlock>>>` which is pointer-sized when `None`.
  Worst case (all 32 slots filled with decoded blocks): same ~45 MB as the channel
  (the blocks move from channel to VecDeque, not both simultaneously).

**Total pipeline overhead: ~46 MB**, independent of input file size. For context:
- Denmark (465 MB) processing uses ~300 MB RSS for `check_refs` (RoaringBitmap of node IDs)
- Planet (80 GB) processing will use multiple GB for application data structures
- 46 MB is 0.06% of a 80 GB file's size

### B. What's the real-world impact?

**Negligible.** The TODO.md (lines 38-41) explicitly states:

> Hotpath profiling (Denmark through Japan) shows the pipeline is balanced at all tested
> scales -- I/O thread doesn't stall, rayon workers are barely loaded, main thread is
> the bottleneck. Low priority -- configure when someone reports a problem on a
> memory-constrained system.

The I/O thread reads compressed blobs (~32 KB) from disk. At NVMe sequential read
speeds (~3 GB/s), reading one blob takes ~10 us. The READ_AHEAD=16 channel provides
~160 us of buffering. The decode pool typically decompresses a blob in 200-500 us
(zlib decompression of 32KB -> 1.4MB). So READ_AHEAD=16 gives ~3-8 blobs worth of
headroom before the I/O thread stalls. This is more than enough -- the I/O thread is
never the bottleneck because decode is slower than read.

**Could too-large queue depths hurt?** Only on severely memory-constrained systems.
Doubling DECODE_AHEAD to 64 would add ~45 MB. This is not meaningful.

**Could too-small queue depths hurt?** The DECODE_AHEAD=32 channel interacts with decode
thread count. If decode_threads=14 (on a 16-core machine), at most 14 blobs can be
in-flight in the decode pool simultaneously. DECODE_AHEAD=32 means the channel has room
for 32 decoded results before backpressure kicks in. This gives 32-14=18 slots of
"headroom" for out-of-order completion. This is generous.

### C. What did the reviewer miss?

The **three-channel interaction** in `into_blocks_pipelined` (reader.rs:218-246):

```
Stage 1 (I/O) --[READ_AHEAD=16]--> Stage 2 (decode) --[DECODE_AHEAD=32]--> Stage 3 (reorder)
                                                                                |
                                                                          [BLOCK_QUEUE=8]
                                                                                |
                                                                        consumer iterator
```

`into_blocks_pipelined` wraps the entire pipeline in a background thread and adds a
third `sync_channel(BLOCK_QUEUE)` where `BLOCK_QUEUE=8` (reader.rs:16). This means:

- 16 compressed blobs in the raw channel
- 32 decoded blocks between decode pool and reorder buffer
- 8 decoded blocks between the pipeline wrapper and the consumer iterator

In the worst case, this is 16 * 32KB + (32 + 8) * 1.4MB = **~56 MB**. The extra 8
blocks add ~11 MB. Not a problem, but worth noting that `into_blocks_pipelined` has
50% more in-flight memory than `for_each_block_pipelined` (~56 MB vs ~46 MB).

### D. What's the concrete fix?

**Not needed at current scale.** If configurability were desired, the cleanest approach
would be a pipeline config on `ElementReader`:

```rust
reader.pipeline_config(PipelineConfig {
    read_ahead: 16,
    decode_ahead: 32,
    block_queue: 8,
})
```

But this adds API surface for no demonstrated benefit. The TODO.md already has this
tracked as "configure when someone reports a problem."

**Recommendation: No action.** The current values are well-chosen and the overhead is
bounded at ~50 MB regardless of file size.


## 4. Finding 3: Decode Thread Heuristic

### A. Is this finding real?

**Partially.** The default `available_parallelism() - 2` is fixed at pipeline startup,
but the reviewer missed that **user override already exists**:

```rust
// reader.rs:76-78
pub fn decode_threads(mut self, n: usize) -> Self {
    self.decode_threads = Some(n.max(1));
    self
}
```

This is passed to `run_pipeline` at reader.rs:200 and consumed at pipeline.rs:80:
```rust
let decode_threads = decode_thread_count.unwrap_or_else(|| {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(4)
});
```

### B. What's the real-world impact?

**The default is good.** The rationale (pipeline.rs:63-77) is sound: 2 threads are
reserved for the I/O reader (Stage 1) and the consumer (Stage 3/main thread). The
remaining cores go to decode. This leaves the global rayon pool free for consumer-side
parallelism (e.g., the `cat` command uses `into_blocks_pipelined` + rayon `process_batch`
for parallel re-encoding).

For consumer-heavy workloads (geometry processing in elivagar), the user can call
`.decode_threads(4)` to reduce decode threads and leave more cores for application work.
This is already possible -- no change needed.

### C. What did the reviewer miss?

Nothing significant. The `.decode_threads()` API was already highlighted in the CLAUDE.md
project instructions. The reviewer's suggestion is "already done."

### D. What's the concrete fix?

**None needed.** The API exists and the default is well-documented.


## 5. Additional Findings

### 5.1 [medium] Rayon decode task panic causes silent pipeline deadlock

**Location:** pipeline.rs:106-124

When a decode task is spawned via `decode_pool.spawn(move || { ... })`, if the closure
panics, rayon catches the panic but the task's `tx` clone is dropped without sending.
The sequence number for that blob is never delivered to the reorder buffer.

**What happens:**
1. Blob N is dispatched to the decode pool.
2. The decode closure panics (e.g., corrupt zlib data triggers a panic in a dependency).
3. Rayon catches the panic. The `tx` clone is dropped.
4. The reorder buffer (Stage 3) waits forever for sequence N. It receives N+1, N+2, ...
   but cannot drain past the gap at N.
5. The DECODE_AHEAD channel fills up (32 items). Decode threads block on `tx.send()`.
6. The READ_AHEAD channel fills up (16 items). The I/O thread blocks on `raw_tx.send()`.
7. **Complete deadlock.** All threads are blocked. The pipeline never terminates.

**Mitigating factor:** In practice, `blob.to_primitiveblock_pooled()` (blob.rs:354-359)
calls `decompress_blob` then `PrimitiveBlock::new`. These return `Result`, so most
errors are caught. A panic would require an actual bug in `flate2`, `zstd`, or the
wire-format parser -- unlikely but not impossible.

**Also mitigating:** `std::thread::scope` at pipeline.rs:47 means that if a scoped
thread panics, `scope` propagates the panic after joining all threads. But the I/O
thread and dispatch thread are the scoped ones -- the rayon tasks are not scoped threads.
If a rayon task panics:
- The dispatch thread eventually exhausts `raw_rx` and drops its `dispatch_tx` clone.
- But the rayon pool may still have the panicked task's `tx` clone live (it was moved
  into the closure, which rayon caught the panic from -- the `tx` is dropped when the
  panic is caught, because it was moved into the closure).

Actually, wait. Let me re-examine. When a rayon task panics, the closure is unwound, so
all local variables (including `tx`) are dropped. The `tx` clone IS dropped. But no
message was sent for that sequence number.

The reorder buffer loop (pipeline.rs:164) receives from `decoded_rx`. After all senders
are dropped (all rayon tasks complete + dispatch thread drops its `dispatch_tx` clone),
`decoded_rx` returns `None` and the loop exits. At that point, `next_seq` is less than
the total blob count, and the buffer has unfilled gaps. The pipeline returns `Ok(())`
without processing some blobs.

**Revised assessment:** The pipeline does NOT deadlock -- it silently drops blobs. The
reorder loop exits when the channel closes, even if some sequence numbers were never
delivered. The consumer never sees the skipped blobs and no error is reported.

This is a **silent data loss** bug, not a deadlock. It requires a panic in a rayon task,
which is unlikely but theoretically possible.

**Fix:** Wrap the decode closure body in `std::panic::catch_unwind` and send an error
on panic:

```rust
decode_pool.spawn(move || {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // existing decode logic
    }));
    match result {
        Ok(()) => {} // item already sent inside the closure
        Err(_) => {
            let err = new_error(ErrorKind::Io(
                std::io::Error::other("decode task panicked")
            ));
            drop(tx.send((seq, Some(Err(err)))));
        }
    }
});
```

Complexity: **small** (~15 lines). Risk: low. But the bug is unlikely to trigger in
practice.

### 5.2 [low] Reorder VecDeque can temporarily exceed DECODE_AHEAD

**Location:** pipeline.rs:161-172

The comment at line 143-144 claims:

> The out-of-order window is bounded: at most DECODE_AHEAD items can be in-flight, so
> the deque never grows larger than that.

This is **almost correct** but has a subtle edge case. The `decoded_tx` channel has
capacity `DECODE_AHEAD=32`. But the dispatch thread clones `decoded_tx` for each rayon
task (pipeline.rs:102: `let tx = dispatch_tx.clone()`). Each clone is an independent
sender. The channel capacity bounds the number of **buffered** items in the channel, but
senders can still successfully `send()` even when the channel is "full" if the receiver
is actively draining.

Actually, no. `sync_channel(32)` means the channel has exactly 32 buffer slots. A
`send()` blocks when all 32 slots are occupied. The number of senders is irrelevant to
the buffer size.

The reorder VecDeque receives items from the channel one at a time and stores them until
they can be drained in order. In the worst case: blob 0 is the slowest to decode, while
blobs 1-32 all complete first. The channel can hold 32 items, so blobs 1-32 fill the
channel. As the main thread drains the channel (one `recv()` per iteration of the for
loop), each item goes into the VecDeque. After draining all 32 items from the channel,
the VecDeque has 32 items (all waiting for blob 0). Then blob 0 arrives in the channel,
the main thread receives it, puts it at index 0, and drains all 33 items.

**Worst-case VecDeque size:** The VecDeque can hold at most `DECODE_AHEAD + decode_threads`
items. Here is why: the channel buffers `DECODE_AHEAD` items. But `decode_threads` tasks
can simultaneously be in the process of completing and calling `tx.send()`. If the
channel has room, they all succeed and the items are in the channel. But if the channel
is full and the main thread receives one item, one sender unblocks and sends, the main
thread receives another, etc. The VecDeque grows by one for each `recv()`.

In practice, the VecDeque size is bounded by `DECODE_AHEAD` because items leave the
channel and enter the VecDeque -- they are not in both simultaneously. The maximum
VecDeque occupancy equals the number of items received but not yet drained, which is
at most DECODE_AHEAD (because senders block when the channel is full, limiting total
in-flight items).

**Revised assessment:** The comment is correct. The VecDeque is bounded by DECODE_AHEAD.
The `with_capacity(DECODE_AHEAD)` pre-allocation is correct and never needs to grow.
**Not a bug.**

### 5.3 [low] ThreadPool created per pipelined read call

**Location:** pipeline.rs:87-99

```rust
let decode_pool = match rayon::ThreadPoolBuilder::new()
    .num_threads(decode_threads)
    .build()
{
```

Every call to `for_each_pipelined`, `for_each_block_pipelined`, or
`into_blocks_pipelined` creates a new dedicated rayon `ThreadPool`. This means:

- Thread creation: `decode_threads` threads are spawned (e.g., 14 on a 16-core machine)
- Thread destruction: all threads are joined when the pool is dropped (at the end of
  `run_pipeline`, when the dispatch thread's scope exits)

`ThreadPool::build()` cost: spawning 14 OS threads takes ~1-5 ms depending on the OS.
For a Denmark file (1300 ms pipelined read), this is <0.4% overhead. For planet scale
(~30s+ pipelined read), it is negligible.

**Mitigating factor:** Most commands call pipelined read once or twice per invocation.
The `extract --smart` command does up to 3 passes (extract.rs has 7 callsites to
`into_blocks_pipelined` but they are across different strategy branches, not all
executed). Thread pool creation once per pass is fine.

**Is a global/cached decode pool better?** Potentially, but it would require the pool
to outlive the `thread::scope` in `run_pipeline`, which complicates the ownership model.
The current design is simple and correct. The cost is measurably zero.

**Recommendation: No action.**

### 5.4 [informational] into_blocks_pipelined is pipeline-inside-pipeline

**Location:** reader.rs:218-246

`into_blocks_pipelined` spawns a background thread that runs `run_pipeline` (which
itself uses `thread::scope` to spawn the I/O thread and dispatch thread). The results
are forwarded via a third `sync_channel(BLOCK_QUEUE=8)`.

Thread inventory for `into_blocks_pipelined`:
1. **Background thread** (reader.rs:227): runs `run_pipeline`, acts as the pipeline's
   "main thread" (Stage 3 reorder + forwarding to `tx`)
2. **I/O thread** (pipeline.rs:49): Stage 1, sequential reads
3. **Dispatch thread** (pipeline.rs:79): Stage 2 orchestrator, fans out to rayon pool
4. **Rayon pool threads** (14 on a 16-core machine): Stage 2 workers
5. **Consumer thread** (wherever `PipelinedBlocks::next()` is called): the actual main
   thread

Total: 3 OS threads + decode_threads rayon threads + 1 consumer thread = **18 threads**
on a 16-core machine.

The 3-channel buffering:
- raw channel: 16 * ~32KB = ~512 KB (compressed blobs)
- decoded channel: 32 * ~1.4MB = ~45 MB (decoded blocks)
- block channel: 8 * ~1.4MB = ~11 MB (decoded blocks, forwarded)

Peak: **~57 MB** in pipeline buffers. The decoded and block channels hold different
blocks (a block moves from decoded channel -> reorder buffer -> block channel), so the
peak is 32 + 8 = 40 blocks * 1.4MB = ~56 MB plus the raw channel.

This compares to `for_each_block_pipelined` which uses only 2 channels and ~46 MB.
The extra ~11 MB from `BLOCK_QUEUE=8` is the cost of the iterator abstraction.

**Not a problem.** All commands that use `into_blocks_pipelined` do meaningful work per
block (writing to disk, building spatial indexes, etc.), so the 8-block buffer provides
useful decoupling between pipeline throughput and consumer processing time.

### 5.5 [informational] Backpressure and PrimitiveBlock lifetime in for_each_pipelined

**Location:** reader.rs:159-181

`for_each_pipelined` calls `for_each_block_pipelined` with a closure that iterates
elements within the block:

```rust
self.for_each_block_pipelined(|block| {
    block.for_each_element(|element| {
        // element borrows from block
        f(element);
    });
    Ok(())
})
```

Each `PrimitiveBlock` is alive for the entire duration of the closure. The closure
iterates all elements (up to 8000 per block), calling `f(element)` for each. The
`element` borrows from `block`, so `block` must stay alive.

**Memory implication:** The consumer holds exactly 1 `PrimitiveBlock` at a time (~1.4 MB).
When the closure returns `Ok(())`, the block is dropped, and `run_pipeline` delivers the
next block. Backpressure is automatic: while the consumer processes one block, the
pipeline can buffer up to `DECODE_AHEAD=32` more blocks. If the consumer is very slow,
the pipeline fills the channel and stalls. This is correct behavior.

**The interaction between consumer speed and pipeline memory:** If the consumer processes
each block in 1 ms but decode takes 0.5 ms, the pipeline stays ahead and the channel
stays partially empty. If the consumer takes 10 ms per block (heavy geometry processing),
the channel fills to capacity (32 blocks, ~45 MB) and the pipeline naturally throttles.
The 45 MB is the steady-state maximum regardless of consumer speed.

**Not a problem.** The design is correct.


## 6. Cross-Box Interactions

### Box 2 (Blob Decode): DecompressPool contention

The pipeline's Stage 2 creates one `DecompressPool` (pipeline.rs:100) shared across all
rayon decode tasks via `Arc<DecompressPool>`. The pool uses a `Mutex<Vec<Vec<u8>>>` for
buffer storage. With `decode_threads=14` workers, this Mutex is contended: each worker
locks the pool to get a buffer (`pool_get`), decompresses, then the buffer is returned
to the pool when the `PooledBuffer` is dropped (which happens when the `PrimitiveBlock`
is dropped by the consumer on the main thread).

Contention on `DecompressPool.buffers` Mutex:
- **Get path** (rayon thread): lock, pop, unlock. Very fast (~50ns).
- **Put path** (main thread, via Drop): lock, push, unlock. Very fast (~50ns).

The Mutex is held for <100ns per operation (Vec push/pop). With 14 threads, contention
is minimal -- the decode work (200-500 us per blob) dwarfs the lock hold time by 3
orders of magnitude.

### Box 4 (Indexing/Mmap): Design comparison

`MmapBlobReader` (mmap_blob.rs) is a simpler sequential reader with no pipeline. It
produces `MmapBlob`s that must be decoded one at a time by the consumer. The design is
fundamentally different: no parallelism, no buffering, no reorder.

For parallel processing of mmap data, the consumer must implement their own
parallelization. The `MmapBlobReader` does not provide a `par_map_reduce` or pipelined
equivalent. This is intentional -- mmap is used for seek-based access patterns
(IndexedReader), not for full-file parallel scans.

### Box 8 (Commands): API usage patterns

| API | Commands using it |
|-----|------------------|
| `for_each_pipelined` | `check_refs`, `node_stats` (2 commands) |
| `into_blocks_pipelined` | `cat`, `tags_count`, `tags_filter`, `add_locations_to_ways`, `extract`, `getid` (6 commands, 13 callsites) |
| `par_map_reduce` | (none -- only benchmarks and tests) |
| `for_each` (sequential) | (none in commands -- only in `fileinfo` via BlobReader directly) |

`into_blocks_pipelined` is the dominant API, used by 6 of 8 commands. The iterator
interface enables loop control (early exit in extract, conditional processing in
tags_filter) and batch collection (cat collects into Vec for parallel re-encoding).

The two commands using `for_each_pipelined` (check_refs, node_stats) process elements
individually and accumulate into data structures (RoaringBitmap, statistics). They could
use `into_blocks_pipelined` but there is no benefit -- element-level iteration is
simpler when the consumer does not need block-level control.


## 7. Recommended Actions (Prioritized)

### Priority 1: Add panic recovery to decode pool tasks (medium)

**Location:** pipeline.rs:106-124
**Risk:** Silent blob skipping on rayon task panic (section 5.1)
**Fix:** Wrap decode closure in `catch_unwind`, send error on panic
**Complexity:** Small (~15 lines)
**Impact:** Prevents silent data corruption in pathological cases

### Priority 2: Add `/// # Memory` doc section to par_map_reduce (trivial)

**Location:** reader.rs:350 (doc comment)
**Risk:** Library users calling `par_map_reduce` on planet files and OOMing
**Fix:** Add doc comment warning about O(file_size) memory usage
**Complexity:** Trivial (5 lines)
**Impact:** Prevents user surprise

### Priority 3: No action on queue depths (resolved)

Already tracked in TODO.md. 51 MB pipeline overhead is negligible. Hotpath profiling
confirms pipeline is balanced. No change needed until someone reports a problem.

### Priority 4: No action on decode thread heuristic (resolved)

`.decode_threads(n)` API already exists. Default is well-chosen. No change needed.

### Priority 5: Consider chunked par_map_reduce if demand arises (deferred)

If a library user reports OOM on planet-scale `par_map_reduce`, implement a chunked
variant. Until then, the pipelined reader (`for_each_pipelined` /
`into_blocks_pipelined`) is the recommended API for planet-scale use and has no memory
issues.
