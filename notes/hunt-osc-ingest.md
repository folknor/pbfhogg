I would make three substantial rewrites: compile OSC directly into sorted native changes, move simplification ahead of XML rendering, and replace diff's residual-block machinery with persistent cursors over decoded data. Those remove work and ownership bottlenecks across whole pipelines.

This is a source-only assessment. I made no edits and ran no workloads, so the payoff rankings below are architectural hypotheses.

1. **Highest priority for ingest: replace the mutable overlay pipeline with parallel parsing and deterministic reduction.**

   Apply-changes currently completes an entirely serial ingest before its parallel PBF work starts. In [rewrite.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/rewrite.rs:343), it parses one OSC into `CompactDiffOverlay`, then converts that overlay into `SortedDiff`.

   That serial path includes substantially more than XML tokenization:

   - Element attributes are scanned separately for ID, metadata, latitude and longitude.
   - `CompactSink` creates fresh staging vectors for each live element. Tag parsing allocates key and value strings, including a key string when interning subsequently finds an existing key.
   - At element end, it allocates another vector of borrowed tag pairs, then copies the payload into the compact arena.
   - Insertion consults the live index and version-floor map, updates the index, and removes the ID from the deleted set.
   - After parsing, `KindTable::build` extracts hash-table entries, comparison-sorts them, unzips them into ID and offset arrays, and separately sorts deletes.

   The relevant implementations are [CompactSink](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:1251), [overlay insertion](/home/folk/Programs/pbfhogg/crates/lib/src/osc/parse.rs:426), and [KindTable::build](/home/folk/Programs/pbfhogg/crates/lib/src/osc/sorted.rs:56).

   **The stronger design is an ingest compiler.** Give every element a stable source position, such as `(input ordinal, element ordinal)`. Parse independent batches on workers directly into native headers and batch-owned payload arenas. Attribute interpretation should traverse each attribute list once; normalized strings should append directly to reusable storage. References, members and tags can occupy separate arenas addressed by spans, removing the requirement to assemble an owned element before compacting it.

   Partition small record descriptors by kind and ID range, sort them using integer keys, and reduce each object's history in source order. Perform batch-local reduction during ingest, retaining the version-history summary required for later merges. This removes the single mutable overlay as the owner through which every record must pass.

   The result for apply-changes should be an immutable, range-partitioned change set containing:

   - Sorted upsert IDs and sorted tombstones.
   - Native metadata with explicit presence bits.
   - Node-coordinate columns and addressable way/member/tag payloads.
   - Shard-local dictionaries with stable string IDs.
   - Efficient range slicing and node lookup.

   `SortedDiff` already provides much of the right consumer-facing shape. The opportunity is to **produce that shape directly**, eliminating the hash-indexed construction detour. Its current conversion already drops the parsing maps; dropping those maps again is not a remaining optimization.

   For workflows that merge OSCs before applying them, the apply-changes neighbour should accept this frozen native result, or an ordered collection of OSC inputs. Currently its command implementation accepts a single OSC path. A native handoff would eliminate a merge-to-XML-to-gzip-to-inflate-to-parse round trip wherever callers currently compose those operations.

   **Why this is plausibly large:** it parallelizes attribute interpretation, allocation, compaction and conflict resolution, while removing several allocations and copies per populated element. Those costs recur across millions of changes even when the final set contains few duplicates.

   **The main risk is reduction semantics.** The winner is the last admitted change by position, not the highest-version record. Equal versions are legal; an unversioned change does not erase an earlier version floor; deletes carry version history. A batch summary must preserve enough information to detect cross-batch regressions and identify the relevant offending position. Collapsing a batch to only its final record is incorrect.

   Parse, compact and sort should therefore form **one integrated ingestion process with one read of the input**. Arbitrary OSC cannot yield its final globally sorted survivors immediately: a later record can replace any earlier ID. Sorted runs and their final reduction remain necessary unless input ordering has actually been established.

