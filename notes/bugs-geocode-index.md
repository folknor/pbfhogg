# Bug hunt: geocode index (`src/geocode_index/`)

Hunt date 2026-08-12. Single Opus hunter, scope: format.rs, builder/, reader.rs,
mod.rs. Findings only, unverified.

## Pass 2a ignores non-dense nodes; a legitimate PBF silently yields an empty index

`src/geocode_index/builder/pass2.rs`, Phase 2a closure matches only
`Element::DenseNode(..)`. `Element::Node` is a first-class variant
(`src/read/elements.rs`) that the rest of the codebase handles everywhere (cat,
getid, tags_filter, degrade, extract). A PBF encoding nodes as
`PrimitiveGroup.nodes` (osmconvert output, hand-built fixtures, any producer that
does not dense-pack) gives: zero coordinates written into the rank coord array,
so every way's refs resolve to the (0,0) sentinel, `coords.is_empty()`, every way
dropped; zero address nodes. Build "succeeds", writes a valid header, exits 0.
The only guard is the build-time smoke test, and it is gated on
`addr_point_count > 0`, which is exactly 0 in this case - so not even the warning
fires.

Confidence: high. Fix is one match arm plus an unconditional "0 streets and 0
addr points" hard error.

## Admin interior coverage collapses for any polygon whose vertex-average is outside it

`pass3.rs::admin_cells_for_polygon`: interior cells come solely from a flood-fill
seeded at the arithmetic mean of the exterior ring vertices, and
`if !point_in_polygon(centroid) { return entries; }` - i.e. only the edge cells
survive. Vertex-mean is not a centroid and is outside the polygon for any
strongly concave or archipelagic boundary (Norway, Croatia,
Denmark-with-islands, most coastal municipalities, any relation with several
outer rings where one ring is a thin strip). Result: reverse-geocoding a point
deep inside such a boundary returns no admin match at that level, silently, while
a point near its edge does. There is also no fallback seed and no diagnostic
counter for "polygon got 0 interior cells".

Confidence: high on the mechanism, high that it bites real data. The hunter calls
this the single largest correctness hole in the scope; a real fix is a proper
interior seed (point-on-surface / label point, or seed from every edge cell's
inside-neighbours), not a tweak.

## `cover_segment` silently under-covers long segments and silently duplicates on short ones

`pass3.rs`. Three defects in one function:

- `step_deg` is a hardcoded `match level { 17 | 14 | 10 | _ => 0.001 }`.
  `--street-level`, `--coarse-level`, `--admin-level` are settable per run, and
  `BuildConfig::validate` only range-checks and orders them - it says nothing
  about the step table, so any non-default level gets a 0.001 degree step (~110 m)
  regardless of the cell size at that level. At level 20 (~10 m cells) coverage
  is essentially random.
- `steps` is clamped to 256, so max reliably-covered segment length is
  `256 * step_deg`: ~0.077 degrees (~8.5 km) at level 17, ~1.28 degrees at level
  10. Longer segments are sampled coarser than a cell, so intermediate cells are
  dropped with no diagnostic. At the admin level this matters most, because
  `assemble_one_relation` Douglas-Peucker-simplifies country rings down to
  `max_admin_vertices` (default 500) - a country ring at 500 vertices has
  segments hundreds of km long, so its edge-cell ring is full of holes.
- The dedup buffer is a fixed `[u64; 8]`; once a segment touches more than 8
  distinct cells, `seen` stops growing and cells get `emit`ed repeatedly.
  Duplicates flow straight into the bucket records and end up as duplicate
  `SegmentRef`s in a cell's entry list (inflating the per-cell u16 count and
  query work). Harmless to answers, wasteful, and it makes the "1-4 cells per
  segment" comment load-bearing when nothing enforces it.

## Cell levels read off disk are unvalidated in `Header::from_bytes`

`Header::from_bytes` reads the three level bytes out of the index file with no
range check. `s2::CellID::parent(level)` computes `1 << (2 * (30 - level))`, so
`level > 30` underflows `MAX_LEVEL - level` - panic in debug, silent garbage cell
IDs in release. A corrupt, truncated or hand-edited header therefore propagates
that panic into every `Reader::query`. Nothing enforces `coarse_level <
street_level` on the read side either, and the query path parents fine cells to
the coarse level, which is only meaningful when coarse is the shallower of the
two.

This is a different trust boundary from the builder: `BuildConfig::validate` now
rejects out-of-range and mis-ordered levels at build time, but the reader must
not trust bytes it did not write. Range-check the levels at parse time and fail
with a format error rather than a panic.

Confidence: high.

## Adversarial index files can panic the reader (integer overflow to OOB index)

