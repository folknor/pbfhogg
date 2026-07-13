# Spatial indexing in PBF format

> **STATUS 2026-07-13: speculative research, no work planned.** The
> impact assessment below caps the win at ~0.5 % for single extracts;
> the only compelling consumer is multi-extract with many regions.
> Linked from TODO.md's research / stretch list. Revisit only if
> repeated small spatial queries against one planet file become a real
> workload.

## Problem statement

Planet-scale PBF files (87 GB) contain ~430K node blobs. To find elements
in a geographic region, the current approach scans every blob header
sequentially to check its spatial bbox against the query bbox. With v2
indexdata, this avoids decompression for non-matching blobs - but still
requires reading all headers.

For a small bbox query (e.g., "all nodes in Copenhagen" from a planet
file), we read ~430K blob headers (~20 MB of header data) to find the
~50 matching blobs. A spatial index would find those 50 blobs in O(log N)
with no sequential scan.

## What would change

### Current flow (extract with indexed PBF)
```
1. Scan all blob headers sequentially         O(N) headers, ~0.5s planet
2. Build schedule of matching blobs            O(M) matching
3. Pread + decode matching blobs in parallel   O(M) blobs, decode-bound
```

### Proposed flow (spatial index)
```
1. Read spatial index                          O(1), ~1ms
2. Query R-tree for overlapping blobs          O(log N + M), ~0.1ms
3. Pread + decode matching blobs in parallel   O(M) blobs, decode-bound
```

**Savings:** step 1 drops from ~0.5s to ~1ms for planet. For Denmark
(~7K blobs), step 1 is ~5ms - negligible. The spatial index only
matters at planet scale.

But step 1 is only 0.5s out of a total extract time of ~100-200s at
planet scale. The improvement is **0.25-0.5%** - not worth the
complexity on its own.

## When spatial indexing becomes valuable

### 1. Interactive/repeated queries

If a service makes many spatial queries against the same planet PBF
(e.g., a tile server, a geocoding service, an API), the O(N) header
scan on every query adds up. A spatial index makes each query O(log N).

Current usage: pbfhogg is a batch processing tool. There's no
persistent server mode. The geocode reader (`geocode_index::Reader`)
is the closest thing - but it has its own spatial index (S2 cells).

### 2. Tiny queries on huge files

Extracting a single city from a planet PBF: the query bbox covers
< 0.001% of the file. Without a spatial index, we still scan 100%
of headers. With an index, we jump directly to the relevant blobs.

But `extract` already handles this efficiently - the header scan is
fast (~0.5s) and the real work is in decoding the matching blobs.

### 3. Way and relation spatial queries

Currently, only node blobs have spatial bboxes (v2 indexdata). Ways
and relations have no spatial information - spatial queries must
decode them to check coordinates. If way blobs also had spatial
bboxes (computed from their node coordinates during ALTW), a spatial
index over way blobs would enable direct spatial queries for ways.

However, way IDs are chronological (not geographic), so way blob
bboxes would be large - recent-ID blobs span most of the mapped world.
Estimated skip rates: ~30% for Denmark, ~45% for Copenhagen, not the
50-80% one might hope for. Geography-sorted way blobs (Hilbert curve)
would give 90%+ skip but breaks Sort.Type_then_ID ordering. See
[way-blob-bbox-speculation.md](way-blob-bbox-speculation.md) for the
full analysis.

### 4. Multi-region classification

For multi-extract with 100+ regions, checking each blob against 100
bboxes is O(N × R) where R is the region count. A spatial index on
the blob bboxes enables O(N × log R) via R-tree query for each blob,
or O(R × log N) via querying the tree for each region.

This is the same problem as the spatial index TODO item for
multi-extract regions - but applied to blobs instead of elements.

## Design options

### Option A: External sidecar index file

A separate `.spatial` file alongside the PBF:
```
planet.osm.pbf
planet.osm.pbf.spatial    # R-tree over blob offsets + bboxes
```

**Format:** packed R-tree (flatbuffers or custom binary). Stores
(blob_offset, blob_size, bbox) tuples in an R-tree layout.

**Pros:**
- No PBF format changes - any tool can generate the sidecar
- Can be regenerated independently
- Works with existing PBFs

**Cons:**
- Extra file to manage (can get out of sync)
- Not self-contained
- Tools must know to look for it

### Option B: Spatial index blob in PBF header

Extend the PBF format with an additional blob after the OsmHeader
that contains the spatial index:

```
[OsmHeader blob]
[SpatialIndex blob]  ← new
[OsmData blob 1]
[OsmData blob 2]
...
```

The SpatialIndex blob would be a custom protobuf message containing
the R-tree serialization. Readers that don't understand it would skip
it (unknown blob type).

