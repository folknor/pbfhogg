The biggest opportunity is **to avoid encoding fields whose meaning has not changed**. I would rewrite the command-to-encoder boundary around source-backed wire data, then replace the dense-node builder's intermediate integer columns. Both justify substantial internal rewrites.

My ranking is:

| Priority | Opportunity | Conviction and scope |
|---|---|---|
| 1 | Preserve source fields and string identities through transformations | High-conviction architectural rewrite |
| 2 | Build dense output as packed bytes, with one final assembly | High-conviction encoder rewrite |
| 3 | Schedule encoding by output block, independent of merge ownership | High-conviction rewrite for specific workloads |
| 4 | Make ALTW coordinate scratch directly usable as PBF fields | Medium-value structural change |
| 5 | Remove remaining allocations, repeated bookkeeping and writer handoffs | Medium-value local changes |

These are throughput hypotheses grounded in executable code. I ran no measurements, read no project Markdown or git history, and changed no files.

**1. Preserve source wire data through selection, cleaning and merging.**

The common rebuild path discards information the encoder could reuse.

For example, [extract_block_pass2](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/common.rs:580) turns retained ways into decoded references, resolved tags and resolved metadata, then feeds those into `BlockBuilder`. [Tag filtering](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:415) does the same. [Cat's cleaning path](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:969) reconstructs geometry and tags while changing metadata.

For an unchanged retained way, that entails:

- Decode reference deltas into absolute IDs.
- Materialize those IDs in a vector.
- Resolve string IDs to strings.
- Hash the strings into another block-local table.
- Delta-encode the same references again.
- Rebuild metadata and nested messages.

The structure requires these operations because `BlockBuilder` principally accepts semantic values. The command loses source provenance before deciding how output should be represented.

There is already substantial evidence in the code that this reconstruction is avoidable:

- Ways and relations expose their packed field bytes through [raw accessors](/home/folk/Programs/pbfhogg/crates/lib/src/read/elements.rs:296).
- Apply-changes uses [raw surviving-base writers](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/element_writes.rs:252).
- ALTW [retains original way bytes while appending coordinates](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/reframe.rs:58).
- Renumber [patches selected wire fields](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:1).

The stronger design is to carry source ownership, dictionary identity, framing and field spans until output construction. A selection command should produce retained element positions or runs. A cleaning command should produce retained elements with specific metadata replacements. The output worker should copy unchanged fields and encode replacements.

For ways and relations, reference and member-ID deltas reset per element. They can survive arbitrary element reorderings without being decoded and regenerated. Even when output needs another dictionary, the reference arrays, member IDs and member types remain reusable. Only dictionary-dependent fields need remapping.

Dense nodes require a more deliberate implementation:

- Whole unchanged groups can retain their packed columns.
- A retained contiguous run can copy its interior deltas, adjusting the first delta of each affected column.
- Sparse selections need new deltas between survivors, but can still retain tag bytes under an identity dictionary and avoid resolving unchanged metadata.
- Separate output groups provide independent delta restart points, subject to the output element cap and ordering contract.

This should be a full rewrite of selected command paths, including their decode interaction. Adding another semantic helper above `BlockBuilder` would retain most of the cost.

The dictionary design is central. [StringTable](/home/folk/Programs/pbfhogg/crates/lib/src/write/string_table.rs:48) already uses an arena, so ordinary interning does **not** allocate once per unique string. The remaining structural problem is repeatedly converting source IDs into strings and then hashing those strings.

I would use two concrete output strategies:

- For output derived predominantly from one source block, borrow its serialized dictionary and preserve IDs.
- For output combining sources, use a lazy `(source dictionary, source SID) -> output SID` mapping. Hash each referenced source string on its first use in that output block, then use indexed remapping.

Current [pre-seeding](/home/folk/Programs/pbfhogg/crates/lib/src/write/string_table.rs:135) copies and hashes every source entry, including unused entries. An identity-preserving output needs neither that arena copy nor a lookup index unless new strings must be matched against existing strings. Raw-only output should never build that index; mixed output can build it when needed.

This has the broadest plausible payoff: it removes hashing, reference reconstruction, metadata reconstruction and some memory traffic together. It applies to partial selections and partial modifications, where whole-blob passthrough cannot help.

The important risks are:

- Preserving large dictionaries for sparse selections can increase output size and compression work. Such output needs referenced-entry compaction.
- Coordinate and timestamp framing belongs to the block. Copied fields must retain compatible framing or be transformed explicitly.
- Dense metadata presence, signed wrapping, empty tag keys and duplicate source strings require faithful handling.
- Source buffers remain pinned while jobs reference them. Account for pinned bytes, including mostly consumed blocks.
- Recovery behavior matters. For example, `cat --clean user` deliberately avoids resolving a discarded bad user SID.
- Structural validation currently does not establish that every string reference is valid. Raw copying must not bypass the checks necessary for valid output.

At the decoder seam, I want validated field spans, source dictionaries, dense column spans and useful restart information. [GroupProjection](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1319) already receives parsed wire structures during validation. That is a useful attachment point: an encoding-oriented projection could retain the information instead of walking back through semantic element views. It needs extension for complete message ranges and dense boundaries.

**2. Replace dense integer accumulation with packed-byte construction.**

For nodes that genuinely need encoding, the present builder introduces a large intermediate representation.

[add_node_columns](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:748) computes deltas and pushes them into integer vectors. [add_dense_metadata](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1000) does the same for metadata. At finalization, [encode_dense_nodes_group](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:2029) traverses those vectors, creates packed bytes in scratch, copies them into enclosing messages, then copies the dense body into the final block.

For a tagless node with complete metadata, those numeric columns contain roughly 57 bytes per node before allocation slack: ID, coordinates, metadata and a four-byte tag delimiter. Those bytes are written and read again before the much smaller packed representation exists.

The redesign I would build is:

1. Compute each output delta once.
2. Immediately append its canonical varint to the appropriate packed-byte column.
3. Track column lengths as they grow.
4. Compute enclosing lengths from those byte lengths.
5. Assemble column bodies directly into the final PrimitiveBlock buffer, once.

That removes the integer-delta vectors and the packed-field-to-DenseNodes-to-final-block copy hierarchy. It requires no separate varint-width pass over the values.

There should also be a bulk input path. The decoder's existing [DenseNodeColumns](/home/folk/Programs/pbfhogg/crates/lib/src/read/columnar.rs:53) covers IDs and coordinates for classification, but it does not carry the metadata, dictionary identities or tag spans needed for efficient output. Where a command already has decoded columns, the encoder should consume those columns directly. Where source bytes survive, it should use opportunity 1 instead.

The same final-assembly approach can improve ways and relations. [Their current encoders](/home/folk/Programs/pbfhogg/crates/lib/src/write/encode.rs:184) stage packed fields, copy them into an element body, copy that into a group, then copy the group into the block. A worker can instead assemble known-length spans directly, or reserve nested prefixes and compact their gaps once.

**Yes, encoding should write into the actual buffer compression reads.** On the final assembly path, that should be one worker-owned buffer. The compressor then borrows that buffer until compression finishes.

That last point is partly implemented already. [lend_encoded](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1982) and [FramedBlocks](/home/folk/Programs/pbfhogg/crates/lib/src/commands/mod.rs:155) let compression read builder bytes directly. There is no universal extra builder-to-compressor copy to remove. The larger opportunity is eliminating intermediates *inside* encoding and using the existing direct-consumption arrangement consistently.

This is a high-conviction rewrite because it removes work on every genuinely rebuilt node. Its wall-time benefit remains dependent on compression and scratch I/O.

The main risks are different:

- Interleaving writes to several byte columns could have worse instruction behavior than the current separate loops.
- Prefix backpatching can replace several copies with an equally expensive compaction if designed poorly.
- Encoded-size accounting must handle outlier elements and byte limits.
- Partial metadata must retain its semantic distinctions.

The optional partial-node path deserves separate attention. It currently constructs both [a plain shadow representation](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:905) and [a dense candidate](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:2230), then keeps the smaller. Since that representation choice is internal, it can select one legal representation from accumulated size information without encoding both. This is worthwhile, but its value depends on the prevalence of such metadata.

**3. Make output blocks the encoding work unit.**

Two paths tie expensive encoding to an owner that has sequential responsibilities.

First, sort's overlap sweep already borrows source elements on its common path. Replacing Owned types would therefore miss its main remaining cost. But [Output::encode](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:458) encodes survivors on the sweep thread, and [ship](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:522) compresses them there too. Parallelism is across overlap runs. One large overlap component gives one thread merge, encoding and compression work.

Second, repack sends every source block smaller than the requested cap through [WorkerOutput::Owned](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:700). The [ordered consumer](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:522) then encodes those elements into its central builder.

That owned conversion is substantial. [Owned elements](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:188) contain separately allocated strings, tag vectors, reference vectors and member vectors. Their [read helpers](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:694) copy source strings, and relation output [allocates another borrowed member vector](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:459). OSC inputs already have [an arena-backed representation](/home/folk/Programs/pbfhogg/crates/lib/src/osc/compact.rs:1), yet some [output helpers](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/element_writes.rs:46) materialize vectors again to satisfy the builder.

I would separate ordering decisions from payload construction:

- The merge or packing coordinator decides survivor order and output-block membership.
- It emits bounded jobs containing source-backed descriptors, dictionary handles and necessary changed values.
- Workers encode and compress complete output blocks.
- The output owner writes completed frames in sequence.

For repack, this allows blocks spanning multiple source blobs without central encoding or per-element string ownership. For a large sort overlap run, the sequential merge remains, but payload encoding and compression can proceed across several cores.

Source-backed descriptors should be the default. When sparse tails need evacuation to release large source blocks, use a compact batch arena with ranges and dictionary IDs. The lifecycle should be per batch, rather than a forest of individual allocations.

This is plausibly a major win for repack's grow case and large sort overlap components. It contributes little to already-sorted sort output copied as raw blobs.

The risks are source-buffer retention, ordered-output backpressure, duplicate precedence across job boundaries, and lifetime management. Byte-bounded jobs must charge their pinned inputs as well as output storage. Sort must finish the relevant survivor and version-order decisions before handing off a job.

**4. Store ALTW coordinates in their final packed-field shape.**

External ALTW has already eliminated considerable re-encoding, but its producer and consumer disagree about layout.

[Stage 3](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/coord_payloads.rs:838) emits interleaved latitude and longitude delta-varints. [Stage 4](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage4.rs:1167) scans every varint boundary and copies the bytes into separate latitude and longitude buffers. It then copies those into the way, group and output block.

Stage 3 already knows way boundaries. It should emit per-way latitude and longitude byte spans with lengths, optionally including the final field headers. Stage 4 could append those spans directly.

This removes a per-reference byte walk and the deinterleaving buffers. It is a coherent producer-consumer format change, with plausible value because reference counts are large. I rank it below the first three because it benefits one command path and leaves the coordinate join and much scratch I/O intact.

The risks are payload framing, straddler assembly, pin bitmap placement and maintaining way alignment. The coordinate slot format in `coord_pack.rs` is a separate concern; I found no reason to redesign it for encoding throughput.

**Medium-value local changes remain, but I would keep them secondary.**

- **Finish encoding and compression on the same worker where appropriate.** [Tag-filter pass 2](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:1138) returns uncompressed owned blocks, then [the writer dispatches framing jobs](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1121). The current owned handoff avoids a payload copy, but still moves buffers through another scheduling boundary. `FramedBlocks` supplies the direct alternative.
- **Use dense presence tracking for tag keys.** Each tag currently inserts a SID into `FxHashSet`. A SID-indexed bitset or generation array plus a first-seen list can remove that per-tag hash operation while retaining the final key sort.
- **Omit the dense tag stream for entirely tagless groups.** The builder writes one delimiter per node. The [reader explicitly accepts an empty stream as all tagless](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1575). Track whether any tags occur and avoid producing the delimiter column when none do.
- **Remove repeated relation and reference materialization.** Iterator or column inputs can eliminate temporary member and reference vectors. This becomes much more valuable when coupled with raw-field preservation.
- **Consider bulk varint output only after fixing representation.** The dependency's encoder pushes scalar varint bytes; a bulk writer with one capacity reservation can reduce repeated buffer management. That remains a local optimization with less convincing end-to-end payoff than avoiding the varints altogether.

`header_builder.rs` is appropriately outside the performance priorities: its construction cost is amortized per file.

I also found correctness and preservation issues worth flagging:

- **Malformed tag references can silently shorten output.** [TagIter::next](/home/folk/Programs/pbfhogg/crates/lib/src/read/elements.rs:786) ends iteration on an out-of-range SID. The rebuild and Owned conversion paths above commonly use `tags()`. Checked iterators exist, but those paths can turn malformed input into successful output with missing tags. A wire-preserving rewrite needs explicit validation rather than inheriting this behavior.
- **Re-encoding loses subsecond timestamps.** [Plain metadata conversion](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:542) and [dense conversion](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:605) divide milliseconds by 1000; the builder writes default timestamp framing. A source timestamp with finer precision changes on rebuild. If the command promises metadata preservation, that is a fidelity defect.
- **Inline way locations are discarded by several rebuild paths.** Extract, tag filtering and the borrowed sort sweep call plain way encoders without carrying source locations. Some callers explicitly classify their output as re-encoded without enrichments, so this needs judgment against the command's promise. The data loss itself is clear in the code.
- **Owned way output silently drops mismatched locations.** [add_owned_way](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:449) falls back to a plain way when a nonempty locations vector disagrees with references. The direct builder location API refuses that mismatch. The fallback can conceal an internal transformation error.
