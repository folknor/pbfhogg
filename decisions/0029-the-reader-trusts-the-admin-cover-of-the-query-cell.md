# ADR-0029: The geocode reader trusts the admin cover of the query's own cell

Date: 2026-11-20
Status: Accepted
Amends 0022 (its last sentence names this as the separate decision)

## Decision

Geocode index format 9 makes the admin cell lists a cover the reader relies
on, stated over the cell the reader assigns a query to rather than over a
cell's geometric footprint: every admin polygon whose point-in-polygon test
holds for a query is listed under that query's cell, a polygon occurs at
most once per cell list, and a set interior flag means the test holds for
every query assigned to the cell. Extra unflagged entries are allowed. The
rules are written at `AdminEntry` in `geocode_index/format.rs`.

The reader reads only the query's own admin cell, accepts a flagged entry
without a test and tests the rest. It keeps no attempted-polygon set in
either search, since one list cannot repeat a polygon. Open does not
certify the cover: doing so means recomputing it, and an index that breaks
it is corrupt in the same way as one with altered vertices.

ADR-0022's builder already meets these rules: its padded edge cover makes
every non-edge cell uniform under the test, and its flood fill from every
inside neighbour of every edge cell reaches each inside component. The
version bump is the signal ADR-0022 asked for. Format 8 indexes were in
fact all built by that builder, but other producers could also write the
format, and format 8 never promised the cover. The bump makes it a promise.

The cover's rounding is proven inside `PATH_COVER_UV_SLACK` only while a
ring's unwrapped frame stays within `ADMIN_FRAME_MAX_ABS_LONGITUDE_DEG`
(1e5 degrees). The builder drops a polygon with any ring past that bound
right after ring assembly, before the empty-index gate and before any
admin record is written, and warns; a record left in the index without
its cells would itself break the cover rules. Holes are rings of their
polygon, so a hole past the bound drops its whole polygon, never just the
hole.

Measured on plantasjen at `a7946048` (germany, 100,000 points in the
country box): about 130 us per query before, about 26 us after.

## Alternatives considered

- Keep nine cells and add the attempted-polygon set to the ranked path.
  Rejected: once the own cell is complete, the neighbours only add work.
- Trust the cover without a version bump, reasoning that every format 8
  index came from the ADR-0022 builder. Rejected: format 8 called the flag
  a hint. A producer that stored a hint, or listed a polygon only in the
  cells its edges touch, would have been answered wrongly with no change
  on its side.
- State the cover over closed geometric cells. Rejected: S2's point-to-cell
  rounding can assign a point just outside a cell's footprint to that cell,
  so a producer that covered footprints exactly could flag a polygon the
  point lies outside.
- A hierarchical admin index with prepared per-cell edge lists (format
  redesign). Not done here; it would further cut the containment test on
  boundary cells, the cost that remains.
- Refuse the whole relation when one of its rings passes the frame bound.
  Not done: the check runs on assembled polygons, after relation assembly,
  and the relation's other polygons are sound. Such rings do not occur in
  real boundaries.
- A prepared per-cell edge list evaluated with the crossing expression
  alone is not enough for that later redesign: the ray from a query also
  crosses edges outside the cell, so each cell would need a base parity
  per ring and per longitude retry.
