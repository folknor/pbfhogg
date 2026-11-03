# ADR-0022: Cover admin edges in the point-in-polygon geometry; keep the reader's interior bypass off

Date: 2026-11-03
Status: Accepted
Supersedes 0015

## Decision

The geocode builder's admin cells are a conservative cover of the boundary
the reader's point-in-polygon test actually has (each ring's planar edges in
`geo::pip_ring_frame`, its implicit closing edge, and for an unwrapped ring
the antimeridian over the ring's latitudes), computed by the same
lat/lon-linear path cover the street cells use, followed by the existing
flood fill from the inside neighbours of the edge cells. A cell is flagged
interior only if it is not an edge cell and its centre is inside, so a
flagged cell lies wholly inside its polygon as the reader's test defines
it. The reader still runs point-in-polygon on every candidate and ignores
the flag: it applies the entries of all nine neighbourhood cells to a query,
and a flag describes its own cell, not the query; and indexes built before
this cover carry flags on cells a boundary runs through, with nothing in
the file to tell them apart. Trusting the flag needs an index-level signal
that its flags are sound plus a reader that consults only the query's own
cell; that is a format change and a separate decision.

## Alternatives considered

- Keep the sampled segment cover and wait for an S2 polygon `Region`
  (ADR-0015). Rejected: the sampler missed cells clipped near a corner
  (about one street segment in six at level 17), and a `RegionCoverer`
  covers geodesic edges, not the planar lon/lat edges the reader tests, so
  it could not make the flag sound without a second dilation anyway.
- Cover polygon edges with `RegionCoverer` and keep the flood fill. Rejected
  for the same geometry mismatch: geodesic and planar edges part by
  kilometres on long admin edges.
- Make the reader's point-in-polygon periodic in longitude instead of
  covering the antimeridian. Rejected for now: it changes answers for
  winding rings, where covering the seam changes none.
- Port the Traccar builder (S2 loop covering, interior covering, erosion).
  Rejected: its spherical geometry does not match the planar reader test;
  its erosion exists to absorb that mismatch, which an exact planar cover
  does not have.
