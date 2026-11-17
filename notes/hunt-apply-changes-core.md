The strongest rewrite is a sparse wire editor for touched blobs, paired with an output path that commits file extents and completed frames directly. I would replace the current element-by-element rebuild and the duplicated ordering machinery.

The ordinary indexed fast path already avoids decompressing untouched blobs and uses kernel copies on the default backend. The remaining opportunities are more specific: amplification inside touched blobs, oversized passthrough operations, and dependencies that unnecessarily serialize discovery, computation and output.

This is a source-only assessment. I made no edits and ran no benchmarks. Payoff rankings below are architectural judgments, not measured speedups.

**1. Replace touched-blob reconstruction with sparse wire editing. Highest conviction; a full rewrite.**

The current worker decompresses a candidate, constructs a validated `PrimitiveBlock`, performs a precise overlap walk, and then starts another walk to rebuild the blob. A precise hit stops early, so this is not necessarily two complete element walks. Nevertheless, every surviving dense node subsequently passes through coordinate and metadata accessors, tag decoding, builder columns, delta reconstruction and encoding. See [handle_candidate](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/streaming.rs:669) and [rewrite_block_parallel](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/rewrite_block.rs:107).

The particularly expensive structural choice is the combination of:

- Output blocks capped at 8,000 elements.
- Every output block containing surviving base elements receiving the source blob's entire string table.
- Pre-seeding that table copying and hashing all its strings again.

If one source blob becomes `r` such output blocks and its serialized string table occupies `S` bytes, those blocks serialize approximately `r * S` table bytes. That multiplies table construction, raw output volume and compression work. This follows directly from [pre_seed](/home/folk/Programs/pbfhogg/crates/lib/src/write/string_table.rs:135), the capacity flushes and subsequent reseeding in [element_writes](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/element_writes.rs:80).

I would replace this with an editor whose unit of work is the decompressed source blob:

1. Validate and discover edits together, producing runs of retained rows, replacements, deletions and insertions.
2. Preserve the source string-table slots. Copy its encoded table and append strings needed by changes. A mapping from diff string IDs to appended PBF IDs avoids hashing every source string. Duplicate string contents are legal; deduplicating against the entire source table is unnecessary.
3. Copy surviving ordinary element messages as wire spans, changing only fields the command actually transforms.
4. Edit dense columns by retaining packed runs and repairing deltas at edit boundaries.
5. Produce one or a few blocks based on encoded byte size and PBF limits, rather than fragmenting every source into 8,000-element blocks.

Dense editing is the substantial part. Removing or inserting a row affects the next retained delta in each differential column. Metadata columns have independent delta states and presence rules. Tags have variable-length row runs. The implementation must recover boundary states and repair those transitions; simply deleting bytes would corrupt subsequent values.

This can still copy large retained spans without rebuilding every retained value. It does **not** make the whole operation proportional to the number of changed elements: validation, locating row boundaries, recovering delta states, compression and output still require work over touched payloads.

There is useful machinery already available. [validate_in_place](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1076) and `BlockBuffer::project` allow discovery to ride on structural validation. `GroupProjection` supplies parsed wire fields without first constructing the public block representation. Its hooks would need richer span information and careful handling of dense-first iteration order. The [renumber wire rewriter](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:427) also demonstrates preserving dense payload fields while replacing selected wire content.

For ways and relations, the present code already copies packed references, member arrays and tags through raw-field builder methods. The opportunity there is smaller than for dense nodes: remove repeated envelope construction, table rebuilding and buffer copies, rather than claim a reference decode that does not happen today.

The compression neighbour should accept a sequence of retained spans and generated fragments backed by a stable source-buffer owner. That would let the editor feed compression without first assembling another complete uncompressed blob. This should be implemented for this concrete editor first.

The main risks are delta repair, partial metadata, string-index validation, nondefault coordinate and timestamp framing, and exact output sidecars. The editor must regenerate correct indexdata and tagdata and follow the existing rules for dropping stale enrichment fields. It must also retain the ability to return the original compressed frame when inspection finds no effective edit.

The payoff case is strong because this removes whole categories of work: repeated tables, repeated survivor encoding, temporary dense columns and excessive frame creation. Merely increasing the builder cap addresses only part of that problem.

