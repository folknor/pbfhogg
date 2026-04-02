# Columnar decode integration research

## Current state

Prototype shipped (commit `e0b0780`). `DenseNodeColumns` in
`src/read/columnar.rs` batch-decodes IDs, lats, lons into contiguous
arrays. `collect_matching_ids_bbox` does branchless bbox classification.
Used only in extract's node classification for bbox regions
(`src/commands/extract.rs` line 2216).

ASM inspection confirms LLVM does NOT autovectorize the bbox loop —
the `push()` side effect prevents it. Explicit AVX2 intrinsics are
the only path, but the theoretical max gain is 2.8% of total extract
time (not worth the complexity yet).

## Integration opportunities

### 1. Multi-extract node classification (high value)

Multi-extract (line 738) uses element-by-element iteration with an
O(N) inner loop per node (N = number of regions). With columnar decode:

```
columns.decode_dense_columns(block);
for i in 0..n {
    columns.collect_matching_ids_bbox(
        bbox_ints[i].min_lat, bbox_ints[i].max_lat,
        bbox_ints[i].min_lon, bbox_ints[i].max_lon,
        &mut region_ids[i],
    );
}
```

This is N passes over contiguous i32 arrays vs N × 8000 element
iterations with method calls through the `DenseNode` abstraction.
Cache-friendlier and eliminates the element dispatch overhead.

**Alternative: single-pass multi-region classification**

A new `collect_matching_ids_multi_bbox` method that tests each node
against all N regions in one pass:

```rust
pub fn collect_matching_ids_multi_bbox(
    &self,
    bboxes: &[BboxInt],
    out: &mut [Vec<i64>],  // one Vec per region
) {
    for i in 0..self.len() {
        let lat = self.lats[i];
        let lon = self.lons[i];
        for (j, bbox) in bboxes.iter().enumerate() {
            if lat >= bbox.min_lat && lat <= bbox.max_lat
               && lon >= bbox.min_lon && lon <= bbox.max_lon {
                out[j].push(self.ids[i]);
            }
        }
    }
}
```

Single pass over lat/lon arrays, N bbox tests per node. For N=5,
this is 5 comparisons per element vs 5 separate array passes.

### 2. ALTW node scan (medium value)

`add_locations_to_ways` pass 0 filters node IDs by blob-level bbox
(already done via indexdata). The actual node coordinate extraction
in pass 2 reads node IDs + lats + lons — exactly what columnar
provides. Currently uses `extract_node_tuples` which does
element-by-element parsing.

### 3. External join stage 2 (medium value)

Stage 2 of the external join scans node blobs for IDs + coordinates
using a wire-format scanner (`node_scanner.rs`). The scanner already
operates at wire level (no PrimitiveBlock), so columnar decode would
be a different approach — decode into arrays first, then filter. The
wire-format scanner may already be optimal here since it avoids the
PrimitiveBlock overhead entirely.

### 4. Geocode builder pass 2 (medium value)

Pass 2 scans nodes for coordinates, matching against admin boundary
polygons. Currently uses `elements_skip_metadata()`. Columnar would
give contiguous lat/lon arrays for point-in-polygon batch testing.
This is the prerequisite for GPU-accelerated PIP (TODO.md stretch).

### 5. Node stats (low value)

`node_stats.rs` collects coordinate statistics. Already converted to
sequential BlobReader. Columnar would give contiguous arrays for
min/max/histogram computation. Low priority — diagnostic command.

## Scratch buffer reuse for output Vec

The extract columnar path (line 2226) allocates a fresh `Vec<i64>`
per block for the bbox match results. This should use thread-local
scratch, same as the `COLUMNS` struct:

```rust
thread_local! {
    static COLUMNS: RefCell<DenseNodeColumns> = ...;
    static IDS_SCRATCH: RefCell<Vec<i64>> = RefCell::new(Vec::new());
}
```

Workers clear and reuse the Vec across blocks. Currently a minor
allocation (~64 KB per block at 8000 nodes) but adds up at planet
scale (600K blocks × 64 KB = 38 GB cumulative churn).

## Columnar for ways and relations

Ways have two relevant fields for classification: refs (node IDs)
and the way ID itself. Delta-decoded refs could be batch-decoded
into a contiguous `Vec<i64>` for set-intersection tests. But way
classification typically needs per-way grouping of refs (e.g., "does
any ref of way W match the bbox_node_ids set?"), which doesn't map
naturally to columnar layout.

Relations have members (type + ID + role_sid), which are variable-
length per relation. Not a natural columnar target.

**Conclusion:** Columnar decode is primarily valuable for dense nodes
(three fixed-width parallel arrays of equal length). Ways and relations
are better served by element-by-element iteration or wire-format
scanners.

## Relationship to SIMD

Columnar arrays are the prerequisite for SIMD. Without contiguous
arrays, SIMD can't load 8-wide vectors. The current finding is that
the bbox classify loop is only 2.8% of extract time — not worth
explicit AVX2 intrinsics for this loop alone.

SIMD becomes worthwhile when:
- Multi-region classification (N bbox tests per node, amortized)
- Polygon PIP (expensive per-node computation)
- Batch varint decode in protohoggr (different target, broader impact)

The columnar infrastructure is the right foundation — the SIMD payoff
comes from the consumers, not the bbox check itself.
