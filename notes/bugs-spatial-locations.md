# Bug hunt: spatial and node-location resolution

Hunt date 2026-08-12. Single Opus hunter, scope: `src/geo.rs`, `src/scan/`,
`src/commands/spatial.rs`, `src/commands/extract/`, `src/commands/altw/`, and
`src/idset.rs` as used from this scope. Findings only, unverified.

## The geocode index builder still filters way coordinates on `(0, 0)`

Out of scope for this document and re-filed where it will be worked: see
"The way-coordinate collection still spells absence as `(0, 0)`" in
`notes/bugs-geocode-index.md`. Kept here only as a pointer, because this is
the third site of the same defect and the first two were fixed from this
document.

## The PBF read path does not validate coordinate ranges

Filed 2026-08-20, alongside the bbox domain check. `parse_bbox` now rejects a
latitude outside -90..90 or a longitude outside -180..180, and the sparse node
store refuses to index a node whose coordinates are outside the same domain. The
general read path (`read/elements.rs`, the dense node iterators) still hands out
whatever the file says. Closed-document territory - recorded here because the
bbox work made the asymmetry visible, not as a request to change the read path.

## `multi-extract` compresses single-threaded

`extract/multi.rs` uses a sync-mode `PbfWriter` on the consumer thread, so all
outputs are compressed serially by one thread. A switch to the parallel writer
was measured in round 2 and reverted because two interleaved Denmark 5-region
pairs had opposite signs: parallel/sync was 1200/1100 ms, then 501/1101 ms.
The result is inconclusive rather than evidence for a speedup. A larger-scale
design needs a stable matched measurement rather than assuming the
single-region writer shape transfers.

## Duplicated pread consumer drains

`pread_execute` (`extract/common.rs`) duplicates its entire consumer body twice
(in-loop drain and post-loop drain); `multi_extract_pread_write` does the same.
Both are copy-paste that will drift.

Left open in round 2 because it is a maintenance refactor with no reproduced
correctness failure, and changing both ordered drains during the final
correctness round would add risk without fixing a demonstrated wrong answer.

## Ring simplification can change polygon topology

Douglas-Peucker can introduce self-intersections that flip containment near the
boundary, which matters because simplified admin rings are what the geocode
reader uses for point-in-polygon queries.

The recursion and stack-overflow defect was fixed in round 2 with explicit
stacks. This topology sub-point remains open because no topology-preserving
replacement or biting real caller-order fixture was established in this round.

Precise scope, so this does not have to be rediscovered:

- **Producer.** `src/geo.rs` Douglas-Peucker, called from
  `geocode_index/builder/admin.rs::assemble_one_relation`, which simplifies
  every outer and inner ring of every admin relation.
- **Knob.** `--max-admin-vertices`, default 500. It is a *vertex count*, not a
  distance tolerance, so the effective simplification epsilon is whatever it
  takes to reach 500 vertices. A country boundary is therefore simplified far
  more aggressively than a municipality, which makes a topology flip most
  likely exactly where the ring is longest. This is the same finding as the
  uniform-cap entry in `notes/bugs-geocode-index.md`, seen from the other end.
- **Consumer, and why it matters most here.**
  `geocode_index/reader.rs` runs `geo::point_in_polygon` against the simplified
  rings for every admin result. A self-intersection near the boundary makes the
  even-odd crossing count wrong on one side of the fold, so a query resolves to
  the wrong administrative area - a wrong answer, not a missing one, and one
  that no validation in the reader can detect because the ring is well-formed
  as a byte sequence.
- **Blast radius.** Bounded to points within the simplification error of a
  boundary, which for a country-scale ring at 500 vertices can be kilometres.
- **What a fix needs.** Either a topology-preserving simplifier (reject a
  candidate removal whose new segment crosses another segment of the same or a
  sibling ring), or a metric tolerance in place of the vertex cap so the error
  is bounded in metres rather than in vertices. The structural rewrite proposed
  in `notes/bugs-geocode-index.md` - an `s2::RegionCoverer` over a real loop
  region - would let the cap become a tolerance, which is why the two are
  worth doing together.
- **What is missing to act.** A fixture: a ring whose DP simplification at a
  given cap provably self-intersects, plus a query point whose containment
  flips. Round 2 did not establish one, and without it any fix is unfalsifiable.

## Round-2 closing notes (2026-08-20)

**A fifth exact-versus-conservative bypass existed.** Round 2 separated
`BboxInt::from_bbox` (conservative, blob selection) from
`BboxInt::inside_bbox` (exact, point membership) and fixed three consumers.
Enumerating every consumer of `BboxInt` afterwards found two more:

- `extract --strategy smart`'s columnar node classifier
  (`collect_matching_ids_bbox`) still ran against the widened box - the exact
  counterpart of the multi-extract classifier that round 2 did fix. Confirmed
  by revert: the node one decimicrodegree outside a 55.6 boundary reappeared.
- `export --bbox` converted the user's box to `BboxInt` and rounded every
  feature coordinate into decimicrodegrees to test it. Both sides are f64
  degrees, so it now compares in degrees and does not round-trip at all.

The derivation itself was also still wrong one layer down: `bbox / 1e-7` is not
the inverse of `point_e7 as f64 * 1e-7`. `55.6 / 1e-7` is exactly `556000000`,
whose forward conversion is strictly below `55.6`. Bounds are now walked until
the forward conversion satisfies the requested comparison, and `from_bbox` is
derived from `inside_bbox` by widening one unit, so the superset relation holds
by construction instead of by argument.

**The missing-node guard was counting the wrong thing.** `loc_missing` was
`needed - coord_pairs_extracted`, and that counter increments per extracted
tuple - including when the per-worker map's `insert` replaces an existing id.
A base carrying one node twice therefore paid for a genuinely absent one.
Resolution is now read from the merged map's unique key count
(`barrier_loc_map_size`). The sibling counter in the external
`add-locations-to-ways` backend was checked and is NOT exposed: it counts way
ref slots on both sides of the subtraction, and the tuple pointer stops at the
first record for an id, so a duplicate base record adds nothing.
