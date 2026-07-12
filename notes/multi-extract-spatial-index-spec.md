# Multi-extract spatial index (region grid) — implementation spec

Written against `reference/technical-implementation-spec.md` (the contract this
document must satisfy). Source item: the `TODO.md` "Multi-extract → v2
improvements → Spatial index" bullet (Milestone 3), which reads:

> **Spatial index** — grid or R-tree over regions for O(1) per-element lookup
> instead of O(N). Required for 200+ regions where linear scan becomes the
> bottleneck. Simple grid (3600×1800 cells of 0.1°, precompute overlapping
> regions per cell) is sufficient.

Test-placement bricks cite `reference/testing.md` (the tier/placement contract).
There is no owning `notes/*.md` writeup for this item beyond the TODO bullet.

---

## 1. Problem and premise

Single-pass multi-extract (`src/commands/extract/multi.rs::try_extract_multi_single_pass`)
classifies every node in the input against all `N` regions. Node classification
is the only element phase driven by coordinates, and it runs two coordinate hot
loops, both O(N) per node:

1. **All-bbox columnar path** — `src/read/columnar.rs::DenseNodeColumns::collect_matching_ids_multi_bbox`
   (around lines 118-142): for each node, `for each bbox { 4 int compares }`.
   Used when every region is `Region::Bbox`.

2. **Polygon / mixed path** — `src/commands/extract/multi.rs` node-classify
   closure (around lines 295-317): `for i in 0..n { if slots[i].region.contains_decimicro(&bbox_ints[i], lat, lon) { ... } }`
   per `DenseNode` / `Node`. Used when at least one region is `Region::Polygon`.

At 200+ regions the per-element `O(N)` factor dominates the classify phase. A
grid over the regions reduces the per-node candidate set to only the regions
whose bounding box overlaps the node's grid cell, typically 0-2 regions instead
of N.

**The load-bearing correctness invariant:** output is byte-for-byte identical to
the current linear-scan implementation for every region count and every region
shape. The grid is a pure candidate-*pruning* accelerator — it narrows *which*
regions are tested per node, and every surviving candidate STILL runs the exact
`contains_decimicro` / bbox test that the linear scan runs. The grid never
decides a match; it only prunes regions that provably cannot match.

This spec converts BOTH hot loops. It does NOT touch the way-classify,
relation-classify, or any write-phase loop (see §7 — those are reference-membership
loops, not spatial, so a spatial grid cannot prune them).

---

## 2. Survey of the ground

- **Region representation** (`src/commands/extract/mod.rs`): `Region` is either
  `Bbox(Bbox)` or `Polygon { polygons, bbox }`. Every region has a bounding box
  via `Region::bbox() -> &Bbox` (for a polygon, the precomputed enclosing box of
  all exterior rings). `Region::contains_decimicro(&BboxInt, lat, lon)` is the
  per-point test: bbox regions do 4 i32 compares; polygon regions do the i32
  bbox reject then f64 ray-casting only for points inside the bbox.

- **Integer bbox** (`src/commands/extract/common.rs`): `BboxInt { min_lon,
  min_lat, max_lon, max_lat }` in decimicrodegrees (10⁻⁷°, i32). Built by
  `BboxInt::from_bbox`: **`min` via `floor`, `max` via `ceil`** — so the integer
  box is a superset of the true f64 box. `BboxInt::contains(lat, lon)` is
  `lat ∈ [min_lat,max_lat] && lon ∈ [min_lon,max_lon]`.

- **Multi-extract structure** (`multi.rs::try_extract_multi_single_pass`):
  precomputes `bbox_ints: Vec<BboxInt>` (one per slot), opens N sync writers,
  builds per-kind blob schedules, then runs a 3-phase barrier:
  1. Node classify → `bbox_node_ids: Vec<IdSet>` (the two hot loops above), via
     `parallel_classify_phase`. Chooses the columnar path iff
     `all_bbox = slots.iter().all(|s| matches!(s.region, Region::Bbox(_)))`.
  2. Way classify → `matched_way_ids`: `for i in 0..n { if w.refs().any(|r| bbox_node_ids[i].get(r)) }`.
  3. Relation classify → `matched_relation_ids`: `relation_has_matched_member`.
  Each classify phase is followed by a parallel write phase. Way/relation
  classify and all write phases test **IdSet membership**, not coordinates.

- **Classify infrastructure** (`src/scan/classify.rs::parallel_classify_phase`):
  `classify` closure is `Fn(&PrimitiveBlock, &mut S) -> R + Send + Sync`;
  `merge` is `FnMut`. The classify closure runs on worker threads and may only
  borrow shared state immutably. A read-only grid captured by shared reference
  satisfies this.

