1. **[P2] Re-encoding can silently discard tags after a corrupt string index.**

   Claim broken: input-reachable faults produce refusals, and repack preserves tags.

   A structurally valid tag array can contain a valid pair, an out-of-range string index, then another valid pair. [TagIter::next](/home/folk/Programs/pbfhogg/crates/lib/src/read/elements.rs:786) returns the first pair, then ends permanently at the invalid index. Repack passes that iterator into the builder at [repack/mod.rs:747](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:747). The builder successfully encodes the shortened list, losing both the invalid tag and subsequent valid tags.

   This also affects [cat's cleaning path](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:1005) and [owned conversions](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:691). CORRECTNESS.md acknowledges the limitation; it remains incompatible with the stated refusal contract. The wrong boundary is the transformation caller supplying a best-effort iterator. Carry fallible tag iteration through owned conversion and encoding so the error cannot become ordinary end-of-input.

2. **[P2] Invalid library compression settings panic instead of returning a refusal.**

   Claim broken: input-reachable faults are refusals.

   `Compression::Zlib(10)` is constructible through the public enum. [compress_zlib](/home/folk/Programs/pbfhogg/crates/lib/src/write/framing.rs:570) forwards it directly into `flate2::Compress::new`; the locked zlib-rs backend asserts when initialization rejects that level. Consequently, a synchronous header write or path constructor can unwind instead of returning its declared `Result`.

   The string parser protects CLI users, but library callers bypass it. [compression.rs](/home/folk/Programs/pbfhogg/crates/lib/src/write/compression.rs:7) explicitly acknowledges this and cites compatibility as the reason. Under the stated pre-1.0 constraints, that rationale does not justify keeping the fault: use checked compression-level types or validate at the writer boundary before creating output.

3. **[P3] “Negative IDs rejected project-wide” is an incorrect contract description.**

   [DEVIATIONS.md:564](/home/folk/Programs/pbfhogg/DEVIATIONS.md:564) makes that claim, then immediately documents accepting negative IDs in diff. The encoding scope also accepts them: the existing [builder test](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:3706) deliberately adds node `-1` and successfully finalizes it.

   [ADR-0002](/home/folk/Programs/pbfhogg/decisions/0002-negative-ids-rejected-project-wide.md) explicitly chooses enforcement at particular sites, with acceptance and silent omission elsewhere. Here the blanket description is wrong; the detailed ADR establishes that universal rejection is not the implemented policy. Narrow the claim to the actual per-operation rules.

4. **[P3] Metadata documentation describes behavior the builder no longer performs.**

   [commands.md:1278](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1278) says rebuilding discards metadata without a version and fills missing fields with defaults. The actual [rebuild path](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:747) uses `element_metadata` and `add_node_partial`, preserving partial fields. A versionless node carrying only a timestamp retains that timestamp.

   Likewise, [CORRECTNESS.md:170](/home/folk/Programs/pbfhogg/CORRECTNESS.md:170) says metadata-less nodes receive zero-filled DenseInfo padding beside other nodes. [begin_dense_node](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1040) instead seals the run when metadata presence changes.

   The code agrees with the newer metadata-preservation contract in DEVIATIONS.md. These older passages are wrong.

One additional surprise: [ParallelGzipWriter's empty-stream test](/home/folk/Programs/pbfhogg/crates/lib/src/write/parallel_gzip.rs:998) deliberately requires zero output bytes, without even an empty gzip member. Its current OSC caller always writes document framing, so I found no current CLI trigger.

These findings are source-traced, not executed reproductions. No files were changed, and no builds or tests were run.
