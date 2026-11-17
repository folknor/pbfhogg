The strongest move is a rewrite of the file ingestion path around **leased byte extents, early routing, and independent I/O completion**. Tuning the existing frame buffers would leave the main structural costs intact.

I would prioritize four opportunities. The first is the broadest architectural change; the second offers the clearest command-specific payoff. These are source-based judgments, not measured speedup claims. I read source only, ran no benchmarks, and changed no files.

1. **Replace per-frame materialization with an extent-based read and decode engine. High-conviction architectural rewrite.**

   The current ownership unit is a blob-sized allocation:

   - [`BlobReader::next_frame`](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob.rs:843) allocates a fresh `Vec` for every body.
   - [`Frames::next_raw`](/home/folk/Programs/pbfhogg/crates/lib/src/read/frame.rs:686) allocates a complete frame and copies the prefix and header into it.
   - Buffered input copies buffered bytes into those allocations, although sufficiently large remainders already bypass the buffer.
   - Direct input always reads into its private aligned buffer and then copies into the destination.

   The O_DIRECT structure is especially limiting. [`DirectReader::fill_with`](/home/folk/Programs/pbfhogg/crates/lib/src/read/direct_reader.rs:239) issues one synchronous read into a 256 KiB buffer. [`append_with`](/home/folk/Programs/pbfhogg/crates/lib/src/read/direct_reader.rs:289) drains that buffer by copying, then issues the next read. This gives the sequential direct reader one outstanding storage request, regardless of how many decode workers exist.

   The normal pipeline already overlaps reading with decoding. The missing opportunities are multiple outstanding storage reads, eliminating the intermediate copy, and recycling storage across many frames.

   The stronger design would be:

   - Read contiguous file extents into a bounded pool of owned buffers, aligned for direct I/O.
   - Keep several extents in flight independently of decoding and output.
   - Walk frame boundaries over completed extents and issue descriptors containing provenance, header metadata, and leased payload ranges.
   - Give decompression a borrowed slice backed by the lease.
   - Release compressed storage immediately after inflation, rather than retaining it throughout the subsequent block transform.
   - Recycle an extent after its final user releases it.

   Sequential framing still has to discover each next boundary from the preceding header. That does not require storage submission to wait for boundary discovery: contiguous reads can run ahead without knowing where blobs begin.

   This can eliminate a substantial fraction of compressed-byte copies and allocator traffic, while allowing direct reads to exploit storage concurrency. It applies to virtually every full-file command and every repeated pass. Its wall-time payoff will be largest when ingestion cannot keep decode workers fed, or when inexpensive transforms expose memory movement.

   There are two important neighbor requirements.

   First, decompression should accept **immutable payload storage plus metadata**, rather than requiring a particular owning `Blob` shape. [`WireBlob::parse`](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob_wire.rs:322) already slices compressed data without copying, and [`pool_wrap`](/home/folk/Programs/pbfhogg/crates/lib/src/read/decompress.rs:222) already demonstrates returning storage through `Bytes::from_owner`. Those are useful mechanisms, but the current frame allocation happens before them.

   Second, the scheduler should admit storage by **retained extent capacity**, not just blob count or payload length. Several blobs may share one extent, and one delayed blob can pin the entire allocation.

   Risks are manageable but fundamental: blobs crossing extent boundaries, oversized frames, short reads, cancellation with outstanding I/O, and premature buffer reuse. I would initially coalesce boundary-crossing blobs only, keeping ordinary blobs as slices. Raw uncompressed blocks need special treatment because their input lease may survive as block storage.

   I would favor explicit owned extents over making whole-file mmap the universal replacement. `PinnedInput` preserves file identity, not immutability; truncation can turn mapped access into a process fault. Mapping also moves demand I/O into decoder page faults rather than giving the reader explicit completion and error control.

