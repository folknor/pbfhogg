I would rewrite the execution path around work that can run through to its final result before ordering is restored. The biggest opportunities are removing duplicate payload work, eliminating serial ownership between decode and compression, and separating input progress from ordered consumption.

This is a source-only assessment. The payoff estimates below are architectural hypotheses, not measurements.

1. **Fuse simple extract's classify and write phases. This is the clearest concrete opportunity.**

   Despite its name, `extract_simple_single_pass` does not make one payload pass. Nodes are classified, their ID set is completed, and then the node schedule is read again for output. Ways follow the same pattern, including another decompression and block construction. Relations also have separate classify and write phases. See [the node phases](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/simple.rs:417), [the way phases](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/simple.rs:505), and [the write executor](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/common.rs:315).

   The dependency is weaker than the implementation makes it:
   
   - Node selection needs the region. The resulting node ID set is needed by ways.
   - Way selection needs the completed node set. The resulting way ID set is needed by relations.
   - Relation selection needs those completed sets.

   Encoding a selected node does not need the completed node set for unrelated nodes. Encoding a selected way does not need the completed way set for unrelated ways. The current shape turns a dependency **between kinds** into an additional classify/write barrier **within every kind**.

   I would replace each kind's pair of phases with one decode, select, encode and compress pass. A worker would produce final frames plus compact membership updates for the next kind. Verified passthrough would produce a copy range or retained raw frame. The next kind would wait for membership construction, while already produced output continued draining.

   This removes actual work: repeated reads, decompression, parsing, selection lookups and pool-to-pool transitions. It is more compelling than making an existing queue cheaper. The existing `BlockBuffer::project` and `ValidatedBlock::into_block` already support validating and probing once, then constructing an element view only when necessary. See [those APIs](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1150).

   The important correctness wrinkle is duplicate IDs. Today's write pass selects by the completed ID set, so one matching occurrence can cause other occurrences of that ID to be emitted. A fused implementation must preserve that behavior, including groups crossing blob boundaries. On honestly sorted input, boundary-group reconciliation can handle this. Accepted inputs with dishonest ordering claims require additional care; replacing ID membership with per-occurrence predicates would silently change results.

   **Conviction:** high that the phase seam deserves removal. The exact gain depends on how much output is passthrough and how expensive classification is relative to encoding.

2. **Use one CPU scheduler for read, transform, encode and compression, with one final ordering boundary.**

   The library currently has several independent execution systems:

   - `run_pipeline` creates a private Rayon decode pool.
   - Ordered and classify pools create scoped OS workers.
   - The writer dispatches compression/framing onto `Host::executor`.
   - The CLI supplies `Executor::Ambient`, independently of the resource values used to size the other pools.

   The separation is explicit in [the host executor](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:1606), [reader pool creation](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:787), and [CLI host construction](/home/folk/Programs/pbfhogg/crates/cli/src/host.rs:21). `sized_workers` subtracts a fixed reserve from machine parallelism; it does not allocate a share of a common execution budget.

   The most wasteful composition appears in commands such as single-pass tags-filter:

   `decode + filter + encode -> ordered consumer -> writer dispatch -> compress + frame -> ordered writer`

   The transform already runs on a decode worker, but its `OwnedBlock`s must reach the serial consumer before compression can even be dispatched. That consumer assigns output order, obtains writer permits, and transfers ownership to another pool. See [tags-filter](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:549) and [writer dispatch](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1122).

   Several other paths already demonstrate the stronger kernel: extract and cat-clean workers decode, transform and frame locally. Therefore, “fuse transformation into decode” is not a new repository-wide opportunity. **Carry that fusion through compression and remove the intervening ownership coordinator** is the remaining opportunity.

   I would give each operation an execution context spanning all its readers and writers. Its CPU workers would execute ready kernels from any stage. The normal kernel would keep a blob's decompressed bytes, transform scratch, builder and codec work together until it produces final frames. Large expansions could create compression child tasks on the same scheduler.

   Workers should publish directly into output positions identified by something like `(input task, child ordinal)`, followed by an explicit end marker. That accommodates zero-output filters and one-to-many transforms without requiring the consumer to enumerate every output block first. Passthrough ranges belong in the same output protocol. Ordering would be restored when committing bytes, rather than once before compression and again afterward.

   Commands should hand this engine **block or key-range kernels**, with worker-local state, projection requirements, output destinations and dependency information. A per-element `FnMut` callback should remain an adapter for callers that require it, not the internal command execution model.

   Read and write CPU work should share this scheduler. Blocking reads, backend writes and arbitrary caller callbacks need separate treatment. Simply moving today's blocking worker loops onto one Rayon pool would create starvation and deadlock opportunities when all workers wait for permits, later-task channels, or work queued on that same pool.

   This is a full execution-engine rewrite. Its plausible payoff comes from removing ownership transfers and allowing CPU capacity to follow the actual bottleneck. Sharing threads alone does not guarantee a large win: two oversubscribed pools can already keep every core busy. The stronger claim concerns eliminating the seams, retaining locality and avoiding stage-specific idle capacity.

   Teardown remains significant. [JoinedPool](/home/folk/Programs/pbfhogg/crates/lib/src/joined_pool.rs:37) and [threads::scope](/home/folk/Programs/pbfhogg/crates/lib/src/threads.rs:166) deliberately wait through worker destruction. A shared scheduler needs operation-owned task groups and scratch lifetimes that preserve completion guarantees without requiring a fresh worker set for every phase.

   **Conviction:** high in the architecture; workload-dependent in the wall-time gain.

