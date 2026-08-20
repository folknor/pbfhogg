# Bug hunt: geocode index (`src/geocode_index/`)

Hunt date 2026-08-12. Single Opus hunter, scope: format.rs, builder/, reader.rs,
mod.rs. Findings only, unverified.

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

  Round 1 narrowed but did not close this. `Reader::open` now validates every
  offset-into-a-file relationship in the format, so a file truncated *relative
  to its header count or to a referring offset* is rejected at open. What
  remains is the read functions' own habit of answering with a zeroed record
  instead of `None` for any index that gets past open validation - a
  defence-in-depth gap rather than a reachable one today, but it will become
  reachable again the moment a new record kind is added without a matching
  `validate_*` arm. The durable fix is to make the read functions return
  `Option` and force callers to handle absence, not to keep adding validation
  arms.
- `u32` offset fields with hard-error guards but no headroom.
  `AdminCell.entries_offset` and `AdminPolygon.vertex_offset` cap
  `admin_entries.bin` / `admin_vertices.bin` at 4 GiB. The builder guards these
  properly (`write_admin_data`, `write_admin_index` - genuinely careful code),
  but the guard is a build failure at planet scale, not a mitigation. Since
  `GeoCell` was already widened to u64 in FORMAT_VERSION 2, widening these two in
  the same spirit costs one version bump and removes a class of "planet build
  dies at minute 18".
- `format.rs` - the orphaned "u32 offset fields" doc comment above
  `NO_DATA_U64` was fixed in round 1. Still wrong:
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

Round 1 bounded `cover_segment`'s work rather than rewriting it: the 256-step
cap that silently under-covered is gone, and the unbounded sampling that
replaced it is now bounded by capping the configurable S2 levels at 19, derived
from a per-segment budget of 2^24 samples and the widest separation two legal
coordinates can have. That is a stopgap with a proof attached, not a fix. The
sampler is still sampling; a `RegionCoverer` would make the level cap
unnecessary, because an exact traversal's cost scales with the cells actually
covered rather than with the cell width.

## Cross-reference: Douglas-Peucker topology (open, and worst here)

`notes/bugs-spatial-locations.md`, "Ring simplification can change polygon
topology", is a spatial-locations finding whose worst consumer is this scope:
the geocode reader runs point-in-polygon against exactly the rings DP
simplified, so a self-intersection introduced by simplification resolves a
query to the wrong administrative area. That entry carries the precise
producer, knob, consumer, blast radius and the missing fixture. It is listed
here so a later round of *this* document does not rediscover it from the
reader's end.

## Round-1 closing notes (2026-08-20)

**The round's own fix opened a new hole, and the review caught it.** Removing
`cover_segment`'s 256-step cap was correct - it silently under-covered - but
removing it without bounding the work replaced silent under-coverage with
unbounded work, since the configuration accepted S2 levels through 30 and the
sample count doubles per level. Both are wrong. The bound now sits on the
level, validated before any input is read: the widest separation two legal
coordinates can have is a constant, so worst-case samples per segment is a pure
function of the level, and level 19 is exactly where that worst case meets the
2^24-sample budget. A unit test walks every level and asserts the boundary
lands where the constant says, so the constant cannot be raised without raising
the budget it is derived from.

**Two more defective tests, both in this round's own work.**

- *Thirty-fourth, species ASSERTING THE BUG AS CORRECT.* Found by the reviewer,
  not the author. `--force` removed the old header before `require_indexdata`
  and `ElementReader::open`, so a typo in the input path destroyed a valid
  index; the new test asserted that the header was gone, codifying the
  destruction as the contract. Invalidation moved to immediately before Pass 2
  - the first output mutation - and the test was replaced by two: one asserting
  a bad input leaves the index byte-identical and still queryable, one
  asserting a build that dies *after* invalidation leaves a directory that
  fails to open. The pair is what pins the contract; either alone is satisfiable
  by the wrong implementation.
- *Thirty-fifth, species COVERS ONLY THE VARIANT ITS AUTHOR IMAGINED.*
  `header_rejects_invalid_s2_levels_on_read` poked one of three level bytes and
  asserted only the range rule. The same fix added a coarse-versus-street
  ordering rule that no test touched: a header with `coarse >= street` is in
  range on every field, so the ordering check could have been deleted outright
  with nothing failing. Split into a range test over all three fields and a
  dedicated ordering test.

**The reviewer's guess that a fourth unvalidated offset existed was low.** There
were four more, not one: `AdminCell.entries_offset`, `StreetWay.node_offset`,
`InterpWay.node_offset` and `AdminPolygon.vertex_offset`, plus the whole
string-pool offset class. All are validated at open now, and the new test drives
one corruption per kind rather than one corruption.

**The presence encoding was re-derived, not reused.** The geocode builder
carried a verbatim copy of the ALTW sparse store's `pack`/`unpack` - same
constants, same layout, separate code. That is the drift risk the carry-forward
warned about, realised. The encoding now lives once in `src/coord_pack.rs` with
its layout, domain and Null Island tests, and both callers import it. A
divergent third copy is now a code review question rather than an invisible
corruption.

**Disconnected interior pockets are genuinely handled.** Seeding from every
inside neighbour of every edge cell reaches every interior region, because
every component of a polygon's interior is bounded by part of its boundary. A
two-lobe fixture confirms it empirically: restricted to a single seed the
builder produces 64 interior cells in the west lobe and 0 in the east.

**Null Island as a class is closed.** The geocode way-coordinate collection was
the last coordinate index in the crate spelling absence as all-zero. The blob
index's "no bbox" was retired earlier by v3's inverted box. The only remaining
`(0, 0)` is the ALTW `--ignore-missing-nodes` filler, which is opt-in, counted
and gated by a post-write check - a deliberate output, not an ambiguous
sentinel. `docs/guide/correctness.md` states the class closed.
