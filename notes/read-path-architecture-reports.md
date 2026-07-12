# Read-path architecture: the two theorizing reports (2026-07-11, verbatim)

> **Empirical verdicts landed 2026-07-12** - the follow-up items these
> reports surfaced were implemented env-gated and measured in one
> overnight A/B run (commit `a65cecc`, plantasjen). Scorecard against the
> reports' own conviction levels at the end of this file
> ("Empirical verdicts"). Headline: the reports' two "high-conviction"
> *full rewrites* split - fusion (section 6) KEPT with 6-8% command
> wins, but the ordered-pipeline batch rebuild (section 5) REVERTED (no
> isolated win, and it regressed the fused combination +9.3%). Both
> medium-value read-buffer items (fadvise batching, byte-aware knobs)
> also reverted inside the noise floor.

Context: the bench-read parallel-mode planet OOM investigation. Two
independent deep passes were run against the same brief
(code-only ground rules, aggressive-rewrite framing contract): codex
gpt-5.6-sol at xhigh, and a Fable agent. Both reports are preserved
verbatim below because they contain durable analysis beyond the landed
fix (the par_map_reduce bounded-fold rewrite). The follow-up items they
surface are indexed in TODO.md; this file is the full source.

Key durable takeaways not consumed by the landed fix:

- Robustness gap: the code caps decompressed content at 32 MiB but does
  NOT equivalently reject an oversized `BlobHeader.datasize`, so
  malicious input can exceed the assumed compressed-body bound (codex
  section 3).
- Ordered-pipeline batch rebuild opportunity (both reports, high
  conviction, touches the production spine - full re-verification
  required).
- Command-transform fusion into decode workers for getid pass 2,
  getparents full-scan arm, tags-filter, altw decode-all (codex section
  6: removes a second rayon dispatch and ~90 MB per-64-block batch
  materialization; land one command at a time).
- Sequential-path double copy: `PrimitiveBlock::new` copies the whole
  decompressed buffer; ~300+ GB of pure memcpy per planet pass; the
  scratch-reusing constructors already exist (both reports).
- Per-blob `posix_fadvise(DONTNEED)` with a cumulative prefix range =
  1.45 M ever-growing advisory syscalls on the 8k encoding (both).
- Count-only buffer knobs (read_ahead 16, decode_ahead 32, BLOCK_QUEUE
  8, command batches of 64) should become byte-aware (both).
- `for_each` and the blobreader bench arm leave parse_indexdata on
  though nothing reads Blob::index() there (Fable 3.4).
- The 0.39-0.43 RSS-to-bytes-read ratio observed during the OOM was
  swap arithmetic (RSS pinned at ceiling, cold Vec front swapped out,
  death at RAM + free swap exhausted), not partial retention (Fable
  section 1, arithmetic closes exactly).

---

## Report 1: codex gpt-5.6-sol, xhigh

# Read modes at planet scale on a 26 GB host

## Executive conclusion

Yes, all four read modes can work at planet scale on this host.

The contrary prior is false because the parallel failure is not an inherent cost of parallel decoding. It is caused by one explicit implementation decision: `par_map_reduce` reads and retains every compressed `OsmData` blob before starting Rayon. Nothing about parallel decode requires that materialization.

Three modes already demonstrate the feasible memory shape:

- Sequential holds approximately one compressed blob and its decoded representation.
- BlobReader does the same at a lower abstraction level.
- Pipelined bounds compressed and decoded data with channels and performs decompression and parsing in parallel.
- Parallel is the outlier because it uses whole-file accumulation. Replacing that with a bounded, worker-resident fold gives real parallel map/decode throughput with a design ceiling well below 2 GB for normal planet inputs.

My recommendation is direct: delete the collect-then-`into_par_iter` implementation. Replace it with a one-pass, byte-bounded parallel fold. Do not route it through the current descriptor-first scanner or merely divide the whole-file collection into giant chunks.

## 1. Exact OOM mechanism

The parallel CLI arm calls:

```text
run_bench_read
  -> ElementReader::par_map_reduce
     -> collect_osm_data_blobs
        -> BlobReader::next for every remaining frame
     -> Vec<Blob>::into_par_iter
```

The decisive code is in reader.rs, in `par_map_reduce` and `collect_osm_data_blobs`.

For every frame, blob.rs does the following:

1. Allocates `Vec::with_capacity(header.datasize)`.
2. Reads the complete Blob protobuf body into that allocation.
3. Converts the allocation into `Bytes`.
4. Calls `WireBlob::parse`.
5. Stores a `Bytes::slice` for the compressed payload in `WireBlob`.
6. Wraps that `WireBlob` in `Blob`.
7. `collect_osm_data_blobs` pushes the `Blob` into one file-wide `Vec`.

The slice is zero-copy, but it pins the complete allocation from which it was sliced. Therefore every collected `Blob` keeps its complete per-frame body allocation alive. This is not one shared parent buffer, a partially bounded window, or an oversized channel. It is a separate owned allocation per frame, all retained until EOF.

Rayon starts only after:

```rust
let blobs = collect_osm_data_blobs(self.blob_iter)?;
```

