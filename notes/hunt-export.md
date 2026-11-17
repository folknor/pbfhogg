Read-only review completed. No files changed; no builds or tests run. These findings come from tracing the code, not executed reproductions.

The shipped formats are `geojsonseq` and `geojson`. GeoParquet is explicitly [parked research](/home/folk/Programs/pbfhogg/notes/geoparquet-export.md:1), so its absence is not a defect.

1. **[P2] Corrupt tag indices silently change feature selection, geometry and properties.**  
   Export uses infallible `tags()` for ordinary nodes, dense nodes and ways in [mod.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/mod.rs:178). An out-of-range string index terminates those iterators successfully.

   If the first tag is bad, export counts a tagged element as untagged and drops it. If a later tag is bad, it emits only the prefix. A way carrying `building=yes`, then a bad tag, then `area=no` becomes a Polygon because classification never reaches the override.

   This breaks the stated refusal policy and the documented "`area=no` always disqualifies" rule. [CORRECTNESS.md](/home/folk/Programs/pbfhogg/CORRECTNESS.md:43) already acknowledges the iterator limitation. The defect here is export choosing that lossy boundary despite available `tags_checked()` APIs.

2. **[P2] Valid clockwise rings can remain clockwise through floating-point cancellation.**  
   [geometry.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/geometry.rs:39) calculates shoelace area using absolute longitude and latitude products. Consider this clockwise ring, in `(lon, lat)` order:

   ```
   (128, 64)
   (128, 64.0000001)
   (128.0000001, 64)
   (128, 64)
   ```

   These are ordinary decimicrodegree coordinates. The calculation subtracts products around 8192 to recover a twice-area around `1e-14`; that contribution rounds away. The calculated area becomes zero, so `reverse` remains false and the output stays clockwise.

   This breaks the unconditional counterclockwise exterior-ring promise in [ADR-0010](/home/folk/Programs/pbfhogg/decisions/0010-geojson-export-format-and-area-heuristic.md:14). The shared [geo::signed_area](/home/folk/Programs/pbfhogg/crates/lib/src/geo.rs:1076) already avoids this cancellation with a local origin; export duplicates the vulnerable calculation.

3. **[P2] Geometry validation accepts degenerate polygons, including ones created by output rounding.**  
   [write_way_geometry](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/geometry.rs:91) checks position count and three distinct *unrounded* positions. It never rejects zero area.

   A closed area-tagged way with coordinates `[(0,0),(1,1),(2,2),(0,0)]` passes and is counted as an exported Polygon. Separately, a valid granularity-1 triangle `[(0,0),(0,1e-9),(1e-9,0),(0,0)]` passes the distinct-position check, then [seven-decimal formatting](/home/folk/Programs/pbfhogg/crates/lib/src/coord_fmt.rs:27) collapses every output position to `[0,0]`.

   Both break the promise that invalid way geometry is skipped and counted. Geometry validation needs to judge the coordinates actually emitted.

4. **[P2] Export emits geographic coordinates outside the promised GeoJSON domain.**  
   [Point serialization](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/geometry.rs:24) performs no coordinate validation. Way validation also checks no geographic bounds.

   A tagged node at latitude 100 is exported successfully. A way containing libosmium's undefined-location values, `214.7483647` on both axes, can also be emitted and counted as valid.

   [DEVIATIONS.md](/home/folk/Programs/pbfhogg/DEVIATIONS.md:17) promises RFC 7946 coordinate order and validity. The general reader explicitly leaves coordinate values unchecked, so the missing validation belongs at export's output boundary.

5. **[P2] `--metadata` can emit timestamps that are not RFC 3339.**  
   [properties.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/properties.rs:64) passes every decoded timestamp to [format_epoch_secs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/mod.rs:853), without checking the representable calendar range.

   With default date granularity, a raw timestamp of `253402300800` produces `"10000-01-01T00:00:00Z"`. The formatter's `04` specifies minimum width, so it prints five-digit years successfully. That violates the [RFC 3339 UTC promise](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1615). The formatter documents its four-digit-year limitation; export fails to enforce it.

6. **[P2] Enabling metadata deletes tags that collide with no emitted property.**  
   [properties.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/properties.rs:49) reserves all six metadata names whenever `--metadata` is enabled, regardless of which metadata fields exist.

   For a node without metadata but carrying the OSM tag `@user=survey`, ordinary export retains the tag. Adding `--metadata` removes it, while emitting no replacement `@user`.

   This contradicts [ADR-0010's emitted-name rule](/home/folk/Programs/pbfhogg/decisions/0010-geojson-export-format-and-area-heuristic.md:10). The implementation is wrong under that rule. If reserving the entire namespace is intentional, the documented rule needs to say so.

There are also three concrete documentation contradictions:

- **Negative-ID rejection is not project-wide.** Export passes selected node and way IDs directly into [numeric `@id` properties](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/properties.rs:44), with no sign check. A tagged node with ID `-1` exports successfully. This contradicts the scope description and [DEVIATIONS.md's summary](/home/folk/Programs/pbfhogg/DEVIATIONS.md:564). ADR-0002's detailed policy describes localized enforcement, so the blanket claim overstates the implementation.
- **Export does not assemble boundary relations.** [DEVIATIONS.md](/home/folk/Programs/pbfhogg/DEVIATIONS.md:386) says it does. Export's [element dispatch](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/mod.rs:267) ignores every relation, matching the CLI documentation and deferred-work notes. The deviation rationale is wrong; missing relation support is not an implementation defect.
- **GeoJSON collections are streamed, not accumulated.** [pipeline.md](/home/folk/Programs/pbfhogg/reference/pipeline.md:300) says `geojson` is buffered into one FeatureCollection. [FeatureWriter](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/writer.rs:42) writes each feature immediately, retaining only the current feature buffer. The architectural description is wrong.

Two further observations worth retaining: timestamps silently lose their subsecond portion, and the first output error stops serialization but still allows decoding to continue through the remaining input. The latter is already recorded as deferred work. I found no additional concurrency or partial-publication defect in the reviewed export path.
