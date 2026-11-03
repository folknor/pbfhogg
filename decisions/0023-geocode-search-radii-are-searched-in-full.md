# ADR-0023: A geocode search radius is searched in full, by the reader

Date: 2026-11-03
Status: Accepted

## Decision

The fine and coarse search radii in a geocode index header are promises:
`Reader::query` answers each class with the nearest record closer than the
fine radius, else the nearest closer than the coarse radius, wherever in the
radius it lies, and `Reader::candidates` lists every record closer than the
radius. The reader used to read only the query's cell and its eight
neighbours at each level, which reach about one smallest cell width (some
46 m at the default street level 17, 367 m at the default coarse level 14),
less than the default radii (75 m, 1000 m): a record inside the radius but
outside those cells was passed over, for a farther one or for nothing.

The reader keeps the index format and finds the cells itself:

- It reads the 3x3 block first and computes a proved lower bound on the
  reader's distance from the query to any point outside it. A class whose
  bound (its best so far, else the radius) is within that is complete, which
  is the ordinary case.
- Otherwise it walks outward from the block over edge neighbours, reading a
  cell while a lower bound on its distance (from s2's padded `rect_bound`) is
  below some class's bound. The points nearer than a bound form a connected
  open set, so the walk reaches every cell that can hold one; the exact cell
  cover of ADR-0022 lists each record under the cell holding its nearest
  point.
- In `query`, a class the fine block holds nothing of is left to the coarse
  search when the coarse radius is at least the fine one: the nearest within
  the coarse radius is then also the nearest within the fine radius when
  there is one there.
- Before and inside the coarse search, a class is dropped where no coarse
  cell under the 3x3 block of much wider cells around the query could hold
  one (interpolation tested against the coarse cells that list
  interpolation, kept in memory from open; the other classes against all
  coarse cells).
- The segment distance is the true minimum over the segment's path: the
  query's longitude image a turn away is tried where it can be nearer, and
  only a segment of no length is replaced by its ends.

Because the work a query does grows with the square of radius over cell
width, a radius over `GEOCODE_MAX_RADIUS_CELL_WIDTHS` smallest cell widths at
its level is refused by the build and by `Reader::open`.

## Alternatives considered

- Declare the radius a cutoff ("nearest within the nine cells"). Rejected:
  a nearer record inside the stated maximum distance is silently passed over,
  and on real data the gap was large: on uniform points over Germany the old
  reader answered 8.4 % of streets and 10.4 % of addresses wrongly (nothing,
  or a farther one), mostly at the coarse level.
- Dilate the builder's cover by the radius so the reader reads one cell.
  Rejected: entry files grow several-fold (an address point goes from one
  cell to about ten at the fine level and twenty at the coarse level), the
  build slows, it needs a format signal, and existing indexes stay wrong.
- Change the default levels or radii so the nine cells cover them, and
  refuse wider radii at build time. Rejected: it leaves every existing index
  wrong and still needs a proof of the nine cells' reach in the reader's
  metric, which the block bound above now provides anyway.
- Walk every cell within the radius on every query. Correct, and the first
  version measured; it cost up to 27 % per query on points with nothing near
  (open sea), which the block bound and the presence test remove for every
  query that has its answer nearby.
