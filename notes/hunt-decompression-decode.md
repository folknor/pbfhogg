The biggest opportunity is to stop treating a validated `PrimitiveBlock` followed by an `Element` walk as the normal execution path. I would rewrite the hot read/transform path around projections that validate, consume and optionally rewrite wire fields on the same worker. `Element` can remain an adapter for consumers that need it.

The current reader already avoids many obvious costs: packed fields borrow their bytes, the read string table stores offsets rather than allocated strings, decompression buffers are pooled, and worker-side transforms exist. The remaining structural waste is repeated interpretation of those bytes and reconstruction of data that the command never changes.

These are source-based judgments, without measured speedup claims. I read code and dependency source only, made no edits, and ran no benchmarks.

1. **High conviction: fuse validation with the actual decode and computation.**

   [Block validation](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1435) parses every ordinary element and its `Info`, checks every packed array, and fully decodes dense `keys_vals` to validate tag runs. Most parsed information is then discarded. Normal iteration parses the element messages again; `info()` reparses metadata again.

   Group framing is also repeatedly walked: validation scans the framing and then independently searches for nodes, ways, relations and dense nodes. Iteration repeats those searches. These are message-header traversals, not repeated reads of every payload byte, but they still occur for every ordinary element.

   Dense tags have the clearest duplication:

   - Validation decodes key/value varints to verify widths, pairing and run counts.
   - [`DenseNodeIter::next`](/home/folk/Programs/pbfhogg/crates/lib/src/read/dense.rs:195) decodes them again to locate each node's tag slice.
   - A tag iterator decodes that slice again to return its contents.

   Even `elements_skip_metadata()` still decodes dense coordinates and locates tag boundaries. Its metadata saving does not remove construction-time metadata validation.

   **The stronger redesign:** one block scan establishes scalar framing, string-table access and group layout. The consumer specifies which fields need values and which need only validation or preservation. Requested packed fields are validated while being decoded and consumed. Unrequested fields retain the checks necessary to preserve the operation's existing failure behavior.

   For example, spatial classification should validate and decode ID/latitude/longitude directly into classification batches. It should not first count those arrays, construct a block, rediscover the dense message, decode three complete columns, and finally classify them. [Smart extract currently follows that sequence](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/smart.rs:390).

   I would use bounded numeric batches: decode, perform prefix sums and coordinate conversion, classify, then reuse the batch. Full columns remain appropriate when a consumer revisits them; they should not be compulsory intermediates.

   The existing [`GroupProjection`](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1319) interface is a useful foothold, but extending it with more post-validation hooks is insufficient. Today its dense hook receives arrays after their validation traversals have already happened. The rewrite must reach inside that seam so the useful computation participates in the first traversal.

   **Why this can pay:** it removes repeated work across many commands, including repeated passes, without expanding everything into owned rows. ID-only and coordinate-only consumers also stop advancing unrelated streams.

   **Risks:** mixed groups must retain dense-first element order; wrapping deltas, narrowing and metadata presence must remain exact. Provisional results must be discarded when a later field invalidates the block. A large descriptor for every dense node would undermine the memory benefit, so the representation should remain column-oriented and selective.

