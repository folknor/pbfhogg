[//]: # (Full-rebuild optimisation plan for build-geocode-index.)
[//]: # (Arc landed 2026-04-18; this note now tracks follow-ups and larger rewrites.)

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

**Further drop at 2026-04-19:** planet **432.9 s -> 383.7 s (-49.2 s,
-11 %)** at commit `052da8b`, UUID `f832d3d6`, `--bench 1`. No
build-geocode-index change landed between `82db8ed` and `052da8b`;
the win came from cross-cutting read/write-path restructures (commits
between `aee7727` and `052da8b` - blob wire-format split, raw-frame
split, write framing/pipeline split). Worth a `--bench 3` confirm
before treating this as the new baseline - single-sample at this
scale sits inside run-to-run variance.

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

## Current unconstrained review, 2026-04-25

This section treats `build-geocode-index` as an internal implementation that
may be rewritten end-to-end as long as it produces the same reader-visible
index and stays safe on a host with ~28 GB free RAM. Existing passes, helper
APIs, `PrimitiveBlock` usage, and scratch formats are not constraints.

Measured phase weights from planet result `38565e43` (`8e3a0d1`, 2026-04-20,
~435 s wall) give the current target shape:

- Pass 1 relations: ~42 s.
- Schedule walk: ~6.7 s.
- Pass 1.5 referenced-node way scan: ~18 s.
- Pass 2a node scan: ~65 s.
- Pass 2b way scan: ~129 s.
- Interpolation resolve: ~31 s.
- Pass 3 cell assignment/finalization: ~106 s, including ~57 s in Stage B.
- Peak HWM: ~22.6 GB on the 26 GB bench host, with swap observed. The command
  is close enough to the RAM edge that memory-reducing rewrites matter even
  when they are not pure CPU wins.

### G1. Full rewrite: geocode-relevant way extraction store

This is the strongest new direction.

**Bottleneck.** The builder currently processes way blobs twice. Pass 1.5
wire-scans every way blob to discover referenced node IDs. Pass 2b rereads and
decompresses way blobs, constructs `PrimitiveBlock`s, classifies ways again,
resolves coordinates, and writes the street/interpolation/building/admin
outputs.

**Why the current structure causes it.** Pass 1.5 only persists the
referenced-node bitset. It throws away exactly the information Pass 2b later
needs: which ways were geocode-relevant, their tags, and their node refs.
That keeps the PBF itself as the implicit way scratch store, forcing a second
full way pass.

**Stronger redesign.** Make a compact geocode-relevant way store the spine of
the command:

- Relation pass produces `needed_admin_ways`.
- A single way-extraction pass wire-scans way blobs once.
- For every relevant way, append a scratch record containing kind flags
  (street/building/interp/admin), way ID, needed tag strings or local string
  references, delta-varint node refs, and input-order metadata.
- While writing that scratch, populate the referenced-node set.
- Node pass fills the coordinate store.
- A resolver reads the relevant-way scratch in order, resolves coordinates,
  writes `street_ways.bin`, `street_nodes.bin`, `interp_nodes.bin`, building
  address points, and admin geometries.
- If practical, the resolver also emits Pass 3 cell-bucket records directly
  from resolved street/interp geometry, so Pass 3 Stage A stops rereading the
  generated street/interp files.

**Why it is plausibly high-payoff.** It deletes the current full Pass 2b PBF
decode path, which is the largest single measured phase (~129 s). It also
sets up a follow-on deletion of part of Pass 3 Stage A. The rewrite uses
scratch disk instead of RAM, which matches the command budget better than
keeping more way state resident.

**Risks.** The scratch record must preserve the same output ordering and
string interning semantics as the current blob-sequence merge. Building
address centroids and admin way geometries must produce byte-equivalent
geometry or be validated through reader-level query tests. Scratch volume may
be large, but unlike RAM it is explicitly allowed; make records compact and
sequential.

**Classification.** Full coherent pass 1.5 + pass 2b rewrite, potentially
growing into a pass 1.5 + pass 2b + pass 3A rewrite. This is the first large
rewrite to try.

### G2. Full rewrite/local hybrid: DenseNodes wire scanner for Pass 2a

**Bottleneck.** Pass 2a uses `parallel_classify_phase`, which decompresses
node blobs and builds `PrimitiveBlock`s. The phase then does only two simple
things: if a node is in the referenced set, write `(lat, lon)` into the compact
coord array; if it has address tags, write an address point.

**Stronger redesign.** Replace the node `PrimitiveBlock` path with a DenseNodes
wire scanner:

- Resolve `addr:housenumber`, `addr:street`, and `addr:postcode` string-table
  indices once per blob.
- Walk DenseNodes packed IDs/lat/lon/tags directly.
- For every referenced node, write coordinates by rank or by whatever coord
  addressing G1 ultimately chooses.
- For address-tagged nodes, materialize only the strings needed for the
  `AddrPoint`.

**Why it is plausibly high-payoff.** Pass 2a is ~65 s at planet. The command
does not need generic element objects for this phase, and the existing
Pass 1.5 way scanner already proves command-specific wire scanning is viable.

**Risks.** DenseInfo/tag boundary handling must stay aligned with node rows.
The current code only handles `Element::DenseNode`; a wire scanner should
either support the same effective input envelope or fail loudly on unsupported
non-dense node forms.

**Classification.** Intrusive local-to-Pass-2a rewrite. Lower strategic value
than G1, but likely simpler to validate.

### G3. Full rewrite/local hybrid: relation wire scan + one metadata schedule

**Bottleneck.** Pass 1 relations cost ~42 s and use the generic
`ElementReader`/`Relation` abstraction. The command only needs a narrow slice
of relation data: boundary/admin tags, names/postcodes/country code, way
members, and member roles.

**Stronger redesign.**

- Replace Pass 1 with a relation-blob schedule plus direct protobuf scanner.
- Resolve relevant tag keys once per relation blob.
- Extract only relation IDs/tags and way members needed for admin/postal
  boundaries.
- Fold relation, node, and way schedule construction into one metadata walk
  so the separate schedule phase disappears.

**Why it is plausibly high-payoff.** It attacks ~42 s of relation work plus
the ~6.7 s schedule phase, and it removes another generic decode path from the
builder. It is less important than G1 because it does not change the core
node/way dependency, but the measured phase is no longer "tiny."

**Risks.** Relation member role parsing must match current behavior exactly
for `inner` versus outer. Admin/postal boundary filtering must preserve the
same string interning order or the validation strategy must move from
byte-for-byte output diffs to semantic reader checks.

**Classification.** Medium-to-large rewrite. Best after G1/G2 unless relation
profiling becomes the governing phase.

### G4. Full rewrite: packed, bounded Pass 3 Stage B finalizer

**Bottleneck.** Pass 3 Stage B reads bucket files into memory, parses each
15-byte disk record into padded Rust structs, sorts all 256 buckets in
parallel, then serially groups and writes final cell/entry files. This costs
~57 s at planet and is the governing RSS peak.

**Stronger redesign.**

- Use a naturally aligned 16-byte bucket record: `cell_id: u64` plus a packed
  `type/index/segment` payload.
- Process buckets with a bounded worker set, not all buckets resident at once.
- Prefer radix/two-level partitioning on `cell_id` over comparison sort if
  bucket sizes stay large.
- Emit completed bucket groups through an ordered final writer so global
  cell-id monotonicity is preserved without retaining every parsed bucket.

**Why it is plausibly high-payoff.** This targets both wall and peak RSS. It
reduces parse overhead, removes the padded-struct expansion, lowers live
bucket memory, and should eliminate the observed swap risk on 26-28 GB hosts.

**Risks.** The current 15-byte record saves disk space; a 16-byte record grows
scratch by ~6.7 %. That is acceptable if it lowers CPU/RSS, but it must be
measured. The writer must preserve byte-compatible final files and exact
per-cell grouping.

**Classification.** Full Pass 3 finalizer rewrite. High value after G1, and
possibly the first rewrite if the immediate goal is more RAM headroom.

### Target end-state pipeline

The cleanest end state I currently see is:

1. One header/schedule walk plus relation wire scan.
2. One way-extraction wire pass to scratch + referenced-node set.
3. One DenseNodes wire pass to fill the coordinate store and write node address
   points.
4. One relevant-way scratch resolver to write street/interp/building/admin
   outputs and emit cell buckets directly.
5. One packed bounded Stage B finalizer for fine/coarse cell indexes.
6. Admin polygon assembly/index write and header/string file write.

This is still an external-join style command because ways define coordinate
demand, nodes provide coordinates in ID order, and output/query structures are
way/cell ordered. The current pass boundaries are replaceable; the data
dependencies are real.

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

## Suggested ordering

If picking up this note cold:

1. **G1 relevant-way extraction store.** It attacks the largest measured phase
   and deletes the duplicate way PBF pass.
2. **G4 packed bounded Stage B finalizer.** Do this first only if the immediate
   problem is RAM/swap headroom; otherwise do it after G1 so the finalizer can
   consume any new direct cell-bucket stream.
3. **G2 DenseNodes wire scanner.** Simpler than G1, but it leaves the duplicate
   way pass intact.
4. **G3 relation wire scan + one metadata schedule.** Worth doing after the
   hotter node/way paths, or earlier if relation profiling becomes dominant.

## Former "leave alone" guidance, revised

- **The ~16 GB anon `coord_mmap`.** Sized by geocode's filtered
  `referenced_count` - only nodes referenced by geocode-relevant ways.
  At planet this is well below ALTW's total unique-referenced count
  (~10 B, measured 2026-04-16 when an ALTW reshape OOM'd at Europe
  with a 29 GB coord table). Geocode's tag-filter pre-narrowing is
  what keeps this structure viable in RAM; do not copy this pattern to a
  command that touches **all** way refs. It remains acceptable as the first
  coordinate store for G1/G2. It is not sacred: if the relevant-way scratch
  rewrite changes referenced breadth or if Stage B memory remains the limiting
  peak, re-measure `referenced_count` and revisit the coordinate addressing
  scheme.
- **`PrimitiveBlock` in Pass 2.** Old guidance said to leave it alone. The
  unconstrained review reverses that for hot phases: Pass 2a should become a
  DenseNodes wire scanner (G2), and Pass 2b should be deleted by the
  geocode-relevant way store (G1).
- **Pass 1 (relation scan).** Old guidance treated it as too small. Current
  planet numbers put relations around ~42 s plus a separate schedule walk.
  It is not the first rewrite, but a relation wire scanner plus one metadata
  schedule (G3) is now a credible follow-up.
- **Output file formats.** The reader-visible final format should stay stable
  unless there is a reader/product reason to bump it. Build-time scratch
  formats are completely replaceable. In particular, G1's relevant-way store
  and G4's packed cell-bucket records should not preserve old temp-file shapes
  for their own sake.

## Invariants to preserve

- **Sorted + indexed PBF precondition.** Enforced at entry via
  `require_indexdata`; sorted-PBF node-before-way invariant is what
  makes Phase 2a/2b a clean barrier.
- **Disjoint rank ranges across node blobs.** Phase 2a writes to
  `coord_mmap` concurrently via `CoordMmapShared::write_coord` without
  atomics; correctness depends on `IdSet::rank(id)` (formerly
  `IdSetDense`) being unique
  per set ID + sorted PBF guaranteeing each ID in at most one blob.
  `debug_assert!` in `write_coord` catches bounds regressions.
- **Bucket-order cell_id monotonicity.** Pass 3 Stage B asserts
  cell_id monotonicity across buckets; buckets partition by top 8
  bits of cell_id so bucket N's min > bucket N-1's max by
  construction.
- **Relevant-way scratch ordering.** G1 may replace Pass 2b, but the final
  `street_ways.bin` / `interp_ways.bin` indexes and all `SegmentRef` records
  must remain internally consistent. For byte-for-byte validation, emit in the
  same blob/way order as the current ordered merge. If a rewrite deliberately
  changes ordering, use semantic `Reader` validation instead of raw directory
  diff.
- **String pool determinism.** Current output string offsets are assigned by a
  single `StringPool` as relation/node/way records merge. Parallel scanners
  must either carry raw string bytes into an ordered intern phase or accept
  non-byte-identical output and validate semantically. Do not intern through a
  shared mutex in the hot scanner path as the default design.
- **Zero-coord sentinel in way coord resolution.** `(lat == 0 &&
  lon == 0)` reads drop as "missing" (both in Pass 2b's coord_slice
  read and in the sequential predecessor). A real node at Null
  Island is silently dropped - `KNOWN LIMITATION` comments mark the
  sites; fix shape is a presence bitmap alongside the coord array.
- **Scratch formats are not reader formats.** G1 and G4 can change temporary
  record layouts freely. Reader-visible files listed in `format.rs` should
  remain stable unless there is a product reason to bump `FORMAT_VERSION`.

## Cross-validation

There's no `brokkr verify` for the geocode index. During the arc we
used byte-for-byte `diff -r old_index/ new_index/` on Denmark between
commits - works because every landed commit preserved blob-sequence
ordering via `parallel_classify_phase`'s ReorderBuffer or equivalent.
For the narrower follow-ups, the same check still applies: if output order is
preserved, diff works. For G1/G2/G3/G4 rewrites that intentionally change
ordering, string offsets, or scratch finalization, use `Reader` query results
on fixed coordinate samples plus file-structure invariants rather than relying
on raw directory diff.
