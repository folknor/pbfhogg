# G5: interpolation endpoint resolver CSR - implementation spec

## Standing references

- Contract this spec is written against:
  `reference/technical-implementation-spec.md`.
- Source item this spec is spawned from:
  `notes/geocode-build-opportunities.md`, sections "G5. Contained local
  change: interpolation endpoint resolver", "Pass 2 interp resolve - local
  follow-up, not the main rewrite", and "Interpolation endpoint CSR - data
  structure for G5".
- Test placement/tier contract (this spec adds tests):
  `reference/testing.md`.
- Measurement record: `reference/performance.md`
  (build-geocode-index section, planet phase breakdown), plus
  `reference/performance-history.md` and `.brokkr/results.db`.

## Problem

`resolve_interpolation_endpoints_mmap`
(`src/geocode_index/builder/interp.rs`) is the only sequential-by-design
phase left in the geocode build. Planet phase breakdown (`82db8ed`, UUID
`b4b25c05`, plantasjen, `--bench 1`, `reference/performance.md`):
**30.6 s / 7 % of planet wall, 1.0 avg cores, 9.04 GB peak anon**.

The phase has two parts today, both sequential:

1. **Spatial-index build** - one `FxHashMap<u64, Vec<u32>>` mapping S2
   cell IDs (at `street_level`, default 17) to address-point indices. Built
   by scanning every `AddrPoint` in the mmap'd `addr_points.bin`. At planet
   this is ~1 GB heap: ~150 M address points across ~10 M distinct S2
   cells, each cell an individually allocated `Vec<u32>`.
2. **Endpoint resolution** - for each interpolation way, read its first and
   last node coordinate, project each to its `street_level` cell, look up
   that cell plus its 8 `all_neighbors`, and pick the nearest address point
   with a matching `street_offset` to supply the start/end house number.

## Failure history this spec must respect

A naive parallelization was attempted and reverted (commit `363c579` ->
reverted `7cb807b`, results invalidated via `brokkr invalidate --commit
363c579`). It measured **Europe 183.4 s -> 199.1 s, +15.7 s net
regression**:

- **Rayon fold+reduce for the index build regressed** (Europe INDEX_BUILD
  ~12 s sequential -> 23.7 s @ 10.3 cores). Per-worker partial
  `FxHashMap<u64, Vec<u32>>` merged at the end costs ~20 M lookup+push
  operations (one per addr point) that sequential insertion never pays.
  **This spec does not re-attempt fold+reduce.** It replaces the map with a
  sort-based CSR, whose merge step is a linear group-by over already-sorted
  data, not a hashmap union.
- **`par_iter_mut()` over endpoints did not help at Europe scale** (3.6 s @
  1.0 cores; interp way count ~1-2 k, chunk size defaults to 1, overhead
  dwarfs work). Planet has ~50-100 k interp ways and should amortize, but
  only once the index-build regression is gone. This spec parallelizes
  endpoint resolution too, but the keep/revert verdict on that half is read
  at planet, not Europe (see gates).

The reverted attempt added sub-markers
`GEOCODE_INTERP_RESOLVE_INDEX_{START,END}` and
`GEOCODE_INTERP_RESOLVE_ENDPOINTS_{START,END}`. This spec re-introduces the
same marker names so the two halves are measured independently and the
planet baseline row remains name-comparable.

## Survey of the ground

### Current call site

`src/geocode_index/builder/mod.rs`, Pass 2, between markers
`GEOCODE_PASS2_INTERP_RESOLVE_START` and `..._END`:

```rust
let resolved = interp::resolve_interpolation_endpoints_mmap(
    &mut interp_ways,      // &mut [SlimInterpWay]
    &addr_points_mmap,     // &[u8], addr_points.bin
    &interp_nodes_mmap,    // &[u8], interp_nodes.bin
    &strings,              // &StringPool
    config.street_level,   // u8, default 17
);
```

The function mutates each `SlimInterpWay`'s `start_number` / `end_number`
in place and returns the count resolved. The returned count is only used
for an `eprintln!`. Immediately after, `interp_ways` is serialized to
`interp_ways.bin` (`InterpWay` records). Nothing else reads the transient
index; it is dropped at function return.

### Types (all internal to `src/geocode_index/builder/`, freely rewritable)