**Pros:**
- Self-contained - one file
- Generated during `cat` (indexdata generation pass)
- Unknown blob types are safely skipped by other tools

**Cons:**
- Non-standard PBF extension (no other tool would generate or use it)
- The index must reference blob offsets that are determined at write
  time - requires a two-pass write or post-write fixup

### Option C: Embedded in existing indexdata

Extend the per-blob indexdata to include a back-reference to the
spatial index. The spatial index itself is stored in the header or
a sidecar. Each blob's indexdata stores its position in the R-tree.

This is overly complex for minimal benefit.

### Option D: Grid-based index (simpler than R-tree)

Instead of an R-tree, divide the world into a fixed grid (e.g.,
3600 × 1800 cells of 0.1°). For each cell, store a list of blob
offsets whose bbox intersects that cell.

**Pros:**
- Much simpler than R-tree (no balancing, no node splitting)
- O(1) lookup per grid cell
- Fixed-size grid header (~25 MB for 0.1° resolution)

**Cons:**
- Wasted space for ocean cells (80%+ of cells have no blobs)
- Large blobs (spanning many cells) are stored in many cell lists
- Not adaptive - poor for very small or very large query regions

**Variant: sparse grid** - only store cells that have blobs.
Use a hash map or sorted array of (cell_id, blob_list) pairs.
~50K-100K populated cells for planet, ~500 KB index size.

### Recommendation

**Option A (sidecar)** for pragmatic reasons - no PBF format changes,
works with any existing PBF, can be generated independently. Use the
sparse grid variant (Option D) for simplicity - R-tree is overkill
for ~430K blobs with ~50K-100K distinct grid cells.

**Option B (in-PBF)** is cleaner long-term but requires the two-pass
write issue to be solved. Could be added during `cat` (indexdata
generation already does a full scan).

## Implementation sketch (Option A + sparse grid)

### Grid generation (during `cat` or standalone command)

```
pbfhogg spatial-index planet.osm.pbf
```

1. Scan all blob headers (same as existing schedule building)
2. For each node blob with bbox, compute intersecting grid cells
3. Build sparse grid: HashMap<cell_id, Vec<(blob_offset, blob_size)>>
4. Serialize to `planet.osm.pbf.spatial`

### Query integration

```rust
fn load_spatial_index(pbf_path: &Path) -> Option<SpatialIndex> {
    let sidecar = pbf_path.with_extension("pbf.spatial");
    if sidecar.exists() {
        Some(SpatialIndex::open(&sidecar)?)
    } else {
        None // fall back to sequential header scan
    }
}
```

In `extract` and other spatial commands:
```rust
if let Some(ref spatial) = spatial_index {
    // O(1) grid lookup: find blobs intersecting the query bbox
    let matching_blobs = spatial.query_bbox(&query_bbox);
    // Build schedule directly from matching blobs
} else {
    // Existing sequential header scan
}
```

### Way blob spatial indexing

The spatial index becomes much more valuable when extended to way blobs.
During ALTW (add-locations-to-ways), way coordinates are resolved. A
post-ALTW `cat` can compute bboxes for way blobs and include them in
the spatial index.

For simple extract, this would eliminate the way classification pass
entirely for blobs outside the extract region - no decompression needed.

## Relationship to other work

- **v2 indexdata** already stores node blob bboxes - the spatial index
  aggregates these into a queryable structure
- **Multi-extract spatial index** (TODO.md) is for regions, not blobs -
  complementary but different
- **Way blob bboxes** depend on ALTW enrichment - the spatial index for
  ways requires LocationsOnWays data or a separate coordinate lookup
- **Geocode index** uses S2 cells for spatial lookup - the PBF spatial
  index uses a simpler grid because blob bboxes are larger than individual
  elements

## Impact assessment

| Use case | Current | With spatial index | Improvement |
|----------|---------|-------------------|-------------|
| Extract Copenhagen from planet | 0.5s scan + 100s decode | 0.001s lookup + 100s decode | 0.5% |
| 1000 small extracts from planet | 500s scan + 100Ks decode | 1s lookup + 100Ks decode | marginal |
| Multi-extract 100 regions | 0.5s scan × 100 | 0.1s total | 99.8% scan reduction |
| Way spatial skip (with way bboxes) | decode all way blobs | skip ~30% (Denmark) | ~30% decode reduction |

Way blob spatial skip is limited by chronological ID ordering - see
[way-blob-bbox-speculation.md](way-blob-bbox-speculation.md). The
transformative change would be geography-sorted way blobs (90%+ skip)
but that breaks Sort.Type_then_ID. Multi-extract benefits most
(per-region selectivity compounds across many regions).