2. **High conviction: replace decode/rebuild transforms with block-native selection and wire rewriting.**

   [Tag filtering](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:407) resolves tags into a temporary vector, tests expressions, decodes selected references or members, extracts metadata, and feeds everything through `BlockBuilder`. [Extract output](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/common.rs:602) and time filtering have similar loops.

   The command often changes only membership. Nevertheless, selected tags are resolved and re-interned, references are delta-decoded and delta-encoded, metadata is reconstructed, and the output dictionary is rebuilt.

   **The stronger redesign:** produce a selection mask and write surviving wire segments directly.

   For ordinary nodes, ways and relations, retain the source string table and coordinate/date framing. Copy surviving messages, or rewrite only fields that the command actually changes. Preserve fields only where the command promises them: for example, a command that deliberately strips way locations must still strip them.

   Dense nodes require a coherent column rewrite rather than message copying. For each surviving contiguous run:

   - Repair its first delta relative to the preceding output value.
   - Copy the remaining unchanged delta bytes.
   - Apply the same treatment to cumulative metadata columns.
   - Copy selected tag runs and non-delta metadata entries.

   That removes most numerical reconstruction and encoding for long surviving runs. Sparse selection needs more delta repair, but still need not resolve unchanged strings.

   The repository already contains concrete precedents: [renumber's wire rewriters](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:1), [ALTW's way reframe](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/reframe.rs:1), and [apply-changes' raw way writes](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/element_writes.rs:252). The opportunity is to give other hot transforms equally direct implementations.

   Merely switching callers to `pre_seed_string_table()` is a smaller improvement. [Pre-seeding still copies and hashes the dictionary](/home/folk/Programs/pbfhogg/crates/lib/src/write/string_table.rs:136), and dense raw adds still reconstruct nodes. A source-preserving writer should borrow or copy the encoded dictionary directly unless it actually needs to extend or remap it.

   **Dictionary-based predicates belong in this rewrite.** Tag filtering currently repeats string matching per tag occurrence. Compile key/value match classifications against block-local string IDs, then evaluate numeric pairs while scanning tags. Wildcards and expression lists can be evaluated once per encountered dictionary entry rather than once per occurrence.

   [Tags-count already demonstrates numeric tag aggregation before string resolution](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_count/tally.rs:158). Tag filtering and geocode's fixed-key extraction do not consistently exploit that representation.

   **Why this can pay:** it removes work on both sides of the decoder/encoder boundary. For high-retention filters, cleaning, touched apply-changes blocks and repacking, avoiding reconstruction is a stronger opportunity than making reconstruction slightly faster.

   **Risks:** retaining a whole dictionary for a tiny surviving subset can enlarge output and increase compression/I/O cost. Sparse outputs may need dictionary compaction. Coalescing source blocks requires SID translation; preserving separate groups or blocks can avoid some translation, subject to command-specific packing promises. Duplicate dictionary entries, empty keys, partial metadata and cumulative user SIDs require explicit handling.

3. **High conviction for mismatched block sizes: give diff persistent cursors and shared decoded frames.**

   [`BlockRun::elements()`](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:436) starts a fresh element iterator and calls `.skip(self.skip)`. Every residual merge reconstructs these iterators through [`element_merge_pair`](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:1348).

   Consequently, one large block overlapping many smaller blocks repeatedly decodes its consumed prefix. With dense nodes, skipping advances coordinates, tags and metadata as well as IDs. For a large block consumed in roughly equal pieces, repeated prefix traversal can grow with the number of opposing blocks rather than remaining linear in its own elements.

   There is a second duplication: [`Frame`](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:486) shares compressed bytes across work units, but [each unit decompresses and constructs its own block](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:961). Window clipping adds another element walk.

   **The stronger redesign:** make a decoded frame an owned immutable object with resumable cursors. A cursor records message position and, for dense groups, packed-stream positions, cumulative values and tag position. Residual processing resumes there instead of replaying a prefix.

   For frames crossing work-unit boundaries, share one decompressed representation and construct seek checkpoints during its first scan. Units start at checkpoints and decode only their assigned portion. Keep this bounded to active frames and outstanding units, rather than caching a whole input.

   **Why this can pay:** it removes repeated decompression and potentially superlinear decode work. This is especially persuasive for independent snapshots encoded with very different block densities.

   **Risks:** ownership and memory admission must cover shared decoded frames; sharing cannot turn into an unbounded cache. Checkpoints need correct state for every delta stream. Duplicate IDs, history versions and negative-ID ordering must remain coherent at unit boundaries. The payoff will be smaller for similarly partitioned inputs.

4. **A concrete ownership seam worth rewriting: repack's grow path.**

   When an input block contains fewer elements than the requested cap, [repack converts its elements to `Owned*` values and sends them to the merge thread](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:700). The [owned conversions](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:673) allocate tag strings, reference vectors, member roles and user strings. The merge thread then re-encodes them.

   This makes changing block density invoke a general-purpose object materialization path and centralizes encoding for that path.

   **The stronger redesign:** after obtaining actual per-kind counts, assign output-cap intervals to independent output tasks. Tasks consume source block slices, translate dictionaries where necessary, and encode/compress their own complete output blocks. The ordered consumer receives finished frames.

   This needs cooperation from scheduling and encoding, but no preservation of the current worker/merge ownership split. It also naturally uses the dense slicing and cursor machinery described above.

   **Why this can pay:** it removes per-element ownership allocation and a serial encoder from the workload where many source blocks must coalesce.

   **Risks:** output tasks may share source blocks, so decode ownership must prevent duplicated inflation. Large dictionaries and cross-block SID translation can become the next bottleneck. Actual counts, mixed inputs and byte-size limits must drive partitioning rather than unverified index claims.

The projection boundaries I would want commands to use are:

| Consumer work | Values actually needed |
|---|---|
| ID checks, rank construction, ID selection | Kind and ID |
| Spatial classification | ID and coordinates |
| Way dependency discovery | Way ID and references |
| Relation closure | Relation ID and typed member IDs; roles only when relevant |
| Tag matching | Numeric tag pairs and dictionary predicate classifications |
| Time selection | ID/order, timestamp and visibility first |
| Geocode address extraction | ID/coordinates plus selected address keys |
| PBF subset output | Selection plus retained wire fields |
| Text/geometry export | Selected properties, geometry and requested metadata |

Existing `KindIds`, `WaySpans`, inspect projections and raw coordinate scanners cover parts of this already. The change is to make their useful decode occur during validation, then carry that representation through the consumer and writer. I would implement this concretely in the hot commands before designing a universal query framework.

**Decompression itself has worthwhile local opportunities, with lower architectural conviction.**

[Zstd decoding](/home/folk/Programs/pbfhogg/crates/lib/src/read/decompress.rs:257) creates a streaming decoder for every blob and uses `Read::take(...).read_to_end(...)`, despite already holding the compressed payload in memory. The installed dependency exposes reusable bulk decompression contexts and direct destination-buffer decoding.

A worker-owned zstd context with bounded direct output is the clearest local change. It can remove repeated context construction and the streaming I/O machinery. Preserve output limits, raw-size checks, concatenated-frame behavior and corruption detection; passing an oversized recycled `Vec` as the output bound would be incorrect.

Zlib already reuses decompressor state and writes into reusable storage. A different whole-buffer DEFLATE implementation is a plausible kernel replacement, but the code does not establish that it would beat the current zlib-rs backend. I would rank that below the demonstrated repeated work above.

Likewise, `protohoggr` exposes batch cumulative decoding and unchecked varint reads that these readers do not use. Neither is an architectural fix by itself. The existing batch helper counts before decoding, and unchecked reads retain the preceding validation pass. The stronger numerical implementation is a strict batch decoder that validates, counts, performs prefix sums and produces requested values together. SIMD is useful implementation work inside that design, particularly for multi-byte coordinate streams.

I would not prioritize the decompression-pool mutex, inline offset-table layout or header parsing. Nor would I fuse inflation with an incremental protobuf parser first: protobuf field order, dictionary references and DEFLATE backreferences make that much more complicated while leaving inflation work largely unavoidable.

Two neighboring findings materially affect this assessment:

- **GeoJSON export is serial through decompression and formatting.** [It calls `reader.for_each()`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/mod.rs:172), whose [implementation decompresses and consumes blocks sequentially](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:235). Existing worker-side fused transforms could format bounded output chunks in parallel and leave only ordered writing serialized. That is a substantial command-level opportunity even before the decoder rewrite.
- **Repeated relation-closure passes in tags-filter repeatedly decode relation blobs.** Retaining a compact typed adjacency representation from the first relevant scan would let closure iterations operate on that graph. Whether this dominates total wall time depends on the selection, so I would place it below the universal decode and output-path changes.

There are also correctness findings to carry into any rewrite:

- [Re-encoding metadata truncates milliseconds to seconds](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:532), and owned node conversion narrows nanodegrees to decimicrodegrees. Inputs with finer timestamp or coordinate precision therefore lose information through ordinary rebuild paths. Under the stated preservation contract, that is a correctness issue; retaining source framing and wire fields avoids it.
- [Infallible tag iterators stop at an out-of-range SID](/home/folk/Programs/pbfhogg/crates/lib/src/read/elements.rs:782), while construction does not check SID bounds. Several transformation loops use these iterators, so malformed indices can produce successfully written elements with shortened tag lists. Raw rewriting must explicitly handle this rather than accidentally copying invalid indices or giving them new meanings.
- Diff's claim validation has a windowing hole: [clipping stops at the window boundary](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:1012), while residual checks only see the clipped run. An understated index range can leave real trailing elements outside every assigned unit. Actual ID bounds should be established during the initial decoded scan before those bounds control partitioning.