- **Config cap is parse-only, NOT a global bound** (`mod.rs::parse_extract_config`,
  verified line 374): the JSON config parser rejects `extracts_arr.len() > 500`.
  But the public entry point `extract_multi(input, slots: &[ExtractSlot], ...)`
  (`mod.rs`, verified line 504) takes an arbitrary slot slice and applies **no**
  cap; `try_extract_multi_single_pass` is reached with whatever `N` the caller
  passes. So **`N ≤ 500` cannot be assumed** by the grid — a library caller can
  drive it with any region count. This is why the coverage budget (§3.6), not the
  500 cap, is what bounds the CSR: relying on 500 would both be unsound (the API
  isn't capped) and would silently couple the grid to a limit it does not enforce.
  Enforcing 500 inside the grid would itself be a new behavior restriction (a
  config that runs today under linear scan must keep running), so the grid instead
  falls back to linear when over budget.

- **Antimeridian** (`mod.rs::bbox_from_polygons`): a polygon whose exterior ring
  crosses the antimeridian gets `bbox.min_lon = -180.0`, `bbox.max_lon = 180.0`
  (full-width box). `Region::Bbox` cannot cross the antimeridian — `parse_bbox`
  and the config bbox parser both reject `min_lon >= max_lon`.

- **Standing decisions:** this change touches no ADR in `decisions/`, no accepted
  edge case in `CORRECTNESS.md`, and no `DEVIATIONS.md` entry — because output is
  byte-for-byte identical. It establishes no new architecture policy (it is an
  internal pruning accelerator, not a format/CLI/invariant change), so it is
  **not** ADR-worthy and adds no `decisions/*` file. This is stated per the
  survey requirement to check the record.

- **Pre-existing verify discrepancy (hold constant):** `TODO.md` records that
  `brokkr verify multi-extract --regions 5` on Denmark shows strip-4 with 1 fewer
  node than sequential (41643 vs 41644), only at 5 regions where strip boundaries
  fall on exact integer longitudes. It is attributed to floating-point rounding
  in **brokkr's** bbox strip generation, not a pbfhogg bug, and predates this
  work. This spec does not touch it and must not perturb it. Because the grid
  produces identical output to the linear scan, verify results (including any
  pre-existing strip discrepancy) are unchanged whether the grid is engaged or
  not. Correctness gating here compares grid output against the linear-scan
  result on identical inputs (§6), NOT against that separate brokkr discrepancy.

- **No `notes/altw-optimization-history.md` / "Don't re-attempt" entry** covers a
  region grid; this approach is not a logged failure. (The logged multi-extract
  failure is *partial raw passthrough*, pinned in `multi.rs` — unrelated.)

- **Crates:** the grid is hand-rolled over `std::vec::Vec`. **No new crate is
  required** and none may be added (the implementer runs offline). Confirmed
  against the workspace `Cargo.toml`: no dependency is needed.

---

## 3. Target artifact: the `RegionGrid`

New module `src/read/region_grid.rs`, declared `pub(crate) mod region_grid;` in
`src/read/mod.rs`. It lives under `src/read/` (not `src/commands/extract/`)
because both call sites need it: the polygon path (in `commands/extract/multi.rs`)
and the columnar path (in `read/columnar.rs`). Placing it in `read/` lets
`read/columnar.rs` reference it without `commands` depending back into a
sibling — no dependency cycle.

### 3.1 Grid geometry (pinned)

Domain: longitude `[-180°, +180°]`, latitude `[-90°, +90°]`, expressed in
decimicrodegrees (10⁻⁷°, the coordinate unit used everywhere in multi-extract).

```
const CELL_SIZE_DMD: i64 = 1_000_000;   // 0.1° in decimicrodegrees
const LON_CELLS: usize = 3600;          // 360° / 0.1°
const LAT_CELLS: usize = 1800;          // 180° / 0.1°
const NUM_CELLS: usize = LON_CELLS * LAT_CELLS; // 6_480_000
const LON_OFFSET_DMD: i64 = 1_800_000_000; //  180° — shifts lon into [0, 3.6e9]
const LAT_OFFSET_DMD: i64 =   900_000_000; //   90° — shifts lat into [0, 1.8e9]
```

**Cell-index math (free functions, used by both rasterization and query):**

```rust
#[inline]
fn cell_lon(lon: i32) -> usize {
    // i64 arithmetic: (lon + 1.8e9) can reach 3.6e9, which overflows i32.
    let raw = (i64::from(lon) + LON_OFFSET_DMD) / CELL_SIZE_DMD;
    raw.clamp(0, (LON_CELLS - 1) as i64) as usize
}

#[inline]
fn cell_lat(lat: i32) -> usize {
    let raw = (i64::from(lat) + LAT_OFFSET_DMD) / CELL_SIZE_DMD;
    raw.clamp(0, (LAT_CELLS - 1) as i64) as usize
}

#[inline]
fn cell_of(lat: i32, lon: i32) -> usize {
    cell_lat(lat) * LON_CELLS + cell_lon(lon)
}
```

Notes, all pinned:

- **i64 arithmetic is mandatory.** `lon + 1_800_000_000` can reach `3.6e9`,
  which exceeds `i32::MAX` (`2.147e9`). Widen to `i64` before the add. `lat`
  fits either way but uses the same form for symmetry.
- **Integer floor division of a non-negative value is exact and monotonic.** The
  `+ OFFSET` shift makes the dividend non-negative for every in-domain
  coordinate, so `/` behaves as floor (no round-toward-zero surprise on
  negatives).
- **Boundary points.** `lon = +180°` (`+1.8e9`) yields raw index `3600`, clamped
  to `3599`. `lat = +90°` yields `1800`, clamped to `1799`. A point exactly on
  an internal cell edge lands in the higher-indexed cell (e.g. `lon = -179.9°`
  → raw `1` → cell 1). Poles and the antimeridian therefore map to valid cells
  with no panic.
- **Out-of-domain coordinates** (e.g. a malformed node at `lon > 180°`) are
  clamped into `[0, LON_CELLS-1]` / `[0, LAT_CELLS-1]` rather than panicking.
  Clamping is safe for correctness (§3.4).

### 3.2 Per-cell storage: CSR (pinned)

```rust
pub(crate) struct RegionGrid {
    /// Prefix-sum offsets, length NUM_CELLS + 1. Cell c's candidate region
    /// indices are region_indices[cell_starts[c] .. cell_starts[c+1]].
    cell_starts: Vec<u32>,
    /// Flattened per-cell region indices (index into the caller's region slice).
    region_indices: Vec<u32>,
}
```

Compressed-sparse-row, not `Vec<Vec<_>>`. Rationale (a resolved decision, not a
choice left open): a `Vec<Vec<u32>>` over 6.48M cells costs ~155 MB just for the
6.48M empty `Vec` headers (24 bytes each), paid even when almost every cell is
empty. CSR pays a flat `(NUM_CELLS+1) × 4 B = 25.92 MB` for `cell_starts` plus
`4 B` per `(region, cell)` pair in `region_indices`.

**Memory at typical region counts:**
- Subnational regions (~1°×1° ≈ 10×10 = 100 cells) × 200 regions →
  20 000 pairs → 80 KB. Total ≈ **26 MB**.
- Country-sized regions (~10°×10° ≈ 100×100 = 10 000 cells) × 200 regions →
  2 000 000 pairs → 8 MB. Total ≈ **34 MB**.
- Pathological (full-world bboxes, 6.48M cells each): the pair count and byte cost
  blow up fast and are NOT gated by output-buffer size — the `N × ~1.5 GB`
  per-region buffers only apply to planet-scale ID sets, so a **tiny** input with
  many full-world regions has small buffers but a huge grid. Concretely (each
  pair = 4 B in `region_indices`, plus the flat 25.92 MB `cell_starts`):
  - 16 full-world regions → 103 680 000 pairs → 414.72 MB indices → ~440.6 MB.
  - 200 full-world regions → 1 296 000 000 pairs → ~5.21 GB.
  - 500 full-world regions → 3 240 000 000 pairs → ~12.99 GB.
  - 663 full-world regions → 4 296 240 000 pairs > `u32::MAX` (4 294 967 295) —
    the prefix-sum offset itself overflows even though every per-cell count is
    tiny. Since `extract_multi` is uncapped (§2), this regime is reachable.

  A tiny PBF that succeeds under the current linear scan must not OOM or overflow
  under the grid. This is handled by the **coverage budget with linear fallback**
  (§3.6): before allocating anything, the total pair count is summed in `u64`; if
  it exceeds the pinned byte budget OR would not fit the `u32` offset type, the
  build is skipped and classification runs the existing linear scan verbatim.
  There is therefore no "known ceiling" that degrades availability — over-budget
  configs run linear, producing byte-identical output.

Region indices are `u32`. Pinned: **`u32`**. This is NOT justified by the 500
config cap (which the public API does not enforce, §2) but by the coverage budget:
the budget caps `region_indices.len()` well below `u32::MAX`, and an **independent**
`total_pairs > u32::MAX` guard in the budget check (§3.6) forces fallback before any
`u32` offset can overflow — so the `u32` choice stays sound even if the byte budget
is later raised. `u16` is rejected (`N` is uncapped, so a region index can exceed
65 535 in principle, and `u16` saves only the pair array's width at realistic
scales where the flat `cell_starts` dominates anyway).

### 3.3 Build (rasterization, pinned)

```rust
impl RegionGrid {
    /// Rasterize N region bounding boxes into the grid. Each entry is
    /// (min_lat, max_lat, min_lon, max_lon) in decimicrodegrees — exactly the
    /// BboxInt of the region (min via floor, max via ceil). Works uniformly for
    /// Bbox and Polygon regions: a polygon is rasterized by its enclosing bbox
    /// (conservative cover, see §3.4).
    /// Returns `None` when the region set is over the coverage budget (§3.6);
    /// the caller then runs the existing linear scan (byte-identical output).
    pub(crate) fn build(region_bboxes: &[(i32, i32, i32, i32)]) -> Option<RegionGrid> { ... }

    /// Candidate region indices whose bbox overlaps the cell containing (lat,lon).
    #[inline]
    pub(crate) fn candidates(&self, lat: i32, lon: i32) -> &[u32] {
        let c = cell_of(lat, lon);
        let start = self.cell_starts[c] as usize;
        let end = self.cell_starts[c + 1] as usize;
        &self.region_indices[start..end]
    }
}
```

Build is a coverage-budget check (§3.6) followed by a two-pass counting sort
(deterministic, allocation-bounded):

0. **Coverage budget (before any large allocation).** Sum, in `u64`, over all
   regions, each region's rasterized cell-rectangle area
   `(cell_lon(max_lon) - cell_lon(min_lon) + 1) * (cell_lat(max_lat) -
   cell_lat(min_lat) + 1)`. If `total_pairs * 4 > GRID_MAX_INDEX_BYTES` OR
   `total_pairs > u32::MAX as u64`, return `None` (caller runs linear). This area
   sum is O(N) and allocates nothing, so an over-budget config bails **before**
   the 25.92 MB `counts`/`cell_starts` allocation. See §3.6 for the constant.
1. **Count.** `let mut cell_starts = vec![0u32; NUM_CELLS + 1];` (this vector is
   reused in place as `cell_starts`, no second big allocation — see the peak note
   below). For each region `i`, compute its cell rectangle
   `[cell_lon(min_lon) ..= cell_lon(max_lon)] × [cell_lat(min_lat) ..=
   cell_lat(max_lat)]` and increment `cell_starts[c + 1]` for every cell `c` in
   that rectangle. (Using `c + 1` primes the prefix sum; slot 0 stays 0.)
2. **Prefix sum in place.** Fold the counts into offsets within the same vector:
   `for c in 1..=NUM_CELLS { cell_starts[c] += cell_starts[c-1]; }`. Now
   `cell_starts` is the CSR offset array (`cell_starts[0] = 0`, monotone
   non-decreasing, `cell_starts[NUM_CELLS] = total_pairs`). No separate `counts`
   vector survives this step.
3. **Scatter.** Allocate `region_indices` of length `cell_starts[NUM_CELLS]`.
   Take one `cursor: Vec<u32> = cell_starts.clone()` (the single transient copy),
   walk regions again in the same order, writing region index `i` into
   `region_indices[cursor[c]]` and `cursor[c] += 1` for each cell `c` in the
   region's rectangle. Drop `cursor` after the scatter.

**Build-transient memory peak (pinned; the RSS gate reads against it).** Steady
state retains `cell_starts` (25.92 MB) + `region_indices` (4 B/pair). During
scatter, the `cursor` clone adds another ~25.92 MB, so the transient peak is
`cell_starts` + `cursor` + `region_indices` ≈ **52 MB + 4 B/pair** (roughly
52-78 MB across the realistic 26-34 MB steady-state configs), not 26 MB. The
in-place prefix sum (step 2) is what keeps it at ~52 MB of flat overhead rather
than ~78 MB: a naive build with separate `counts`, `cell_starts`, and `cursor`
vectors alive simultaneously would hold three 25.92 MB arrays (~78 MB). Fold
counts into `cell_starts` in place so only `cell_starts` + `cursor` (two arrays)
coexist. Still negligible against the `N × ~1.5 GB` output buffers at planet
scale, but the steady-vs-peak distinction is stated because §6.7 reads RSS.

`cell_lon(min) ≤ cell_lon(max)` and `cell_lat(min) ≤ cell_lat(max)` always hold
because `min ≤ max` in every `BboxInt` and `cell_*` is monotonic — the rectangle
is never empty or inverted, and the area sum in step 0 is therefore always ≥ 1
per region. Regions are rasterized in ascending region-index order (the natural
`for i in 0..n` walk); within each cell `region_indices` ends up sorted ascending
as a side effect. **This ordering is not a correctness requirement** — each
region's matched IDs go to its own `out[j]` / `scratch[j]`, so candidate order
within a cell cannot affect any output vector (§3.4). It is retained only because
the natural loop produces it for free.

### 3.4 Why the grid never changes output (the invariant, proven)

Let `M(lat, lon)` be the cell mapping. For any region `R` with integer bbox
`[min_lat,max_lat] × [min_lon,max_lon]`:

- If a point `p = (lat, lon)` satisfies the linear-scan test for `R`, then it
  passes `R`'s `BboxInt::contains` first (bbox regions test the bbox directly;
  polygon regions bbox-reject before the ray-cast). So
  `min_lat ≤ lat ≤ max_lat` and `min_lon ≤ lon ≤ max_lon`.