2. **Highest priority for merge-changes: select survivors before rendering XML.**

   The simplify path currently uses rendered XML as its retained change representation. [render_batch](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/simplify.rs:52) parses and formats every change, retaining its XML alongside an element descriptor. [dedupe](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/simplify.rs:162) then creates another key array, sorts it, and serially scans the groups for version validation and winner selection.

   Output has another complete materialization boundary: each action is divided into roughly one chunk per worker, every chunk is built and optionally compressed, and `all_chunks` retains the complete output before draining begins.

   This creates three avoidable costs:

   - Superseded changes are fully formatted and escaped.
   - Expanded XML and its descriptors remain resident throughout sorting and reduction.
   - Rendering/compression finishes before output writing starts.

   **I would replace this pipeline completely.** Parse and validate into compact records that preserve what merge-changes actually re-emits, reduce histories, then render only survivors. Use bounded output batches with an ordered drain so formatting, compression and writing overlap.

   The compact representation here needs different information from apply-changes. Merge-changes preserves create versus modify, delete bodies and optional delete coordinates. It also passes through lexical metadata such as timestamp and changeset text. Reusing today's overlay unchanged would discard required information. Build the representation this command needs; a common representation can follow if it proves useful.

   Partition reduction by object key so workers validate independent histories and choose winners concurrently. Combine any regression results by source position. Preserve the command's existing distinction between validating all input and subsequently reporting version regressions.

   **Why this is plausibly large:** it removes work proportional to discarded payload, reduces the working set touched during reduction, eliminates the serial whole-set reduction tail, and overlaps output with encoding. On low-duplication input, the saving from avoiding loser rendering will be smaller, but the materialization and drain improvements still apply.

   The unsimplified merge already has parallel readers, worker encoding, parallel gzip members and bounded admission. Simply adding those features would repeat existing work. Its remaining structural seam is [LexBatch](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:867): the reader tokenizes every child, copies its start-tag content into batch text, records a descriptor, and workers replay those events into another representation.

   I would replace that event transcript with immutable decompressed buffers plus whole-element spans and action context. A lightweight framing pass establishes legal boundaries; workers perform complete element parsing and validation. That removes per-child transcript storage and lets more tokenization work run concurrently.

   The framing implementation must understand quoted attributes, comments and other XML constructs. Splitting at textual `</node>` or similar byte patterns is insufficient.

3. **Highest priority for planet diff: persistent decoded cursors and comparison-oriented decoding.**

   The indexed diff already runs parallel work units. Its largest structural weakness is how decoded residuals are represented.

   [BlockRun::elements](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:434) starts a new block iterator and calls `.skip(skip_count)`. [element_merge_pair](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:1349) recreates these iterators whenever a residual meets another block.

   `BlockElementsIter` has no specialized seek operation. For dense nodes, advancing through that prefix reconstructs IDs, coordinates, metadata and tag boundaries again. A large block overlapping many smaller blocks consequently pays for increasingly long prefixes repeatedly. The extra work can grow with the number of overlapping blocks, although the actual merge should visit each element once.

   Work-unit boundaries add another duplication: the planner shares compressed frames across units, but [BlockPairSide::decode](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:961) independently decompresses and parses a crossing frame in each unit. Sharing the compressed allocation does not share that decode work.

   **I would replace `BlockState { block, skip_count, ... }` with a persistent cursor over owned decoded storage.** The cursor should retain its actual packed-stream positions and delta accumulators. Residuals advance that cursor. Work units should receive ranges over shared decoded storage, or checkpointed ranges that can start without replaying the block prefix.

   This needs an ownership redesign, not a lifetime workaround around today's borrowed iterator. Buffer ownership can remain immutable and reference-counted while cursors store offsets and decoding state.

   **High conviction:** removing prefix replay and duplicate boundary decoding. The payoff is especially strong when input producers use different block sizes or packing boundaries. It will matter less for similarly packed inputs with little residual overlap.

   **The broader comparison rewrite has medium conviction but potentially wider payoff.** Diff compares content, which excludes metadata, yet its ordinary element iteration reconstructs dense metadata on both sides. Block construction also validates messages that iteration subsequently parses again.

   The decoder neighbour already offers useful foundations:

   - [BlockBuffer::project and GroupProjection](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1150) can retain information while the validator is already visiting it.
   - [DenseNodeColumns](/home/folk/Programs/pbfhogg/crates/lib/src/read/columnar.rs:53) provides batch-decoded IDs and coordinates.
   - [DecodeTransform](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:236) supports work performed on decoder workers.

   I would have validation produce compact comparison views: IDs, normalized node coordinates, tag spans and element locations, with persistent access to refs and members. Compare those views in batches and access output metadata when required.

   Dense metadata needs care: its delta encoding means obtaining a late element's metadata can require processing preceding deltas. “Lazy metadata” alone does not make that cost disappear. Checkpoints or decoding fused with validation are needed to remove repeated work reliably.

   There is also a major adjacent opportunity for unindexed inputs. [StreamingBlocks](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:54) currently decompresses, constructs blocks, merges and emits on one thread. Decode independent PBF blobs in parallel, derive actual ranges from validated data, then schedule comparisons over those ranges. That would remove missing indexdata as a reason for serial decompression and comparison.

   Risks include added memory traffic from projections, decoded-buffer lifetime management, duplicate IDs at boundaries, and preserving canonical signed-ID ordering. Content equality must retain its current semantics, including tag order and optional way-location comparison. Equal versions cannot establish equal content.