- `SlimInterpWay` (`pass2.rs`): `street_offset: u32`, `node_file_offset:
  u64`, `node_count: u16`, `start_number: u32`, `end_number: u32`,
  `interpolation_type` (plus fields). `start_number == 0 && end_number ==
  0` is the "unresolved" sentinel (documented KNOWN LIMITATION in `pass2.rs`
  and `interp.rs`; leave the sentinel semantics untouched - this spec does
  not change the format or bump `FORMAT_VERSION`).
- `AddrPoint` (`format.rs`, `ADDR_POINT_SIZE = 20`): `lat_e7: i32`, `lon_e7:
  i32`, `housenumber_offset: u32`, `street_offset: u32`, `postcode_offset:
  u32`. Read by index via `read_addr_point_mmap`.
- `NodeCoord` (`format.rs`, `NODE_COORD_SIZE = 8`): read by byte offset via
  `read_node_at`.
- `StringPool` / `read_string_from_pool` (`strings.rs`): house-number
  string lookup.

### Standing decisions checked

No ADR (`decisions/*`), `CORRECTNESS.md`, or `DEVIATIONS.md` entry governs
interpolation endpoint resolution or the geocode index internals. This is a
private-internal rewrite of one function plus its transient data structure;
no on-disk format, no CLI surface, no wire encoding changes. `FORMAT_VERSION`
is untouched. No new ADR is warranted - this establishes no policy, it is a
contained data-structure swap. If planet measurement (below) shows the
endpoint-resolution parallelism nets negative and is reverted, that verdict
is recorded in `reference/performance-history.md`, not an ADR.

### Scope boundary (stopping rule)

In scope: the body of `resolve_interpolation_endpoints_mmap` and its private
helper `find_endpoint_house_number_mmap`, both in `interp.rs`; the two
sub-markers at the call site region. Out of scope and explicitly NOT touched:
`SlimInterpWay` layout, `InterpWay`/`AddrPoint` on-disk format,
`FORMAT_VERSION`, the sentinel semantics, `read_addr_point_mmap` /
`read_node_at` / `parse_house_number` (reused verbatim), Pass 2b way
extraction, the surrounding pass structure. The G1/G4 architectural rewrites
named in `geocode-build-opportunities.md` are separate TODO items and are not
started here.

## Target end state (concrete artifacts)

Replace the transient `FxHashMap<u64, Vec<u32>>` with a sort-based CSR built
in parallel, then parallelize endpoint resolution over interpolation ways.

### The CSR type

New private struct in `interp.rs`:

```rust
/// Compressed-sparse-row map from S2 cell id (at street_level) to the
/// address-point indices whose projected cell equals it. Built once per
/// build, read-only during endpoint resolution, shareable across rayon
/// workers without locks.
struct CellAddrCsr {
    /// Distinct cell ids, ascending. Binary-search key.
    cell_ids: Vec<u64>,
    /// Length `cell_ids.len() + 1`. Row i occupies
    /// `values[offsets[i]..offsets[i + 1]]`.
    offsets: Vec<u32>,
    /// Address-point indices, grouped by cell in `cell_ids` order.
    values: Vec<u32>,
}

impl CellAddrCsr {
    /// Address-point indices projected into `cell_id`, or `&[]` if none.
    fn get(&self, cell_id: u64) -> &[u32] {
        match self.cell_ids.binary_search(&cell_id) {
            Ok(i) => {
                let (lo, hi) = (self.offsets[i] as usize,
                                self.offsets[i + 1] as usize);
                &self.values[lo..hi]
            }
            Err(_) => &[],
        }
    }
}
```

### Build (parallel collect + sort + linear group-by)