- `cell_lat`/`cell_lon` are **monotonic non-decreasing** (floor of a scaled
  affine map, then identical clamp). Hence `cell_lat(min_lat) ≤ cell_lat(lat) ≤
  cell_lat(max_lat)` and likewise for lon. So `p`'s cell lies inside `R`'s
  rasterized cell rectangle, meaning **`R` is registered in `candidates(p)`**.

Therefore `candidates(p) ⊇ { R : R matches p under the linear scan }`. Every
candidate is then re-tested with the exact same `contains_decimicro` / bbox test,
so false-positive candidates are rejected and the surviving set is *exactly* the
linear-scan match set — for every point, every region, every region count. Output
is byte-for-byte identical.

- **Conservative polygon cover confirmed:** rasterizing a polygon by its bbox can
  register it in cells the polygon does not actually cover. Those are pure false
  positives: the candidate re-check runs the real polygon ray-cast
  (`contains_decimicro`) and returns false. A false-positive cell costs one extra
  containment test and never a wrong result. This is the safe default the TODO
  bullet calls for.
- **Out-of-domain / boundary safety:** clamping is applied identically during
  rasterization and query, and remains monotonic, so the superset property holds
  even for coordinates outside `[-180,180]×[-90,90]`. Concretely, for a point to
  match `R` it must have `lon ≤ R.max_lon` and `lon ≥ R.min_lon`; the same clamp
  applied to `R.max_lon`/`R.min_lon` and to the point keeps the point's cell
  inside `R`'s rectangle. Clamping never drops a real match. (`parse_bbox`,
  verified `mod.rs:64`, only enforces `min < max`; it does **not** range-check to
  ±180/±90, so a `Region::Bbox` can carry `BboxInt` bounds outside the domain and
  the out-of-domain path is genuinely reachable, not hypothetical.)
- **Below-domain / trunc-toward-zero monotonicity (one sentence, because Rust `/`
  truncates toward zero rather than flooring):** for a below-domain coordinate the
  shifted dividend `x + OFFSET` is negative, so `/ CELL_SIZE_DMD` truncates toward
  zero to `0` or a small negative, and the subsequent `clamp(0, …)` maps every
  such value to cell 0 — a constant, hence trivially monotone non-decreasing — so
  truncation cannot reorder a below-domain point relative to any in-domain point
  and the superset property survives the negative branch too.
- **Over-budget fallback is part of this invariant, not an exception to it:** when
  the coverage budget (§3.6) returns `None`, node classify runs the unmodified
  linear scan, whose output IS the reference the whole invariant is defined
  against. So the identical-output guarantee holds for **every** region count and
  shape *including* the over-budget path — trivially, because that path executes
  the linear code with no grid involved. Availability equivalence follows: any
  config that completes under linear scan today still completes (it simply does
  not build a grid when over budget).
- **Antimeridian:** an antimeridian-crossing polygon has `bbox = [-180,180]` lon,
  so it rasterizes across the full longitude range (all 3600 columns of its
  latitude band) — conservative and correct. Bbox regions cannot cross the
  antimeridian (rejected at parse). No wrap logic; the grid never wraps.

### 3.5 Thread-safety

`RegionGrid` is built once, before the node-classify phase, on the main thread.
It is then **read-only** for the remainder of the run. It is shared into the
`parallel_classify_phase` classify closure by immutable reference
(`&Option<RegionGrid>` / `&RegionGrid`). `Vec<u32>` is `Sync`, `candidates`
takes `&self`, and there is no interior mutability, so the `Fn + Send + Sync`
bound on the classify closure is satisfied with no lock. Way/relation phases do
not use the grid.