has returned. That exactly explains the observed signature:

- One thread throughout: the collection pass is sequential.
- No user-space decode work: `blob.decode()` is only called inside the later `into_par_iter`.
- Mostly kernel and memcpy time: the process is reading and copying Blob bodies.
- Growth follows bytes, not blob count: payload allocations dominate. The outer `Vec<Blob>` is small by comparison.
- Europe completes because its accumulated live payload remains below the host ceiling.
- Both planet encodings fail before decoding begins because both have more compressed data than available memory.

For scale, the schedule-like structural overhead is not the problem:

- 50,816 entries at 24 bytes each would be about 1.2 MB.
- 1,453,433 entries at 24 bytes each would be about 33.3 MiB before spare `Vec` capacity.

The retained Blob bodies are the problem.

The measured 0.39 to 0.43 resident ratio should not be interpreted as the code retaining only 40 percent of each payload. At the ownership level, each live `Bytes` slice pins its entire Blob body allocation. The ratio is between the measurement's physical-residency/read accounting and that logical ownership, not evidence of a partial collection.

The comments in `par_map_reduce` explicitly describe the whole-file collection as safe. They are wrong for the supported planet workload and should be removed with the implementation.

## 2. What each mode actually does

### Sequential

`ElementReader::for_each` reads one `Blob`, decodes it, walks its elements, and drops it before advancing.

Its live data is input-size independent. One inefficiency remains: `decompress_blob` produces a `Bytes`, then `PrimitiveBlock::new` copies it into another `Vec` so metadata can be appended inline. During construction, it can temporarily hold:

```text
compressed Blob body + first decompressed buffer + copied decompressed buffer
```

That affects memory bandwidth, but not planet feasibility.

### Parallel

`ElementReader::par_map_reduce` separates reading and decoding into two complete phases:

```text
read and retain the whole file -> parallel decode/map/reduce
```

Its memory is `O(compressed file size)`, plus decoded worker data once phase two begins. On a 26 GB host, this can never finish for an 86 to 98 GB input.

This is an implementation failure, not a fundamental parallelism limit.

### Pipelined

`ElementReader::for_each_pipelined` uses pipeline.rs:

```text
sequential reader -> bounded raw channel -> Rayon decode
                  -> bounded decoded channel -> reorder -> caller
```

Its default bounds are:

- 16 raw blobs read ahead.
- 32 admitted decode items.
- File-order delivery through `ReorderBuffer`.

The permit remains attached to a decoded item until that item is delivered, so completion skew cannot make the decoded window grow with the file.

Decode and parsing are parallel. The element callback is sequential on the calling thread because the API accepts an ordered `FnMut`. For the benchmark's trivial counting callback, that is a reasonable division of work. For expensive element callbacks, it is a real serialization boundary.

### BlobReader

The CLI manually constructs:

```rust
BlobReader::new(BufReader::new(File::open(path)?))
```

and decodes one Blob at a time. Its memory is constant.

The benchmark does not construct this mode comparably to the other three:

- It uses the standard 8 KiB `BufReader`, while `ElementReader::from_path` reaches `FileReader::buffered`, which uses 256 KiB.
- It does not use `BlobReader::from_path`, so Linux page-cache eviction support is not enabled.
- Its timer includes the header frame, while `ElementReader` parses its header before the timer starts.

These do not affect the feasibility verdict, but they mean the four timing numbers are not purely comparisons of decode architecture.

## 3. Quantitative memory feasibility

Let:

- `C` be one compressed Blob protobuf body.
- `D` be one decompressed PrimitiveBlock.
- `P` be the decode worker count.
- `T` be the user-defined reduction value.

The code limits decompressed content to 32 MiB. It does not equivalently reject an oversized `BlobHeader.datasize`, so malicious input can exceed the assumed compressed-body limit. The figures below concern well-formed PBF inputs.

| Mode | Required live data | Conservative scale |
|---|---|---:|
| Sequential | `C + 2D` during the current copy-heavy construction | Under roughly 96 MiB at 32 MiB per component, normally far lower |
| BlobReader | Same `C + 2D` shape | Same bound, observed tiny RSS |
| Pipelined | Up to 16 compressed bodies and 32 decoded or decoding bodies, plus worker scratch | Roughly 1.5 GiB at pathological 32 MiB bodies, normally tens to low hundreds of MiB |
| Parallel, current | Sum of every compressed body in the file | 86 to 98 GB class, impossible here |
| Parallel, proposed | Byte-bounded compressed queue plus `P` worker decode buffers and `P` partial reductions | Set an explicit ceiling below 2 GiB |

The code itself describes a typical decoded block as about 1.4 MB. With 30 workers, merely keeping one such decoded block per worker costs about 42 MB. Add compressed buffers, parser scratch, and bounded read-ahead, and real parallelism still needs nowhere near 26 GB.

A sensible proposed budget is:

- 256 MiB maximum queued compressed data.
- 1 GiB maximum admitted decompressed data.
- Reusable worker scratch and partial results outside those budgets.
- A target total reader working set below 2 GiB.

