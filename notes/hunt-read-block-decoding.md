The strongest finding is a safe-API soundness hole in `PrimitiveBlock::from_vec`. I also found silent content replacement and several header-decoding defects. These findings come from static reading; I changed no files and ran no builds or tests.

1. **[P1] Oversized `from_vec` input can produce an invalid `&str` through safe code.**

   [from_vec](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:566) accepts an unrestricted `Vec<u8>`, while [inline_tables](/home/folk/Programs/pbfhogg/crates/lib/src/read/wire.rs:425) narrows buffer lengths and table positions to `u32`. The decompression path's size limit does not protect this public constructor.

   A concrete witness is a buffer of exactly `2^32` bytes: begin with `0a 03 0a 01 61 1a 00 1a`, then a five-byte length encoding for `2^32 - 13`, then fill that unknown field's payload with `ff`. Validation sees the valid string `"a"` and skips the unknown fields. Construction wraps the appended string-table position to zero.

   `string_table_entry(0)` subsequently interprets the original first eight protobuf bytes as an offset/length pair pointing into the unvalidated `ff` payload. It passes those bytes to [from_utf8_unchecked](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:693).

   This breaks Rust's string validity requirements and the type's documented UTF-8 invariant. The constructor must enforce the size bound needed by its representation before scanning or narrowing.

2. **[P1] A repeated string-table message silently changes element content.**

   [scan_block_fields](/home/folk/Programs/pbfhogg/crates/lib/src/read/wire.rs:319) retains only the last `PrimitiveBlock.stringtable` occurrence.

   For example, give a block these two occurrences:

   - First: `["", "name", "original"]`
   - Second: `["", "name", "replacement"]`

   An element with key index `1` and value index `2` should still resolve to `name=original` under protobuf's embedded-message merge semantics: the repeated strings concatenate. This decoder instead returns `name=replacement`. Every index remains valid, so even `tags_checked()` reports success.

   An empty final occurrence similarly erases the entire earlier table. Earlier occurrences also escape string-table validation.

   This breaks the protobuf interpretation claim and silently changes content. libosmium refuses multiple populated string tables. Merging or refusing would satisfy the project's correctness stance; keeping the last does not. The same policy appears in the independent [metadata scanner](/home/folk/Programs/pbfhogg/crates/lib/src/blob_meta/scan.rs:197), so both ends need correction.

3. **[P1] Unknown required features never prevent data decoding.**

   [Header parsing](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:137) collects arbitrary `required_features` strings. [ElementReader's header boundary](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1350) accepts the result without checking support. I found no later enforcement.

   A file declaring an unsupported required feature, followed by otherwise ordinary data, is decoded successfully. Re-encoding can then emit a header containing only the builder's known requirements, erasing the unsupported requirement.

   This breaks the meaning documented by `HeaderBlock::required_features`: these are features the parser needs to implement to interpret subsequent blocks. The [reference decoder](/home/folk/Programs/pbfhogg/research/libosmium/include/osmium/io/detail/pbf_decoder.hpp:1040) rejects unknown requirements. Data decoding should refuse them at the header boundary.

4. **[P2] Header parsing ignores wire types for recognized field numbers.**

   [WireHeaderBlock::parse](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:130) dispatches on `field` alone. It calls the expected value reader even when the actual tag declares another wire type.

   For example, append `87 02 00` to an otherwise valid header. That is field 32 with invalid wire type 7. The parser accepts it as replication timestamp `0`. Likewise, `82 02 00`, an empty length-delimited field 32, becomes a timestamp rather than being skipped as an unknown typed field.

   This violates protobuf decoding and the refusal contract by manufacturing metadata from bytes of another type. Matching `(field, wire_type)`, as the element parsers already do, fixes the interpretation.

5. **[P2] Repeated header bounding boxes overwrite fields that should survive.**

   [The bbox assignment](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:132) replaces the entire previous message. [WireHeaderBBox::parse](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:76) initializes every coordinate to zero.

   A complete bbox followed by an empty bbox occurrence therefore becomes `(0, 0, 0, 0)`. Protobuf merging leaves the complete bbox unchanged. Splitting coordinates across occurrences also loses earlier coordinates.

   The incorrect extent reaches `HeaderBlock::bbox`, inspection and output-header derivation. This is another silent embedded-message replacement, outside the recorded repeated-`Info`/`DenseInfo` refusal policy.

6. **[P1, known boundary defect] Re-encoding commands still publish silently shortened tag lists.**

   [The string lookup helper](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:2170) turns a bad index into iterator exhaustion. Both ordinary and dense tag iterators discard the bad tag and everything after it.

   That best-effort behavior is accurately recorded in `CORRECTNESS.md`. The defect crosses into consumers: [repack's writer path](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:732) consumes these infallible iterators, and its owned path does the same. A structurally accepted element with valid tags, then an out-of-range index, then more valid tags becomes a shorter, valid output element with success status.

   This breaks [repack's explicit tag round-trip promise](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1345) and the stated refusal-over-silent-wrong-answer contract. The read API documents its limitation; the semantic rewrite boundary is wrong. A fallible builder intake would let rewrites consume checked tags and propagate the existing lookup error.

7. **[P2, known downstream defect] A broken boundary-member role becomes an outer ring.**

   Following `RelMember::role()` reaches [geocode Pass 1](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass1.rs:103), where `unwrap_or("")` discards its error. [Empty roles mean outer](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass1.rs:26).

   A negative or out-of-range role index therefore selects a way as outer geometry. A corrupt inner-ring role can alter administrative containment and reverse-geocode results while the build succeeds.

   `CORRECTNESS.md` acknowledges this behavior. It still conflicts with the stated prohibition on silent wrong answers. The decoder correctly returns an error; the geocode consumer discards it.

Several contract statements are also inaccurate:

- **Negative IDs are not rejected project-wide.** The scoped scalar and delta decoders accept them, and existing tests deliberately round-trip signed extremes. [ADR-0002](/home/folk/Programs/pbfhogg/decisions/0002-negative-ids-rejected-project-wide.md) describes selective enforcement and accepted exceptions. The blanket statement in the task description and [DEVIATIONS.md](/home/folk/Programs/pbfhogg/DEVIATIONS.md:564) is wrong; it should describe that actual policy.
- **Partial DenseInfo columns are accepted.** [truncation-handling.md](/home/folk/Programs/pbfhogg/reference/truncation-handling.md:284) says they are rejected. [The validator](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1651), tests and current deviation entry intentionally accept arbitrary subsets. That paragraph is stale.
- **`Way::node_locations()` does not consult the header.** Its [documentation](/home/folk/Programs/pbfhogg/crates/lib/src/read/elements.rs:267) promises an empty iterator without `LocationsOnWays`; its implementation reads whatever coordinate arrays the way carries. The documentation should describe payload presence.
- **Repack's “every inline coordinate round-trips exactly” claim is too broad.** [Both rewrite paths](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:637) convert to decimicrodegrees. With granularity `1`, a decoded coordinate of `101` nanodegrees becomes `100`. The [exact-round-trip wording](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1366) should state the supported precision.

I found no additional concrete concurrency defect in the scoped code. The columnar arithmetic matches the element accessors, and the region grid's candidate coverage remains conservative under its exact containment checks.
