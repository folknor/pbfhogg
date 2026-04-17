# GeoJSON export design

## Goal

`pbfhogg export` - streaming PBF → GeoJSON/GeoJSONSeq. The bridge to
the GIS ecosystem. Every OSM analyst needs this; currently requires
ogr2ogr, osmium-export, or osm2pgsql + PostGIS.

## Output formats

### GeoJSONSeq (primary, line-delimited)

One GeoJSON Feature per line, newline-delimited (RFC 8142). No wrapping
FeatureCollection. Streamable - each line is independently valid JSON.

```
{"type":"Feature","geometry":{"type":"Point","coordinates":[12.5,55.6]},"properties":{"name":"Copenhagen"}}
{"type":"Feature","geometry":{"type":"LineString","coordinates":[[12.5,55.6],[12.6,55.7]]},"properties":{"highway":"primary"}}
```

Pros: streaming-friendly, works with `jq`, importable by QGIS/PostGIS.
Cons: not a valid GeoJSON document (no FeatureCollection wrapper).

### GeoJSON (secondary, wrapped)

Standard GeoJSON FeatureCollection. Requires buffering or two-pass
(write features, then close the collection). For small extracts.

```json
{"type":"FeatureCollection","features":[
  {"type":"Feature","geometry":...,"properties":...},
  ...
]}
```

### GeoPackage (future)

Binary SQLite-based format. Requires an SQLite dependency. Much more
complex but the standard for desktop GIS. Out of scope for v1.

## Element → geometry mapping

### Nodes → Point

```json
{"type":"Point","coordinates":[lon, lat]}
```

Straightforward. Requires the node to have tags (untagged nodes are
typically way members, not standalone features).

### Ways → LineString or Polygon

Ways with `area=yes` or certain tag keys (building, landuse, natural,
leisure, amenity) are polygons. All others are linestrings.

**Requires coordinates.** Two sources:
1. **ALTW-enriched PBF** - `Way::node_locations()` provides inline
   coordinates. No external lookup needed.
2. **Raw PBF + dense index** - build a node coordinate index (same as
   ALTW), then look up coordinates per way ref. Memory-intensive.

For v1, require ALTW-enriched input. This keeps the implementation
simple and avoids the memory overhead of building a coordinate index.

```json
{"type":"LineString","coordinates":[[12.5,55.6],[12.6,55.7],[12.7,55.65]]}
```

For closed ways (first ref == last ref) that are area-tagged:
```json
{"type":"Polygon","coordinates":[[[12.5,55.6],[12.6,55.7],[12.7,55.65],[12.5,55.6]]]}
```

### Relations → MultiPolygon (multipolygon/boundary types only)

Multipolygon relations need ring assembly from member ways. The code
for this exists in `src/geo.rs` (ring assembly, hole detection).

**Requires:** all member way coordinates available. With ALTW-enriched
input, member ways have inline coordinates. But the relation's member
ways must be read and assembled.

**Complexity:** ring assembly is non-trivial (ways may be in arbitrary
order, need to be joined end-to-end into rings, outer/inner rings
determined by winding order or `role` tag).

For v1, skip relations entirely or handle only simple cases. Full
multipolygon support is a significant implementation effort.

## Tag → property mapping

### Default: all tags as properties

Every tag becomes a GeoJSON property. Simple, lossless.

```json
{"properties":{"name":"Copenhagen","population":"1345562","capital":"yes"}}
```

### Filtered: expression-based

Reuse the existing tag expression parser (`src/commands/tag_expr.rs`).
Only export elements matching the expression(s).

```
pbfhogg export --expressions "highway" enriched.osm.pbf > highways.geojsonseq
```

### Key selection

`--properties name,highway,surface` - only include specified keys in
the output properties. Reduces file size significantly.

### Type filter

`--type node` / `--type way` / `--type relation` - same as other commands.

## Architecture

### v1: streaming GeoJSONSeq from ALTW-enriched PBF

```
pbfhogg export enriched.osm.pbf > output.geojsonseq
pbfhogg export --type way --expressions "highway" enriched.osm.pbf > highways.geojsonseq
pbfhogg export --bbox 12.4,55.6,12.7,55.8 enriched.osm.pbf > copenhagen.geojsonseq
```

Implementation:
1. Open input with BlobReader (sequential, no pipelined retention)
2. Iterate elements via `elements()` (need metadata for `id` property)
3. For each element:
   a. Check type filter
   b. Check tag expression filter
   c. Check bbox filter (spatial)
   d. Build geometry from coordinates
   e. Build properties from tags
   f. Write GeoJSON feature as one line to stdout/file
4. Flush and close

### JSON serialization

Use `serde_json` (already a dependency) for property serialization.
Coordinates can be written directly (avoid `serde_json::Value` overhead
for geometry - format `[lon,lat]` strings directly).

### Performance considerations

- **No parallel decode for v1** - sequential streaming is simpler and
  GeoJSON writing is I/O-bound (text output is verbose)
- **Coordinate formatting** - `format!("{:.7}", coord)` is slow per
  element. Use `ryu` or `dtoa` for fast float-to-string. Or use the
  existing `format_coord` in `elements_xml.rs` which strips trailing
  zeros.
- **String escaping** - tag values may contain JSON-special characters
  (quotes, backslashes, control chars). Must use proper JSON escaping.
  `serde_json` handles this correctly.
- **Memory** - streaming output means O(1) memory (one element at a time).
  No need to buffer the entire feature collection.

## Relationship to existing code

- `src/geo.rs` - point-in-polygon, ring assembly, simplification
- `src/commands/tag_expr.rs` - tag expression parsing and matching
- `src/commands/elements_xml.rs` - coordinate formatting, metadata access
- `src/commands/extract.rs` - spatial bbox filtering (reuse `BboxInt`)
- `Way::node_locations()` - inline coordinate access from ALTW PBFs

## Open questions

1. **ID property:** include `@id`, `@type`, `@version` as properties?
   osmium-export uses `@id`, `@type`. Useful for round-tripping.

2. **Coordinate precision:** 7 decimal places (decimicrodegree precision)
   or configurable? 7 is standard for OSM.

3. **Area detection heuristic:** hard-code the key list (building,
   landuse, etc.) or make configurable? osmium-export uses a config file.

4. **Multipolygon assembly:** defer to v2 or include a basic version?
   Basic = single-way closed polygons only (no ring assembly).

5. **Enriched input requirement:** error if input lacks LocationsOnWays,
   or fall back to building a coordinate index in memory?