That is a ceiling, not the normal expected RSS. It leaves more than 20 GB for command state on this host.

`T` is caller-controlled and cannot be bounded by the reader. A caller that maps every element to a growing `Vec` can still consume planet-scale memory. That is inherent in the requested result, not in file reading. For the benchmark's three counters, `T` is only 24 bytes per partial result.

## 4. Primary recommendation: full rewrite of `par_map_reduce`

This is the highest-conviction change.

### Target architecture

Replace whole-file collection with a one-pass, bounded parallel fold:

```text
sequential frame pump
  -> byte-bounded batches of compressed blobs
  -> long-lived decode/map workers
  -> one partial T per worker
  -> final reduction
```

The frame pump should:

- Preserve efficient sequential file I/O and kernel readahead.
- Build batches bounded by both blob count and compressed bytes.
- Acquire capacity before reading or admitting another batch.
- Stop promptly when a worker reports an error.
- Recycle compressed storage through a bounded slot pool.

Each long-lived worker should:

1. Receive a compressed batch.
2. Reuse a worker-local decompression buffer.
3. Reuse string-table and group-range scratch.
4. Parse the `PrimitiveBlock`.
5. Invoke `map_op` while the block remains worker-local.
6. Fold directly into one worker-local `T`.
7. Return compressed slots to the producer.
8. Send only its final `T`, or a terminal error, to the coordinator.

The final coordinator reduces at most `P` partial values.

### Why this is stronger than a small chunking patch

A loop that collects N blobs, calls `into_par_iter`, waits, and repeats would cap memory, but it creates synchronization barriers between chunks. Fast workers become idle at every boundary, and throughput becomes sensitive to slow blobs near the end of each chunk.

Long-lived workers and bounded batches provide continuous overlap:

- The reader keeps filling available slots.
- Workers independently consume batches.
- Decode variance is absorbed without global barriers.
- Channel and scheduling costs are paid per batch, not per blob.
- Map work remains on the same worker as decompression and parsing.
- No decoded `PrimitiveBlock` crosses to another thread.

This is a coherent replacement, not a probe or temporary alternate mode.

### Why not use the existing HeaderWalker scanner directly

The descriptor-first APIs in header_walker.rs and classify.rs demonstrate an important good pattern:

- Worker-local compressed and decompressed buffers.
- Parallel `pread`.
- Worker-local parsing and command work.
- Compact bounded results.

But that implementation is optimized for selective reads and type schedules. It is not the ideal full-file reader:

- It performs a header-only pass before body decoding.
- `HeaderWalker` issues a probe `pread` per frame.
- A 1,453,433-blob repack therefore pays about 1.45 million header probes before full body processing.
- The worker descriptor receiver is protected by a mutex and acquired once per blob.
- The whole schedule is materialized even though every body will be read.

The new parallel fold should borrow its worker-resident processing model, not its two-pass schedule mechanism.

### API and semantic compatibility

The replacement can preserve the current public method signature.

Existing semantics already allow arbitrary parallel order. The new grouping of reductions can differ from Rayon's current grouping, but non-associative reductions are already unsuitable for deterministic parallel reduction.

It must preserve:

- Header parsing before iteration.
- Skipping non-`OsmData` blobs.
- Typed I/O, wire, decompression, and UTF-8 errors.
- Panic propagation or explicit panic conversion.
- Correct identity behavior for empty inputs and worker partitions.
- Prompt cancellation without blocked senders.
- Support for generic `R: Read + Send`, not only file-backed readers.

## 5. Second architectural opportunity: rebuild the ordered pipeline around batches

This is also a full coherent rewrite, but it should be a separate landing after parallel mode is fixed.

The current ordered pipeline has a per-blob chain:

```text
reader allocation
 -> raw channel send
 -> dispatcher receive
 -> Rayon spawn
 -> decoded channel send
 -> reorder insertion
 -> consumer callback
```

For 50,816 primary-planet blobs, those seams may be tolerable. For 1,453,433 small repacked blobs, per-blob channel traffic, task creation, atomics, permits, and reorder operations become structural overhead.

The dispatcher is also an extra thread not reflected correctly in the thread-count explanation. With `available_parallelism() - 2` decode workers, the live threads are the caller, reader, dispatcher, and decode pool. `into_blocks_pipelined` adds another pipeline-owner thread and its eight-block output queue.

The stronger design is a byte-bounded ordered batch pipeline:

```text
sequential frame batches
 -> long-lived batch workers
 -> ordered decoded batches
 -> consumer
```

A batch carries a contiguous sequence range. Reordering occurs once per batch, while blocks inside it are already ordered. This would:

- Amortize dispatch and channel operations.
- Remove one-task-per-blob Rayon scheduling.
- Use byte budgets rather than count-only admission.
- Make memory behavior comparable across 50,816 large blobs and 1,453,433 small blobs.
- Correct thread accounting.
- Allow direct ownership return of reusable buffers instead of returning every buffer through one mutex-protected `DecompressPool`.

The ordered `FnMut` callback must remain serialized. Trying to parallelize an arbitrary ordered mutable callback would change the API's meaning. Expensive transformations need a different surface.

