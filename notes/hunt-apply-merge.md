At `63d4ec1d`, I found four implementation defects and two documentation mismatches. This was a static review. No files were changed, and no builds or tests were run.

1. **[P1] apply-changes silently loses creates when the base lacks earlier element kinds.**

   A sorted, indexed base containing only ways, combined with an OSC creating a node, succeeds without writing that node. A base beginning with relations similarly loses node and way creates.

   The [first-item handling](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:732) only flushes a previous kind when one exists. The [final flush](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:457) then excludes kinds preceding the last base kind. Their upsert cursors remain untouched.

   This breaks the documented [insert-on-missing-ID behavior](/home/folk/Programs/pbfhogg/reference/osmium-parity.md:284). Under `--locations-on-ways`, a dropped node can even supply coordinates through the seeded location map without appearing in the output.

   The drain needs to advance through every kind, including kinds absent before its first base blob.

2. **[P1] apply-changes accepts stale changes against newer base objects without the opt-in flag.**

   Given base node 1 at version 9 and a single OSC modification at version 8, the output contains version 8. A version-8 delete removes the version-9 base object.

   The [rewrite dispatch](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/rewrite_block.rs:159) replaces or deletes solely by identity. Version checks occur during OSC accumulation and never compare against the base. Moreover, [SortedDiff stores deletes as IDs alone](/home/folk/Programs/pbfhogg/crates/lib/src/osc/sorted.rs:43), losing their version claims before the merge.

   This breaks the [`--allow-version-regression` contract](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1933): a downgrade succeeds with the flag off. The code is wrong here; limiting the gate to conflicts inside the OSC does not enforce the advertised behavior against the object already held in the base.

   The finished overlay must retain enough version information, including tombstones, for the base merge to enforce precedence.

3. **[P2] blob_merge silently removes tags from malformed elements it rewrites.**

   Give an element three tags whose second tag has an out-of-range string index, and place its blob in an overlap run. The command can publish the element with only its first tag.

   The [sweep encoder](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:461) uses `tags()`, whose invalid-index behavior is to end iteration. The [owned conversions](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:673) used during cursor demotion have the same behavior. This affects both sort and cat --dedupe.

   [CORRECTNESS.md records the gap](/home/folk/Programs/pbfhogg/CORRECTNESS.md:43), but documenting it does not satisfy the stated contract that input-reachable faults produce refusals instead of silently wrong output. The command boundary is wrong: it treats a best-effort iterator as a complete element representation.

   Both borrowed emission and demotion need to propagate checked-tag failures.

4. **[P2] merge-changes corrupts character-referenced whitespace in metadata it promises to preserve.**

   For accepted input such as `timestamp="a&#10;b"`, the parser retains the referenced newline. The writer emits that newline literally. Reading the output normalizes it to a space, changing the attribute value to `a b`.

   The [metadata parser](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:1170) performs XML attribute normalization, but [metadata emission](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:169) uses ordinary escaping for timestamp, changeset, uid and visible. Only user gets the escaping that preserves tabs, carriage returns and newlines.

   This affects both merge modes and the shared tags-filter OSC emitter. These fields deliberately accept arbitrary attribute text, so it breaks the [normalized-text preservation contract](/home/folk/Programs/pbfhogg/CORRECTNESS.md:545). The writer should use the existing whitespace-preserving escaping for these fields too.

Two documentation mismatches also surfaced:

- **Negative IDs are not rejected project-wide.** Both OSC commands accept negative object IDs and references. The blanket description conflicts with the [ADR's explicit decision against universal rejection](/home/folk/Programs/pbfhogg/decisions/0002-negative-ids-rejected-project-wide.md:105). The blanket claim is wrong; the implemented policy is selective enforcement.
- **CORRECTNESS.md incorrectly says every OSC consumer drops delete children.** Its [statement](/home/folk/Programs/pbfhogg/CORRECTNESS.md:556) contradicts merge-changes, which preserves them, consistent with the [CLI contract](/home/folk/Programs/pbfhogg/docs/cli/commands.md:2035). The document should distinguish overlay consumption from OSC re-emission.