3. **Make input scheduling independent of result consumption, and stop requiring EOF before useful payload work begins.**

   There are two distinct bottlenecks here.

   First, [frame_pool](/home/folk/Programs/pbfhogg/crates/lib/src/read/frame_pool.rs:44) uses `run_ordered_streams_from` with the frame reader as its source. In [run_streams_core](/home/folk/Programs/pbfhogg/crates/lib/src/ordered_pool.rs:1264), the initial window is filled before workers start. During steady state, the consumer replenishes that window by calling the source itself.

   Consequently:
   
   - Initial frame reads do not overlap worker processing.
   - A source blocked reading the next frame prevents consumption of results already finished.
   - A slow fold or output submission prevents further input reads.
   - Every terminal-only frame task gets a newly created channel, although it produces exactly one result.

   This affects reindex, repair and check-index. It is a structural limitation of using one thread as both producer and ordered consumer, not a channel-depth problem.

   Second, [schedule_walk](/home/folk/Programs/pbfhogg/crates/lib/src/scan/classify.rs:590) materializes the complete schedule before classification starts. That is appropriate when global planning is actually necessary. It is unnecessary for many independent per-blob transforms and reductions. It serializes the complete header walk ahead of all payload work and leaves workers to reread bodies separately.

   I would replace both arrangements with an independently progressing producer feeding bounded work into the execution context:
   
   - Dense scans read frames sequentially and expose work immediately.
   - Selective scans stream header descriptors and arrange bounded payload reads.
   - Results and output commitments progress independently of further source reads.
   - A catalog can still be retained alongside execution when later phases need it.

   The feeder should also apply header-level filters before allocating and reading bodies where legal. Today `run_pipeline` obtains a complete parsed `Blob`, dispatches it, and only then applies `should_skip_blob` on a worker. Rejected blobs have already paid input allocation, body reading, dispatch, admission and ordered completion costs. The file reader already exposes [header/read/skip operations](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob.rs:912).

   Moving filtering earlier must preserve the existing error contract. Skipping a body currently parsed as a `Blob` envelope could stop detecting an envelope error. On a generic stream, skipping also still requires draining bytes. This needs an explicit decision about which validation rejected blobs promise, not an accidental behavior change.

   There is a hard limit to what scheduling can fix in `HeaderWalker`: the next frame offset depends on the current header. Its [probe/window implementation](/home/folk/Programs/pbfhogg/crates/lib/src/read/header_walker.rs:466) cannot overlap arbitrary cold sparse-header probes without already knowing their offsets. Streaming overlaps that chain with useful work; it does not remove the chain. A validated frame directory supplied by the blob-I/O side would unlock genuinely parallel initial range discovery.

   **Conviction:** high for separating producer and consumer progress. Removing full schedules is high value where no global planning dependency exists, and conditional elsewhere.

4. **Replace task-count backpressure with memory-aware admission and explicit frontier progress.**

   The read pipeline admits at most 32 tasks by default, regardless of worker count, input size or transform result size. Permits remain held through ordered consumption. The ordered pool uses `max(32, workers * 8)`, while a result can contain an entire blob's output for every extraction region. See [the read gate](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:106), [ordered window sizing](/home/folk/Programs/pbfhogg/crates/lib/src/ordered_pool.rs:218), and [multi-extract result construction](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/multi.rs:917).

   These bounds conflate several resources:
   
   - Runnable parallel work.
   - Compressed input storage.
   - Decompressed buffers and scratch.
   - Completed output waiting behind an earlier task.

   That creates opposite problems. Small results can exhaust the task window while consuming little memory, leaving available workers idle. Large or highly expanding results can consume substantial memory despite staying within the same nominal window.

   I would make admission account separately for CPU work and retained bytes. Completed frames would release decode storage immediately but retain output credit until committed. Compact summaries could run farther ahead than expanded geometry or multi-region frames. Admission and output publication should suspend tasks or leave them unissued; they should not park shared CPU workers.

   The scheduler should also prioritize the earliest unfinished output task and dependencies that unblock it. Cost estimates can use compressed size, declared raw size and command-specific expansion information. Large independent groups or output chunks can become child tasks after decompression.

   There are two limits worth stating directly:
   
   - No scheduling policy makes one ordinary zlib stream internally parallel.
   - A bounded ordered stream can still become idle behind a sufficiently slow head. Eliminating that entirely requires more storage, finer legal task decomposition, or both.

   Memory accounting itself can introduce deadlocks: later tasks must not consume every byte needed for the head task to finish. The design needs reserved progress capacity, streaming expansion, and an oversized-item rule. The existing unordered reader's [byte budget](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1067) provides a useful local precedent, but accounts for compressed storage rather than the complete operation.

   Multi-extract deserves a further change: each destination should have its own ordered output frontier. Currently one result contains every region's frames, and one consumer writes all destinations serially. [write_regions](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/multi.rs:1012) makes one slow destination hold up all others. Independent destination progress, under one aggregate memory budget, would remove that artificial coupling.

   **Conviction:** high that the current bounds describe the wrong resources. Throughput payoff is strongest on many-core hosts, skewed blobs and expanding transforms.