## 6. Third architectural opportunity: fuse production transforms into decode workers

Several production paths currently do this:

```text
parallel decode
 -> transfer PrimitiveBlock to consumer
 -> collect 64 PrimitiveBlocks
 -> dispatch those same blocks to another Rayon pool
 -> build output
 -> return output to writer
```

Code using this shape includes:

- `getid` pass 2.
- The pipelined `getparents` arm.
- Single-pass `tags-filter`.
- The decode-all `add-locations-to-ways` output path.

This creates two parallel stages separated by materialized decoded blocks and a serial batching thread. The batch of 64 alone represents about 90 MB at the code's stated 1.4 MB average, before pipeline read-ahead and output allocations.

The stronger command architecture is:

```text
read compressed batch
 -> worker decompresses and parses
 -> same worker performs command transform
 -> worker emits compact stats or OwnedBlock output
 -> ordered consumer writes or merges
```

The existing `scan::classify` functions already prove this ownership shape inside the codebase. A production rewrite should generalize the execution pattern only as far as those commands require. It should not become a generic writer cleanup project.

This is plausibly high-payoff because it removes:

- Cross-thread decoded-block ownership.
- A second Rayon dispatch.
- Batch-wide `PrimitiveBlock` materialization.
- Alternation between decode and transform pools.
- Repeated allocator handoff between worker and consumer threads.

The risk is much higher than the isolated `par_map_reduce` replacement. Output ordering, deterministic framing, mutable command state, early errors, filtering, and writer backpressure are all command-specific. These should land one command at a time.

## 7. Medium-value local changes

These are worthwhile, but they are not substitutes for the parallel rewrite.

### Remove the decompressed buffer copy

`Blob::to_primitiveblock` currently calls `decompress_blob`, then `PrimitiveBlock::new`, which copies the decompressed `Bytes` into a `Vec`.

Add a consuming decode path that decompresses directly into a `Vec` and calls `PrimitiveBlock::from_vec` or its scratch-reusing form. `ElementReader::for_each` can consume each `Blob` and use it.

Expected effects:

- One fewer `D`-sized allocation and copy per blob.
- Sequential peak becomes approximately `C + D`, rather than `C + 2D`.
- Better sequential and BlobReader throughput.

Risk is moderate because `Blob::decode(&self)` has public borrowing semantics and cannot simply be changed to consume `self`. A new consuming method is cleaner.

### Fix the BlobReader benchmark construction

Use the library's path constructor or otherwise equalize:

- Buffer size.
- Header timing.
- Page-cache advice.
- Indexdata parsing settings.

This is a benchmark correction, not a production optimization.

### Batch page-cache eviction

`BlobReader::next` currently calls:

```rust
posix_fadvise(fd, 0, current_offset, POSIX_FADV_DONTNEED)
```

after every blob. That repeats a cumulative-prefix advisory syscall for every frame. On the 1.45-million-blob input, this means 1.45 million calls with ever-growing ranges.

Track a `last_evicted` watermark and advise only a newly consumed, page-aligned interval after a coarse byte threshold. This could materially help high-blob-count sequential and pipelined reads, but it needs measurement because it changes kernel cache interaction.

### Make all buffering byte-aware

`read_ahead(16)`, `decode_ahead(32)`, `BLOCK_QUEUE = 8`, and command batches of 64 are count limits. Their memory meaning changes radically between the primary encoding and the 8k repack.

Retain count limits as defenses, but make bytes the primary admission invariant.

## 8. Production safety

The first rewrite can be strongly insulated.

A code call-site audit shows no production command invokes `ElementReader::par_map_reduce`. Its current in-tree consumers are:

- The synthetic `bench-read` parallel arm.
- Public documentation examples.
- `read_paths` tests.

Therefore the first landing should touch only:

- `ElementReader::par_map_reduce`.
- Its new private bounded execution machinery.
- Removal of `collect_osm_data_blobs`.
- Parallel read-path tests and benchmark wiring.

It should not modify `run_pipeline`, `BlobReader::next`, `PrimitiveBlock`, or production command code. Planet-safe production paths then remain structurally unchanged.

A later ordered-pipeline rewrite would affect shared surfaces used by:

- Geocode builder relation pass through `for_each_block_pipelined`.
- `add-locations-to-ways`, `getid`, `getparents`, and `tags-filter` through `into_blocks_pipelined`.
- `time-filter` through `for_each_pipelined`.

That rewrite must preserve the current implementation until the complete replacement passes:

- Sequential, pipelined, block-iterator, error, early-drop, and reorder-window tests.
- Full reader/writer roundtrips.
- Output verification for every affected command.
- High-blob-count throughput and RSS measurement.
- A large-input memory run proving the byte ceiling under slow-first-block completion skew.
- Eventually, an explicitly authorized planet run before declaring the shared production path planet-safe.

The command-fusion opportunity has an even narrower safe landing strategy: migrate and verify one command at a time. Do not silently reroute all batch consumers through a new generic engine.

## 9. Priority and verdict

### High-conviction full rewrites

