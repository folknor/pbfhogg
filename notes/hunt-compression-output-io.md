The biggest opportunities are at the boundaries around compression: unchanged bytes unnecessarily entering userspace, encoded blocks crossing an ordered boundary before compression, and compressed frames being copied through a single output owner.

I would pursue three substantial rewrites. They fit together, but each removes a distinct cost. These are code-based hypotheses, not measured speedups. I read source only and changed no files.

**1. Make file ranges a first-class output representation, from the command to the disk scheduler.**

This is my highest-conviction opportunity for passthrough-heavy workloads.

There are good range-copy paths already, but commands do not consistently reach them:

- Indexed `cat` reads complete frames and transfers their buffers to the writer in [cat/mod.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:444).
- Both ALTW output routes read unchanged frames synchronously on their ordered consumer thread, allocate a buffer, and then submit it to the writer: [passthrough.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/passthrough.rs:551) and [stage4.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage4.rs:210).
- Header-only reframing still constructs a new contiguous buffer containing a copy of the unchanged compressed body in [framing.rs](/home/folk/Programs/pbfhogg/crates/lib/src/write/framing.rs:831).
- Sort/blob-merge and apply-changes already coalesce adjacent ranges. However, the resulting range remains one indivisible operation. [CopyRun](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1595) can accumulate an entire contiguous region; the parallel backend assigns that operation to one worker, which performs the whole copy in [parallel_writer.rs](/home/folk/Programs/pbfhogg/crates/lib/src/write/parallel_writer.rs:392).

That last point matters: coalescing reduces dispatch overhead but can also concentrate most of a file's I/O onto one worker. The code has no mechanism to split a large range into independently scheduled output extents.

The stronger design is an ordered output recipe containing owned byte segments and references to source-file ranges. A header replacement becomes a small owned header followed by a source range containing the original Blob body. An unchanged run remains a range throughout the command. The output scheduler coalesces adjacent ranges for planning, then splits execution into appropriately sized, independently scheduled operations.

For buffered regular-file output, those operations can use `copy_file_range` with explicit source and destination offsets. For direct output, they should become parallel aligned reads and writes, with boundary handling described below. The current io_uring range path performs synchronous input reads on its single writer thread through [UringSink::copy_range](/home/folk/Programs/pbfhogg/crates/lib/src/write/uring_writer.rs:830); accepting a range does not currently make that path asynchronous end to end.

The payoff is larger than removing a memcpy. It removes frame allocation, userspace payload reads where kernel copying is available, synchronous reads from the ordered consumer, and unnecessary transport of unchanged bytes through command result queues. ALTW is a particularly concrete target because it already knows the unchanged frames' offsets and sizes.

The neighbour requirement is precise: commands should hand output **ranges and replacement segments**, rather than reading a frame merely to call `write_raw_owned`.

The risks are preserving all existing framing and truncation checks, retaining source-file ownership until completion, handling unsupported kernel copying, and preserving exactly which frames and header fields each command carries forward. A header-only scan must still detect malformed framing; replacing a payload read with a range descriptor cannot silently weaken input validation.

**2. Fuse encoding and compression, then cross one output-order boundary.**

Yes: compression should normally run on the worker that encoded the block.

The repository already demonstrates the useful version of this design. [FramedBlocks](/home/folk/Programs/pbfhogg/crates/lib/src/commands/mod.rs:154) uses `BlockBuilder::lend_encoded` and compresses before returning from the worker. This keeps the builder's encoded buffer reusable. Repack uses that route for worker-produced output, and apply-changes also has worker-side framing.

Other important routes still do this:

`decode/transform/encode -> ordered result -> serial consumer -> new compression job -> writer reorder`

Examples include ALTW's [decode-all output](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:1207), its two scheduled output consumers above, and [renumber's rewrite stage](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/pool.rs:119). The writer then reserves a window token and dispatch permit and spawns a separate [FramingJob](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1122).

The structural cost is more than the second dispatch:

