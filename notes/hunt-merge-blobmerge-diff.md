I would make three substantial rewrites: partition blob merges by output key range, give diff a comparison-specific decoder, and replace simplified OSC merge's render-everything pipeline with partitioned reduction. The largest opportunities remove work or a serial owner; buffer tuning is secondary.

This is a source-only assessment. I read the command implementations and their parser, merge-join, decoder and writer dependencies. I did not read project Markdown, git history or benchmark records, run measurements, or edit files. The payoff judgments below are architectural hypotheses, not measured speedups.

1. **Blob merge: replace one sweep per overlap component with parallel output-range merges. High conviction, potentially the largest win.**

   The scheduling unit is currently an entire connected component of overlapping blob ranges. `write_plan` assigns each component to one worker. That worker reads and decompresses its blobs, maintains the cursor heap, resolves duplicates, builds output blocks, compresses them and sends framed chunks to the consumer. See [write_plan](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1428) and [Sweep::run](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:675).

   A connected component can cover a whole element kind even when the actual merge has modest overlap at any individual ID. Two files with different blob boundaries readily produce this shape: successive overlaps connect the ranges into one long chain. The implementation then has substantial parallel capacity but only one useful worker for that component. Compression also occupies that same worker.

   **I would replace the component-owned sweep with a range-partitioned merge executor.** Components can remain useful metadata, but they should not determine concurrency.

   The coherent design is:

   - Decode contributing frames in parallel, once per frame.
   - Produce compact records containing the ordering key, version, original provenance and a handle to the payload.
   - Assign bounded output key ranges to independent merge workers.
   - Let each worker merge, deduplicate and encode its own range.
   - Dispatch completed blocks for compression independently of the merge worker, then assemble output in range order.

   The simplest legal partition boundary is between IDs of the same kind. All copies and versions of an ID stay together. Within a range, preserve the current ordering by version and the duplicate winner: earliest input, then frame, then occurrence. Negative IDs must use the existing OSM ordering, not ordinary signed integer ordering.

   A pathological single-ID group can also be divided by version after normalizing the inputs into `(kind, OSM-ID-key, version, provenance)` order. Equal deduplication keys must remain in one partition.

   **The essential implementation constraint is decode-once sharing.** Giving every range the original compressed frames and letting it independently decode them would trade serialization for repeated work. Dense streams need decoded columns or delta checkpoints, while ordinary elements can use compact offsets into immutable block storage. Truly broad overlap that cannot fit in memory needs one partitioning spill of compact records, followed by independent range merges.

   The existing sweep's cursor demotion is evidence of the ownership problem: sparse tails retain blocks, and the remedy converts those tails into owned elements. Range-oriented storage would make payload lifetime follow the output work that actually needs it. The current memory charge also does not impose a hard bound on dense live overlap. See [cursor demotion](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:402).

   The principal risks are duplicate precedence, versions arriving out of order within a blob, dense delta state, and preserving partial metadata when output blocks change. These are tractable correctness requirements. They do not require retaining the current sweep.

   At the sort seam, I would also change scanned-input handling. Pass 1 currently decompresses untrusted or unindexed payloads to classify them, discards the payload, and pass 2 reads and decompresses rewritten frames again. Mixed-kind frames can subsequently be decoded separately for each kind. A rewrite-oriented ingest path should validate and normalize such frames once into reusable partition records. Trusted disjoint frames should retain the existing header-only classification and raw copy path. See [scan_blob and decide](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:676).