`reader.rs` `U32EntryIter::new` / `SegmentEntryIter::new` do
`let off = offset as usize; if off + 2 > mmap.len()`. `offset` is a `u64`
straight off disk. For `offset >= usize::MAX - 1` the add wraps in release
(`off + 2` == 1), the bounds check passes, and `mmap[off]` panics with
index-out-of-bounds; in debug it panics on the add. Only `NO_DATA_U64` is
special-cased. Same shape in `read_record::<N>(mmap, offset)` where `offset`
derives from `way.node_offset as usize + segment_index * 8`. The reader is
otherwise carefully bounds-checked, so this reads as an oversight, not a policy.
Use `checked_add` / `offset.try_into()`.

## Header written last, everything else first - a crashed `--force` rebuild leaves a mismatched index

`builder/mod.rs`: the guard only refuses when `geocode_header.bin` exists and
`--force` is absent. With `--force`, all 18 data files are recreated in place
while the old header remains on disk. If the build dies anywhere (ENOSPC, OOM,
SIGKILL - all realistic at planet scale), the directory now pairs the previous
run's header (counts, cell levels, replication sequence, search radii) with this
run's partial data. `Reader::open` validates magic and version only; it never
cross-checks `geo_cell_count` etc. against file lengths. A reader opens it
happily and answers wrong. Fix: unlink the header first thing (or build into a
temp dir and rename), and have `Reader::open` verify each file length against the
header counts.

## Stage B's parallelisation defeats the bucketing it parallelises

`pass3.rs::run_stage_b` collects `sorted_buckets: Vec<Vec<ParsedBucketEntry>>`
over all 256 buckets before the serial group-and-write loop. The module doc and
`builder/mod.rs` both claim the design exists so it can "process one bucket at a
time" instead of "accumulating all cell entries in RAM (~19 GB at planet)". It
accumulates all of them, and in a struct that is 16 bytes padded vs 15 on disk,
so peak RAM is higher than the shape being avoided - twice (fine tree, then
coarse tree). This is a documented invariant that the code violates. The right
shape is a bounded-concurrency pipeline: parse and sort a small window of buckets
ahead of the writer, dropping each as it is consumed.

## `--max-admin-vertices 500` is applied uniformly from municipality to country

`admin.rs::assemble_one_relation` simplifies every outer ring to the same vertex
cap. For an admin_level=2 country, that is a boundary displaced by kilometres,
and the reader's `admin_polygon_contains` runs point-in-polygon against exactly
that geometry - so country attribution near borders is wrong by the DP tolerance,
permanently, with no record of the error magnitude. A per-level or per-area cap
(or a tolerance in metres rather than a vertex count) is the correct knob.

## `into_result` documents a postcode fallback that does not exist

`reader.rs`: "postcode fallback from nearest address if no postal boundary".
Nothing in `into_result` looks at postal boundaries or backfills a postcode.
Either implement it or delete the sentence - `Candidates::into_result` is the
public ranking contract.

## Coarse fallback is all-or-nothing

`Reader::query` only descends to the coarse tree when both street and address are
missing. A rural point with a street 40 m away but the nearest address 300 m away
returns street-only; the coarse address (and therefore its postcode) is never
looked at. Same asymmetry in `candidates()`. Interpolation results are never a
trigger at all. Worth stating as intended or fixing; today it is neither
documented nor obviously deliberate.

## `search_admin_all` dedup is O(n^2) on a `Vec` and only records successful polygons

`reader.rs`: `seen.push(poly_idx)` happens inside the `contains` branch, so a
polygon that fails PIP is re-fetched and re-PIP'd once per neighbouring cell (up
to 9x) for every query. Cheap fix, but the shape (linear scan of a Vec used as a
set) is the smell.

## Two nearly identical search implementations

`search_street_level_best` and `collect_candidates` are the same loop with
different accumulators, and `Reader::interpolate` / `resolve_interpolation` are a
verbatim duplicate of the interpolation math. Both pairs have already drifted in
small ways (the `best.X.is_none() || dist_sq < ...` redundancy exists only in
one). Collapse to one traversal parameterised by a sink trait.

## Smaller / cross-cutting

- Antimeridian (cross-scope, `src/geo.rs`). `approx_distance_sq` subtracts raw
  longitudes with no +/-180 degree wrap. Every geocode distance - address, street
  segment, interpolation, and `segment_length_radians` - is therefore wrong by
  ~2 pi radians for candidate pairs straddling the antimeridian, so they are
  silently dropped from results. `point_in_polygon` does handle the antimeridian
  (`point_in_ring_with_antimeridian`), which shows the codebase knows about the
  problem; the distance path was just missed.
- NaN / out-of-range query coordinates are not rejected by `Reader::query`; they
  flow into `LatLng::from_degrees` and produce arbitrary cell IDs rather than an
  error.