```rust
fn build_cell_addr_csr(addr_mmap: &[u8], street_level: u8) -> CellAddrCsr {
    use rayon::prelude::*;
    let addr_count = addr_mmap.len() / ADDR_POINT_SIZE;

    // 1. Parallel projection: one (cell_id, addr_idx) pair per addr point.
    //    No shared map, no fold/reduce merge. Drive the parallelism from an
    //    INDEXED source so `collect` writes into one preallocated Vec instead
    //    of stitching worker-local fragments: `filter_map` is unindexed and
    //    its `collect` builds per-worker Vecs then concatenates them (an extra
    //    copy pass that can transiently hold ~2x the pair buffer). Every addr
    //    point projects to exactly one pair - the source is dense, no filter
    //    is needed - so map an indexed record iterator directly:
    let mut pairs: Vec<(u64, u32)> = addr_mmap
        .par_chunks_exact(ADDR_POINT_SIZE)
        .enumerate()
        .map(|(idx, rec)| {
            let pt = AddrPoint::from_bytes(rec.try_into().expect("chunk size"));
            let ll = LatLng::from_degrees(
                pt.lat_e7 as f64 * 1e-7, pt.lon_e7 as f64 * 1e-7);
            let cell = CellID::from(ll).parent(street_level as u64).0;
            (cell, idx as u32)
        })
        .collect();
    // `par_chunks_exact` is an IndexedParallelIterator and `map` preserves
    // that, so `collect` allocates the final Vec once and each worker fills
    // its own contiguous span - no fragment concat, no double buffer.

    // 2. Parallel sort by (cell id, addr idx). The idx tiebreak is NOT
    //    cosmetic: `find_endpoint_house_number_mmap` replaces its best
    //    candidate only on strict `dist_sq < best_dist_sq` and anchors on the
    //    first exact match, so on a distance tie (or multiple exacts) the
    //    LOWEST index must win to match the sequential hashmap build, which
    //    pushed indices in ascending order. A cell-only unstable sort would
    //    leave same-cell indices in arbitrary order and silently change which
    //    house number a tied endpoint resolves to. Sorting by (cell, idx)
    //    restores ascending-idx order within each cell and makes the
    //    byte-identical guarantee real.
    pairs.par_sort_unstable_by_key(|&(cell, idx)| (cell, idx));

    // 3. Linear group-by over sorted pairs -> CSR. No hashmap.
    let mut cell_ids = Vec::new();
    let mut offsets = vec![0u32];
    let mut values = Vec::with_capacity(pairs.len());
    let mut i = 0usize;
    while i < pairs.len() {
        let cell = pairs[i].0;
        cell_ids.push(cell);
        while i < pairs.len() && pairs[i].0 == cell {
            values.push(pairs[i].1);
            i += 1;
        }
        offsets.push(values.len() as u32);
    }
    CellAddrCsr { cell_ids, offsets, values }
}
```

Notes pinning the bricks:

- `values.len()` fits `u32`: at planet ~150 M address points; `u32::MAX`
  is ~4.29 B, comfortable. The existing addr-index type is already `u32`
  (`read_addr_point_mmap(mmap, index: u32)`), so this introduces no new
  ceiling. If a future dataset exceeds `u32` address points the existing
  code already breaks first; no new guard is owed here.
- Peak transient memory during build: `pairs` is `Vec<(u64, u32)>`,
  16 bytes/elem after padding, ~2.4 GB at planet - versus the old ~1 GB
  fragmented hashmap. This is a real increase in this phase's peak
  (9.04 GB governing figure includes surrounding retained state, and Pass 3
  fine Stage B at 22.53 GB is the run-governing peak, so this phase's local
  bump does not move the run ceiling). Note the group-by (step 3) reads
  `pairs` while it fills the CSR `values`, so `pairs` (~2.4 GB) and the CSR
  (~720 MB, below) are BOTH live at the group-by peak - the phase's transient
  high-water is ~3.1 GB, not 2.4 GB. `pairs` is dropped at the end of
  `build_cell_addr_csr`, before endpoint resolution begins (scope it inside
  the fn and return only the CSR). The CSR itself is ~720 MB:
  `cell_ids` ~10 M u64 = ~80 MB, `offsets` ~10 M u32 = ~40 MB, `values`
  ~150 M u32 = ~600 MB. (An earlier draft said "~1.8 GB"; that headline
  contradicted its own breakdown and is wrong - the fields sum to ~720 MB.)
  The bench gate reads peak anon both for the whole run (confirm Pass 3 fine
  Stage B remains the ceiling) AND scoped to the interpolation phase (confirm
  the ~3.1 GB local high-water is what we expect and does not creep).

### Endpoint resolution (parallel over interp ways, read-only CSR)

`find_endpoint_house_number_mmap` keeps its exact matching logic; only its
`cell_to_addrs: &FxHashMap<u64, Vec<u32>>` parameter changes to
`csr: &CellAddrCsr`, and the two `.get(&cell_id)` sites become
`csr.get(cell_id)` returning `&[u32]` (the closure body iterates the slice
identically - same nearest-with-matching-street_offset, same exact-match
tie-break, same `all_neighbors` walk). The behavior is byte-identical to
today for any given input - but ONLY because the build sorts by
`(cell, idx)`: the tie-break at `dist_sq < best_dist_sq` and the first-exact
anchor are order-sensitive within a cell, so byte-identity depends on the CSR
presenting each cell's indices in the same ascending-idx order the sequential
hashmap did. See the step-2 sort note in the build. Only the container
changed; the iteration order within a cell is preserved.