2. **Diff: make comparison the decoder's primary product, with full element views created only when needed. High conviction.**

   Diff already has a compressed-byte equality shortcut. Adding that shortcut is not an opportunity: `blobs_byte_equal` already skips a fresh pair with matching claims and identical compressed payloads. Its usefulness declines when compression, metadata, string tables or block boundaries differ. See [the current equality shortcut](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:1316).

   After that shortcut fails, the current path constructs complete `PrimitiveBlock`s and iterates full elements. Dense iteration decodes metadata even though content equality ignores it. The comparison then resolves tags and roles and decodes reference sequences element by element. Most of that work is spent proving that unchanged elements are unchanged.

   There are two additional structural costs:

   - Residual comparisons rebuild an iterator with `block.elements().skip(skip_count)`. `BlockElementsIter` has no specialized skip implementation, so a large block compared against many smaller blocks repeatedly traverses its consumed prefix.
   - A frame crossing work-unit boundaries shares its compressed allocation, but is decoded separately in each unit and walked for clipping.

   These follow directly from [BlockRun::elements](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:434), [element_merge_pair](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:1349), and [the unit planner](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/units.rs:560). The residual-prefix work can grow roughly with block length multiplied by the number of counterparts, rather than with the number of elements consumed.

   **I would build a comparison block directly during decoding and validation.** It would contain IDs, normalized node coordinates, tag boundaries, raw ordinary-element field spans and enough offsets or checkpoints to resume at an element. Metadata would remain lazy. Full borrowed elements would be constructed only for changed OSC output, verbose details, or fields required by compact text output.

   The repository already has useful foundations:

   - [BlockBuffer::project](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1150) can collect information during validation without requiring ordinary block construction.
   - [DenseNodeColumns](/home/folk/Programs/pbfhogg/crates/lib/src/read/columnar.rs:53) provides contiguous IDs and coordinates.
   - [project_ids](/home/folk/Programs/pbfhogg/crates/lib/src/read/project_ids.rs:1) demonstrates retaining validated field spans.
   - `elements_skip_metadata` already shows that dense metadata decoding is separable.

   I would extend or replace those interfaces as necessary. The goal is a single validation/projection pass, rather than preserving `PrimitiveBlock` as the mandatory intermediate.

   Equality should proceed through increasingly expensive proofs:

   - Compare compressed payloads where possible.
   - After decompression, recognize identical primitive payloads before full construction.
   - Compare normalized node columns and compact element representations.
   - For ways and relations, accept identical packed fields when their interpretation is identical.
   - Resolve string-table equivalence once where reusable, rather than repeatedly comparing the same strings through every element.
   - Fall back to exact semantic comparison for different encodings.

   Equal numeric string IDs from different tables prove nothing. Different coordinate framing, noncanonical varints, dense versus plain nodes and different dictionaries all need semantic fallback. Metadata versions are also not evidence of content equality. The existing [content relation](/home/folk/Programs/pbfhogg/crates/lib/src/content.rs:171) defines the required field and ordering semantics.

   The decoder contract I would ask for is: **decode each frame once, expose resumable slices, compare without metadata, and materialize output fields on demand.** That also allows crossing units to share decoded state rather than repeat decompression.

   This cannot eliminate the need to read independent files. When compressed equality succeeds almost everywhere, input bandwidth and byte comparison become the floor. The high-payoff case is mostly equal snapshots whose encodings or block boundaries differ.

3. **`merge-changes --simplify`: replace global XML materialization with partitioned last-change reduction. High conviction.**

   This path currently:

   - Lexes every input into batches.
   - Renders every change into normalized XML.
   - Retains every rendered batch and its element records.
   - Builds and sorts another key vector covering every change.
   - Scans object groups serially to validate versions and select survivors.
   - Builds all output chunks before writing them.

   Superseded changes retain their rendered XML until the end. The winning XML is copied into another full output representation. See [simplify::run](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/simplify.rs:79) and [dedupe](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/simplify.rs:162).

   For a week or month of changes, repeated edits are exactly the information simplification removes. Rendering and retaining every historical payload before selecting winners is the wrong end-to-end structure.

   **I would use compact validated changes, partitioned by object identity, with reduction performed before final XML rendering.**

   Parsers would attach original input and element positions. Independent reducers would own disjoint object partitions and retain the latest payload per object. Per-input local compaction could discard superseded payloads early. Version claims would remain available separately wherever needed to preserve chronological validation.

   The version rule needs careful treatment. Simplification chooses the last change by input position, not the highest version. Equal versions are allowed. An unversioned change does not reset the highest previously claimed version. Every claim must still be checked in original order, including claims belonging to discarded payloads. The existing [VersionFloor](/home/folk/Programs/pbfhogg/crates/lib/src/osc/version_floor.rs:45) supplies the rule.

   One workable organization is ordered input epochs within each object partition, with bounded ingestion and compact partition spills when necessary. Another is radix partitioning compact descriptors by object and position. Either removes the need for a global comparison sort of fully rendered history. Only surviving objects require final ordering and output rendering.

   All source elements still need validation. Deferring rendering must not allow malformed superseded changes to disappear silently, and the existing precedence of input failures over version-regression failures needs preserving.

   Output should then flow through bounded render/compress chunks into the destination. The current split into approximately one chunk per worker per action makes task sizes depend on total survivor count and retains the complete output in `all_chunks`. Neither is needed.

   The payoff has two parts: less resident data even when most changes are unique, and less rendering work when many changes are superseded. The main risks are chronological version checking, error selection and accidentally turning last-by-position semantics into highest-version semantics.

