I found six defects from reading the code. No files were changed, and no builds or tests were run.

1. **[P2] Distance and snap coordinates can describe opposite half-world paths.**  
   [reader.rs:2181](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:2181) reconstructs the snap using an integer longitude delta, which always resolves an exact 180-degree separation westward. The distance calculation wraps independently converted radians and can resolve that separation eastward. The [builder explicitly acknowledges this difference](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/cover.rs:469) and covers both paths.

   For such an eastward case, a query at the segment midpoint can get approximately zero distance but a reported snap 180 degrees away. This breaks the `snap_lat_e7`/`snap_lon_e7` meaning and the function's own requirement to interpolate in the distance calculation's frame. The reader is wrong here. Projection should return the actual closest point, or both calculations should consume one shared longitude-frame representation.

2. **[P1] Detectably corrupt entry references produce successful, incomplete answers.**  
   Open deliberately leaves entry contents unchecked. During lookup, an invalid address index is [skipped](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:1809); invalid street or interpolation references become `SEGMENT_MISS`; invalid admin indices are likewise skipped.

   For example, changing a list's sole address index to `addr_point_count` preserves every validated file length and list span. Open succeeds, and querying that address returns `Ok` with no address, or a farther one. The CLI then prints that answer successfully. This breaks your input-fault refusal contract and ADR-0004: these are detectable invalid references, not legitimate absence.

   The same validation gap admits invalid polygon metadata. A positive NaN `area` passes open and is silently excluded by ranked lookup; an unsupported `admin_level` can appear in raw candidates while ranked lookup drops it. The current `QueryError` represents only bad query coordinates, so fixing this requires either complete open-time validation or an index-error path through searches and sinks.

3. **[P2] Admin cells are binary-searched without validating their ordering.**  
   [validate_admin_tree](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:589) validates list spans but never checks cell ordering or duplicate keys. Both admin query paths subsequently binary-search that table. Fine and coarse geo tables correctly check strict ordering.

   Reverse three distinct admin-cell records while preserving their offsets and lengths: open still succeeds, but binary search can miss existing cells and return no containing area. This breaks the format's stated sorted-table contract and the promise to refuse input faults rather than return silently shortened answers. The reader's validation is wrong.

4. **[P2] The empty-string sentinel check accepts a nonempty string.**  
   [reader.rs:577](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:577) claims to validate the sentinel by checking that offset zero decodes. It never checks that it decodes to empty. For example, a pool beginning with bytes `01 78` decodes offset zero as `"x"` and passes that check.

   Optional address names go through `pool_name`, so an absent street stored as offset zero can become `Some("x")`. An address with both name offsets zero can even pass the requirement that it name a street or place. This breaks [ADR-0025](/home/folk/Programs/pbfhogg/decisions/0025-geocode-strings-are-stored-whole.md:8) and the authoritative format's requirement that offset zero hold the one-byte zero sentinel. The validation must enforce that exact sentinel.

5. **[P2] `Candidates::into_result` does not apply the default admin ranking.**  
   [reader.rs:177](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:177) explicitly claims matching `query()` semantics, but it keeps the first equal-area polygon. Ranked queries instead use [area followed by polygon index](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:2279).

   With two containing same-level polygons of equal stored area, presented as indices `[1, 0]`, `query()` selects polygon zero while `candidates().into_result(&reader)` selects polygon one. This occurs with valid index data. Equal stored areas also arise through f32 rounding.

   The candidate path is wrong relative to its claimed default ranking. `AdminMatch` discards polygon identity, so sharing the ranking properly requires retaining that identity through collection.

6. **[P2] The builder wraps negative replication timestamps into enormous positive timestamps.**  
   Outside the starting files, [builder/mod.rs:436](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/mod.rs:436) casts the PBF's signed timestamp directly to `u64`. A timestamp of `-1` becomes `18446744073709551615`, is published, and is returned unchanged by `Reader::replication_timestamp()`.

   This breaks the meaning of the replication timestamp across the PBF-to-index boundary. Whether negative timestamps are supported or refused, wrapping them is incorrect. The producer is wrong: preserve the signed representation with a format change, or refuse values the current format cannot represent.

An additional API hazard: `Candidates::into_result(reader)` and `Reader::interpolate(candidate)` accept candidates from another reader or index generation without detecting the mismatch. They can combine a street and distance from generation A with a house number calculated from generation B's unrelated record at the same index. The required provenance relationship is neither encoded nor documented. Binding candidates to their originating reader would also provide a natural place to retain the polygon identity needed by finding 5.