2. **Decide “skip, copy, or decode” before reading a complete payload. High-conviction redesign with particularly strong payoff for passthrough and selective commands.**

   Several paths obtain bytes that their decision immediately makes unnecessary.

   In the normal pipeline, filtering happens in [`spawn_decode_task`](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:536), after the reader has read and allocated the complete body and parsed its envelope. Thus blob pruning saves decompression, but not payload ingestion. Single-pass tag filtering reaches this path through [`with_blob_filter`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:526).

   Plain `cat` is an even clearer example. [`cat_passthrough`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:433) reads complete frames, then sends indexed ones straight to `write_raw_owned`. Type-filtered cat likewise reads the entire frame before deciding to discard it in [`cat_type_filtered`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:893).

   The writer already has the missing capability: [`write_raw_copy`](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1577) accepts an input file range, and blob merge already coalesces adjacent ranges with [`CopyRun`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1594).

   I would replace the mandatory “produce owned frame” step with three outcomes:

   - **Skip:** validate the required framing and advance without materializing the payload.
   - **Copy:** retain a source range and coalesce adjacent unchanged frames.
   - **Decode:** obtain a payload lease and send it to a worker.

   For indexed plain cat, long unchanged stretches could become range-copy requests without passing their payloads through frame allocations at all. For selective reads, excluded bodies would avoid allocation and userspace copying, with physical I/O savings depending on selection density and readahead.

   The output seam should also support a new small header followed by an existing payload lease. Reindexing currently builds another full frame and copies the unchanged compressed body in [`reframe_with`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/reindex.rs:264). A header rewrite should not require copying the payload merely to make one contiguous `Vec`.

   This is a coherent routing redesign, not simply replacing one write call. It needs to handle mixed indexed/unindexed input, stream input, output backends that cannot use kernel range copies, and interleaving copied stretches with rebuilt blobs. On incompatible backends, the same route can use pooled extents instead.

   The main correctness risk is changing validation behavior. The pipeline currently parses blob envelopes even for filtered-out blobs. Moving the filter earlier must preserve the required refusals, including malformed envelope fields and truncated bodies. A seekable envelope validator can inspect field tags and lengths while skipping length-delimited payload contents, but simply skipping everything after `BlobHeader` would change accepted inputs and exit behavior. Existing metadata trust and `--distrust-metadata` behavior must also remain intact.

3. **Merge header discovery and payload scheduling into one progressive ingestion operation. High-conviction architecture for dense scans; conditional payoff for sparse scans.**

   The current header walker and body readers have separate ownership of bytes they may both read.

   [`PreadSource::refill`](/home/folk/Programs/pbfhogg/crates/lib/src/read/header_walker.rs:466) reads probes or larger windows. Those windows can contain substantial payload data. Yet [`HeaderWalker::next_header`](/home/folk/Programs/pbfhogg/crates/lib/src/read/header_walker.rs:822) exposes metadata only, and later [`pread_data`](/home/folk/Programs/pbfhogg/crates/lib/src/read/header_walker.rs:879) reads the payload into another buffer. The existing window cannot supply a payload lease.

   Schedule construction also commonly forms a full-file barrier. [`schedule_walk`](/home/folk/Programs/pbfhogg/crates/lib/src/scan/classify.rs:616) finishes the header walk before returning the schedule; workers subsequently issue one positional body read per entry. The unordered worker loop in [`run_unordered_per_blob`](/home/folk/Programs/pbfhogg/crates/lib/src/scan/classify.rs:1181) combines each blocking read with its decode work.

   Consequently:

   - Bytes brought into header windows can be copied or fetched again by body readers.
   - The first useful decode waits for schedule construction.
   - Storage requests follow individual blob ownership rather than contiguous file regions.
   - Decode workers can block on storage even when other completed input could be processed.

   I would have one progressive scanner produce both catalog entries and work. For dense consumption, it would read extents once, parse their headers, retain catalog metadata where later phases need it, and immediately dispatch payload slices. For sparse consumption, it would preserve header probing, then batch selected nearby ranges into extent reads.

   Crucially, **coalescing prefetch advice is not coalescing reads**. The existing `WillneedPrefetch` helps residency, but workers still issue separate body reads. Actual extent reads would reduce syscall count and provide storage independent of worker progress.

   Some commands genuinely need global information before work begins. Those should retain their compact planning barrier. The redesign should remove barriers that merely result from an API returning `Vec<ScheduledBlob>`, not pretend all global dependencies disappear.

   There is another serialized seam in [`frame_pool`](/home/folk/Programs/pbfhogg/crates/lib/src/read/frame_pool.rs:74): the same calling thread reads source frames and folds results. [`run_streams_core`](/home/folk/Programs/pbfhogg/crates/lib/src/ordered_pool.rs:1319) even fills the initial window before starting workers, and later refills between folds. Reading and folding therefore cannot overlap with each other. A fold blocked on writer backpressure stops input progress.

   I would separate storage completion, worker execution, and ordered consumption, using reusable result slots for these terminal-only tasks. The current streamed engine creates a fresh result channel per task in [`issue_job`](/home/folk/Programs/pbfhogg/crates/lib/src/ordered_pool.rs:1439); that machinery is broader than frame_pool needs.

   The payoff is strongest for dense scans, reindexing, repair, and lightweight transforms. Sparse header-only discovery retains an unavoidable dependency: the next header offset is unknown until the current header supplies its size. Reading every payload to overcome that dependency would defeat sparse pruning.

   Risks include overreading large gaps, pinning windows behind slow consumers, and changing which failure reaches the caller first. Those belong in the engine's design, not in command-specific patches.