Cognitive-complexity note: the project denies `cognitive_complexity`. The
`par_iter_mut().map()` closure below carries the `node_count` guard, two
`let-else` node reads, two `find_endpoint_house_number_mmap` calls and the
resolved-count branch. If Brick 1's `brokkr check` trips the deny, factor the
per-way body into a free function `resolve_one_way(iw: &mut SlimInterpWay,
interp_nodes_mmap, addr_mmap, strings, csr, street_level) -> u32` and let the
closure be `.map(|iw| resolve_one_way(iw, ...)).sum()`. This is a mechanical
extraction, not a design change; call it out so the implementer expects it.

Resolution loop becomes:

```rust
let resolved = interp_ways
    .par_iter_mut()
    .map(|iw| {
        if iw.node_count < 2 { return 0u32; }
        let Some(start_coord) =
            read_node_at(interp_nodes_mmap, iw.node_file_offset) else { return 0; };
        let last_offset = iw.node_file_offset
            + (iw.node_count as u64 - 1) * NODE_COORD_SIZE as u64;
        let Some(end_coord) = read_node_at(interp_nodes_mmap, last_offset) else { return 0; };
        let start_hn = find_endpoint_house_number_mmap(
            start_coord, iw.street_offset, addr_mmap, strings, csr, street_level);
        let end_hn = find_endpoint_house_number_mmap(
            end_coord, iw.street_offset, addr_mmap, strings, csr, street_level);
        if let (Some(s), Some(e)) = (start_hn, end_hn) {
            iw.start_number = s;
            iw.end_number = e;
            1
        } else { 0 }
    })
    .sum();
```