4. **OSC output scratch deserves a targeted architectural change, with payoff dependent on output volume.**

   Indexed OSC derivation already compresses fragments on workers. Its remaining overhead is the storage path.

   [UnitWriters](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:457) may spill completed members to unit files. The coordinator copies those into three aggregate action fragments. [assemble_osc](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:876) subsequently copies those fragments into the destination. Spilled bytes can therefore pass through two scratch files before reaching output.

   I would write the create stream directly into the staged destination while comparison runs, retaining later action streams as ordered chunk references. Assembly should consume worker spill extents directly, eliminating their intermediate copy into aggregate files.

   Preserving create/modify/delete grouping means later groups sometimes must wait in memory or scratch until the create length is known. That ordering requirement does not require two scratch generations.

   This is a coherent output rewrite, but I rank it below comparison work for small diffs between two planets. It becomes much more valuable for large exports of changes or largely disjoint inputs. Its main risks are ordering, cancellation and retaining fragments until their final consumer finishes.

**Gzip remains a separate throughput ceiling.** [open_osc_reader](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:471) performs gzip inflation synchronously inside the same reader that drives XML tokenization. Parallel element parsing alone cannot exceed a single inflater's output rate.

I would decouple inflation from framing immediately and inflate independent files concurrently. Merge-changes already provides file-level concurrency; the native ingest redesign should provide it too. General parallel inflation of one ordinary gzip member is a separate, medium-conviction rewrite with potentially high upside. It requires handling DEFLATE bit boundaries and dictionary dependencies, plus member/trailer validation. Arbitrary compressed byte ranges are not independent jobs.

The medium-value local changes are straightforward and useful, but they do not replace the rewrites above:

- **Reuse parser staging and append into arenas.** Existing `append_str_attr` and `parse_member_into` helpers already avoid some allocations that `CompactSink` still incurs.
- **Replace the OSC interner's owning string keys.** It stores each distinct string in both its arena and a `HashMap<String, u32>`. The builder's [arena-backed HashTable](/home/folk/Programs/pbfhogg/crates/lib/src/write/string_table.rs:88) demonstrates an existing slot-index approach.
- **Remove apply-time reference/member vectors.** [add_osc_way and add_osc_relation](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/element_writes.rs:47) collect compact iterators into new vectors before encoding. Builder interfaces accepting iterators or suitable packed fields would remove that materialization. Dictionary-ID translation caches could also avoid repeatedly resolving and hashing interned keys, roles and users within an output block.
- **Use reusable byte serialization for borrowed PBF elements.** [write.rs](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:456) still creates temporary numeric strings, start tags and coordinate clones per emitted element or child.
- **Fix the adjacent OSC tag-filter output tail.** It [collects rendered batches](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/osc.rs:250) and then [compresses through one GzEncoder](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/osc.rs:400). Parallel compression machinery already exists elsewhere in the crate.

I also found correctness issues worth separating from performance work:

- **Borrowed PBF-to-OSC emission drops visibility.** [push_info_attrs](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:398) receives no visibility argument, although both plain and dense metadata expose it. A created or modified element carrying `visible=false` loses that field. The same adapter floors millisecond timestamps to whole seconds, losing subsecond metadata where present.
- **Malformed trailing child attributes can escape validation.** [Attribute helpers](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:144) return immediately after finding the requested field. A child such as `<nd ref="1" broken/>` leaves `broken` unexamined. I checked the installed quick-xml source: start-event construction does not itself iterate and validate the attributes.
- **OSC re-emission collapses some explicit empty metadata into absence.** [OwnedMetadata::append_attrs](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:168) tests string emptiness, while parsing stores no separate presence bits for those fields. An explicit `user=""`, for example, is omitted on output.

These are source-level findings without runtime reproduction. Any new parser or compact representation should preserve required refusals and field presence explicitly, including when a record's payload is later discarded by simplification.
