# ADR-0026: Interpolation ways are read as OSM tags them

Date: 2026-11-05
Status: Accepted

## Decision

The geocode builder treats any way with `addr:interpolation` and no
`addr:housenumber` as an interpolation way; `addr:street` on the way is not
required. Its street is its own `addr:street` when present, else the one
non-empty street that the address points at its first node and at its last
node share. The street is chosen before any number is read, and none or
more than one refuses the way.

Every node of the way, not just its ends, is looked up against the address
table by coordinate (ADR-0024). A node whose same-street addresses carry
one positive decimal number is an anchor; consecutive anchors bound a
section, and each section is written as its own `InterpWay` record over
its slice of the way's nodes (the split node is shared, no copy). An
interior node with one non-numeric number is passed over. Equal numbers
across a section, a wrong-parity `even` or `odd` section, disagreeing
strings at any node, an all-digit `0` or overflow, or a node ref Pass 2
could not resolve refuse the whole way, which keeps one zero-sentinel
record. A section whose anchors are adjacent numbers is dropped without
refusing the way. The index format is unchanged; record counts are
sections, not OSM ways.

## Context

The classifier required `addr:street` on the way, a shape carried over
from traccar-geocoder (`process_interpolation_way`, `if (!street)
return;`), whose design began from TIGER-style ranges on the street way.
The OSM wiki (Addresses, Interpolation) puts the street and numbers on the
end nodes and says the way "doesn't take addr:street=*", so the conforming
ways - most of the planet's roughly 2M - were dropped. Only the two end
nodes were read, so a way with an address node inside it (10, 20, 40) was
interpolated 10..40 over its whole length; the wiki and Nominatim
(`osmline_update`) treat each stretch between address nodes as its own
range. A closed outline with one address at its closing node resolved to a
one-number range answered along its whole perimeter.

## Alternatives considered

- Store one record per way with an anchor list and interpolate piecewise
  in the reader (a format bump). Not taken: a section record answers every
  query the anchor list would, and the reader needs no change.
- Take the street from the first end node carrying one, as Nominatim's
  `get_interpolation_address` does (in query order). Rejected: at a corner
  node with two streets it picks one arbitrarily; requiring a single shared
  street refuses instead of guessing.
- Read `addr:place` in place of `addr:street`. Not taken here: address
  points ignore `addr:place` too, so it is an address-wide policy, and
  interpolation follows whatever address points carry.
- Numeric steps (`addr:interpolation=2` and so on). Not taken: the record
  has no step value; such ways stay unresolved.
