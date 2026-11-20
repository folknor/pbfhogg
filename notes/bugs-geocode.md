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

## GEO-026 - A collinear admin outer ring publishes and never contains a point

Reported by: spar on the empty-index gate (source inspection, not run).

A named admin relation with one closed outer way `A-B-C-A` whose three nodes
share latitude 0 at distinct longitudes, built with `--admin-tolerance 0`,
passes ring assembly (`geo/rings.rs`) and admin assembly (`builder/admin.rs`),
and the reader accepts its zero area. Every edge is horizontal, so the
containment test never toggles: the polygon counts toward the empty-index
gate and is listed in cells, yet answers no query. A plain zero-signed-area
filter is not the fix, since an accepted self-crossing ring whose lobes cancel
also has zero signed area but queryable lobes.

## GEO-027 - Zero search radii disable every distance match

Reported by: spar on the empty-index gate (source inspection, not run).

`--search-radius 0 --coarse-search-radius 0` is accepted (`format.rs` radius
validation), but the reader's strict distance comparison then rejects even an
exact hit, so an address- or street-only index publishes and can never answer.
Admin lookup still works, so refusing the pair at option validation would also
reject useful admin-only builds. Policy question: refuse the pair, refuse it
only when the output has no admin polygons, or document it.

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

## GEO-023 - A tangent exclave drawn by open ways withholds the relation

Reported by: the crossing-ring review (spar with codex).

Open outer ways share one graph, and `NestedRings` runs per role before any
inner exists. A valid mainland whose enclave hole holds an exclave, all
three rings meeting at one shared node, is withheld when the outers are
split into open ways: the exclave cycle sits inside the mainland cycle of
the same graph. Drawn as closed ways the same rings are never compared and
publish correctly, so coverage depends on where the mapper split a way.
Over-refusal, not a wrong answer. Closing it needs nesting judged across
roles (a containment forest with alternating outer and inner roles), not
per role.

## GEO-024 - Invalid admin ring topology is published under even-odd, not refused

Reported by: the crossing-ring review (spar with codex).

A self-crossing ring, overlapping outers of one relation, or a hole that
crosses its shell is published with a defined answer (even-odd parity per
ring, union of outers, owned holes subtracted; `docs/cli/commands.md`
states it), where libosmium refuses the area. Overlapping outers also rank
by each record's own area. Refusing instead is a policy change to the CLI
contract and needs, per the `geo/rings.rs` module doc: a containment
forest with alternating roles, cyclic edge order at shared nodes, seam
normalisation for touching holes, a periodic frame for seam contacts and a
subquadratic sweep, measured against the admin assembly phase. Maintainer's
call. Valid topology already answers correctly apart from the ownership
vote (GEO-019).

## GEO-025 - Holes sharing an edge can both miss a point at roundoff scale

Reported by: the crossing-ring review (spar with codex).

Two closed inner rings that split one hole along a shared edge, wound the
same way, traverse that edge in opposite directions, and the reader's f64
ray intercept differs between the two directions. A point strictly inside
one hole, within about one ulp of the seam, can miss both, so the shell
matches. Within the tolerance contract but not exact; an exact or
direction-normalised edge intercept would close it.