4. **For repeated passes, share useful interpretation of the input rather than blindly caching the entire file. Potentially very high payoff, but workload-specific rewrites.**

   Some commands already reuse header catalogs. Tags-filter retains a catalog for later schedules, and extraction carries schedules between phases. Recommending another generic header cache would miss what remains.

   The avoidable repetition is deeper. In unsorted extraction, [`collect_pass1_generic`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/smart.rs:188) performs separate node, way, and relation sweeps. Each sweep reads blobs and decompresses spatially admitted data before selecting elements for that phase. The dependency ordering is real, but repeated full PBF interpretation is not inherently required.

   A stronger extraction implementation would read and decode once to produce:

   - Node membership results.
   - Compact way IDs and references.
   - Compact relation membership edges.

   It would resolve the node-to-way-to-relation dependencies over those records, then reread only the original data needed to emit the output. Mixed and unindexed blobs particularly benefit because their contents need not be decompressed independently for every kind phase.

   Similarly, tags-filter's [`collect_relation_member_closure`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:1211) rereads and decodes its relation schedule on every convergence iteration. Build the relation dependency representation once, then traverse a frontier of newly included relations.

   These are command algorithm rewrites crossing the read/decode boundary. Their potential payoff is larger than a faster blob copy because they remove repeated decompression and parsing as well as input reads.

   I would not make “one physical read for every multi-pass command” a universal objective. Later output decisions can depend on data discovered near EOF, and retaining all decoded planet data would create a much larger working set. Compact projections, with bounded storage and spill where necessary, are the useful shared product. A final reread of selected original payloads can still be the right design.

   The risks are projection size, scratch traffic exceeding the avoided input work, and losing semantics through an incomplete projection. Dependency records need only preserve dependency semantics; output should still come from original elements unless the replacement representation deliberately preserves everything required.

There are also several worthwhile **medium-value local changes**, distinct from those rewrites:

- **Remove a definite compressed-body copy in scan helpers.** [`decompress_blob_data_into`](/home/folk/Programs/pbfhogg/crates/lib/src/read/decompress.rs:441) calls `WireBlob::parse_slice`, which copies the input into `Bytes`. The borrowed [`decompress_blob_raw`](/home/folk/Programs/pbfhogg/crates/lib/src/read/decompress.rs:506) already parses and decompresses from `&[u8]`. Check-index, cat indexing/reindexing/repair, getid, and some merge work reach the copying helper. This is a concrete missed internal capability, independent of the larger reader rewrite.

- **Expose the existing direct-read buffer instead of copying it out.** [`DirectPositional::read_exact_at`](/home/folk/Programs/pbfhogg/crates/lib/src/read/direct_reader.rs:551) reads into aligned scratch, then copies into the caller's buffer. [`read_aligned_in`](/home/folk/Programs/pbfhogg/crates/lib/src/read/direct_reader.rs:476) already returns the requested slice without that copy, and [`FrameBuf::read`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1292) uses it. A callback or explicitly borrowed worker scratch can extend that capability to other direct consumers.

- **Batch cache eviction over newly consumed page ranges.** [`evict_consumed_prefix`](/home/folk/Programs/pbfhogg/crates/lib/src/read/file_reader.rs:62) advises `0..offset` after every blob. Track a page-aligned eviction watermark and issue larger incremental ranges. That removes per-blob advisory calls and repeated coverage of old ranges. It must respect the caller's cache policy and, with leased storage, the actual lifetime of readers using those pages.

- **Avoid repeated zeroing in positional buffers.** [`ReadBuf::read_exact_at`](/home/folk/Programs/pbfhogg/crates/lib/src/read/read_buf.rs:51) resizes before reading, so growth and length oscillation zero bytes immediately overwritten by `pread`. A sound uninitialized-read primitive can remove this. It is narrower than eliminating entire copies or passes and would not be my first project.

- **Raw blobs need cooperation from block decode to stay uncopied.** The owning decode routes copy raw payloads into a `Vec`, even though another helper can return the existing `Bytes`. The deeper constraint is that block construction appends lookup tables to mutable payload storage. Letting block decode keep immutable input bytes with separate metadata would unlock raw input leases. This is useful for uncompressed workloads, not a primary win for compressed planet input.

Two correctness findings surfaced during the reading:

- [`WireBlob::parse`](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob_wire.rs:328) and [`parse_raw_envelope`](/home/folk/Programs/pbfhogg/crates/lib/src/read/decompress.rs:572) dispatch known fields by field number without checking their wire type. A wrong-typed payload field is interpreted as length-delimited data anyway; `raw_size` is interpreted as a varint regardless of its tag. Unlike `WireBlobHeader`, these parsers can reinterpret malformed envelope bytes rather than reject or skip them according to wire type.

- [`WireBlobHeader::parse`](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob_wire.rs:93) defaults `datasize` to zero and never checks its presence. An omitted required size therefore becomes a zero-byte payload descriptor. Header-only and raw passthrough paths can accept that descriptor while a later data decode fails differently.
