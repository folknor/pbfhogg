# Geocode defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.
```

Defects from the 2026-11-17 defect hunt in the geocode index builder, the
index format and reader, and `geo.rs`. Every finding was made by one codex
hunter per scope reading source at `63d4ec1d`. None was built, run or
reproduced. "Reported by" names the scope or scopes, and the bracketed
priority is the hunter's own label. Findings that match an existing mechanism
were added to `notes/bugs.md` instead of being filed here: builder tag
truncation (since fixed: block validation refuses a tag string index past
the table), candidate
ranking ties (BUG-026) and building placement at the antimeridian (BUG-027).
Entries are in no order of importance.

## GEO-005 - The builder can publish an index with nothing queryable

Reported by: geocode-builder [P2].

The empty-index gate (`builder/mod.rs`) counts interpolation source records
before resolution. Use two untagged nodes and one way tagged only
`addr:interpolation=all`, with no streets, addresses or admin polygons. The
interpolation record passes the gate, resolution keeps it with zero endpoints
(`builder/interp.rs`), the reader correctly refuses to answer from it, and the
build succeeds anyway. This contradicts the gate's own comment and the promise
in `docs/cli/commands.md` to refuse an index with nothing queryable. The gate
should judge usable output after resolution.

## GEO-006 - A negative replication timestamp wraps to an enormous positive one

Reported by: geocode-builder [P2], geocode-reader [P2].

`build_generation` (`builder/mod.rs`) casts the header's signed timestamp
straight to `u64`. A timestamp of -1 becomes `18446744073709551615`, is
published, and is returned unchanged by `Reader::replication_timestamp()`.
This breaks the claim in `docs/guide/advanced.md` that this metadata describes
the input header. The builder checks representability for the neighbouring
replication sequence but not for the timestamp. Either keep signed timestamps
(a format change) or refuse values the format cannot represent.

## GEO-007 - Public geometry calls can hang while unwrapping longitudes

Reported by: geocode-builder [P2].

`unwrap_ring_longitudes` (`geo.rs`) repeatedly adds or subtracts 360. With an
infinite longitude subtraction never changes the value, and a large finite
longitude such as `1e308` behaves the same because subtracting 360 rounds back
to itself. `point_in_ring_with_antimeridian` with the ring
`[(0,0), (INFINITY,0), (0,1)]` never returns, and `point_in_polygon` and
`simplify_ring` reach the same code. These public signatures document no
finite-coordinate precondition and no nontermination. The builder's own
coordinates cannot reach this, so it is a public API fault. Fix: validate the
domain or use bounded normalization.

## GEO-008 - Integer longitude normalization overflows on its accepted type

Reported by: geocode-builder [P2].

`wrapped_longitude_delta_e7` (`geo.rs`) adds half a turn before reducing
modulo a turn. `i64::MAX` overflows that addition: with overflow checks it
panics, and without them it returns the wrong longitude, because wrapping
modulo `2^64` changes the later remainder. `normalize_longitude_e7` inherits
the problem, and its documented job is folding an unwrapped `i64` longitude
with no narrower precondition. Fix: reduce modulo the full turn before adding
the half turn. Current builder callers stay within a safe range.

## GEO-009 - Admin ranking area and containment use different longitude frames

Reported by: geocode-builder (flagged as surprising; the documented policy
defines ranking by stored planar area, so the hunter notes that changing it
would change policy).

The area calculation (`builder/admin.rs`) uses raw longitudes, while
containment unwraps antimeridian crossings. A wider dateline-crossing
rectangle can therefore store a smaller ranking area than a narrower one.

## GEO-010 - Snap and distance can describe opposite half-world paths

Reported by: geocode-reader [P2].

`reader.rs` rebuilds the snap point from an integer longitude delta, which
always resolves an exact 180-degree separation westward. The distance
calculation wraps independently converted radians and can resolve the same
separation eastward. The builder (`builder/cover.rs`) explicitly acknowledges
this difference and covers both paths. In an eastward case, a query at the
segment midpoint can get a distance of about zero but a reported snap 180
degrees away. This breaks the meaning of `snap_lat_e7` and `snap_lon_e7`, and
the function's own requirement to interpolate in the distance calculation's
frame. Projection should return the actual closest point, or both calculations
should share one longitude-frame representation.

## GEO-014 - Candidates carry no binding to the reader that produced them

Reported by: geocode-reader (flagged as an API hazard).

`Candidates::into_result(reader)` and `Reader::interpolate(candidate)` accept
candidates from another reader or another index generation without detecting
the mismatch. They can combine a street and distance from generation A with a
house number computed from generation B's unrelated record at the same index.
The required provenance relationship is neither encoded nor documented.
Binding candidates to their originating reader would also keep the polygon
identity that BUG-026 in `notes/bugs.md` needs.

## GEO-015 - Pass 2a reports the last error of a blob, not the first

Reported by: the codex spar of the geocode coordinate-domain work.

`run_pass2`'s node worker (`builder/pass2.rs`) records a failure in
`state.error` and continues the blob, so a later failure in the same blob
(a conflicting duplicate coordinate, an out-of-range rank) overwrites an
earlier one, such as the out-of-domain refusal. The build still fails and
publishes nothing; only the reported cause can be the wrong one. Keep the
first error (or stop the blob at it).

## GEO-017 - Assembled admin rings are not checked against crossing or shared edges

Reported by: the ring-assembly rewrite (node-ID biconnected blocks).

Rings join by node ID and ring sets must be provably disjoint within one
connected graph, but two shapes still pass unvalidated. Rings that cross, or
touch somewhere other than a shared node (a bow-tie built from distinct node
IDs at crossing segments), are never tested against each other. And an edge
shared between the outer and inner roles, or between two relations' ways,
under different way IDs is not detected, because the both-roles refusal keys
on way ID only. Either can publish a polygon whose even-odd interior is not
what the relation describes. A segment-intersection pass over the assembled
rings (or a planar-embedding validation) would close both.

## GEO-018 - Open ways sharing a side between two holes are refused as a theta

Reported by: the ring-assembly rewrite.

Two inner rings built from open ways that share one side form a theta graph:
a biconnected block with a degree-3 vertex, which the assembler refuses as
ambiguous, withholding the whole relation. The relation is well formed (two
adjacent holes), so this is an over-refusal, the same trade as the other
ambiguous-topology refusals. Face parity on a validated planar embedding
would publish it; the current design deliberately refuses instead.

## GEO-019 - hole_inside_outer is sensitive to tangency and ring start

Reported by: the ring-assembly rewrite.

The hole-ownership vote (`hole_inside_outer`, 2-2 vote among sampled
vertices) can lose when a hole vertex lies on the outer ring's edge, and the
outcome can depend on which vertex the hole ring starts at. Equal rings in a
different rotation can then be assigned to a different outer or to none. Use
a vote that excludes boundary vertices or a sample that is rotation
invariant.

## GEO-020 - Polar admin rings depend on member order through the closing edge

Reported by: the ring-assembly rewrite.

The reader frames a ring that winds around a pole by closing it with an edge
that depends on where the ring starts, so member order still changes the
answer for such rings. The assembler now keeps the start deterministic (the
earliest source edge) but the dependence is in the reader's framing, which
predates the rewrite. The framing should be start-invariant.

## GEO-021 - The geocode verifier drops absent member ways where the builder withholds the relation

Reported by: scout lateral in the ring-assembly rewrite; not verified.

The verifier's `AdminModel` skips member ways that are absent from the input,
while the producer withholds the whole relation when any member way or node
is missing. On an extract the model can therefore expect an area the index
does not hold. Not confirmed against a real extract; check the model before
trusting an extract-based verify run.

## GEO-022 - The coarse-level dangling-reference refusal has no test

Reported by: the reader corrupt-entry refusal work (commit `8d473cc3`).

Lookup now refuses a dangling entry reference with a `QueryError` at the
first read. The existing fixture damages fine and coarse entries together,
and both lookups fail at the first bad fine entry, so the refusal on the
coarse level (`reader.rs`, the coarse `search_level` path) is never
exercised. A test needs a query point with no fine hit whose coarse list
holds a dangling reference; the synthetic builder inputs do not give one
easily.