1. Replace `par_map_reduce` with a one-pass, byte-bounded, worker-resident parallel fold.
2. Rebuild the ordered pipeline around byte-bounded batches and long-lived workers.
3. Fuse decode and command transformation for production paths that currently hand decoded blocks into a second Rayon stage.

### Medium-value local work

1. Add a consuming zero-copy Blob decode path.
2. Correct the BlobReader benchmark setup.
3. Evict page-cache ranges incrementally rather than advising the whole prefix per blob.
4. Replace remaining count-only buffer limits with count plus byte limits.

## Final answer to the feasibility question

All four modes can work on a 26 GB-in-reality host.

The necessary reader memory is not proportional to the 86 to 98 GB input. Sequential and BlobReader need one-block working sets. Pipelined already demonstrates bounded parallel decode. Parallel needs the same bounded ownership discipline, with map and fold performed on the decode workers.

A clean implementation can cap the parallel reader below roughly 2 GB while maintaining enough resident blocks to saturate dozens of decode cores. The current OOM is entirely self-inflicted by whole-file compressed-Blob collection.

---

## Report 2: Fable

# Report: bench-read modes at planet scale on a memory-constrained host

Commit context: working tree at HEAD (evidence table cites 58743ba). Host: plantasjen, 30 GB RAM, 39 GB swap (verified: `free -g` reports 30/39, with ~6 GB swap currently occupied), 24 hardware threads. All conclusions from code plus the brief's measurements only.

## 1. Diagnosis: the exact parallel-mode accumulation mechanism

**The mechanism is `collect_osm_data_blobs` in `src/read/reader.rs`, called by `ElementReader::par_map_reduce` as its "Phase 1".** It sequentially iterates the whole `BlobReader` and pushes every OSMData `Blob` into a single unbounded `Vec<Blob>` before any parallel work begins:

```rust
// reader.rs, par_map_reduce:
self.blob_iter.set_parse_indexdata(false);
let blobs = collect_osm_data_blobs(self.blob_iter)?;   // <- whole file into RAM
blobs.into_par_iter().try_fold(...)                    // <- never reached at planet scale
```

Each `Blob` retains the blob's entire compressed payload: `BlobReader::next` (blob.rs) reads `datasize` bytes into a fresh exact-capacity `Vec<u8>`, converts to `Bytes`, and `WireBlob::parse` (blob_wire.rs) stores `BlobData::Zlib/Zstd/Raw` as zero-copy `Bytes` slices that pin the parent allocation. Retention is therefore ~1.0x of payload bytes read - effectively the file size - regardless of blob count or encoding. Rayon's `into_par_iter()` in Phase 2 is the first use of the global rayon pool, and Phase 1 never completes at planet scale, so no worker thread is ever spawned.

The function's own doc comment acknowledges the collect ("For a planet file (~80 GB), this requires ~80 GB of RAM") and its inline "Memory safety analysis" comment rationalizes it with a false premise ("any system processing the planet file already needs substantial RAM for the decoded data") - disproved by the sibling pipelined mode completing the same planet file with tiny RSS.

### Reconciling every signature detail

- **peak_threads = 1 for the entire run.** Death occurs inside Phase 1, a single-threaded read loop. The rayon global pool initializes lazily on first parallel call; Phase 2 is never reached. No decode thread ever exists.
- **~0.7-0.8 cores, all kernel time, zero user time.** Phase 1 is `read()` syscalls + page-cache memcpy + (as memory pressure mounts) kswapd swap-out, all kernel work. Decompression (user CPU) lives entirely in Phase 2.
- **The fractional retention ratio (~0.39-0.43 of bytes read) is not a partial-retention mechanism. It is a swap artifact.** Allocated anonymous memory grows at ~1.0x bytes read; the host has 39 GB of swap. Once anon RSS hits the ~24 GB ceiling, the kernel pushes the cold front of the Vec (touched once, never again) to swap while RSS stays pinned near the ceiling. The observed "ratio" is ceiling / bytes_read at the sample points. The arithmetic closes exactly:
  - primary: died at 61.8 GB read = 24.06 GB RSS + ~37.7 GB swapped; 24 + ~38 GB free swap = ~62 GB total anon capacity.
  - 8k repack: died at 56.2 GB read = 24.25 GB RSS + ~32 GB swapped; consistent with ~33 GB of swap free at run time (the host shows 6 GB of swap already occupied today).
  - SIGKILL comes when RAM + free swap is exhausted, i.e. at bytes_read = ceiling + free_swap, not at 0.4 x anything.
- **Encoding-independence and blob-count irrelevance.** Retention is per payload byte, not per blob. 50.8K blobs at 1.7 MB and 1.45M blobs at 67 KB retain the same ~1.0x. Europe survives because 35 GB < 24 GB RAM + free swap (with heavy swap traffic hidden inside its runtime).
- **Nearly identical times-to-death (83.7 s vs 83.3 s)** despite different file sizes: both runs are feed-rate-bound (~675-740 MB/s single-threaded read under swap pressure) and both die at nearly the same absolute byte count because the ceiling is a host property.

