Static reading found five defects or contract mismatches. No files changed, builds performed, or tests run.

1. **[P1] Blob boundaries change the answer for repeated IDs.**  
   In [merge_join.rs:1356](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:1356), the merge consumes all equal-ID elements in the current pair of blocks before fetching either side's next block.

   Consider identical element streams containing two identical `n1` records: old packs them into two blobs; new packs them into one. The result is one common, one created, one deleted. Without indexdata, the sequential merge reports two common.

   This breaks content equality and the claimed independence from indexdata. Derive also publishes a create followed by a delete for `n1`; applying it removes the object, breaking the [content roundtrip promise](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1653). All ranges and counts can be accurate. The merge needs to handle complete runs of equal IDs across blobs; derive must refuse inputs whose multiplicity OSC cannot represent faithfully.

2. **[P2] Derive successfully publishes invalid XML for accepted PBF strings.**  
   [push_attribute_escaped](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:31) escapes markup and whitespace, but neither it nor quick-xml's writer rejects XML 1.0 forbidden characters. PBF string validation checks UTF-8 only.

   A created element with a tag value containing U+0000 or U+0001 therefore reaches the published `.osc` verbatim. These characters cannot appear in XML 1.0, even through character references; conforming readers reject the result. This breaks the promise that `--format osc` writes OSC XML. The incorrect boundary is the shared OSC writer: unrepresentable strings need a refusal before publication.

3. **[P2] Derive emits timestamps its own OSC consumer rejects.**  
   [push_info_attrs](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:410) formats PBF timestamps without checking the output year. A changed element dated `10000-01-01` produces a timestamp with a five-digit year, while [parse_rfc3339_utc](/home/folk/Programs/pbfhogg/crates/lib/src/commands/mod.rs:898) requires exactly 20 bytes and four year digits.

   Derive succeeds, but applying its output fails, breaking the content roundtrip promise even though metadata differences themselves are excluded from comparison. The public delete timestamp parameter has the same problem; [values above `i64::MAX` are also silently clamped](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:1077). The producer must enforce the consumer's representable timestamp domain or the consumer must support the emitted domain.

4. **[P2] `--increment-version` silently stops incrementing.**  
   [write_delete_element](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:1068) uses `saturating_add(1)`. Deleting an element at version `2147483647` with this flag emits the unchanged version and reports success.

   That contradicts the flag's explicit promise to bump the version by one. An unrepresentable increment should be refused.

5. **[P2] Unindexed gzip derivation bypasses the worker cap.**  
   [assemble_osc](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:908) sizes gzip workers from host parallelism without receiving the requested `Parallelism`. When either input is unindexed, `diff --format osc -j 1 -o changes.osc.gz` can consequently start a compression pool using every reported core.

   This contradicts the [documented rule that `N` caps worker pools at `N`](/home/folk/Programs/pbfhogg/docs/cli/commands.md:261). These are compression workers, not the separately exempted reader or drain threads.

Additional findings:

- **Outside scope: advertised version protection excludes the base PBF.** The [CLI contract](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1933) says stale changes are refused against the version already held for an object. In practice, the gate operates only while parsing OSC into a fresh overlay. [The base rewrite](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/rewrite_block.rs:159) replaces or deletes matching objects without comparing their versions. A base `n1 v10` accepts a lone OSC modification `n1 v9` with regression permission disabled. The internal API explicitly describes an overlay-only gate, so the CLI documentation overstates the protection.

- **Documentation drift:** [single-sided counts](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1659) are documented as claimed counts, but [the implementation counts decoded elements](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:531). [Quiet diff](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1628) claims the same refusals as text without `-c`, although it enables byte-equal skipping and can accept corrupt bodies ordinary text diff rejects. The project-wide negative-ID rejection description also has an [explicit, implemented exception for this scope](/home/folk/Programs/pbfhogg/DEVIATIONS.md:568).

- **Structural performance opportunity:** [each residual comparison](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:1356) reconstructs iterators, and [BlockRun::elements](/home/folk/Programs/pbfhogg/crates/lib/src/osc/merge_join.rs:437) walks from the block's beginning through `skip`. One large dense block against many small blocks repeatedly decodes consumed prefixes, producing quadratic work. Persistent decoded cursors would address this together with the repeated-ID boundary defect.
