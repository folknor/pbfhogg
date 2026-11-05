# ADR-0024: An interpolation end takes the address on its node

Date: 2026-11-03
Status: Accepted; extended by ADR-0026 (a way without `addr:street` takes
its street from its end addresses, and interior address nodes split it)

## Decision

The geocode builder resolves an interpolation way's start and end numbers
from the address points on its first and last node and nowhere else: an
address point with the way's street name whose stored coordinate equals the
node's (longitude folded so +180 and -180 degrees are one meridian). OSM
draws an interpolation way between two address nodes, so its end nodes are
the addresses; coordinate equality is what node identity leaves behind in
the builder's address table, which holds no node IDs.

An end resolves when the same-street address points at it all carry one
house-number string and that string is a positive decimal integer that
fits a u32. A way resolves when both ends do, its `addr:interpolation` is
`all`, `even` or `odd`, and an `even` or `odd` way has both ends of that
parity. Any other way keeps the zero sentinel (CORRECTNESS.md) and never
answers a query. Disagreement is judged on the strings before parsing, so
`10` beside `10a` refuses rather than resolving to 10.

No cell level, search radius or distance takes part, so the result does
not depend on `--street-level`. The reader also refuses a wrong-parity
`even` or `odd` range where its search sinks admit a hit, for indexes
built before this rule. The index format is unchanged.

## Context

The resolver used to scan the end's street-level cell and its eight
neighbours for same-street address points and take the nearest by an
unscaled planar E7 distance, preferring one within 1 E7 unit. Its reach was
about one cell width and moved with the level: on Germany
(`germany-20260224-seq4704`) 71 of 78 ways resolved at level 17 and 77 at
level 16. Measuring every way showed what the extra reach bought: only
eight ways had an address on both end nodes; the other 70 were closed
outlines tagged `addr:interpolation`, of which 63 resolved at level 17 to
a one-number range borrowed from a house about 3 to 102 m away, several
to a number that was not even the nearest, because the planar metric
ignores the cosine of latitude. Leading-digit parsing turned `57a`..`57e` into 57..57 and
`18-20` into 18, and every mode other than `even` and `odd` was read as
`all`. An `even` 10..13 range answered 14 at its far end.

## Alternatives considered

- A stated metre radius, searched in full as the reader does since
  ADR-0023. Rejected: it makes the reach level-independent but keeps the
  inference that produced the borrowed numbers, and no radius is right:
  nothing in the data says a nearby address belongs to the end.
- Reuse the fine search radius. Rejected for the same reason, and it would
  tie what the index stores to a query setting.
- Keep the nine cells and document that ends must lie within about one
  street cell of their address. Rejected: it documents a silent wrong
  answer rather than removing it.
- Match the way's end node IDs. Not taken: address points carry no node
  ID, and with the same-street and disagreement rules no case was found
  where coordinate equality gives a wrong number that node identity would
  not (an untagged end node exactly on another same-street address takes
  that address, which is the address at that point).
- Keep the 1 E7 tolerance. Rejected: node identity implies equal stored
  coordinates, so the tolerance only ever admitted a distinct node.