**2. Replace the two ordered delivery paths with one output plan. High conviction structurally; potentially the largest win when passthrough dominates.**

The current path orders results in the apply-changes drain, submits each rewritten frame through `PbfWriter`, then passes through the writer's own sequencing, admission and delivery machinery before reaching the positioned-write pool. The apply drain additionally owns gap creates and the global upsert cursors. See [run_drain](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:246), [frame submission](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:901) and [writer reservation](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:147).

There is also a more concrete I/O bottleneck: passthrough coalescing has no byte bound. A long untouched run remains pending until a rewrite, create, type transition or EOF flushes it. The entire run then becomes one `CopyRange` operation assigned to one writer worker. The range-copy loop completes that operation serially. Having a parallel writer pool does not parallelize one large range.

This both delays the start of copying and gives the pool highly uneven work. Its round-robin dispatcher can then block on one worker's full queue while another worker is available. See [flush_copy_range](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:938) and [PoolDispatch::send](/home/folk/Programs/pbfhogg/crates/lib/src/write/parallel_writer.rs:286).

I would construct an ordered plan containing three concrete operations:

- Copy an unchanged source extent.
- Edit a source blob using assigned change slices.
- Encode an independent create run.

Assign gap and trailing creates during planning, rather than discover and encode them while the output drain is committing earlier work. Workers should receive immutable ownership of their change intervals. The output coordinator should combine lengths, assign final byte offsets and dispatch positioned operations.

Only the prefix of **output lengths** must become known in order. Payload construction and physical writes do not have to complete in that order. Once offsets are assigned, large copy ranges should become balanced, coarse operations that available I/O workers can execute. Finished frame batches should reach that same sink directly, bypassing another general compression and reorder pipeline.

This also gives a better replacement for the current [sequence window](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/window.rs:1). Today, cheap passthrough descriptors and potentially enormous rewritten results consume identical tokens, returned only after ordered drain processing. One slow blob can therefore stop discovery of later work. Separate the bounded metadata plan from bounded payload storage, and guarantee progress for the earliest unfinished output operation.

There are real dependencies to preserve. Compressed lengths are unknown until compression completes. Mixed-sign index ranges do not always identify the actual first element. Duplicate IDs can straddle blob boundaries, and the current code replaces each existing occurrence. Partitioning must account for those cases rather than assuming strictly disjoint positive ranges.

The failure protocol also deserves preservation in substance: publish producer failures, wake blocked participants, join all work, and publish no incomplete output. Those guarantees do not require retaining the existing actors and channels.

For the default backend, the target userspace traffic should be approximately:

`header/probe bytes + compressed candidate payload bytes`

Untouched payload bytes should remain in the kernel. False-positive candidates still count as userspace traffic even when ultimately copied unchanged.