### 3.6 Coverage budget and linear fallback (pinned)

```rust
/// Hard ceiling on region_indices bytes. Over this, RegionGrid::build returns
/// None and classification runs the existing linear scan (byte-identical output).
const GRID_MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB
/// = 67_108_864 pairs (256 MiB / 4 B). Convenience form of the same budget.
const GRID_MAX_PAIRS: u64 = GRID_MAX_INDEX_BYTES / 4; // 67_108_864
```

**One mechanism, three hazards.** Before allocating the grid, `build` sums the
total `(region, cell)` pair count in `u64` (§3.3 step 0: the sum of each region's
rasterized cell-rectangle area). It falls back to linear (`build` returns `None`,
caller keeps the verbatim `for i in 0..n` scan) if EITHER:

1. `total_pairs * 4 > GRID_MAX_INDEX_BYTES` — the OOM guard. Covers the
   tiny-input-many-full-world-regions case: 16 full-world regions = 103.68M pairs
   = 414 MB > 256 MiB → fallback, so a small PBF cannot be OOM'd by the grid.
2. `total_pairs > u32::MAX as u64` — the offset-overflow guard, kept **independent**
   of the byte budget so that raising `GRID_MAX_INDEX_BYTES` in the future can
   never silently reintroduce the 663-full-world-region `u32` prefix-sum overflow.
   (At the pinned 256 MiB budget, guard 1 already fires long before guard 2; guard
   2 is the belt to guard 1's suspenders.)

**Budget rationale (pinned constant).** 256 MiB admits every realistic
multi-extract config with wide headroom: 200 country-sized regions (~100×100
cells) = 2M pairs = 8 MB; 500 subnational regions (~10×10 cells) = 50K pairs =
0.2 MB — both are two to three orders of magnitude under budget. It rejects only
the full-world/near-full-world blowups (≥16 full-world regions), which are exactly
the configs where the linear scan is cheap (few regions) or the grid would cost
hundreds of MB to gigabytes. Falling back there costs nothing measurable and
preserves availability. The budget is deliberately well below `u32::MAX * 4`
(≈16 GB) so guard 2 can only ever matter after a future budget bump. The constant
is a code ceiling, not a runtime knob — no CLI flag, no env var (§5).

**Availability equivalence and identical output.** Any config that completes under
today's linear scan completes under this change: if it is over budget, the grid is
never built and the linear scan runs unchanged; if it is within budget, §3.4
proves the grid reproduces the linear result byte-for-byte. The over-budget
fallback output is byte-identical to linear **by construction** — it *is* the
linear code path. The load-bearing identical-output invariant (§1, §3.4) therefore
explicitly includes the over-budget fallback.

---

## 4. Integration

### 4.1 Build gate and threshold (pinned)

```rust
const GRID_REGION_THRESHOLD: usize = 16;
```

In `try_extract_multi_single_pass`, immediately after `bbox_ints` is computed and
before `MULTI_NODE_CLASSIFY_START`:

```rust
let region_bboxes: Vec<(i32, i32, i32, i32)> = bbox_ints
    .iter()
    .map(|b| (b.min_lat, b.max_lat, b.min_lon, b.max_lon))
    .collect();
// build() returns None when over the coverage budget (§3.6); n < threshold also
// yields None. Either way both classify paths fall back to the verbatim linear scan.
let grid: Option<RegionGrid> = if n >= GRID_REGION_THRESHOLD {
    RegionGrid::build(&region_bboxes)
} else {
    None
};
```

Below the threshold OR over the coverage budget (§3.6), `grid` is `None` and both
classify paths keep their existing `for i in 0..n` linear scan verbatim. At/above
the threshold and within budget, both paths iterate the grid's candidate slice.
**Output is identical in all three cases** (below-threshold linear, over-budget
linear, grid) — the threshold is purely a performance knob (building and iterating
a 6.48M-cell grid for a handful of regions is net overhead; the linear scan is
cheaper at small N), and the budget is a safety valve, not an output switch. `16`
is chosen because at N≥16 the grid's typical 0-2 candidates per node clearly beat
16 linear tests, while the one-time build (~26 MB steady, ~52-78 MB transient
peak, §3.3, plus a counting sort proportional to the regions' total cell coverage)
amortizes over millions of nodes; it is validated / tunable via the bench (§6.7),
and any threshold yields identical output.

**Note on `region_bboxes` (cosmetic, R1 nit):** in the `all_bbox` branch, the
existing local `bboxes: Vec<(i32,i32,i32,i32)>` (`multi.rs:255`) is element-wise
identical to `region_bboxes`. `region_bboxes` is nevertheless built once up front
because the grid must be built **before** the `all_bbox` / else split (both
branches consume `grid`), and `bboxes` is scoped inside the `all_bbox` arm. The
duplicate tuple `Vec` (a few KB at realistic N) is accepted for clarity; an
implementer may instead build the tuple `Vec` once above the split and have the
`all_bbox` arm reuse it as `bboxes`, dropping the second allocation. Either form
is correct; the region-index `j` addresses the same region in the grid and in
`bboxes[j]` because both derive from the same `bbox_ints` in the same order.

### 4.2 All-bbox columnar path

Add a grid-aware sibling to `collect_matching_ids_multi_bbox` in
`src/read/columnar.rs`:

```rust
/// Same result as `collect_matching_ids_multi_bbox`, but only tests each node
/// against the candidate regions from `grid` for that node's cell. The bbox
/// test itself is unchanged, so output is identical to the full scan.
#[inline]
pub(crate) fn collect_matching_ids_multi_bbox_grid(
    &self,
    bboxes: &[(i32, i32, i32, i32)],
    grid: &crate::read::region_grid::RegionGrid,
    out: &mut [Vec<i64>],
) {
    let n = self.len();
    for i in 0..n {
        let lat = self.lats[i];
        let lon = self.lons[i];
        let id = self.ids[i];
        for &j in grid.candidates(lat, lon) {
            let (min_lat, max_lat, min_lon, max_lon) = bboxes[j as usize];
            let hit = (lat >= min_lat) as u8
                & (lat <= max_lat) as u8
                & (lon >= min_lon) as u8
                & (lon <= max_lon) as u8;
            if hit != 0 {
                out[j as usize].push(id);
            }
        }
    }
}
```

In `multi.rs`, the `all_bbox` classify closure dispatches on `grid`:

```rust
|block, (columns, scratch)| {
    block.decode_dense_columns(columns);
    for v in scratch.iter_mut() { v.clear(); }
    match &grid {
        Some(g) => columns.collect_matching_ids_multi_bbox_grid(&bboxes, g, scratch),
        None    => columns.collect_matching_ids_multi_bbox(&bboxes, scratch),
    }
    scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>()
}
```

`bboxes` here is the existing `Vec<(i32,i32,i32,i32)>` built from `bbox_ints` —
the same tuple layout `RegionGrid::build` consumes, so region index `j` addresses
the same region in `bboxes[j]` and in the grid. `grid` is borrowed (`&grid`); the
closure stays `Fn + Send + Sync`.

**Dense-only, inherited deliberately (R1 caveat — do NOT "fix"):** the `all_bbox`
columnar path decodes only dense-node columns (`decode_dense_columns`) and never
inspects `Element::Node` (sparse nodes). The grid sibling
`collect_matching_ids_multi_bbox_grid` preserves this exactly — it operates on the
same `DenseNodeColumns`. This matches the linear baseline, which is also dense-only
on the all-bbox path, so grid == linear holds. An implementer MUST NOT extend the
grid columnar method to also classify sparse `Element::Node` nodes: the linear
baseline does not, so doing so would make the grid path emit IDs the linear path
omits and **break the byte-for-byte invariant**. (The mixed/polygon path, §4.3,
does handle both `DenseNode` and `Node`; that is correct there because its linear
baseline also handles both.) The CLI equality fixtures (§6.6) therefore use dense
nodes for the all-bbox parity tests.

**Per-node cost trade (R1 nit — why the threshold exists):** the linear
`collect_matching_ids_multi_bbox` inner loop is branchless bitwise-AND over
contiguous tuples and autovectorizes. The grid sibling replaces it with an
indirect gather over `bboxes[j]` through the candidate slice, which does NOT
vectorize — each candidate is a dependent load. The grid wins by testing 0-2
candidates instead of N, but each individual test is more expensive than the
vectorized form. At small N the vectorized full scan beats the gather; that
crossover is exactly what `GRID_REGION_THRESHOLD` (§4.1) gates. Sell the change as
"far fewer tests, each slightly costlier," not simply "fewer tests."

### 4.3 Polygon / mixed path

The `else` (non-`all_bbox`) classify closure replaces its two `for i in 0..n`
loops with candidate iteration when the grid is present. **Pinned factoring
(not merely recommended):** the `DenseNode` and `Node` arms are unified into one
`(id, lat, lon)` extraction so both share a single candidate-iteration body. This
is pinned — not left to implementer taste — so that two implementers produce the
same artifact. The exact form:

```rust
|block, scratch| {
    for v in scratch.iter_mut() { v.clear(); }
    for element in block.elements_skip_metadata() {
        let (id, lat, lon) = match &element {
            Element::DenseNode(dn) => (dn.id(), dn.decimicro_lat(), dn.decimicro_lon()),
            Element::Node(nd)      => (nd.id(), nd.decimicro_lat(), nd.decimicro_lon()),
            _ => continue,
        };
        match &grid {
            Some(g) => {
                for &j in g.candidates(lat, lon) {
                    let i = j as usize;
                    if slots[i].region.contains_decimicro(&bbox_ints[i], lat, lon) {
                        scratch[i].push(id);
                    }
                }
            }
            None => {
                for i in 0..n {
                    if slots[i].region.contains_decimicro(&bbox_ints[i], lat, lon) {
                        scratch[i].push(id);
                    }
                }
            }
        }
    }
    scratch.iter_mut().map(std::mem::take).collect::<Vec<_>>()
}
```

`contains_decimicro` is unchanged and runs for every candidate, so bbox and
polygon regions produce exactly the linear-scan result. The unified
`(id, lat, lon)` extraction is a pure readability/structure change (both original
arms already ran the identical `for i in 0..n { contains_decimicro }` body), so it
cannot alter output; it is pinned as the single artifact both implementers write,
rather than left as an either/or that would let two implementations diverge in
structure.

`contains_decimicro` is `pub(crate)`-reachable within the extract module; no
visibility change is needed since both arms already call it.

---

## 5. Stopping rule (scope boundary)

**In scope:** the two coordinate-driven node-classify hot loops (§1). Only node
classification maps coordinates to regions; that is the only place a spatial grid
can prune.

**Explicitly out of scope, with justification:**
- **Way classify** (`for i in 0..n { if w.refs().any(|r| bbox_node_ids[i].get(r)) }`)
  and **relation classify** (`relation_has_matched_member`) test **IdSet
  membership by element reference**, not coordinates. A way's inclusion depends on
  whether any of its node refs landed in a region's `bbox_node_ids`, which has no
  spatial coordinate to bucket. A spatial grid cannot prune these; they remain
  O(N) linear and are not modified.
- **Write phases** (`multi_extract_pread_write_nodes` / `multi_extract_pread_write`
  block closures) loop `for i in 0..n { if id_set[i].get(id) }` — again IdSet
  membership, not spatial. Unchanged.
- The `all_bbox` blob-level `contained_in` computation and the raw-passthrough
  path (`NodeBlobInfo`, the pinned "do not add partial passthrough" block) are
  untouched.
- No new CLI flag, no config change, no env-var switch (the threshold is a code
  constant, not a runtime knob — per the "cleanliness is a deliverable" stance).

**Teardown blast radius:** one new file (`src/read/region_grid.rs`), one line in
`src/read/mod.rs`, one new method in `src/read/columnar.rs`, and the two closures
plus the grid-build block in `src/commands/extract/multi.rs`. Nothing else changes.

---

## 6. Verification (gates) and keep/revert

This is one coherent landing: add the `RegionGrid` module + the columnar method +
wire both call sites + the threshold gate + the coverage budget, all in a single
change. It cannot be split into an "add module" and "wire it" pair that both keep
`brokkr check` green and mean anything — an unwired module is dead code clippy will
reject. Land it whole; keep or revert on the gates below.

**The landing gate is CORRECTNESS: grid output byte-for-byte identical to linear
across region counts and shapes (including the over-budget fallback).** This needs
no large dataset — it is proven by the in-tree parity gates (§6.1-§6.2, §6.5-§6.6)
plus the small-dataset external cross-validation (§6.3). The performance
measurement (§6.7) *validates the win* but does NOT gate landing; a within-noise
bench with green parity still lands, and any parity failure reverts regardless of
speed.

Ordering within the landing does not matter for greenness because the feature is
inert until wired; the commit lands all pieces together.

### 6.1 Correctness gate — `brokkr check`

Command:

```
brokkr check
```

Runs gremlins + clippy + tier-1 tests (including the new §6.4 unit tests, the §6.5
cross-method parity units, and the new tier-1 CLI test). Must be green. This is the
primary structural gate: every internal contract the change can break (cell math,
rasterization, the superset invariant, the coverage-budget fallback, end-to-end
wiring at N≥threshold) has a tier-1 test.

### 6.2 Output-equality gate — grid vs linear, in-tree

The load-bearing invariant (byte-for-byte identical output) is proven at three
layers, all self-contained; the unit layers run under `brokkr check`, the CLI
byte-for-byte and tier-2 matrices run via `brokkr test` (§6.6):

1. **Unit-level superset proof** (§6.4 `superset_matches_linear`, `region_grid.rs`):
   assert `{ i : bbox_i.contains(p) } ⊆ candidates(p)` and that filtering
   `candidates(p)` by the exact bbox test reproduces `{ i : bbox_i.contains(p) }`
   exactly, over the pinned 8 seeds × 24 regions × 5_000 points. Deciding-layer
   equality, independent of the threshold.
2. **Real-method parity** (§6.5): `columnar_grid_parity` and
   `polygon_mixed_grid_parity` run the ACTUAL linear and grid classification
   methods on identical columns/points/regions and assert the per-region
   `Vec<Vec<i64>>` are equal including push order — the equality the byte output
   depends on, at the method layer.
3. **End-to-end byte equality** (§6.6): the tier-1 CLI test pins grid == linear-
   oracle at N ≥ threshold; the tier-2 `multi_extract_forced_linear_vs_grid_bytes`
   runs the SAME config once grid / once forced-linear and asserts the output files
   are byte-for-byte identical — the direct proof on the real write path, including
   the small-N and over-budget paths guarded by their own tier-2 cases.

### 6.3 External cross-validation gate — `brokkr verify`

Command (region count chosen to engage the grid, i.e. ≥ `GRID_REGION_THRESHOLD`):

```
brokkr verify multi-extract --dataset denmark --variant indexed --regions 20
```

Denmark is sufficient: the question is whether single-pass multi-extract output
still matches the sequential/osmium reference when the grid is engaged, which is a
correctness question a small real dataset answers. Zero diffs is the bar, save the
documented parity exceptions in `reference/osmium-parity.md`.

**Hold the pre-existing discrepancy constant:** `TODO.md` notes a brokkr
strip-generation FP discrepancy that appears specifically at
`--regions 5` on Denmark (strip boundaries on exact integer longitudes). That is
orthogonal to the grid and predates it. Use `--regions 20` (not 5) for the grid
gate so the run engages the grid without landing on the known-fragile 5-strip
boundary; any residual strip-boundary FP diff, if it recurs at some region count,
is the same brokkr-side issue and is not caused by this change (the grid produces
identical output to the linear scan — verify the two agree via §6.2, do not chase
the brokkr FP diff here).

### 6.4 New unit tests — `src/read/region_grid.rs` `#[cfg(test)] mod tests`

Tier-1 inline unit tests (die with the module on rewrite — the intended coupling
per `reference/testing.md`). Every parameter below is **pinned** (exact
coordinates, exact seeds, exact case counts) so two implementers write the same
tests. The in-test PRNG is a fixed **xorshift64** (`x ^= x<<13; x ^= x>>7;
x ^= x<<17`), no `rand` crate; seeds are stated per test.

- `cell_index_math` — deterministic boundary/edge fixtures, exact expected cells:
  - origin `(lat=0, lon=0)` → `cell_lat(0)*3600 + cell_lon(0) = 900*3600 + 1800 =
    3_241_800`.
  - a mid-cell point `(lat=123_456, lon=654_321)` → assert against the hand-computed
    `cell_of`.
  - exact west/south domain corner `(-900_000_000, -1_800_000_000)` → cell 0.
  - exact east/north domain corner `(+900_000_000, +1_800_000_000)` → clamps to
    `(LAT_CELLS-1)*3600 + (LON_CELLS-1) = 1799*3600 + 3599 = 6_479_999`.
  - **exact 0.1° cell-edge point** `lon = -1_799_000_000` (i.e. -179.9°, an exact
    cell boundary) → raw index 1 → column 1 (lands in the higher-indexed cell);
    pair with `lat = -899_000_000` → row 1.
  - **lon exactly on a region max_lon:** a region with `max_lon = 500_000_000` and
    a point at `lon = 500_000_000` → the point's column equals
    `cell_lon(500_000_000)`, which must be ≤ the region's rasterized max column
    (regression pin for the integer-longitude-boundary hazard the pre-existing
    brokkr strip discrepancy lives at).
- `out_of_domain_clamps`: `lon = 2_000_000_000` (> 180°) and `lat = 1_000_000_000`
  (> 90°) clamp to the last column/row without panic; `lon = -2_000_000_000`,
  `lat = -1_000_000_000` (below domain, exercising the trunc-toward-zero branch)
  clamp to cell 0 without panic.
- `rasterize_exact_cell_set`: a single region `bbox = (min_lat=10_000_000,
  max_lat=30_000_000, min_lon=-20_000_000, max_lon=40_000_000)` is registered in
  exactly the inclusive cell rectangle `[cell_lon(min_lon)..=cell_lon(max_lon)] ×
  [cell_lat(min_lat)..=cell_lat(max_lat)]` and in no other cell (walk all
  `NUM_CELLS` cells, assert the membership set equals the computed rectangle).
- `rasterize_full_width_antimeridian`: a region with `min_lon=-1_800_000_000,
  max_lon=+1_800_000_000, min_lat=0, max_lat=0` covers all `LON_CELLS = 3600`
  columns of its single latitude band and no other row.
- `superset_matches_linear` (the invariant, unit level): **exactly 8** seeded
  region sets from xorshift64 seeds `1, 2, 3, 5, 8, 13, 21, 34`, each with
  **exactly 24 regions** (> threshold) drawn from a fixed mix — indices 0-5 small
  (~1° boxes), 6-11 large (~30° boxes), 12-17 disjoint strips, 18-20 mutually
  overlapping, 21 pole-spanning (`max_lat = 900_000_000`), 22 full-width-lon
  (antimeridian), 23 out-of-domain (`max_lon = 2_000_000_000`). For each set draw
  **exactly 5_000** seeded points (including some out-of-domain by construction),
  and assert `{ i : bbox_i.contains(p) }` (brute-force linear) `==`
  `{ i ∈ candidates(p) : bbox_i.contains(p) }` (grid-pruned) for every point.
  Also assert the raw superset `{ i : bbox_i.contains(p) } ⊆ set(candidates(p))`.
  This is the equivalence proof at the deciding layer, independent of the threshold.
- `coverage_budget_falls_back`: construct `region_bboxes` of **exactly 16
  full-world** boxes (`min_lat=-900_000_000, max_lat=900_000_000,
  min_lon=-1_800_000_000, max_lon=1_800_000_000`) → 103_680_000 pairs → over the
  256 MiB budget → assert `RegionGrid::build(&boxes)` returns `None` (no
  allocation, no panic). Assert a **within-budget** control (16 small ~1° boxes)
  returns `Some`. Assert the `u32`-overflow guard independently with a synthetic
  area sum > `u32::MAX` (e.g. via a helper that takes a precomputed pair total, or
  document that 663 full-world boxes exceed both guards and the byte guard fires
  first) — the test's job is to pin that over-budget → `None`, never a panic or OOM.

### 6.5 Cross-method parity tests (unit) — prove grid == linear on the real methods

The §6.4 `superset_matches_linear` test proves the pruning invariant over abstract
bbox tests. These additional **unit** tests prove the two ACTUAL classification
methods (not a re-derived bbox test) agree, which is what the byte-for-byte
invariant ultimately rests on:

- `columnar_grid_parity` (in `src/read/columnar.rs` `#[cfg(test)] mod tests`):
  build one `DenseNodeColumns` populated with **exactly 2_000** xorshift64 points
  (seed `0x9E3779B97F4A7C15`, some out-of-domain), and one region list of
  **exactly 24** bboxes (same fixture family as §6.4). Run
  `collect_matching_ids_multi_bbox(&bboxes, &mut lin)` and
  `collect_matching_ids_multi_bbox_grid(&bboxes, &grid, &mut gr)` on the SAME
  columns into two `Vec<Vec<i64>>`, and assert `lin == gr` **element-by-element
  including order within each inner Vec**. This pins that the grid columnar sibling
  reproduces the linear columnar method byte-for-byte, including push order.
- `polygon_mixed_grid_parity` (in `src/commands/extract/mod.rs` or a test module
  that can reach `contains_decimicro` and `Region`): construct **exactly 24**
  regions mixing `Region::Bbox` and `Region::Polygon` (including a polygon with a
  hole, an antimeridian-crossing polygon → full-width bbox, and a pole-adjacent
  polygon), and **exactly 2_000** seeded points. Run the polygon/mixed classify
  body two ways on the identical points/regions — once linear (`for i in 0..n`),
  once grid-pruned (`for j in candidates`) — each producing per-region `Vec<i64>`,
  and assert equality including order. Uses the real `contains_decimicro`, so it
  proves conservative-cover false positives are rejected identically.

### 6.6 New CLI end-to-end tests — `tests/cli_extract.rs`

Follows the existing multi-extract test convention (`run_extract_multi`,
`write_test_pbf_sorted`, `read_all_elements`, `node_ids`/`way_ids`/`relation_ids`,
`CliInvoker`). Per-command CLI split (`reference/testing.md`). All fixtures use
**dense** nodes (the all-bbox path is dense-only, §4.2). Pinned parameters:

- **Root/`tier1`** — one small contract: `multi_extract_grid_matches_linear_bbox`.
  Build a sorted dense PBF with **exactly 500 nodes** on a deterministic grid of
  coordinates (e.g. lon/lat stepped in 0.05° increments so points land both inside
  cells and on 0.1° edges), plus 10 ways and 3 relations referencing them. Define
  **exactly 20 bbox regions** (≥ threshold): 12 disjoint tiles, 4 mutually
  overlapping, 2 sharing an exact integer-longitude edge, 2 touching the domain
  extremes. Run multi-extract; assert each region's `node_ids`, `way_ids`,
  `relation_ids` equal the sets an in-test linear membership function computes over
  the same bbox list. Exercises the grid path (N ≥ threshold) end-to-end and pins
  grid == linear.
- **`tier2`** (module `mod tier2`, `#[ignore]`d out of tier 1):
  - `multi_extract_grid_polygon_regions`: **exactly 20 polygon** regions —
    including one antimeridian-crossing polygon (its exterior ring crosses ±180°,
    so `bbox_from_polygons` gives it a full-width bbox) and one pole-adjacent
    polygon (`max_lat` near +90°). These are NOT optional: the fixture builder
    constructs the coordinates directly, so "if the builder supports" does not
    apply. Assert the grid path reproduces the linear polygon result exactly.
  - `multi_extract_grid_mixed_bbox_polygon`: **exactly 20 regions**, 10 bbox +
    10 polygon (forces the non-`all_bbox` path with the grid engaged).
  - `multi_extract_below_threshold_unchanged`: **exactly 8 regions**
    (< `GRID_REGION_THRESHOLD`) produce output equal to the linear oracle over the
    same regions (guards the small-N path, grid == `None`).
  - `multi_extract_forced_linear_vs_grid_bytes`: the **byte-for-byte** end-to-end
    test. Under the `test-hooks` feature, a process-global
    `region_grid::FORCE_LINEAR: AtomicBool` makes `RegionGrid::build` return `None`
    when set. On ONE fixed config of **exactly 20 regions** (mixed bbox+polygon,
    the same fixture as `multi_extract_grid_mixed_bbox_polygon`), run
    `extract_multi` twice to distinct output dirs — once with the flag clear (grid
    built) and once set (forced linear) — then assert every per-region output file
    is **byte-for-byte identical** (`std::fs::read` equality, not ID-set equality)
    between the two runs. This is the direct proof of the load-bearing invariant on
    the real write path. Because the toggle is a process-global static
    (per-`testing.md` static-atomic hook shape), this test lives in `mod serial`
    (or its own `tests/fault_extract_grid.rs` binary) and runs single-threaded so a
    concurrent sibling cannot observe the toggle. Also run a second pair with
    **exactly 20 all-bbox** regions to cover the columnar path's forced comparison.

  (`tier3`/`platform` need no additions — no slow fixture and no I/O feature is
  introduced. The forced-compare static toggle is the only new serial state.)

**Exact command to run the tier-2 tests** (they are `#[ignore]`d, so `brokkr
check` skips them; run each by exact name with `brokkr test`, which passes
`--include-ignored` single-threaded — see AGENTS.md):

```
brokkr test multi_extract_grid_polygon_regions --sweep all --timeout 60
brokkr test multi_extract_grid_mixed_bbox_polygon --sweep all --timeout 60
brokkr test multi_extract_below_threshold_unchanged --sweep all --timeout 60
brokkr test multi_extract_forced_linear_vs_grid_bytes --sweep all --timeout 60
```

### 6.7 Performance measurement (validates the win — NOT the landing gate)

**The landing gate is CORRECTNESS, not speed.** This feature lands or reverts on
the byte-for-byte parity gates (§6.1 `brokkr check`, §6.2 output-equality, §6.5
cross-method parity, §6.6 CLI incl. the forced byte-for-byte comparison) and the
§6.3 external cross-validation — none of which needs a large dataset. The
performance measurement below **validates that the win is real**; it does not gate
landing. If the parity gates are green and `brokkr check` passes, the change is
correct and lands even if a given bench run is within noise; if any parity gate
shows an output difference, it reverts regardless of speed.

The change is nonetheless on a measured path (node classify), so it owes a
measurement conformant with `reference/technical-implementation-spec.md`
(§10: pinned `--variant`, host + commit-hash baseline, stated noise bound):

- **Host:** `plantasjen` (the reference host; confirm with `brokkr env`).
- **Variant:** `indexed` (pinned — never read a verdict off a mixed variant).
- **Dataset/benchmark:** `brokkr multi-extract --dataset japan --regions N --bench`.
  Brokkr generates the N regions as **longitude strips**, so this is the benchmark
  brokkr actually supports — do NOT invent a 200-region dataset that does not
  exist. Japan is the smallest dataset whose node count makes the per-element
  `O(N)` classify factor visible.
- **Baseline (pre-grid, linear scan):** commit `3b73845` (current HEAD before this
  change lands), built and benched in brokkr's own worktree via `--commit`. This
  is the copy-pasteable ref the keep verdict is read against.
- **Noise bound:** `--bench 3` best-of; a delta under ~5% at these sub-10-s japan
  walls sits inside bench-to-bench variance (per `performance.md`'s stated band)
  and is reported "within noise," not as a win. A real win must clear that bound.

Run the after/before pair across region counts that bracket the threshold and reach
the intended many-region regime — **below** threshold (grid off), **just above**
(grid on, threshold crossover), and a **higher** count in the target regime:

```
brokkr multi-extract --dataset japan --regions 8   --variant indexed --bench
brokkr multi-extract --dataset japan --regions 24  --variant indexed --bench
brokkr multi-extract --dataset japan --regions 128 --variant indexed --bench
brokkr multi-extract --dataset japan --regions 8   --variant indexed --commit 3b73845 --bench
brokkr multi-extract --dataset japan --regions 24  --variant indexed --commit 3b73845 --bench
brokkr multi-extract --dataset japan --regions 128 --variant indexed --commit 3b73845 --bench
```

(Group all HEAD cells then all `--commit` cells, ending on a HEAD build, per the
AGENTS.md `--commit` build-thrash rule.) `--regions 8` is below
`GRID_REGION_THRESHOLD = 16` and must be statistically flat between HEAD and
baseline (grid never builds — a regression there would mean the small-N path was
disturbed). `--regions 24` is just above the threshold and shows the crossover;
`--regions 128` shows the many-region win the feature targets. Read peak anon RSS
from `brokkr sidecar <UUID> --human` and confirm the grid's ~26-34 MB steady /
~52-78 MB transient build peak (§3.3) does not move the memory headline against the
`N × ~1.5 GB` output buffers. Commit first, then bench (never bench uncommitted
code). Record the before/after wall + RSS, host + commit hash, in
`reference/performance.md` (new current baseline), moving the superseded number and
the arc narrative to `reference/performance-history.md`.

Expected: the many-region classify phase is faster with no output change and no RSS
regression. The build cost (a counting sort proportional to the regions' total cell
coverage, milliseconds against a multi-second classify) should not dominate at
`--regions 24+`; if it does at the crossover, retune `GRID_REGION_THRESHOLD` (a
pure perf knob — any threshold yields identical output, §4.1). No baseline numbers
are asserted in this spec; they are captured at landing time on `plantasjen`,
because inventing a host/commit-anchored figure here would violate the
measurement-record rules.

---

## 7. Summary of resolved decisions

| Decision | Resolution |
|---|---|
| Grid geometry | 3600×1800 cells of 0.1° (1e6 dmd) over ±180°/±90°; `cell_of` via i64 offset-shift + floor-div + clamp to `[0,3599]/[0,1799]`; index `lat_cell*3600 + lon_cell`. |
| Coordinate mapping arithmetic | i64 (avoids i32 overflow at `lon + 1.8e9`); floor division of shifted non-negative value; monotonic. |
| Boundary / out-of-domain | Clamp to last cell (poles, +180°, malformed coords); identical clamp on rasterize and query keeps the superset property. |
| Rasterization | Each region rasterized by its `BboxInt` (min floor / max ceil) inclusive cell rectangle; uniform for bbox and polygon regions. |
| Polygon cover | Conservative bbox cover; false-positive cells re-checked by `contains_decimicro`, never wrong. Confirmed as the safe default. |
| Per-cell storage | CSR (`cell_starts: Vec<u32>` len 6.48M+1, `region_indices: Vec<u32>`); ~26 MB steady + 4 B/pair; ~52-78 MB transient build peak (in-place prefix sum + one `cursor` clone). Rejects `Vec<Vec<_>>` (155 MB header overhead). |
| Region index width | `u32`; bounded by the coverage budget (§3.6), NOT by the 500 config cap (public `extract_multi` is uncapped). |
| Coverage budget + fallback | `GRID_MAX_INDEX_BYTES = 256 MiB` (= 67_108_864 pairs). Sum pair count in u64 before allocating; if over budget OR `> u32::MAX` pairs, `build` returns `None` and classify runs linear. Covers OOM (tiny input + full-world regions), u32 prefix-sum overflow, and availability equivalence with one mechanism. |
| Hot loops converted | BOTH: columnar `collect_matching_ids_multi_bbox` (new `_grid` sibling, dense-only — do NOT extend to sparse) and the polygon/mixed `for i in 0..n` closure (unified `(id,lat,lon)` arm, pinned). |
| Threshold | `GRID_REGION_THRESHOLD = 16`; below it OR over budget, verbatim linear scan; identical output in all cases; pure perf knob. |
| Thread-safety | Built once before classify; read-only; shared by `&`; `Fn + Send + Sync` satisfied, no lock. |
| Out of scope | Way/relation classify and all write phases (IdSet-membership, not spatial); no CLI/config/env change. |
| Identical-output invariant | `candidates(p) ⊇ linear match set` (monotone mapping incl. trunc-toward-zero below-domain branch); every candidate re-tested exactly → byte-for-byte identical for every N and shape, INCLUDING the over-budget linear-fallback path. |
| Landing gate | Correctness (byte-for-byte parity gates §6.1-§6.6 + external §6.3), not speed. §6.7 perf validates the win but does not gate landing. |
| New crates | None; hand-rolled over `std::vec::Vec`. |

---

## 8. Review reconciliation (R1 opus, R2 codex)

Both reviews confirmed the CORE design correct — cell-index math, i64 overflow
guard, superset/monotonicity invariant, both hot-loop integrations, thread-safety,
no new crate — and none of that was re-litigated. This section records how each
review finding was resolved after validating it against the code
(`extract/mod.rs`, `extract/multi.rs`, `extract/common.rs`, `read/columnar.rs`,
`scan/classify.rs`).

### Folded

- **CSR unbounded size / OOM (R2 High 1, R1 Minor).** Verified: `extract_multi`
  (`mod.rs:504`) is uncapped; a tiny input with full-world regions can build a
  multi-GB grid. Resolved toward R2 with the **coverage budget + linear fallback**
  (§3.6): pinned `GRID_MAX_INDEX_BYTES = 256 MiB`, u64 pre-allocation pair sum,
  fallback to linear when over budget. One mechanism covers OOM, availability
  equivalence, and (with the independent guard below) overflow.
- **`N ≤ 500` false; u32 offset overflow (R2 High 2, R1 Minor).** Verified: the
  500 cap is parse-only (`mod.rs:374`); the public API is uncapped; 663 full-world
  regions overflow the u32 prefix sum. Folded: §2 corrects the claim; §3.6 adds an
  **independent** `total_pairs > u32::MAX` guard so a future budget bump cannot
  reintroduce the overflow; §3.2 re-pins `u32` on the budget, not the cap.
- **Tests do not prove byte-for-byte (R2 High 3, R1 Minor boundary/dense gaps).**
  Folded into §6.4-§6.6: exact `DenseNodeColumns` parity (`columnar_grid_parity`,
  compares `Vec<Vec<i64>>` incl. order), exact polygon/mixed parity on identical
  points via real `contains_decimicro`, a forced same-config linear-vs-grid
  **byte-for-byte** end-to-end test (`multi_extract_forced_linear_vs_grid_bytes`),
  a `coverage_budget_falls_back` test, deterministic boundary/edge fixtures (lon ==
  region max_lon, exact 0.1° cell edge, both domain extremes, out-of-domain,
  overlaps, holes, antimeridian, pole-adjacent), exact seeds/counts (xorshift64,
  8×24×5_000; 20-region CLI fixtures), and the exact `brokkr test` tier-2 commands.
- **Performance/keep-revert contract (R2 Medium 4).** §6.7 rewritten conformant
  with `reference/technical-implementation-spec.md`: pinned `--variant indexed`,
  copy-pasteable baseline `--commit 3b73845`, host `plantasjen`, ~5% noise bound,
  `brokkr multi-extract --dataset japan --regions N --bench` (the strip benchmark
  brokkr actually supports) at 8/24/128 (below / crossover / target regime). States
  plainly that perf VALIDATES the win and does NOT gate landing; correctness parity
  + `brokkr check` is the landing gate. No 200-region dataset invented.
- **Transient build memory (R1 Moderate, R2 Medium 5).** §3.3 states the ~52-78 MB
  transient peak vs ~26 MB steady, and pins the in-place prefix sum (fold counts
  into `cell_starts`) + single `cursor` clone to hold it at ~52 MB flat overhead.
- **Trunc-toward-zero monotonicity (R1 Minor, R2 "correct as written").** §3.4 adds
  the one-sentence below-domain argument (negative dividend truncates then clamps
  to constant cell 0 → monotone).
- **Dense-only all-bbox caveat (R1 Minor).** §4.2 pins that the grid columnar
  sibling must NOT be "fixed" to classify sparse `Element::Node` — the linear
  baseline is dense-only, so doing so would diverge.
- **Unpinned choices (R2 Medium 6).** §4.3 pins the unified mixed-classifier arm as
  the single artifact (was "recommended"); §6.4-§6.6 pin all test parameters
  (seeds, counts, fixtures, PRNG).
- **R1 nits.** §3.3 drops the overstated ascending-order rationale (candidate order
  cannot affect output); §4.2 names the lost-vectorization per-node cost as the
  reason the threshold exists; §4.1 notes the redundant `region_bboxes` allocation
  and the optional single-build reuse.

### Rejected / not adopted

- **Enforce the 500 cap inside the grid (R2 alternative to the budget).** Rejected:
  R2 itself flags this as a new behavior restriction. A config that runs under
  linear scan today must keep running; capping would break availability. The budget
  + fallback (which R2 endorses as the primary remedy) preserves it. Only the
  budget mechanism was folded.
- **"Known ceiling, no special handling" for the pathological case (original
  spec).** Rejected in favor of the fallback — the ceiling degraded availability
  for reachable (uncapped-API) configs.
- **Nothing else was rejected on merit.** The remaining review content was either
  the confirmed-correct core (not re-litigated per the orchestrator's instruction)
  or already present in the spec.