Secondary detail confirming anon-only growth: `BlobReader` issues `posix_fadvise(DONTNEED)` behind the read head on every blob (blob.rs), so page cache never accumulates; the growth is purely the Vec.

## 2. Feasibility: can all four modes work at planet scale on ~26 GB?

**Yes. The standing prior ("I don't think a universe exists where all 4 read modes CAN work on a 26 GB-constrained host") is disproved.** Three of the four already work by measurement; the fourth fails for an incidental implementation choice, not an architectural necessity - unordered map-reduce is the *easiest* of the four semantics to bound, because it needs no ordering buffer at all. Memory floors:

| Mode | Structure | Memory floor (planet, realistic / hard bound) | Status |
|---|---|---|---|
| sequential | one blob + one decoded block at a time | ~15-20 MB / ~100 MB (bounded by the 32 MB `MAX_BLOB_MESSAGE_SIZE` cap) | works today; measured 16 MB |
| blobreader | same shape | same | works today |
| pipelined | 16 raw blobs (`read_ahead`) + 32 admitted decoded blocks (`decode_ahead`, permit-bounded through the reorder buffer) + `DecompressPool` (<= 64 x 4 MB) | ~100-500 MB / ~1.3 GB | works today |
| parallel (rebuilt) | schedule Vec (24 B/blob) + N workers x (compressed blob + decompressed block + scratch) + bounded result channel | ~50-300 MB / ~1.5 GB at 22 workers with worst-case legal 32 MB blobs | requires the rewrite |

Every floor is bounded by a constant window times per-blob size, independent of file size, with >15x margin against the 24-26 GB ceiling. The pipelined path's admission gate (`AdmissionGate` + `Permit` riding through the channel and reorder slots in pipeline.rs) is genuinely airtight: completion skew cannot grow decoded-block memory with file size.

The kicker: the codebase **already contains a planet-proven bounded parallel read engine** that is exactly the shape par_map_reduce needs - `parallel_classify_phase` / `parallel_classify_accumulate` in `src/scan/classify.rs` (pread-from-workers over a `HeaderWalker` schedule, per-worker thread-local alloc/free, bounded descriptor and result channels). Its own doc comment records the precedent: migrating a production planet workload onto it took a 24 GB anon peak down to ~7 GB. `par_map_reduce(map, identity, reduce)` maps onto `parallel_classify_accumulate(worker_init = identity, classify = fold map into acc, merge = reduce)` almost term for term. The four bench-read modes simply were never rebased onto the newer engine.

## 3. Structural opportunities, ranked

### 3.1 High-conviction rewrite: rebuild `par_map_reduce` on the worker-pull pread engine; delete the batch-collect

**Bottleneck:** whole-file materialization (the OOM), plus a second, less obvious one: even if the collect survived, Phase 1 is a serialized single-threaded feed at ~0.7-1.5 GB/s with 22+ cores idle, followed by a decode phase that re-touches ~60+ GB of swapped/cold memory. The batch-collect design serializes I/O and decode instead of overlapping them - it is strictly worse than streaming even on a big-RAM host.

**Why the current structure causes it:** `par_map_reduce` predates the pread engine and was justified against a straw man (`par_bridge()` mutex contention). The "alternatives considered" comment rejects channel-based streaming as "more complex, backpressure tuning" - but that machinery now exists twice in the codebase (pipeline.rs and scan/classify.rs), planet-verified.

**The redesign:** for file-backed readers (the only planet-relevant case), `par_map_reduce` becomes: `HeaderWalker` builds a `(seq, offset, size)` schedule (~24 B/blob); N workers pull entries, `pread` the compressed body, `decompress_blob_raw` into thread-local buffers, build the block via `from_vec_pooled_with_scratch`, fold `map_op` into a per-worker accumulator; accumulators reduce once at the end. No reorder buffer, no ordered handoff, no consumer bottleneck - the map runs on all workers. This is `parallel_classify_accumulate` with a fold, either by calling it directly or by lifting a small generalization into `src/read/`. For generic `R: Read + Send` (non-seekable), either delete the capability (pre-1.0, and nothing in the repo uses it off-file) or keep a bounded streaming fallback: the existing stage-1 feed + fold-inside-decode-task, no decoded channel, no reorder. I would delete it and take the API break; a follow-up can restore it if a real consumer appears.

**Payoff (quantitative, estimated, no planet run):** from the measured evidence, single-thread decode+parse+count is ~563 s (sequential) for the 98 GB file, and count-only consumer work is ~268 s (pipelined, whose decode already parallelizes; 10-11 B elements at ~40 M elements/s of single-thread `Element` delivery is its ceiling). Folding on 22 workers turns the CPU cost into ~563/22 = ~26 s, leaving aggregate pread I/O as the bound: at 2-6 GB/s NVMe queue-depth bandwidth, ~16-50 s. Expected wall time **~40-80 s for planet, at a couple hundred MB of RSS** - versus SIGKILL today, and ~4-6x faster than the best currently-working mode. Memory floor as tabulated above.

