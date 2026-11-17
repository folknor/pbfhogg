Four findings from static tracing. Two are already documented behaviors that contradict the stronger contracts in your question. No files changed; no builds or tests run.

1. **[P1] Corrupt tag indices silently change selections and written content.**

   **Broken claim:** input-reachable faults are refusals, never silent wrong answers.

   [`TagIter`](/home/folk/Programs/pbfhogg/crates/lib/src/read/elements.rs:786) turns an out-of-range string-table index into end-of-tags, discarding that tag and every following tag. All four scoped commands consume these infallible iterators.

   For example, a way containing `highway=primary`, an invalid tag index, then `name=Main` passes structural validation. Filtering for `name=Main` misses it. Filtering for `highway=primary` selects it but writes only the first tag. [`getid`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getid/mod.rs:1769), [`getparents`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getparents/mod.rs:579), and [`time-filter`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/time_filter/mod.rs:543) likewise publish truncated tags when they select that element.

   [`CORRECTNESS.md`](/home/folk/Programs/pbfhogg/CORRECTNESS.md:43) accurately documents this as accepted behavior. That acceptance contradicts the stated refusal contract. The wrong boundary is command consumption: checked tag iterators already exist, but selection and re-encoding discard their error channel. Fallible matching and element conversion should replace these paths.

2. **[P2] Default `tags-filter` confuses matching an element with matching its ID.**

   **Broken claims:** filtering by tag expressions, and stripping tags from referenced objects that do not directly match.

   Pass 1 stores matching IDs in sets. [`filter_block_pass2`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:615) subsequently treats every occurrence of each stored ID as a direct match, without examining its tags.

   A sorted history input containing `n1/v1 amenity=cafe` and `n1/v2 amenity=school`, with no references, produces both versions for `n/amenity=cafe`. Both count as directly matched. `--remove-tags` also preserves the non-matching version's tags.

   Ways expose an additional consequence: if matching `w10/v1` references `n1` and non-matching `w10/v2` references `n2`, both ways are written, but [dependency collection](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:901) collects only `n1`.

   The [CLI contract](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1529) states no uniqueness or history restriction. Osmium [explicitly limits correct history filtering to `-R`](/home/folk/Programs/pbfhogg/research/osmium-tool/man/osmium-tags-filter.md:30). This requires either refusing inputs the ID-only algorithm cannot represent, with an explicit documented restriction, or preserving occurrence identity and matching decisions throughout the operation. It does not require adding history support.

3. **[P2] `time-filter` accepts absent dense timestamps as the epoch.**

   **Broken claim:** elements without timestamps are refused instead of being treated as timestamp zero.

   [`dense_timestamp`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/time_filter/mod.rs:815) checks whether the *group* has a timestamp column, then returns the slot's value. It does not check whether that particular node claims a timestamp.

   A dense node with absent or sentinel version and timestamp zero is unclaimed padding under the project's [metadata rules](/home/folk/Programs/pbfhogg/CORRECTNESS.md:188). [`DenseNodeInfo::claims`](/home/folk/Programs/pbfhogg/crates/lib/src/read/dense.rs:600) correctly reports no timestamp. Nevertheless, a sorted snapshot containing that slot passes `time-filter` at cutoff 250 and emits the node as predating the cutoff.

   The metadata contract is coherent; the time-filter accessor is wrong. It needs the slot's timestamp-presence semantics, rather than column presence alone.

4. **[P2] Negative IDs are not rejected project-wide in these commands.**

   **Broken claims:** the stated negative-ID rejection policy, plus `--id-osm-file` collecting all element IDs and `getid -r` following every reference.

   [`parse_ids_from_pbf`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getid/mod.rs:333) feeds decoded IDs directly into `IdSet`, silently discarding negatives. A mixed positive/negative ID-source PBF therefore succeeds with a shortened request.

   Reference expansion similarly [discards negative relation seeds](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getid/mod.rs:1342) and [negative way references](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getid/mod.rs:1548). Conversely, `tags-filter -R` can write a matching negative-ID element, while default mode loses it through the ID sets.

   [`DEVIATIONS.md`](/home/folk/Programs/pbfhogg/DEVIATIONS.md:647) acknowledges these silent behaviors and explicitly declines further rejection. It accurately describes the implementation, but contradicts the summary contract. Under the stated invariant, the selection boundaries must refuse these values; silent bitmap omission is not rejection.

One structural opportunity: [`tags-filter` relation closure](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:1211) rescans and decompresses the relation schedule once per nesting level, repeatedly collecting already reached members. An explicit graph and worklist would remove that depth-dependent I/O, as `getid` already demonstrates. This is an unmeasured optimization opportunity, not a correctness finding.
