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
ranking ties (since fixed: candidates rank by the query's total order) and
building placement at the antimeridian (BUG-027).
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

## GEO-028 - Unwrapping a winding ring is quadratic in its winding

Reported by: spar on the longitude-unwrap termination fix (lateral).

`unwrap_ring_longitudes` (`geo.rs`) keeps its shift-a-turn-at-a-time loops
for in-range rings, because stored indexes were covered in exactly that
rounding sequence (a closed-form turn count differs at rounding boundaries).
Each vertex costs one iteration per turn the ring has drifted, so a legal ring
repeating `[120, -120, 0]` 100,000 times costs about 15 billion shifts. Only
reachable with an admin ring winding the pole thousands of times; a fix must
keep the legacy frame bit-identical or bump the index format and rebuild covers.

## GEO-029 - Interpolated house numbers are measured to the quantized snap

Reported by: the GEO-009/GEO-010 spar.

`reader.rs` computes an interpolation way's partial length up to the snap
point after it has been truncated to decimicrodegrees, in both the ranked
and the candidate paths, rather than as prefix length plus `t` times the
segment length. The truncation can move a result across a house-number
threshold. Passing `t` through would number the projected point itself.

## GEO-030 - A polar winding admin ring ranks by its raw-frame area

Reported by: the GEO-009/GEO-010 spar.

The ranking score is taken in the containment frame only for a ring with
zero net longitude winding. A ring winding a pole lifts to an open path
whose closing chord, and so both its lifted area and its containment, depend
on the start vertex (GEO-020), so it keeps the raw shoelace. A meaningful
polar ranking needs the polar interior defined first, with containment,
cover and ranking derived from it.

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
