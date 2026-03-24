# Reverse Geocoding: Problem Statement and Design Direction

This document is a problem statement and design-direction memo. It frames the gap,
surveys external approaches for inspiration, identifies reusable primitives in
pbfhogg, and captures the open decisions that the implementation spec must resolve.
It is not an implementation spec.

## The gap

nidhogg is a self-hosted OSM stack serving bbox+tag queries, forward geocoding, and
vector tiles. Its forward geocoding (Tantivy full-text search over place nodes and
eventually addresses) covers the `/search` direction: text query to coordinates.

The biggest missing piece, identified as **high priority** in
[nidhogg's API gap analysis](../nidhogg/research/API_GAP_ANALYSIS.md), is **reverse
geocoding**: coordinates to structured address. This is the Nominatim `/reverse`
endpoint — given a lat/lon, return the nearest street, house number, city, postcode,
and country.

pbfhogg already generates the enriched PBFs that nidhogg ingests. The question is
whether pbfhogg should also generate a spatial index optimized for reverse geocoding
queries, or whether nidhogg should build that index itself during ingest.

## What reverse geocoding requires

A reverse geocoding query takes `(lat, lon)` and optionally a zoom level / radius,
and returns a structured address:

```json
{
  "house_number": "42",
  "road": "Vesterbrogade",
  "postcode": "1620",
  "city": "København",
  "state": "Hovedstaden",
  "country": "Danmark",
  "country_code": "dk"
}
```

Based on survey of existing geocoders (Nominatim, Photon, traccar-geocoder), the
likely required input data falls into five categories. These are provisional — the
implementation spec may revise them as the design solidifies.

1. **Address points** — nodes and building ways with `addr:housenumber` + `addr:street`.
   ~500M for planet. Each produces a (lat, lon, housenumber, street) tuple.

2. **Streets** — highway ways with a `name` tag (excluding footway, path, cycleway,
   etc.). The query finds the nearest named road segment to the input point.

3. **Address interpolation** — ways with `addr:interpolation=even|odd|all` that define
   house number ranges along a street. Given a point on the interpolation line, the
   house number is computed proportionally between the endpoint values.

4. **Administrative boundaries** — relations with `boundary=administrative` at
   admin_level 2 (country), 4 (state), 6 (county), 8 (city). Assembled into
   multipolygons for point-in-polygon containment testing.

5. **Postal code boundaries** — relations with `boundary=postal_code` or areas with
   `postal_code` tags.

A typical query algorithm would then:
1. Find the nearest address point within a search radius (~75m)
2. Find the nearest named street segment
3. Attempt address interpolation if no exact address point
4. Determine administrative hierarchy via point-in-polygon tests
5. Format the response (a serving concern, not an index concern)

## Surveyed design: traccar-geocoder

[traccar/traccar-geocoder](https://github.com/traccar/traccar-geocoder) is a
third-party open-source reverse geocoder, checked out at `traccar-geocoder/` for
reference. It is not ours and we are not building on it — it is surveyed here as one
approach to the problem that achieved sub-millisecond query latency. The design ideas
worth borrowing are noted; the specific format and architecture are not a target.

### Architecture

**Builder (C++, 916 lines):** Parses an OSM PBF with libosmium, extracts streets,
addresses, interpolation ways, and admin boundaries, computes S2 cell IDs for each
geometry, and writes 14 flat binary index files.

**Server (Rust, 812 lines):** Memory-maps all 14 files. Queries compute the S2 cell
for the input coordinates, check the cell and its 8 neighbors, binary search the
sorted cell index, iterate candidate entries, and score by distance.

### S2 cell spatial indexing

S2 is Google's hierarchical spherical geometry library. The Earth's surface is divided
into cells at 30 levels of detail. traccar-geocoder uses:

- **Level 17 (~77m cells)** for streets, addresses, and interpolation
- **Level 10 (~1.2km cells)** for admin boundaries

Each geometry (point, line segment, polygon) is mapped to the S2 cells it covers.
The cell ID is a 64-bit integer that sorts spatially — binary search over a sorted
array of cell IDs gives O(log n) spatial lookup.

### Index format

14 memory-mappable binary files with fixed-size records:

| File | Record size | Purpose |
|------|-------------|---------|
| `geo_cells.bin` | 20 bytes | Merged cell index: cell_id + offsets into 3 entry lists |
| `street_entries.bin` | variable | Per-cell lists of street way IDs |
| `addr_entries.bin` | variable | Per-cell lists of address point IDs |
| `interp_entries.bin` | variable | Per-cell lists of interpolation way IDs |
| `street_ways.bin` | 9 bytes | Way header: node_offset + node_count + name_id |
| `street_nodes.bin` | 8 bytes | (f32 lat, f32 lon) per node |
| `addr_points.bin` | 16 bytes | (f32 lat, f32 lon, housenumber_id, street_id) |
| `interp_ways.bin` | 17 bytes | Way metadata + start/end house numbers |
| `interp_nodes.bin` | 8 bytes | (f32 lat, f32 lon) per node |
| `admin_cells.bin` | 12 bytes | Cell index for admin boundaries |
| `admin_entries.bin` | variable | Per-cell polygon ID lists (high bit = interior) |
| `admin_polygons.bin` | 20 bytes | Polygon metadata: vertices, name, level, area |
| `admin_vertices.bin` | 8 bytes | (f32 lat, f32 lon) per vertex |
| `strings.bin` | variable | Deduplicated null-terminated string pool |

Total index size: ~18 GB for planet. Build time: 8-10 hours on a 192 GB machine.

### Ideas worth borrowing

1. **Merged cell index** — single binary search returns offsets for streets, addresses,
   and interpolation. Avoids three separate lookups per query.

2. **Interior polygon marking** — admin boundary cells fully inside a polygon have a
   high-bit flag, skipping the point-in-polygon ray cast. Only edge cells need
   geometric testing.

3. **Approximate spherical distance** — `dlat^2 + dlng^2 * cos^2(lat)` instead of
   haversine. No trig on the query hot path.

4. **Douglas-Peucker simplification** — admin boundary polygons simplified to max 500
   vertices, dramatically reducing storage and point-in-polygon cost.

5. **Globally deduplicated string pool** — street names repeat massively across cells.
   A single pool with u32 offsets eliminates redundancy.

### Limitations to learn from

- **No incremental updates.** Full rebuild required for each PBF update.
- **8-10 hour build time** for planet (on a 192 GB machine). The C++ builder holds
  all data in memory during the multipolygon assembly pass.
- **Hardcoded S2 levels.** No adaptive resolution for dense urban vs sparse rural.
- **f32 coordinates.** Sufficient precision, but introduces float rounding. i32
  fixed-point (decimicrodegrees) is the same size with exact representation — and
  nidhogg's disk format already uses this convention.

## Why pbfhogg

pbfhogg has several reusable primitives relevant to building a reverse geocoding
index:

| Capability needed | pbfhogg primitive |
|---|---|
| Stream PBF elements at planet scale | `ElementReader::for_each_pipelined` |
| Multipolygon assembly from relations | `extract --smart` (3-pass, relation collection) |
| Disk-backed node coordinate storage | `add-locations-to-ways` (dense/sparse/external) |
| String deduplication | `BlockBuilder` string table (FxHashMap) |
| Multi-pass PBF processing | Standard pattern across merge, extract, ALTW |
| Blob-level spatial filtering | `BlobFilter::with_node_bbox` |

None of these are drop-in solutions — each would need adaptation — but the patterns
are debugged and proven at planet scale.

### The case for pbfhogg over nidhogg

The index build is a PBF processing command: it reads raw OSM data, does multi-pass
extraction with node location resolution, and writes a derived binary format. This is
the same category of work as `add-locations-to-ways`, `extract`, and `sort`. It
belongs in the PBF toolbox, not the serving layer.

nidhogg's ingest already delegates PBF processing to pbfhogg. Adding the reverse
geocoding index build to pbfhogg keeps that boundary clean: pbfhogg produces data
files, nidhogg serves them.

The production pipeline becomes:

```
pbfhogg cat                        (add indexdata)
pbfhogg add-locations-to-ways      (enrich ways with node coords)
pbfhogg build-geocode-index        (generate reverse geocoding index)
nidhogg ingest                     (build query/tile/forward-geocode indices)
nidhogg serve                      (serve all APIs, including /reverse)
```

The index files produced by `build-geocode-index` would live in nidhogg's data
directory alongside the existing disk store files (ways.bin, relations.bin,
geocode_index/, etc.). nidhogg ingest would either invoke the command directly or
expect the files to already exist at a configured path.

### Scale considerations

Planet PBF is ~87 GB with ~8.6B elements. The traccar-geocoder builder requires 192
GB RAM because it holds all extracted data in memory. pbfhogg's existing multi-pass
commands (add-locations-to-ways, extract) are designed for memory-constrained hosts
(30 GB). The index builder must follow the same discipline.

As an example decomposition (not a proposed architecture — the spec will settle this):

- **Pass 1:** Stream nodes, collect address points and node coordinates for ways
- **Pass 2:** Stream ways, resolve street geometries and interpolation ways
- **Pass 3:** Stream relations, assemble admin boundary multipolygons
- **Pass 4:** Compute S2 cell coverage and write sorted index files

The sparse/external index strategies from `add-locations-to-ways` show the pattern
for disk-backed intermediate storage when RAM is insufficient.

## Decisions for the implementation spec

The open questions fall into three groups.

### Query behavior and data model

- **S2 cell levels.** Fixed levels (17 for streets, 10 for admin) keep the index
  format simple and the query path branchless. Adaptive levels add complexity for a
  marginal gain in sparse areas where queries are rare.
  **Direction: fixed levels.**

- **Country-specific formatting.** Display name formatting rules vary by country
  (house number before/after street, postcode placement, etc.). This is a serving
  concern — pbfhogg stores raw addr:* tags and admin hierarchy, nidhogg formats.
  **Direction: formatting lives in nidhogg.**

### Build pipeline and resource model

- **Admin boundary assembly.** Reuse `extract --smart`'s multipolygon logic rather
  than reimplementing. The code is currently coupled to the extract workflow and would
  need refactoring to be callable independently. Duplicating the logic would be a
  maintenance burden. **Direction: refactor and reuse.**

- **Polygon simplification.** Douglas-Peucker with a vertex cap (~500). The interior
  cell optimization means only edge cells need polygon geometry, so precision loss is
  contained to a narrow band where the "correct" answer is already ambiguous.
  **Direction: simplify.**

- **String pool.** Global deduplication. Street names repeat massively ("Hovedgaden"
  appears thousands of times in Denmark alone). The two-pass cost is negligible.
  **Direction: global dedup.**

- **Incremental updates.** Full rebuild is acceptable. The weekly planet refresh
  already triggers a full pipeline, and designing for incremental cell-index mutation
  adds significant complexity for a use case that doesn't exist yet.
  **Direction: full rebuild only.**

### File format and ownership boundaries

- **Index format.** Design our own rather than matching traccar-geocoder's layout. No
  realistic interop scenario. Use i32 fixed-point coords (decimicrodegrees, matching
  nidhogg's convention) instead of f32. Consider varint encoding for variable-length
  entry lists. **Direction: custom format.**

- **Query engine location.** pbfhogg should expose both the writer (the CLI command)
  and a reader library (`pub mod geocode_index { pub struct Reader ... }`). Co-locating
  reader and writer means format changes don't require coordinated updates across two
  repos. nidhogg calls `pbfhogg::geocode_index::Reader::query(lat, lon)`.
  **Direction: reader lives in pbfhogg.**