- `read_nul_string` returns `""` for out-of-bounds offsets and for invalid UTF-8
  and for the genuinely-empty string. A corrupt string pool renders as blank
  names with no signal anywhere.
- `read_addr_point` / `read_street_way` / `read_interp_way` /
  `read_admin_polygon` return zeroed default records on out-of-bounds rather than
  `None`. A truncated `addr_points.bin` therefore yields address matches at
  lat/lon 0 with empty strings instead of an error - silent wrong answers built
  on top of a silent read failure.
- `u32` offset fields with hard-error guards but no headroom.
  `AdminCell.entries_offset` and `AdminPolygon.vertex_offset` cap
  `admin_entries.bin` / `admin_vertices.bin` at 4 GiB. The builder guards these
  properly (`write_admin_data`, `write_admin_index` - genuinely careful code),
  but the guard is a build failure at planet scale, not a mitigation. Since
  `GeoCell` was already widened to u64 in FORMAT_VERSION 2, widening these two in
  the same spirit costs one version bump and removes a class of "planet build
  dies at minute 18".
- `format.rs` - the doc comment "Sentinel value for 'no data' in u32 offset
  fields" is orphaned above `NO_DATA_U64`; the u32 constant it described is gone.
  `GEO_CELL_SIZE`'s section header still says "24 bytes per record" while the
  constant is 32; `StreetWay` says "14 bytes per record" (correct) but
  `AdminPolygon` says 22 with a reserved byte that is undocumented in the header
  table. Minor, but this file is declared "the authoritative on-disk format
  definition".
- `interp.rs::CellAddrCsr.offsets` is `Vec<u32>` with `values.len() as u32` - an
  unchecked truncating cast. It is currently safe only because `addr_point_count`
  is itself u32; that coupling is undocumented.
- `pass1.rs` treats any relation member role that is not literally `"inner"` as
  outer, including `"subarea"`, `"label"`, `"admin_centre"` and empty. Subarea way
  members (rare but present) get spliced into the outer ring assembly and can
  corrupt a boundary. `libosmium`'s multipolygon assembler restricts to
  outer/inner/empty.
- `pass3.rs` Stage A chunking is asymmetric: streets chunk at 100k ways and addrs
  at 500k points, but interpolation ways are collected in a single unbounded
  `par_iter().collect()`. Inconsistent memory bound for no stated reason.
- `assemble_admin_polygons` drops relations silently: if `assemble_rings` returns
  empty (unclosed boundary, missing member ways), the relation vanishes with no
  counter and no warning. At planet scale you cannot tell 100 dropped boundaries
  from 0.

## The way-coordinate collection still spells absence as `(0, 0)`

Filed 2026-08-20 from the spatial/locations round, which fixed the same defect in
`add-locations-to-ways`. `builder/pass2.rs` reads the rank coord mmap and drops
the pair when `lat == 0 && lon == 0`, so a way vertex genuinely at Null Island is
discarded from the geometry - and, in the other direction, an unresolved
reference is indistinguishable from a real one. This is now the last coordinate
index in the crate that does it; `CORRECTNESS.md` "Null Island ambiguity in
coordinate indexes" and `docs/guide/correctness.md` both scope their remaining
entry to this site plus `apply-changes --locations-on-ways`.

The comment at the filter site used to justify itself with "a proper fix would be
a presence bitmap; not worth the extra pass". That argument is now dead: the
sparse ALTW store solved it with no extra memory and no extra pass by storing
latitude biased by 90 degrees, which puts every legal latitude below `2^31` and
frees bit 31 of the low word as an explicit presence flag. The slot here is the
same eight bytes (`lat: i32`, `lon: i32`), so the same encoding drops straight
in - see `pack` / `unpack` in `src/commands/altw/sparse.rs`. The zero-filled mmap
still reads as absent, so nothing else about the pass changes.

Note the interaction with the first finding in this document: while Pass 2a
ignores non-dense nodes, every ref resolves to the sentinel, so fixing that one
first makes this one's blast radius visible rather than universal.

## What the hunter would rewrite rather than patch

The admin-interior, `cover_segment` and `max-admin-vertices` findings are one
problem wearing three hats: admin coverage is built from ad-hoc sampling and a
guessed seed instead of a real S2 region coverer.
`research/traccar-geocoder/` is cited as the precedent and the reader's own
comment admits the interior flag is untrustworthy because of this - the PIP-skip
optimisation is disabled, permanently, to compensate. Replacing `cover_segment`
plus centroid flood-fill with `s2::RegionCoverer` over a proper loop/polygon
region would fix under-coverage, kill the duplicate emissions, make the interior
flag sound (re-enabling the PIP skip), remove the level-hardcoded step table, and
let the DP cap be a metric tolerance instead of a vertex count. That is the
high-payoff structural change in this scope; everything else here is repair work.