5. **Rewrite serial consumers that still perform substantial command work. More workers cannot solve these.**

   Two neighboring examples deserve aggressive treatment.

   **History time-filter** runs parallel decode, then performs version grouping, selection, cloning and re-encoding on the calling thread. See [time_filter_history](/home/folk/Programs/pbfhogg/crates/lib/src/commands/time_filter/mod.rs:207).

   The sequential state is mostly a key-group boundary problem. Workers can resolve complete interior `(kind, id)` groups, validate local ordering and encode their selected versions. They need to return unresolved first and last groups plus boundary metadata. An ordered stitcher then combines groups crossing blobs and releases interior output in position. A group spanning arbitrarily many blobs must remain representable without retaining all its versions.

   That turns the serial work from “every version and every selected element” into “blob boundaries and crossing groups.” It is a coherent rewrite with potentially large payoff on history inputs.

   **Geocode building** still gives a single merge thread ownership of string interning, record offsets, geometry-map insertion and multiple output files. [The way merge](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass2.rs:831) performs per-record work after parallel resolution. [Stage B](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass3.rs:963) similarly rebases and writes complete bucket outputs serially.

   I would replace serial offset assignment with parallel chunk construction and prefix sums. Workers produce independently encoded chunks, lengths and relocation information; offsets are assigned from compact metadata; chunks are patched and written independently. String deduplication should be sharded. If first-occurrence string layout must remain byte-identical, record each string's earliest logical occurrence and assign offsets from that order rather than using one mutating interner during every merge.

   This crosses into the index builder's architecture. It can remove an Amdahl bottleneck, but may add relocation work or intermediate storage. Unlike the extract fusion, its end-to-end benefit needs careful accounting for the extra traffic.

   **Conviction:** high for the history boundary decomposition; medium for the geocode redesign's net throughput payoff.

The smaller changes are useful, but I would keep them below those rewrites:

- **Remove the read dispatcher's per-blob Rayon submission hop.** Long-lived workers can take admitted work directly, retaining scratch. The ordered pool already uses atomic claims for known schedules. This removes a thread and a handoff, but alone does not remove serial consumers.
- **Reuse transform state across blobs.** `DecodeTransform::apply(&self, ...)` has no worker-local initialization surface. Tags-filter consequently creates builders and temporary output state per blob, while classify's APIs already support persistent worker state. This is a medium-value local improvement.
- **Avoid timing uncontended operations.** The read pipeline unconditionally reads clocks and updates shared atomic counters around sends, receives and gate acquisition. Its “wait” totals therefore include fast-path overhead. The ordered pool already uses blocking-only instrumentation. See [read completion](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:577) and [Stopwatch](/home/folk/Programs/pbfhogg/crates/lib/src/counters.rs:258).
- **Remove unnecessary shared buffer ownership on fused paths.** The reader shares one mutex-protected `DecompressPool` across decode workers even when a transform consumes and drops the buffer locally. Worker-local borrowed storage can avoid that ownership machinery. The owned block iterator still needs a cross-thread lifetime solution. See [the pool](/home/folk/Programs/pbfhogg/crates/lib/src/read/decompress.rs:73).
- **Decouple external scratch concurrency from permanently open bucket files.** External stage 1 caps workers by descriptors per worker. A bounded descriptor cache over private shard files could preserve independent writes without making CPU concurrency proportional to open-file capacity. This belongs in the scratch writer and has conditional value on descriptor-constrained hosts. See [worker sizing](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage1.rs:683).

`ReorderBuffer` itself is not where I would spend substantial effort: its indexed insertion and front removal are already straightforward. Nor would I prioritize `RegionGrid` construction, pool startup, or descriptor probing. The expensive remaining ownership is in what crosses those mechanisms and what the single receiving thread must finish before anyone else can progress.
