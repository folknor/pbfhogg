Read-only audit of `63d4ec1d`. These findings follow from source traces; I did not modify files, build, or run tests.

1. **[P1] OSC filtering can reverse an object's final state.**  
   [write_groups](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/osc.rs:419) emits all creates, then modifies, then deletes, regardless of input order. A delete followed by a matching re-create at the same version becomes create followed by delete. Both changes pass the overlay's version gate, but applying the filtered stream now deletes the object. This breaks the meaning of filtering a change stream. The filter's output ordering is wrong.

2. **[P2] Malformed timestamps silently become different, valid timestamps.**  
   The OSC overlay calls [parse_rfc3339_utc](/home/folk/Programs/pbfhogg/crates/lib/src/commands/mod.rs:915), which parses signed integers and checks only upper bounds for time components. `2026-01-01T-1:00:00Z` is accepted and stored as `2025-12-31T23:00:00Z`. Negative days also pass. This breaks the function's documented strict RFC 3339 contract and the refusal policy for malformed input. The timestamp parser is wrong.

3. **[P2] Legal XML character references are refused in numeric attributes.**  
   [parse_i64_attr](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:151), `parse_f64_attr`, and [meta_int](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:408) parse raw attribute text before XML normalization. Consequently, `id="&#49;"`, `ref="&#50;"`, `lat="&#49;"`, and `version="&#49;"` fail instead of reading as numbers. Native timestamp parsing has the same problem. This breaks OSC XML compatibility without a recorded deviation. The numeric parsing boundary is wrong; string attributes already normalize correctly.

4. **[P2] Re-emission changes accepted metadata values.**  
   [scan_owned_metadata_into](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:1170) correctly turns `timestamp="a&#10;b"` into a string containing a newline. [append_attrs](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:169) then uses `append_attr`, whose escaping leaves newline and tab literal. Reading the output normalizes those characters to spaces. This affects merge-changes and OSC tags-filter. Their documented policy explicitly accepts these fields as arbitrary normalized text for pass-through, so native invalidity does not excuse changing them. The writer is wrong.

5. **[P2] `load_all_diffs` silently skips filesystem failures.**  
   [The directory scan](/home/folk/Programs/pbfhogg/crates/lib/src/osc/parse.rs:631) uses `Path::is_file()`, which returns false when metadata lookup fails. A directory that permits listing names but denies searching its children can therefore contain numeric OSC files yet produce an empty overlay and “Loaded 0 diffs.” A metadata failure affecting one entry can silently omit just that diff. This breaks the loader's promise to load the matching files and its explicit protection against successful zero-diff mistakes. Metadata failures need propagation, separately from skipping directories.

6. **[P2] Malformed child attributes can escape validation.**  
   Attribute helpers return immediately after finding their requested value. For example, `<member type="node" ref="1" role="outer" role=broken/>` is accepted: every lookup stops before reaching the malformed trailing attribute. Tags and node references have equivalent holes. Re-emission then silently removes the offending tail. This contradicts the [malformed-role refusal contract](/home/folk/Programs/pbfhogg/CORRECTNESS.md:554). The parser needs to validate the complete child attribute list.

7. **[P2] OSC output can contain forbidden XML characters.**  
   [push_attribute_escaped](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:31) handles XML punctuation and whitespace but neither rejects nor otherwise handles XML-forbidden code points. A PBF tag containing NUL or U+0001 reaches [write_checked_tags_xml](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:368) and is written literally. The command publishes malformed XML instead of refusing an unrepresentable value. This breaks the OSC output-format contract. The output boundary is wrong.

8. **[P2] Derived OSC can be impossible for pbfhogg itself to apply.**  
   The borrowed node writers [format unchecked PBF coordinates](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:463). A new PBF node at latitude 91 produces `lat="91"`, which the OSC parser subsequently refuses. Likewise, [metadata formatting](/home/folk/Programs/pbfhogg/crates/lib/src/osc/write.rs:410) can emit a year beyond four digits, while the overlay timestamp parser requires exactly 20 bytes. These break the [documented derive/apply roundtrip](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1653). The producer should refuse values outside its consumer's representable domain before publication.

9. **[P2] `--increment-version` silently stops incrementing at `i32::MAX`.**  
   [write_delete_element](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/derive.rs:1068) uses `saturating_add(1)`. Deleting an element at version `2147483647` emits that same version despite the [flag promising a bump by one](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1648). Exhausting the representation should produce a refusal, not an unchanged version.

Additional contract discrepancies:

- **Delete-body documentation is stale.** [CORRECTNESS.md](/home/folk/Programs/pbfhogg/CORRECTNESS.md:557) says every consumer drops deleted objects' children. Merge-changes and OSC tags-filter deliberately preserve them. The document is wrong; the implementation agrees with the newer CLI contract.

- **“Nothing the element carries is dropped” overstates re-emission.** [parse_nd](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:263) retains only `ref`; [the emitter](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/emit.rs:204) consequently strips supplied `<nd lat="..." lon="...">` locations. The [preservation claim](/home/folk/Programs/pbfhogg/docs/cli/commands.md:2035) needs narrowing unless preserving those fields is intended.

- **Negative IDs are not rejected project-wide.** OSC parsing accepts them, and [sorted-overlay tests explicitly require that behavior](/home/folk/Programs/pbfhogg/crates/lib/src/osc/sorted.rs:308). The blanket description is wrong; ADR-0002's detailed enforcement table describes the actual narrower policy.

- **The apply-changes version gate does not protect against the base's version.** [Parsing starts with an empty overlay](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/rewrite.rs:344), and [an upsert replaces the base record unconditionally](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/rewrite_block.rs:164). A base v10 can therefore become OSC v9 without `--allow-version-regression`. The implementation matches its overlay-specific API documentation, but the CLI wording should explicitly limit the protection to ordering within the change stream.
