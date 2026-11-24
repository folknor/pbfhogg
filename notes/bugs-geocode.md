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
Each vertex costs one iteration per turn the ring has drifted, so a ring
repeating `[120, -120, 0]` 100,000 times costs about 15 billion shifts. The
geocode builder no longer reaches it: ring assembly withholds any relation
with a winding ring before anything unwraps it. What remains is the public
`geo` functions given such a ring, and the reader on an index built before
that refusal; a fix must keep the legacy frame bit-identical for those
indexes.

## GEO-029 - Interpolated house numbers are measured to the quantized snap

Reported by: the GEO-009/GEO-010 spar.

`reader.rs` computes an interpolation way's partial length up to the snap
point after it has been truncated to decimicrodegrees, in both the ranked
and the candidate paths, rather than as prefix length plus `t` times the
segment length. The truncation can move a result across a house-number
threshold. Passing `t` through would number the projected point itself.

## GEO-031 - Ring assembly's disjointness check depends on ring start

Reported by: the GEO-019/GEO-020 spar (codex, source inspection, not run).

`outside()` in `geo/rings.rs`, which decides whether two rings of one
connected graph are disjoint or refused as nested, returns on the first
eligible edge midpoint of the walk, even when that midpoint is on the other
ring's boundary. The walk starts at the earliest source edge, so member
order can choose between a boundary witness (refusal) and a strict one
(acceptance). Example: a square `(0,0)-(4,4)` and a ring through `(0,4),
(-1,1), (1,-1), (-3,-1), (-3,4)` sharing node `(0,4)`, split into open
members. The hole-ownership witness rule (sorted witnesses, boundary
abstains) would fix it the same way.

## GEO-032 - Ranking area of a crossing ring varies with its start at roundoff

Reported by: the GEO-019/GEO-020 spar (codex, reproduced arithmetic).

`ranking_area` takes an unwrapped ring's shoelace in f64 degrees about its
first vertex, so rotations of one ring can store areas a few square
decimicrodegrees apart (a zero-area wrapped bow tie scored 2.84 or 0 by
start), surviving the f32 cast. Equal-area ties then fall to polygon index.
An i128 shoelace on the integer lift, rounded once, would make it exact.

## GEO-018 - Open ways sharing a side between two holes are refused as a theta

Reported by: the ring-assembly rewrite.

Two inner rings built from open ways that share one side form a theta graph:
a biconnected block with a degree-3 vertex, which the assembler refuses as
ambiguous, withholding the whole relation. The relation is well formed (two
adjacent holes), so this is an over-refusal, the same trade as the other
ambiguous-topology refusals. Face parity on a validated planar embedding
would publish it; the current design deliberately refuses instead.

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
call. Valid topology already answers correctly, except a hole whose every
vertex and edge midpoint lies on its shell, which no witness can place and
is not owned.

## GEO-025 - Holes sharing an edge can both miss a point at roundoff scale

Reported by: the crossing-ring review (spar with codex).

Two closed inner rings that split one hole along a shared edge, wound the
same way, traverse that edge in opposite directions, and the reader's f64
ray intercept differs between the two directions. A point strictly inside
one hole, within about one ulp of the seam, can miss both, so the shell
matches. Within the tolerance contract but not exact; an exact or
direction-normalised edge intercept would close it.