That distinction currently depends on the backend. The default parallel writer really calls `copy_file_range`; [io_uring's CopyRange implementation](/home/folk/Programs/pbfhogg/crates/lib/src/write/uring_writer.rs:830) instead reads into registered userspace buffers. The owned route also reads every passthrough frame into userspace. The output protocol should preserve an extent operation all the way to the backend, with its actual transport cost made explicit.

This redesign cannot remove physical copying on a filesystem that must copy the bytes. It can remove delayed submission, oversized indivisible operations, redundant ordering and avoidable userspace transfers.

**3. Make locations mode depend on needed coordinates, rather than the whole node phase. Potentially the largest mode-specific opportunity; joint work with the sibling scope.**

[Scanner routing](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/scanner.rs:249) forces every node blob through workers under locations mode. Any node upsert also forces every way blob through refresh inspection.

The node-coordinate requirement is much narrower: coordinates absent from the diff but referenced by surviving OSC ways. [NodeLocationIndex](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/node_locations.rs:34) actually constructs the needed IDs in sorted order, then retains them as a hash set for filtering after decode. The scanner cannot use that representation to exclude node ranges.

I would have the locations side provide a sorted coordinate-request table with output slots. Classify a node blob against the union of actual node changes and unresolved coordinate requests. Decode it only when one of those sets intersects it. During that decode, one traversal should provide edit decisions, coordinate results, order checks and claim validation.

The global node-to-way barrier should also be replaced by actual task dependencies:

- Created and modified ways need the coordinates of their references.
- Refreshing a surviving base way uses the diff's coordinate probe, which is already available before base-node processing finishes.
- Neither requirement inherently demands that all preceding node output has passed through the drain.

Workers can compute ready way work earlier while the output plan still places it after node output. Coordinate publication should follow extraction completion, rather than output submission.

Pruning node reads has a correctness caveat: the present locations path rejects some malformed coordinates even on unreferenced nodes. Skipping those payloads changes that refusal behaviour. Any promised validation must either remain or be supplied by trustworthy validation metadata; this is not automatically an observable-contract-free change.

Avoiding the full way refresh scan needs more than rearranging the core. Current way indexdata describes **way IDs**, not referenced node IDs. It cannot prove that a way blob excludes changed nodes. The neighbour would need a maintained node-to-way-blob dependency index or another conservative exclusion structure. Its storage and lookup traffic must be selective enough to beat scanning compressed ways. I would regard that as a separate architectural project, not pretend that an ID-range tweak solves it.

**4. Remove serial header discovery from repeated update chains. Architectural, but lower conviction for a single invocation.**

`HeaderWalker` discovers the next frame offset by reading the current header. On a cold file, classification therefore follows a serial chain of sparse reads. Faster searches against diff IDs do not remove that dependency.

For repeated daily updates, I would have the output side retain a compact blob directory containing offsets, lengths, kinds, counts and ordering bounds. The next apply can intersect changes with that contiguous directory and describe the complementary unchanged extents without probing every scattered header.

The writer already knows the relevant output facts. Maintaining the directory while writing avoids another payload pass. Ordinary PBFs would still need header discovery initially.

The risks are stale directories, binding them reliably to the input content, and preserving validation at the trust boundary. A directory that silently describes another file is a correctness failure. This is compelling for repeated update chains; it is not my first investment for a one-off apply.

**The local changes are worthwhile, but substantially smaller.**

[SortedDiff](/home/folk/Programs/pbfhogg/crates/lib/src/osc/sorted.rs:43) already has sorted IDs, arena offsets and forward seeks. Replacing it wholesale with hash lookups would move away from the access pattern.

I would instead request a sorted per-kind event view containing ID, action, payload location and the metadata needed for winner selection. Planning can assign slices once, avoiding repeated range searches and the separate upsert, delete and create-cursor logic inside every rewrite. Physically repacking change records into that same order could improve locality, but it affects changed records rather than the much larger surviving base population.

Other medium-value changes include batching already-framed output through `write_raw_chunks`, eliminating unused tagdata ownership in descriptors, and using the existing `AscendingTally` where nonnegative claims permit it. Batching reduces writer reservations and channel traffic, but the current backend still expands chunks into individual writes. Changing the receiver mutex or replacing the reorder `BTreeMap` alone is similarly unlikely to rival the larger rewrites.

**I also found three concrete issues worth flagging.**

- **Version selection differs from osmium.** By inspection, a base node at version 10 and a single OSC modify at version 9 is replaced by version 9; an older delete removes it. [KindSeeks](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/classify.rs:125) matches solely by ID, and the rewrite applies that action without comparing base metadata. The ingestion regression gate compares changes within the overlay, not against the base. The vendored [osmium apply-changes implementation](/home/folk/Programs/pbfhogg/research/osmium-tool/src/command_apply_changes.cpp:342) merges by descending version and keeps the first object per ID, so it retains version 10 in that example. The sorted delete representation has already discarded the metadata needed to make that decision correctly.
- **Direct reads do not cover the main base-read path.** `direct_reads` reaches the initial `InputFront`, but the [scanner opens a buffered walker](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/scanner.rs:138) and the [worker pool opens a plain file](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/streaming.rs:320). The heavy reads therefore remain buffered even when direct reads were requested.
- **Reported blob-size percentiles sample only passthrough frames.** The only sample insertions are in the two passthrough dispatch arms. Rewritten frames and create frames never contribute, although [the summary](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/stats.rs:164) labels the result simply as blob sizes.