No `AtomicU32` (the reverted attempt's approach) - the resolved count is a
`map(...).sum()` reduction, each `iw` is independent, and the CSR is shared
`&`. `find_endpoint_house_number_mmap`'s `check_cell` closure captures local
`best_idx` / `best_dist_sq` / `found_exact` per call, so it is already
thread-safe once the shared map is immutable.

### Marker wiring

At the call site in `mod.rs`, keep the outer
`GEOCODE_PASS2_INTERP_RESOLVE_START` / `..._END` pair. Inside
`resolve_interpolation_endpoints_mmap`, emit around the two halves:

```rust
crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_INDEX_START");
let csr = build_cell_addr_csr(addr_mmap, street_level);
crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_INDEX_END");
crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_START");
let resolved = /* par_iter_mut reduction above */;
crate::debug::emit_marker("GEOCODE_INTERP_RESOLVE_ENDPOINTS_END");
```

These are `FOO_START`/`FOO_END` pairs, so `brokkr sidecar <UUID>
--durations --human` gives per-half wall time directly. Keep the early
return (`interp_ways.is_empty() || addr_count == 0`) before the first
marker so empty-input runs emit no zero-width pairs.

Baseline caveat (why the markers land in Brick 1, not later): the recorded
planet baseline `b4b25c05` (`82db8ed`) carries only the OUTER
`GEOCODE_PASS2_INTERP_RESOLVE` pair - the INDEX/ENDPOINTS sub-markers existed
only in the reverted `363c579`, whose results were invalidated. So no
`--commit <old-ref>` run against the pre-change tree can reproduce the
sub-spans; a before/after comparison of the two halves is impossible unless
the durable sub-markers exist in a committed pre-CSR state. Brick 1 therefore
introduces the four sub-markers together with the CSR build but keeps endpoint
resolution SEQUENTIAL, and its bench is captured and committed as the
sequential-CSR baseline. The endpoint-parallelism brick then measures against
that committed sub-marker baseline, not against `b4b25c05`. This also honors
the contract's "measuring instrument before the change it gates"
(`reference/technical-implementation-spec.md`).

## Bricks, in landing order

Each brick is one coherent, fully-intrusive change. `brokkr check` stays
green at every boundary.

### Brick 1: CSR type + parallel build + sub-markers, endpoint loop stays sequential

Rewrite `resolve_interpolation_endpoints_mmap` and
`find_endpoint_house_number_mmap` in `interp.rs`: add `CellAddrCsr`, add
`build_cell_addr_csr`, change the helper's parameter from the hashmap to
`&CellAddrCsr`, and drive endpoint resolution with the CSR. Add the four
sub-markers. Remove the `FxHashMap` import from `interp.rs` if now unused.

**The endpoint loop stays a plain sequential `for iw in interp_ways.iter_mut()`
here** - identical to today's loop body but reading `csr.get(cell)` instead of
the hashmap. Only the index build parallelizes in this brick. Rationale: the
two halves have independent verdicts (index build amortizes at Europe,
endpoint parallelism only at planet), and coupling them into one keep/revert
unit created the contradiction the earlier draft carried - "revert the CSR
build but keep the parallel endpoints" is impossible because the endpoint loop
consumes a CSR that the reverted build never produced. Splitting the landing
removes that trap: Brick 1 is the CSR data-structure swap (build parallel,
resolution sequential), and the endpoint parallelism is a separate, separately
revertible brick (Brick 5) layered on top of it.

Commit this brick and capture its planet sub-marker bench (Brick 5's gate
consumes it as the `--commit <brick1-commit-hash>` baseline): this committed
sequential-CSR state is the baseline the endpoint-parallelism brick measures
against, since the original `b4b25c05` baseline has no sub-markers.

**Gate (correctness, wiring):**
`brokkr check`
- Runs gremlins + clippy + the tier-1 suite across all sweeps. Catches the
  `cognitive_complexity`/`unwrap_used` denies, the CLI-crate rebuild, and
  any type-wiring break. Sufficient because the synthetic geocode
  interpolation tests below run here (tier 1) and pin behavior on fixtures
  with resolved and unresolved interpolation ways.

### Brick 2: named unit + integration tests for the CSR path

The existing synthetic interpolation tests in `tests/geocode_index.rs`
already exercise resolution end-to-end through the stable builder API and
run in `brokkr check` (they are not `#[ignore]`d):
`synthetic_interpolation_query_resolves_even_house_number`,
`unresolved_interpolation_stays_hidden_behind_zero_sentinel`,
`coarse_fallback_recovers_interpolation_outside_fine_radius`. These pass
unchanged proves the CSR is behavior-identical to the hashmap on the
fixtures - they are the primary correctness oracle (no external
osmium/osmosis command resolves interpolation house numbers, so there is no
`brokkr verify` gate for this phase; the in-tree synthetic tests are the
oracle, per `reference/testing.md`).

Add two inline unit tests in `interp.rs` `#[cfg(test)] mod tests` (tier 1,
module-internal, die with the module on rewrite - the correct placement per
`reference/testing.md` "Inline unit tests ... module internals"):

1. `csr_get_matches_naive_hashmap_ordered` - build a small `addr_points.bin`
   byte buffer by hand (a handful of `AddrPoint::to_bytes` records spanning
   at least two distinct `street_level` cells plus an empty cell, with at
   least one cell holding several points inserted OUT of index order so the
   test distinguishes ordered from unordered), build the CSR via
   `build_cell_addr_csr`, and assert `csr.get(cell)` returns the reference
   index list in ASCENDING-IDX ORDER for every queried cell including a miss
   (empty slice). The comparison MUST be ordered slice equality
   (`csr.get(cell) == &expected[..]`), not a set/`HashSet` comparison: the
   whole point of the `(cell, idx)` sort is intra-cell order, and a set
   comparison is blind to exactly the property that governs the runtime
   tie-break. The reference must be built by the same ascending-idx scan the
   sequential hashmap used. This pins both the group-by boundary math
   (`offsets` correctness) and the ordering guarantee.
2. `csr_get_empty_input` - `build_cell_addr_csr(&[], 17)` yields
   `cell_ids` empty, `offsets == [0]`, and `get(anything)` is `&[]`. Pins
   the degenerate path the outer early-return would otherwise mask.
3. `endpoint_tiebreak_picks_lowest_index` - the ordered-slice test alone
   proves the CSR's contents, but not that the resolver's tie-break still
   fires the same way. Construct a tiny scenario: two address points in the
   same `street_level` cell, SAME `street_offset`, EQUAL squared distance to
   a synthetic endpoint, DIFFERENT house numbers, inserted so the
   lower-numbered house has the lower addr index. Call
   `find_endpoint_house_number_mmap` (or drive it through
   `resolve_interpolation_endpoints_mmap` with a one-way slice) and assert it
   returns the lower-index house number. This is the one test that would go
   red under a cell-only unstable sort - the resolved count and the
   set-comparison test both stay green through that bug, so this test is the
   real defense.

These need no dataset and no CLI. They cover the three behaviors no
fixture-driven oracle reaches directly: the group-by offset arithmetic, the
empty CSR, and the distance-tie index tie-break.

**Gate:**
`brokkr check`
- The inline tests plus the three synthetic interpolation tests all run
  here. Green confirms Brick 1's rewrite is behavior-identical on every
  pinned fixture and the CSR internals are correct.

### Brick 3: real-data correctness confirmation (Malta first, Germany fallback)

Denmark has 0 interpolation ways (`reference/performance.md`: "Scandinavian
precise addressing"), so it cannot exercise this phase. The smallest
CONFIGURED real dataset that trips the interpolation predicate is Malta, not
Germany: the 8 MB `malta` PBF (`brokkr.toml` `[plantasjen.datasets.malta]`)
carries ways matching the builder predicate `interp.is_some() &&
addr_st.is_some()` (`pass2.rs`), reportedly ~55 such ways versus Germany's 78.
Use Malta as the first real-data gate - it is faster and answers the same
question ("does the CSR resolve the same set the hashmap did"). Germany
remains the secondary confirmation at slightly larger scale.

**Gate (real-data behavior):**
`brokkr build-geocode-index --dataset malta --variant indexed`
- First, capture the pre-change resolved count by running the same command on
  the pre-Brick-1 tree (the count is not recorded in `reference/performance.md`
  for Malta, so it must be measured, not assumed). Then run HEAD and assert
  the `NN/MM interpolation ways resolved` line is UNCHANGED. If the pre-change
  Malta run resolves zero ways (possible if none of its interpolation ways
  find a same-street address point), Malta cannot exercise resolution - record
  that result in the note and fall back to Germany (`--dataset germany`,
  expected `71/78` at `ed34092`). (This is a run, not a stored bench - no
  `--bench`.)

**Caveat on what the count proves.** The resolved *count* only detects
resolved<->unresolved flips; it is INVARIANT to a way resolving to a
*different* house number via a changed tie-break. So a green count is NOT
proof of value-identity. The real defenses against the ordering class of bug
are the synthetic interpolation tests and the
`endpoint_tiebreak_picks_lowest_index` unit test (Brick 2), not this count.
Do not read a matching count as evidence the resolved *values* are identical.

### Brick 4: CSR-build performance verdict (Europe)

The reverted fold/reduce regressed the index build at Europe (~12 s ->
23.7 s). The CSR build (Brick 1) must not regress it; the sort-based build
should hold flat or improve. Europe (~20 M addr points) is the scale at which
the reverted attempt was measured, so it is the correct scale to prove the new
build does not repeat that regression. Endpoint resolution is still sequential
at this point (parallelism is Brick 5), so this brick isolates the build half.

Commit Brick 1+2+3 first, then, per the `--commit` build-thrash warning in
`AGENTS.md` (group HEAD and worktree runs, end on HEAD):

- After number (HEAD):
  `brokkr build-geocode-index --dataset europe --variant indexed --bench 3`
- Baseline (pre-change) number, from your own branch. Pin the ACTUAL
  pre-Brick-1 commit hash at execution time (record it in the note; the
  literal `<pre-brick1-ref>` placeholder is not copy-pasteable and there is no
  pre-existing ref - it is whatever HEAD was before Brick 1 landed):
  `brokkr build-geocode-index --dataset europe --variant indexed --bench 3 --commit <actual-pre-brick1-hash>`
- Read the two halves with `brokkr sidecar <UUID> --durations --human` and
  compare `GEOCODE_INTERP_RESOLVE_INDEX` and `GEOCODE_INTERP_RESOLVE_ENDPOINTS`
  spans across the two runs. Compare peak anon with `brokkr sidecar <UUID>
  --human`, both whole-run (confirm Pass 3 fine Stage B stays the ceiling) and
  scoped to the interpolation phase (confirm the ~3.1 GB group-by high-water).

`--bench 3` (not `--bench 1`): the `reference/performance.md` default and the
spec contract require best-of-3 for a stored keep/revert verdict; Europe is
cheap enough that 3 runs cost little. (Planet in Brick 5 is the one place
`--bench 1` is defensible, see there.)

Keep/revert bound for Brick 4: the INDEX span must be <= the pre-change
sequential INDEX time within `reference/performance.md` noise rules (numeric:
the best-of-3 INDEX span must not exceed the pre-change best-of-3 INDEX span by
more than the recorded run-to-run noise band for this phase; a >5 % INDEX
regression fails the brick). A CSR build slower than the sequential hashmap at
Europe is a failed brick - revert to the hashmap build and do not proceed to
Brick 5 (the endpoint half has nothing to stand on without a proven CSR).

### Brick 5: parallelize endpoint resolution, prove at planet

Only after Brick 4 proves the CSR build at Europe: change the sequential
`for iw in interp_ways.iter_mut()` loop to the `par_iter_mut().map(...).sum()`
reduction shown in "Target end state". This is the one change in this brick.
It is independently revertible - reverting it restores the Brick-1 sequential
loop over the same CSR, a clean fallback that actually compiles (unlike the
earlier draft's "revert the build, keep the parallel loop", which could not).

The endpoint parallelism only amortizes at planet (~50-100 k interp ways;
Europe's ~1-2 k defaults chunk size to 1, so Europe cannot answer this). Planet
is an explicit user decision per `reference/performance.md` (costs ~7 min, run
on plantasjen to match the baseline host).

**Gate:**
- After number (HEAD, parallel endpoints):
  `brokkr build-geocode-index --dataset planet --variant indexed --bench 1`
- Baseline: the COMMITTED Brick-1 sequential-CSR state (the sub-marker-bearing
  commit captured in Brick 1), NOT `b4b25c05` - that historical row has only
  the outer marker and cannot yield a per-half comparison. Run
  `... --bench 1 --commit <brick1-commit-hash>` grouped after the HEAD run
  (end on HEAD per the build-thrash warning). Compare the
  `GEOCODE_INTERP_RESOLVE_ENDPOINTS` span (and the total
  `GEOCODE_PASS2_INTERP_RESOLVE`) between parallel HEAD and sequential Brick 1.

`--bench 1` here (not 3): planet is a ~7 min run and the baseline `b4b25c05`
itself was captured at `--bench 1`; `AGENTS.md` shows `--bench 1` as accepted
practice for planet-scale commands. Because both sides use `--bench 1`, the
comparison is apples-to-apples. If a stored best-of-3 planet verdict is later
required, it is a separate explicit user decision.

Keep/revert bound for Brick 5 (numeric): the parallel
`GEOCODE_INTERP_RESOLVE_ENDPOINTS` span must be strictly less than the Brick-1
sequential ENDPOINTS span at planet (the endpoint half must net positive on its
own axis), and the total `GEOCODE_PASS2_INTERP_RESOLVE` must not regress. The
original note projects ~25 s recoverable across both halves (target near
~5-8 s once both parallelize) - treat that as the aspiration, but the hard
gate is "parallel ENDPOINTS < sequential ENDPOINTS". If the endpoint half nets
negative at planet even over the proven CSR (measured, not assumed), revert
just this brick - the `par_iter_mut` becomes the Brick-1 sequential loop again,
and the Europe-proven CSR build ships unchanged.

## Records to update on landing

- `reference/performance.md`: replace the "Pass 2 interp resolve
  (sequential)" planet phase row (30.6 s / 1.0 cores) with the new figure
  and commit hash; update the prose "only sequential-by-design phase left"
  sentence (it is no longer sequential). Update the Malta/Germany
  resolved-count lines only if they changed (they must not).
- `reference/performance-history.md`: record the arc - the reverted
  fold/reduce attempt (`363c579`), why it failed, and the CSR replacement's
  before/after with host + commit hashes. If Brick 5 nets negative and the
  endpoint half is reverted, record that verdict here.
- `notes/geocode-build-opportunities.md`: mark G5 and the two follow-up
  sections closed (or update with the landed shape), per the loop's
  note-retirement practice. Do not reference this spec file from code
  comments (`notes/*` are transient); the code comment in `interp.rs`
  carries its own full context.
- `CHANGELOG.md`: a headline perf entry only if the total planet build wall
  moves enough to matter to a library/CLI user (per the CHANGELOG rule -
  headline numbers, not sub-phase deltas). A ~25 s cut on a ~425 s planet
  build is borderline; include it only if Brick 5 confirms the win at the
  total level, phrased as the planet build-time improvement.

## Why this is complete, not aspirational

- Every step from the current hashmap to the shipped CSR is a named brick
  with exact types (`CellAddrCsr`, `build_cell_addr_csr` signatures) and the
  exact resolution loop.
- The one obstacle - "parallelizing the map regressed" - is resolved inline
  by replacing the structure (sort-based group-by, no hashmap merge), not by
  retrying the failed shape.
- Every brick names its exact gate command with dataset and variant pinned,
  and the dataset choice is justified against what that gate can break
  (Malta as the smallest configured resolve-count oracle with Germany as
  secondary, Europe for the index-build regression the reverted attempt
  exposed, planet for the endpoint-half amortization that only appears at
  planet interp-way counts).
- The keep/revert path is explicit and splittable: CSR build and endpoint
  parallelism are independent statements with independent verdicts, so a
  planet endpoint-half loss does not sink the Europe-proven build win.

## Review consolidation (R1 Opus + R2 Codex)

This spec was revised to fold every valid finding from two independent
reviews. Both reviews are superseded by the edits above; this section records
the disposition so the arc is auditable.

Findings folded (all validated against `interp.rs` / `pass2.rs` /
`brokkr.toml`):

- **Order-sensitive tie-break / cell-only unstable sort breaks byte-identity**
  (R1 BUG, R2 P1.1). Confirmed: the map build pushes indices ascending and the
  resolver replaces only on strict `<` with a first-exact anchor, so ties
  resolve to the lowest index. Fixed by sorting `(cell, idx)` and rewriting the
  step-2 sort note and the byte-identical justification.
- **Unit test must assert ordered-slice equality, not set equality** (R1 GAP,
  R2 P1.1). Folded into Brick 2 test 1; added Brick 2 test 3
  (`endpoint_tiebreak_picks_lowest_index`) - the only test that goes red under
  the sort bug.
- **Resolved-count oracle is invariant to value changes** (R1 GAP). Added the
  explicit caveat in Brick 3.
- **Memory arithmetic ~1.8 GB vs ~720 MB breakdown** (R1 SMELL, R2 P2.5).
  Corrected to ~720 MB and added the group-by simultaneous-live note (~3.1 GB
  phase high-water).
- **cognitive_complexity risk on the new closure** (R1 NIT). Added the
  `resolve_one_way` extraction heads-up.
- **Sub-marker baseline is impossible against `b4b25c05`** (R2 P1.2). Fixed by
  landing durable sub-markers in Brick 1 with sequential resolution and
  capturing that committed state as the endpoint-half baseline.
- **keep/revert contradiction + impossible "revert build, keep parallel loop"
  fallback** (R2 P1.3). Fixed by splitting the landing: Brick 1 = CSR build +
  sequential resolution; Brick 5 = parallel endpoints, independently
  revertible to the Brick-1 sequential loop.
- **`filter_map().collect()` is unindexed, not disjoint-slice writes** (R2
  P2.5). Fixed the build to an indexed `par_chunks_exact(...).enumerate()`
  source and corrected the comment.
- **Malta is the smallest configured real dataset, not Germany** (R2 P2.6).
  Confirmed Malta is configured (8 MB) and matches the predicate; Brick 3 now
  gates on Malta first, Germany as fallback.
- **`--bench 3` and numeric keep/revert bounds** (R2 P1.4). Europe (Brick 4)
  switched to `--bench 3` with a numeric INDEX bound (>5 % regression fails).
  Planet numeric bound added (parallel ENDPOINTS < sequential ENDPOINTS).

Partial / nuanced dispositions (not rejections):

- **Planet `--bench 3`** (part of R2 P1.4). NOT adopted for the planet gate
  (Brick 5), which keeps `--bench 1`. Rationale: planet is a ~7 min run, the
  `b4b25c05` baseline was itself `--bench 1`, and `AGENTS.md` documents
  `--bench 1` as accepted practice for planet-scale commands - both sides use
  `--bench 1` so the comparison stays apples-to-apples. The `--bench 3` rule is
  honored where it is cheap (Europe). This is a deliberate divergence from a
  literal reading of the rule, justified in Brick 5.
- **`<pre-brick1-ref>` not copy-pasteable** (part of R2 P1.4). The placeholder
  is inherent to a spec written before Brick 1 exists; adopted as an
  instruction to pin the actual hash at execution time rather than a defect to
  remove.

Findings rejected: none. Every finding from both reviews was validated as
correct against the source and folded.