4. **OSC output: remove mandatory spill-and-reassembly passes. High conviction about the waste; payoff depends on output volume.**

   The two OSC-producing paths have different avoidable amplification.

   **Streaming merge always spills output produced for a non-head input.** The coordinator writes a completed member to that input's scratch file, releases its credit, and later copies the file into the destination. This happens because of input position, even if retaining the compressed member would fit comfortably in the memory budget. Promotion also occupies the sole coordinator while it copies the spill. See [Coordinator::drain and advance_head](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/stream.rs:861).

   I would replace this with a bounded ordered completion window charged for retained encoded output. Scheduling must reserve progress for the head input and keep compression workers nonblocking. Spill would be a consequence of actual storage pressure, rather than the default destination of every non-head member.

   This needs a scheduling rewrite, not just changing `Sink::Spill` to an in-memory vector: a naive shared budget can be exhausted by later inputs while the head waits for credit.

   **Derive has two scratch tiers.** Workers gzip output chunks already. When a unit's channel fills, it switches to per-action spill files for the rest of that unit. The consumer copies those files into the run's three action fragments. Final assembly copies the fragments into the output. See [UnitWriters::emit](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:485), [OscConsumer](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:592), and [assemble_osc](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:870).

   A spilled byte can therefore be written and read twice before its final output write. Parallel gzip is already present; adding compression workers does not address this.

   I would replace the fragment assembler with ordered sequences of completed compressed extents. Preserve the current create/modify/delete grouping:

   - Stream creates into the final staged output as ordered comparison results arrive.
   - Retain or spool modify/delete members once.
   - Append those members directly after the create section completes.
   - Reference worker spill extents directly instead of copying them into another scratch file.

   Global action grouping requires later actions to wait. One holding tier is sufficient. The existing gzip-member representation permits copying completed compressed members without recompression.

   The output-I/O neighbour should provide owned buffers and file extents as first-class output pieces. Unknown compressed lengths still require ordering or buffering before final offsets can be assigned; positioned writes alone do not solve that dependency.

5. **Blob rewrite encoding: stop converting unchanged payload fields through semantic values. High-payoff architectural extension, especially for ways and relations.**

   Borrowed sweep elements avoid owned string allocation, but they still pass through substantial reconstruction. Ways decode refs into a vector. Relations construct member vectors. Tags, users and roles resolve through source dictionaries and are interned into each output dictionary. See [Output::encode](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:477).

   There are already stronger internal APIs: [pre_seed_string_table](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1360), `add_way_raw_bytes`, `add_relation_raw_bytes`, and raw node methods. The sweep does not use them.

   Their current limitation matters: they assume one pre-seeded source dictionary. A merge block can mix many source dictionaries. Calling them unchanged is not a general solution.

   I would make the range-merge encoder operate on source payload spans:

   - Copy way reference arrays and relation member-ID/type arrays without decoding and rebuilding them.
   - Build mappings from source dictionary slots to output slots once per participating source/output block.
   - Rewrite only dictionary-dependent fields.
   - Reuse source framing where compatible; explicitly transform timestamps or coordinates where framing differs.
   - Encode dense nodes from columns, preserving the required metadata presence.

   This removes repeated string hashing and reference reconstruction while keeping output blocks reasonably packed. Flushing on every source switch to preserve one dictionary would create tiny blocks and undermine compression.

   There is also a stronger whole-block opportunity for dedupe: after proving all competitors lose and the surviving block already has the required ordering and uniqueness, copy it through. Byte-identical input frames can often share decoding and classification. Identical bytes alone are insufficient to justify passthrough because a surviving frame may contain duplicates that the overlap path would remove.

   This is an encoder redesign coupled to the range executor, not a small call-site substitution. It carries substantial correctness risk around dictionary references, metadata presence and framing, but removes real work from every rewritten survivor.

The useful smaller changes are narrower:

- Preserve persistent residual iterators in diff instead of restarting with `.skip()`. This is an independently valuable fix with a clear algorithmic benefit on mismatched block sizes.
- Stream simplified OSC output chunks immediately instead of collecting `all_chunks`.
- Compare decompressed payloads before constructing complete blocks when corresponding compressed payloads differ.
- Replace text diff's copying of completed chunk slices with ownership transfer. Its current chunking copies bytes already formatted in a `Vec`; see [TextUnitSink](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/mod.rs:651).
- Reuse relation-member scratch in sweep encoding. This is local allocation reduction, well below the architectural opportunities.
- Treat the serial diff planner as a possible next bandwidth limit after comparison work is removed. It reads both selected payload streams on one thread; independent sequential readers can feed an ordered range planner. I would not claim this is today's dominant cost without measurement.

I also found two correctness concerns worth flagging from the reading:

- **Diff clipping can conceal false index ranges.** A carried frame can be clipped at successive unit boundaries, then cease to be carried when its claimed last key is passed. Payload elements beyond that false claim may consequently belong to no unit. The later claimed-maximum check cannot inspect elements already excluded by clipping. This follows from [carry selection](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/units.rs:629) and [clip](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:1012). A decode-once design should validate actual range coverage before assigning slices.
- **Blob rewrite can silently shorten tags on corrupt string references.** Sweep encoding passes `tags()` into builders. Those iterators terminate on an out-of-range string-table reference, while structural block validation deliberately does not resolve those references. An invalid tag can therefore become a successfully written shorter tag list. The checked iterators already exist. See [sweep encoding](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:477) and [DenseTagIter](/home/folk/Programs/pbfhogg/crates/lib/src/read/dense.rs:752).

My first implementation would be the range-partitioned blob merge. For diff, I would pursue the comparison-specific decoder rather than further elaborate the existing block-pair residual machinery. For long OSC simplifications, I would replace the render-all architecture outright. Those are the strongest opportunities to change the throughput ceiling.