- Compression cannot begin until the encoded result reaches the first ordered consumer.
- `take_owned` transfers the builder's encoding allocation, requiring its replacement for subsequent work; [lend_encoded](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1982) already offers the alternative.
- Uncompressed output occupies upstream queues and crosses threads before compression.
- Decode and compression can run in independently sized CPU pools. The read pipeline constructs its own [decode pool](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:787), while writer framing runs on `Host::executor`. Ordered classify also owns separate worker threads.

The stronger rewrite is one task that reads/decompresses, transforms, encodes, computes the necessary metadata, and compresses its output. It returns an ordered run of finished output segments or ranges, plus statistics. Tasks that emit several blobs should stream bounded pieces with a task ordinal, piece ordinal and explicit completion marker.

One downstream coordinator advances the output frontier and assigns offsets. It should not need to receive an encoded block, spawn its compression, and reorder the same logical work again.

This also needs one CPU scheduling budget across decoding, encoding and compression. Sharing a machine-resource probe is insufficient: independently asking for approximately all available cores still creates competing pools. Fused work naturally spends the same worker's time on those operations and transfers capacity as their relative costs change.

I would retain asynchronous compression for a genuinely serial encoder. Making that caller compress synchronously would sacrifice useful overlap. The architectural distinction should be whether work already runs on a parallel encoding worker, rather than which writer constructor it reaches.

The payoff is strongest where ordered consumers currently mediate large quantities of encoded output, and where separate decode and compression pools compete for cores. It is less compelling in paths already using `FramedBlocks`; those need the downstream ownership rewrite, not another fusion exercise.

The risks are load imbalance from expensive compression levels, tasks with very large output expansion, and scheduling deadlocks if workers block while holding resources needed by other workers. Admission should bound live bytes and prioritize progress near the earliest unfinished output position. Simply moving the existing blocking writer calls inside every encode closure would be an incomplete rewrite.

**3. Replace whole-frame handoff and serial packing with owned segments and parallel output extents.**

This is the strongest rewrite for output dominated by low-cost compression or uncompressed bytes.

The compressor contexts themselves already receive sensible treatment. Zlib and zstd contexts and output scratch are reused in [framing.rs](/home/folk/Programs/pbfhogg/crates/lib/src/write/framing.rs:491). The remaining ownership boundary is expensive:

1. Compression produces bytes in worker-local scratch.
2. [frame_blob_into](/home/folk/Programs/pbfhogg/crates/lib/src/write/framing.rs:248) allocates a contiguous frame and copies its parts into it.
3. Direct output copies that frame into its aligned buffer in [DirectWriter::write](/home/folk/Programs/pbfhogg/crates/lib/src/write/direct_writer.rs:217).
4. io_uring likewise copies it into registered buffers on one thread in [UringState::write](/home/folk/Programs/pbfhogg/crates/lib/src/write/uring_writer.rs:183).

The buffered backend can bypass its `BufWriter` for sufficiently large writes, so the second userspace copy is not universal. The direct and io_uring packing copies are explicit and unavoidable in their current implementations.

The stronger representation is an owned payload allocation with small framing segments, not a freshly flattened frame. Compression should write into an allocation whose ownership can travel to I/O. Two workable approaches are reserved prefix space before the compressed payload, or separate small headers and payloads written with vectored I/O. For `Compression::None`, the encoder's allocation should travel directly rather than being copied into a new frame.

Buffered output can consume these segments using positional vectored writes, batching adjacent frames without concatenating their payloads.

Direct output needs a different physical execution strategy. Arbitrary PBF frame boundaries are not aligned, so “just write the compressor buffer directly” is insufficient. After frame lengths establish output offsets, schedule aligned destination extents. Each extent gathers its contributing segments into one aligned buffer and writes independently. Boundary fragments have one owner, and only the final file tail is padded and truncated.

This preserves the required packing copy where alignment demands it, while eliminating the preceding frame-flattening copy and distributing packing across workers. The single coordinator handles lengths, offsets and completion records rather than touching every output byte.