**Risks:** result-type `T` must be `Send` and per-worker accumulators must stay small - true for reduce semantics by construction, but a caller folding into a giant per-worker collection reinstates the classify-accumulate hazard documented in classify.rs; the doc contract must state the bound. Unordered delivery is already the documented contract. pread on an O_DIRECT-opened fd needs alignment care - simplest is to have the parallel path always open its own buffered fd like `HeaderWalker` does (with `fadvise`), independent of `--direct-io`.

### 3.2 High-conviction architectural direction: one worker-pull read engine, two delivery policies

The read side currently has **two parallel engines**: pipeline.rs (push model: I/O thread -> raw channel -> dispatcher thread -> rayon spawn per blob -> decoded channel -> reorder buffer -> consumer; four thread roles, three handoffs, an admission gate, and a permit object riding every item) and scan/classify.rs (pull model: schedule + shared fd, workers pread/decode/fold, one result channel). The pull model is structurally simpler, has strictly better I/O behavior (parallel preads at queue depth vs one serialized `read()` stream), keeps all alloc/free thread-local by construction, and is the one production has been migrating planet-scale commands onto (verify_ids, tags_filter, altw, geocode passes, diff sharding).

The coherent end state: **a single worker-pull engine in `src/read/`** parameterized by delivery policy - (a) unordered fold (serves par_map_reduce and all classify callers), (b) ordered block stream (serves `for_each_pipelined` / `into_blocks_pipelined` / `for_each_block_pipelined`: workers claim seq numbers, a `decode_ahead`-style window gate bounds admitted-not-delivered blocks, the consumer drains a reorder buffer - semantics identical to today's pipeline, ordering assertions preserved). pipeline.rs then becomes the ordered policy of that engine; the stage-1 feed thread, the raw channel, and the dispatcher thread all disappear for file-backed inputs. Non-file readers (`ElementReader::new(R)`) keep a thin streaming feed or lose pipelined support pre-1.0.

Honest caveat on payoff: ordered consumers are today bound by single-threaded per-element delivery (~40 M el/s), not by the feed, so unifying buys ordered paths robustness and code deletion more than headline speed. The throughput win concentrates in the unordered policy. That is fine - it is the right structure, and it removes the double-maintenance of backpressure logic that already diverged once (admission-gate bug surface in pipeline.rs vs none needed in classify.rs). I would do 3.1 first as its own complete change, benchmark, then fold pipeline.rs into the engine as a second complete change - not as gated experiments.

### 3.3 Medium-value local change: eliminate the redundant full-block copy in the sequential and blobreader paths

`Blob::decode()` -> `to_primitiveblock()` -> `decompress_blob(...)` produces a fresh decompressed `Bytes`, then `PrimitiveBlock::new` immediately does `Self::from_vec(buffer.to_vec())` (block.rs) - a full second memcpy of every decompressed byte in the file (~300+ GB of copying at planet scale for one pass). The pipelined path already avoids this via `to_primitiveblock_inline_with_scratch`; the machinery for the sequential path also already exists (`Blob::decompress_into` + `PrimitiveBlock::from_vec_with_scratch`) - `ElementReader::for_each` and the raw `BlobReader` mode were simply never routed onto it. Bounded win (memcpy at ~10 GB/s is maybe 30-60 s of the 562.7 s), but it is a deletion of pure waste on a path every command's correctness baseline uses. Risk: touches `Blob::decode`'s shared surface; keep the public signature, change only the internal route, and lean on the read-path equivalence tests.

### 3.4 Smaller observations (fix opportunistically, not as goals)

- **bench-read's `blobreader` mode uses `BufReader::new` (8 KB default)** instead of the library's 256 KB `FileReader::buffered` (cli/src/main.rs, `run_bench_read`). The mode partly measures syscall overhead, not the library path; likely most of the 609.7 s vs 562.7 s gap. One-line harness fix.
- `for_each` and the blobreader mode leave `parse_indexdata` on (42-byte copy per blob) though neither calls `Blob::index()`; `par_map_reduce` already turns it off.
- Per-blob `posix_fadvise(DONTNEED)` is one syscall per blob - 1.45 M extra syscalls on the 8k repack. Batch it every N MB.
- **Stale/contradictory doc comments are an active hazard:** the `par_map_reduce` "Memory safety analysis" comment asserts the collect is safe at planet scale (it OOMs), and the `run_pipeline` doc warns of "25+ GB heap retention" from cross-thread `PrimitiveBlock` frees - a pathology that the inline-entries constructors (`from_vec*`, per block.rs's own comment) were built to eliminate, and which the measured evidence (pipelined completes planet with tiny RSS) says is gone. Both comments will anchor future work wrong exactly the way this brief fears; rewrite them alongside 3.1.

## 4. Safety constraint: shared surfaces and insulation

- **3.1 (par_map_reduce rewrite):** `par_map_reduce` has zero production callers - grep shows only the lib.rs doc example, the bench-read CLI entry, and comments. Shared surfaces touched: `ElementReader::par_map_reduce` itself, and (if generalized rather than copied) `scan/classify.rs`, whose production callers (verify_ids, tags_filter, altw, geocode) are planet-verified. Insulation: prefer additive extension or a sibling function in `src/read/`; existing equivalence tests in `tests/read_paths.rs` (`par_map_reduce_count`, `par_map_reduce_collect_ids`) pin semantics; `brokkr verify all` re-verifies classify callers only if that file is touched. Production risk is effectively nil.
- **3.2 (engine unification):** touches `run_pipeline`, whose consumers are the planet-safe production spine (cat, getid, getparents, tags_filter, time_filter, verify_ids, altw, geocode pass1). This is the change that must carry a full re-verification: `brokkr verify all`, the pipeline shutdown/early-exit tests, ordering assertions, and Europe-scale command benches before planet. Keep `for_each_block_pipelined`'s signature and file-order guarantee byte-for-byte; only the machinery behind it changes.
- **3.3 (copy elimination):** touches `Blob::decode`/`to_primitiveblock`, used broadly. Same output type and semantics; the full test suite plus `brokkr verify all` covers it.

## 5. Bottom line

The parallel-mode OOM is not a deep memory-model problem; it is one function (`collect_osm_data_blobs`) implementing 2020-era osmpbf semantics with a whole-file materialization that the rest of this codebase outgrew. The 0.4 retention ratio that looked like a mystery is swap arithmetic on a 39 GB-swap host over a 1.0x-retention collect. All four read modes can run at planet scale on this host with two-orders-of-magnitude memory headroom; the fixed parallel mode should not merely survive but become the fastest mode by roughly 4-6x, because the library already owns the right engine (worker-pull pread + thread-local decode) and simply never pointed its oldest API at it. Do the par_map_reduce rewrite as a complete, unhedged change; then retire pipeline.rs into a delivery policy of the same engine as a follow-up; take the sequential-path copy elimination and the bench harness BufReader fix along the way.

---

## Empirical verdicts (2026-07-12 overnight, commit `a65cecc`, plantasjen)

Every follow-up these reports surfaced was implemented env-gated
(default-off, byte-identical gate-off) and measured in one overnight A/B
run - one binary, one commit, same-day A/B by construction. Full read-out
in `notes/env-gated-readpath-batch.md` (deleted on landing; durable arc
settles into `reference/performance-history.md`). The reports predicted
direction well on fusion and the local reads, but MISPLACED conviction on
the ordered-pipeline rebuild - a caution worth preserving against the next
"high-conviction full rewrite" framing.

| Report prediction | Conviction claimed | Measured verdict |
|---|---|---|
| Replace `par_map_reduce` with bounded worker-resident fold (R1 sec 4, R2 sec 3.1) | high | **VALIDATED** - landed `7532021` pre-batch: planet SIGKILL -> 54 s / 616 MB; the win the reports promised |
| Fuse transforms into decode workers (R1 sec 6) | high | **KEPT** - getid-8k -7.68%, getparents-8k -6.51%, tags-filter-R 8k -6.97%; getid-primary GETID_PASS2 RSS 1.18 GB -> 596 MB (-50%). The 90 MB batch materialization the report named is real and it went away |
| Rebuild ordered pipeline around byte-bounded batches (R1 sec 5, R2 sec 3.2) | high | **REVERTED** - no isolated win (getparents-8k -6.67% but redundant with fusion on the same cell; reads/getid/tags-filter neutral) and the fused combination REGRESSED +9.30% vs baseline, +18.4% vs fusion-alone (216.3 vs 182.7s) - and fusion-alone is the shipped state, so that is the killing figure (deeper reorder skew: high-water 32 vs 14). The per-blob seams the report indicted as "structural overhead at high blob count" cost less than the report assumed; the batch engine's own coordination gave it back |
| Batch page-cache eviction / fadvise watermark (R1 sec 7, R2 sec 3.4) | medium | **REVERTED** - planet-8k all modes +0.75% to +2.68%, inside noise, mildly slower. The 1.45 M advisory syscalls are real but not a measurable wall cost at this scale |
| Byte-aware buffer knobs (R1 sec 7, R2 sec 3.4) | medium | **REVERTED** - no knob improved anything; 8k pipelined +3.13% and CMD_BATCH getid-8k +3.49% regressed past the floor. Count-vs-byte admission was not the binding constraint |
| Sequential-path double copy (R1 sec 7, R2 sec 3.3) | medium | **LANDED separately** `3ccc580` (not in this batch); no regression across 16 cells |
| `BlobHeader.datasize` unvalidated (R1 sec 3) | robustness | **FIXED** `MAX_BLOB_DATASIZE`, 2026-07-11 |

The through-line correction: at 1.45 M blobs the per-blob pipeline seams
are cheaper than both reports estimated, so the two read-side rewrites
premised on "seam overhead x 1.45 M" (the batch engine, the fadvise
watermark, the byte knobs) did not pay. The wins came from the two items
premised on removing WHOLE STAGES of materialization/collection
(par_map_reduce's whole-file collect; fusion's 90 MB per-batch
materialization + second dispatch), not from shaving per-blob constants.
Kept: fusion (item 3) and the unrelated europe WILLNEED prefetch (item 5,
TODO's own item, not from these reports) at ~6%. Item 3/4 resolved as
State 3 of the shared four-state matrix: keep fusion, revert batching.
