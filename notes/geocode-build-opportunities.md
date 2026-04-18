[//]: # (Full-rebuild optimisation plan for build-geocode-index.)
[//]: # (Arc landed 2026-04-18; this note now tracks follow-ups only.)

# Geocode index builder - optimisation plan

> **Scope.** This plan targets wall-time for the *full-rebuild* path -
> `build-geocode-index` against a cold PBF. Complementary effort in
> [incremental-geocode-index.md](incremental-geocode-index.md) targets
> *avoiding* the full rebuild on daily diffs (currently blocked on a
> format-v2 element-ID change; see that doc for the design sketches).

## Status: landed 2026-04-18

Planet: **1,255 s (20.9 min, TAINTED baseline `7e9c2e9`) -> 432.9 s
(7m12s, `82db8ed` UUID `b4b25c05`). -65 %, 2.9x.**
Europe 344 s -> 183.4 s (-47 %). Germany 71 s -> 30.9 s (-57 %).
Denmark 5.0 s -> 3.4 s (-32 %).

Pass 1.5 peak anon 29.5 GB -> 3.0 GB at planet (-90 %), moving the
governing peak out of the OOM-prone phase. Post-arc governing peak is
~25 GB at Pass 3 Stage B - comfortable on 27 GB hosts.

### What landed

| Commit | Item |
|---|---|
| `c977b97` | Instrumentation: phase markers + `hotpath::measure` |
| `63800d3` | **#7** Shared-atomic `IdSetDense` in Pass 1.5 |
| `88cf796` | **#1 Phase 2a** `mallopt(M_ARENA_MAX, 2)` + parallel node scan |
| `1e4461b` | Header-walk consolidation (Pass 1.5 + Pass 2a schedules in one walk) |
| `9603d83` | **#8** Parallel admin polygon assembly |
| `18c13c5` | Phase 2a direct `coord_mmap` writes (removes 1.86 GB-Germany channel traffic) |
| `c96faf4` | **#1 Phase 2b** parallel way scan |
| `18f4c91` | **#5 + #6** Parallel Pass 3 addr/interp cell classification + admin flood-fill |
| `5150b1b` | **#3** Parallel Pass 3 Stage B bucket parse+sort |
| `0d5a6dd` | **#4** Fused fine+coarse Stage A (see open follow-ups) |
| `74a736d` | **#2** Pass 1.5 wire-format scanner (no `PrimitiveBlock` construction) |

Historical detail on per-item motivation, mechanics, and measured wins
lives in the respective commit messages.

## Open follow-ups

### #4 fine+coarse Stage A fusion - needs another pass

`bucketed_cell_assignment_fused` landed at `0d5a6dd` with a measured
Europe delta of -2.8 s / -1.5 %, much smaller than the 40-60 s at
planet prediction. Post-mortem (see commit `0d5a6dd` message for full
sub-phase table):

- Sequential "coarse Stage A" wasn't mostly `cover_segment`. Bucket-
  writer I/O and per-cell dedup hashtable work survive fusion - only
  the "step intermediate points and compute LatLng per step" part is
  actually removed.
- Fused per-call body is slower (streets: 7.2 s fine-only -> 11.6 s
  fused, +60 %). Writes to two bucket trees per emitted cell, tracks
  an extra 4-entry stack set for coarse dedup. Avg cores dropped
  8.1 -> 6.2 under the same rayon schedule.
- Addr fusion is the real win (5.3 -> 3.8 s, -28 % on Europe). Single-
  point cells derive coarse from fine's `CellID` via one extra
  `.parent()` call - no cover-segment work, no extra state.

**Next-pass options.** Not urgent; shipped code is correct and delivers
a small positive win.

- **Partial revert.** Keep the addr fusion, unwind the streets/interp
  fusion. The 3-line addr-derivation is the whole win; streets/interp
  adds ~80 lines for a marginal 0.85 s Europe gain.
- **Different streets/interp shape.** Workers produce `Vec<u64>` (just
  fine cells per segment); serial distribute step computes coarse
  parents and writes both trees. Moves derivation off the hot parallel
  path at the cost of intermediate Vec allocation.
- **Accept.** Planet projection from the Europe ratio is ~7 s saved;
  complexity cost is real but contained to one function.

### Pass 2 interp resolve - investigated and deferred

The 2026-04-18 planet bench surfaced `resolve_interpolation_endpoints_mmap`
at **30.6 s / 7 % of planet wall, single-threaded (1.0 avg cores)**.
Wasn't in the original plan because Germany hides it (3.2 s / 10 %).

**Attempted and reverted (commit `363c579` -> reverted at `7cb807b`,
results invalidated via `brokkr invalidate --commit 363c579`).** Naive
approach: rayon fold+reduce for the spatial-index build (per-worker
partial `FxHashMap<u64, Vec<u32>>`, merged at end) + `par_iter_mut()`
with `AtomicU32` for the endpoint-resolution loop. Added sub-markers
`GEOCODE_INTERP_RESOLVE_INDEX_{START,END}` and
`GEOCODE_INTERP_RESOLVE_ENDPOINTS_{START,END}` so the split could be
measured.

Measured Europe **183.4 s -> 199.1 s (+15.7 s net regression)**.
Sub-phase breakdown:

| Sub-phase | Parallel (Europe) | Pre-change combined |
|---|---:|---:|
| INDEX_BUILD | 23.7 s @ 10.3 cores | ~12 s sequential |
| ENDPOINTS | 3.6 s @ 1.0 cores | ~3 s sequential |

Two lessons:

1. **Fold+reduce hashmap merge is slower than sequential insertion at
   this scale.** Europe has ~20 M addr points; the fold produces ~12
   per-worker maps of ~1.7 M entries each. The reduce step walks each
   side-map and appends Vecs into the final map - roughly 20 M
   lookup+push operations that sequential insertion never does. With a
   fast single-threaded insert path (FxHashMap is already fast), the
   merge overhead dominates.
2. **`par_iter_mut()` on endpoints doesn't help at Germany/Europe
   scale.** Interp way count is small (~78 Germany, ~1-2k Europe) so
   per-thread chunk sizes default to 1 and overhead dwarfs work. Avg
   cores measured 1.0 at Europe. Planet with ~50-100k interp ways
   might benefit, but the index-build regression would have to be
   fixed first for the phase to net positive.

**Path to a working parallel version** (not currently pursued): drop
the `FxHashMap<u64, Vec<u32>>` and replace with parallel collect
`Vec<(cell, idx)>` + `par_sort_unstable_by_key(cell)` + a binary-search
lookup in `find_endpoint_house_number_mmap`. Changes the data structure
and the consumer, bigger diff. Worth ~25 s at planet by projection.
Not worth doing unless the 7 min planet wall needs to drop further.

### Interpolation endpoint CSR - RSS hygiene, not wall

`resolve_interpolation_endpoints_mmap` builds a transient
`FxHashMap<u64, Vec<u32>>` mapping S2 cell IDs to address-point
indices. At planet this is ~1 GB heap (~150 M address points across
~10 M distinct S2 cells, each an individually allocated `Vec`).

A CSR-style layout (one contiguous offsets array + one contiguous
values array, sorted by cell_id, binary-search lookup) would roughly
halve the peak. Short-lived, so this is peak-heap reduction, not
throughput.

Not on the wall-time critical path. Revisit if a smaller-RAM host
needs planet support.

## What to leave alone

- **The ~16 GB anon `coord_mmap`.** Sized by geocode's filtered
  `referenced_count` - only nodes referenced by geocode-relevant ways.
  At planet this is well below ALTW's total unique-referenced count
  (~10 B, measured 2026-04-16 when an ALTW reshape OOM'd at Europe
  with a 29 GB coord table). Geocode's tag-filter pre-narrowing is
  what keeps this structure viable in RAM; do not copy this pattern
  to a command that touches **all** way refs. Right size, right
  indexing; do not try to compact or partition. Any future plan
  change that alters the filter's breadth must re-measure
  `referenced_count` at planet before assuming the RAM footprint
  stays similar.
- **`PrimitiveBlock` in Pass 2.** Full-decode cost amortises across
  cores. A wire-format tagged scanner (like #2's Pass 1.5 path) would
  duplicate tag-resolution logic for modest gain - save as a possible
  tweak if profiling shows tag iteration hot.
- **Pass 1 (relation scan).** Tiny fraction of wall. Not worth
  parallelising - 36.6 s at planet is ~8 % and dominated by single-
  threaded admin-relation metadata collection.
- **Output file formats.** The on-disk layout is consumed by a mature
  `Reader`; do not change byte-level shapes to accommodate build-time
  parallelism. All parallelisation in the landed arc is tmp-file or
  owned-string-channel + sequential merge.

## Invariants to preserve

- **Sorted + indexed PBF precondition.** Enforced at entry via
  `require_indexdata`; sorted-PBF node-before-way invariant is what
  makes Phase 2a/2b a clean barrier.
- **Disjoint rank ranges across node blobs.** Phase 2a writes to
  `coord_mmap` concurrently via `CoordMmapShared::write_coord` without
  atomics; correctness depends on `IdSetDense::rank(id)` being unique
  per set ID + sorted PBF guaranteeing each ID in at most one blob.
  `debug_assert!` in `write_coord` catches bounds regressions.
- **Bucket-order cell_id monotonicity.** Pass 3 Stage B asserts
  cell_id monotonicity across buckets; buckets partition by top 8
  bits of cell_id so bucket N's min > bucket N-1's max by
  construction.
- **Zero-coord sentinel in way coord resolution.** `(lat == 0 &&
  lon == 0)` reads drop as "missing" (both in Pass 2b's coord_slice
  read and in the sequential predecessor). A real node at Null
  Island is silently dropped - `KNOWN LIMITATION` comments mark the
  sites; fix shape is a presence bitmap alongside the coord array.

## Cross-validation

There's no `brokkr verify` for the geocode index. During the arc we
used byte-for-byte `diff -r old_index/ new_index/` on Denmark between
commits - works because every landed commit preserved blob-sequence
ordering via `parallel_classify_phase`'s ReorderBuffer or equivalent.
For the open follow-ups (especially Pass 2 interp resolve
parallelisation) the same check still applies: if output order is
preserved, diff works; if not, fall back to `Reader` query results
on a fixed sample of coordinates.