The current parallel writer is a useful partial implementation: it assigns offsets and performs concurrent positional writes. Its limitation is [round-robin dispatch to private bounded queues](/home/folk/Programs/pbfhogg/crates/lib/src/write/parallel_writer.rs:300). A full queue stalls dispatch even when another worker could accept work. Scheduling byte-sized extents from shared ready work would also solve the giant-range problem in opportunity 1.

The pool and admission policy should belong to the output execution, with per-destination ordering. Today the constants independently prescribe 16 write workers, eight queued operations per worker, 64 framing permits and a 512-item delivery window in [tunables.rs](/home/folk/Programs/pbfhogg/crates/lib/src/tunables.rs:369). These count unlike things: one item may be one frame, many buffers, or a huge file range. They are not a coherent bound on memory or I/O work.

Descriptor budgets are not the primary limitation here: the positional writer workers already share one file descriptor. Increasing descriptor allowances would not remove these bottlenecks.

The payoff is removing full-output memory passes and the single-thread packing ceiling while retaining concurrent I/O. The risks are buffer lifetime through asynchronous completion, correct handling of short writes, non-overlapping direct-I/O extents, and ensuring failure never permits publication of holes. Keep an explicit proof that every scheduled extent completed before marking the output finished.

**There are worthwhile local changes, but I would rank them below those rewrites.**

- **Batch io_uring submissions.** [submit_buffer](/home/folk/Programs/pbfhogg/crates/lib/src/write/uring_writer.rs:245) submits immediately after adding each write. Queue several ready writes before submitting, while still submitting promptly when waiting for buffers or input. This removes submission syscalls without changing bytes. It does not remove the single packing thread.

- **Reuse gzip compression state.** PBF framing caches contexts, but [compress_one](/home/folk/Programs/pbfhogg/crates/lib/src/write/parallel_gzip.rs:701) constructs a new `GzEncoder` and output allocation for each member. Reusable deflate state and output ownership could help the parallel gzip path and its worker-side callers. Preserve member boundaries, header bytes, CRC and trailer exactly. The shared receiver mutex is less persuasive as a first target: it protects job reception, not compression, and jobs are normally substantial chunks.

- **Remove redundant borrowed submissions.** Blob-merge's [write_frame](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1658) builds an owned reframed buffer and submits it through `write_raw(&reframed)`, which copies it for the pipeline. Transferring ownership is a straightforward local saving. The larger replacement is still header-plus-range output.

- **Allow independent destinations to make progress independently.** Multi-extract already frames on workers, but [write_regions](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/multi.rs:1012) writes all destinations sequentially through synchronous writers. One shared output scheduler with separate destination frontiers could remove that serial fan-out. Creating a full fixed-size pool for every region would be the wrong execution shape.

**Durability deserves targeted work only when it dominates the tail.**

The command publication path already avoids a blanket duplicate fsync: bound PBF writers use `Finish::Deferred`, and publication syncs staged contents before installation in [output.rs](/home/folk/Programs/pbfhogg/crates/lib/src/output.rs:2440). I would preserve that responsibility.

Buffered output has no explicit mechanism here to initiate writeback behind the completed output frontier. If substantial dirty data remains at the end, initiating writeback during production could overlap that work with compression. Final fsync must remain the durability barrier, followed by the existing publication and directory-sync protocol. Concurrent preparation of independent staged files could also reduce output-set tails, but must retain the barrier before installation.

I would not begin by rewriting rename/publication bookkeeping. Its cost is amortized, and the obvious recurring costs sit earlier.

Two neighbouring findings are worth carrying forward. First, indexed OSC derivation already compresses on unit workers, but still assembles its final document by reading section scratch files in [derive.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:928). That is another materialization boundary. Action grouping means later sections may genuinely need spilling, so removing all scratch requires more than redirecting the writer. Second, `supports_copy_range()` includes io_uring, whose implementation is a userspace read path; capability names should not be mistaken for equivalent execution costs.

Throughout these changes I would preserve existing encoded blocks, codec settings and compressed bytes. There is enough structural work here without changing compression algorithms or blob partitioning. The large rewrite is the unit of execution and ownership: finished segments and source ranges, one ordering frontier, then independently scheduled physical output.
