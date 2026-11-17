I found seven defects or false guarantees, plus two documentation discrepancies. These findings come from source inspection; I did not modify files, build, or run tests.

1. **[P1] The first-data-blob probe accepts a truncated payload.**

   [probe_to_first_data_blob](/home/folk/Programs/pbfhogg/crates/lib/src/commands/mod.rs:829) returns immediately after reading the first OSMData BlobHeader. It never calls `skip_payload` for that frame.

   A complete OSMHeader followed by an indexed OSMData BlobHeader declaring a nonempty payload, with the file ending immediately afterward, therefore produces `Ok(true)`. [inspect --indexed](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:5071) reports success.

   This breaks the [shape-4 truncation contract](/home/folk/Programs/pbfhogg/reference/truncation-handling.md:130), which explicitly includes these probes. The command-side probe is wrong; `FileReader::skip` cannot enforce a check its caller omits. The [conformance test](/home/folk/Programs/pbfhogg/crates/lib/src/read/frame_conformance_tests.rs:224) misses this because its substitute walk calls `skip_payload` on every frame.

2. **[P1] ElementReader ignores unknown required features.**

   [read_header_blob](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1350) accepts the parsed HeaderBlock without checking its required features. The [header parser](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:137) merely collects their strings.

   Add `required_features = ["UnsupportedExtension"]` to an otherwise readable file: sequential, parallel and pipelined element reading still interpret its data normally.

   This contradicts [HeaderBlock's documented meaning of required features](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:236) and osmium parity: [libosmium explicitly refuses unsupported required features](/home/folk/Programs/pbfhogg/research/libosmium/include/osmium/io/detail/pbf_decoder.hpp:1040). I found no recorded deviation authorizing this. Validation belongs at the boundary where a reader commits to interpreting elements; descriptive header access can still preserve unknown strings.

3. **[P2] PipelinedBlocks can conceal cancellation.**

   [PipelinedBlocks::next](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1393) checks admission against sealing, but never checks cancellation. It returns queued blocks directly.

   Once blocks are queued, cancelling the invocation does not make the next item `Cancelled`. If the background pipeline already completed successfully, the iterator can deliver every queued block and then clean EOF without reporting cancellation at all.

   This breaks [ElementReader::invocation's explicit next-blob guarantee](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:99). The background consumer's checkpoints do not cover the later boundary where the public iterator hands blocks to its caller.

4. **[P2] The documented sub-2-GiB memory guarantee is false.**

   [par_map_reduce promises](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:519) a working set well under 2 GiB even with pathological 32 MiB blobs. Its [admission weight](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:883) counts only the compressed Blob body.

   A legal block near 32 MiB containing millions of empty string-table entries expands into approximately 128 MiB of entry scratch and another 128 MiB of [appended offset tables](/home/folk/Programs/pbfhogg/crates/lib/src/read/wire.rs:438), beside its payload. Several workers processing such blocks exceed the claimed bound while remaining comfortably inside the compressed-byte budget. These sizes are derived from the representation, not measurements.

   The guarantee is wrong. Preserving it requires admission that accounts for decoded storage, tables and worker scratch, rather than treating compressed bytes as the whole working set.

5. **[P2] A replaced pinned input can become a platform refusal.**

   [PinnedInput promises](/home/folk/Programs/pbfhogg/crates/lib/src/read/input.rs:279) `InputReplaced` when fallback path reopening encounters another file, including a FIFO.

   Its [direct reopen](/home/folk/Programs/pbfhogg/crates/lib/src/read/input.rs:516) adds `O_DIRECT` before identity verification. [open_for_read_verified](/home/folk/Programs/pbfhogg/crates/lib/src/direct_io.rs:210) classifies an open failure before running `verify`.

   With procfs reopening unavailable, replace the pinned path with a FIFO. The direct open fails with `EINVAL`, which becomes `PlatformUnsupported`, rather than `InputReplaced`. A replacement on a filesystem refusing direct I/O has the same problem. This changes the typed failure and CLI exit classification. The direct-open boundary is wrong: failed backend setup is being classified before establishing which input it concerned.

6. **[P2] An aligned direct skip can silently accept a concurrently truncated frame.**

   [DirectReader::skip_with](/home/folk/Programs/pbfhogg/crates/lib/src/read/direct_reader.rs:192) validates an aligned target solely against the length captured at open.

   Read a BlobHeader declaring a payload extending beyond the current buffer, then truncate the file inside that payload. If its declared end is aligned, skipping succeeds against the stale length. A following read returns zero, allowing the frame walk to report clean EOF.

   This breaks the unconditional [shape-4 refusal and source-equivalence claims](/home/folk/Programs/pbfhogg/reference/truncation-handling.md:130). Pinning's documented lack of snapshot isolation does not establish that a declared payload still exists. Either this skip needs current extent validation, or the documentation must explicitly limit its guarantee to immutable inputs.

7. **[P3] The decode counters overstate decompression.**

   [The dispatcher increments `pipeline_decode_tasks`](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:835) for every blob, including unknown extension blobs. The ordinary transform skips those without decompression.

   [pipeline_metrics.rs](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline_metrics.rs:85) nevertheless defines the count as one per OSMData blob and says subtracting filter skips gives actual decompressions. A file containing only extension blobs after its header produces a positive inferred decompression count despite performing none. The counter implementation and its documented interpretation disagree.

Two further contract discrepancies:

- **The block APIs advertise an ordering assertion that does not exist.** [Their documentation](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:337) directs callers to `for_each_pipelined` for a debug monotonicity assertion. That method performs no ordering check. The documentation is wrong; its earlier statement that callers must verify ordering is accurate.
- **“Negative IDs are rejected project-wide” overstates the implemented policy.** These readers deliver negative IDs without a blanket refusal. [ADR-0002](/home/folk/Programs/pbfhogg/decisions/0002-negative-ids-rejected-project-wide.md:37) explicitly describes selective enforcement and even [silent dropping in some commands](/home/folk/Programs/pbfhogg/decisions/0002-negative-ids-rejected-project-wide.md:73). This is an existing discrepancy in the project description, rather than an undocumented reader regression.
