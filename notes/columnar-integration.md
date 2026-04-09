# Columnar decode integration research

## Current state

Prototype shipped (commit `e0b0780`). `DenseNodeColumns` in
`src/read/columnar.rs` batch-decodes IDs, lats, lons into contiguous
arrays. Two classification methods:
- `collect_matching_ids_bbox` — single-region branchless bbox check
- `collect_matching_ids_multi_bbox` — single-pass N-region bbox check
  (commit `d9a81ea`)

Used in:
- Single-extract node classification for bbox regions
- Multi-extract node classification for all-bbox regions (commit `d9a81ea`)

All four node classify paths (multi-extract columnar + polygon fallback,
single-extract columnar + polygon fallback) use thread-local scratch
Vecs for output, preserving capacity across blobs (commit `9197763`).

ASM inspection confirms LLVM does NOT autovectorize the bbox loop —
the `push()` side effect prevents it. Explicit AVX2 intrinsics are
the only path, but the theoretical max gain is 2.8% of total extract
time (not worth the complexity yet). The multi-bbox loop is a better
SIMD target: N region tests per node amortizes setup cost.

## Measured results (commit `9197763`, plantasjen)

### Multi-extract Japan 5-region node classify phase

| Metric | Baseline (element-by-element) | +columnar | +scratch reuse |
|--------|------------------------------|-----------|----------------|
| Node classify | 1081ms | 748ms (-31%) | ~748ms |
| Total | 8.1s | 7.3s | 7.3s |
| `parallel_classify_phase` time | 2.03s | 1.72s | 1.68s |

Alloc churn: 8.7 GB in `parallel_classify_phase` — unchanged by
columnar or scratch reuse because `drain(..).collect()` still
allocates fresh destination Vecs for the return channel. The
thread-local scratch preserves source Vec capacity (less allocator
pressure, Dealloc 721→655 MB) but the alloc counter tracks both.

To eliminate the 8.7 GB, `parallel_classify_phase` would need an
in-place merge interface (worker accumulates into thread-local state,
merge callback drains it directly) instead of the current
closure-returns-owned-R pattern. Bigger refactor.

## Integration opportunities

### 1. Multi-extract node classification — DONE (commit `d9a81ea`)

`collect_matching_ids_multi_bbox` tests each node against all N
bboxes in one pass over contiguous i32 lat/lon arrays. Thread-local
`DenseNodeColumns` + `Vec<Vec<i64>>` scratch reused across blobs.
Polygon regions fall back to element-by-element with thread-local
scratch.

### 2. ALTW node scan — NOT A GOOD TARGET

`add_locations_to_ways` external join uses wire-format scanners
(`node_scanner.rs`) which operate at wire level without PrimitiveBlock
construction. Columnar decode would be a different (and slower)
approach. The wire-format scanner is already optimal. Alloc profiling
confirms: `stage2_node_join` is 655 MB (0.87%), not a bottleneck.

### 3. External join stage 2 — NOT A GOOD TARGET

Same as ALTW: wire-format scanner already operates below PrimitiveBlock
level. Columnar would add overhead, not remove it.

### 4. Geocode builder pass 2 — NOT A GOOD TARGET

Pass 2 scans for nodes with specific tags (addresses, POIs) and ways
with specific tags (streets). The primary filter is tag-based, not
coordinate-based. Columnar provides coordinates, not tags. The 12.1 GB
alloc in pass 2 is `parse_and_inline_with_scratch` (protobuf parsing),
not coordinate classification.

### 5. Node stats (low value)

Diagnostic command. Not worth the complexity.

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
- Multi-region classification (N bbox tests per node, amortized) — NOW IN PLACE
- Polygon PIP (expensive per-node computation)
- Batch varint decode in protohoggr (different target, broader impact)

The columnar infrastructure is the right foundation — the SIMD payoff
comes from the consumers, not the bbox check itself.

## Next steps

The remaining alloc reduction opportunity is changing
`parallel_classify_phase` to support in-place worker accumulation
instead of returning owned results per blob. This would eliminate the
`drain(..).collect()` destination allocation (~8.7 GB at Japan scale,
~38 GB estimated at planet scale). This is a worker infrastructure
refactor, not a columnar change.
