# ADR-0027: Geocode addresses name a street or a place

Date: 2026-11-08
Status: Accepted

## Decision

An address point (a node, or a `building` way) has `addr:housenumber` and a
non-empty `addr:street`, a non-empty `addr:place`, or both. `AddrPoint`
stores both names (`street_offset`, `place_offset`, offset 0 absent), and
the reader answers them in separate `street` and `place` fields of
`AddressMatch`, each an `Option`. An empty value is no name: an address
whose names are all empty is not indexed. One function in Pass 2
(`address_names`) applies this to nodes and buildings; the classifier and
the Pass 1.5 wire scanner test presence only, so Pass 1.5 collects a
superset of the refs Pass 2 uses.

An interpolation record numbers along one parent, a street or a place
(`InterpWay.parent_offset` and `parent_kind`). An address is on street `S`
when its street is `S`, and on place `P` when its place is `P` and it names
no street: an address with a street is numbered along its street. A way's
parent is its own non-empty `addr:street`, else its own `addr:place`, else
inferred from its end nodes: the one street their addresses share; if they
share none, the one place their place addresses share. More than one at the
deciding tier, or none, refuses the way. Streets match by name alone.

`addr:interpolation` digits `1` to `9` step by that number (`1` is `all`),
with no parity, as Nominatim reads them. `InterpolationType` owns the
numbering rule for builder and reader alike: two anchors agree when they
are positive, different, and of the declared parity for `even`/`odd`; a
section answers when they agree and at least one number lies strictly
between them; the number at a point is the nearest of the grid `lo, lo +
step, ...` (up to `hi`) and `hi` itself, measured from the lower anchor,
ties to the lower number. A numeric step's anchors need not share a grid.
Wire values: 0 `all`, 1 `even`, 2 `odd`, 3 to 10 the steps 2 to 9.

`addr:substreet` is not read. Format version 5.

## Context

Address points required `addr:street` since the first builder, after
traccar-geocoder, so an address OSM tags with `addr:place` (common in
villages without named streets) was absent, and an interpolation way between
such addresses had nothing to number along. Nominatim maps `addr:place` and
`addr:substreet` to its `place` field and accepts either a street or a place
when an interpolation way inherits its address. Numeric steps had no wire
value, so such ways stayed unresolved. Format version 4 was unreleased, so
version 5 costs no extra rebuild.

## Alternatives considered

- Store the place in the street field (no format change). Rejected: a
  consumer cannot tell a street from a place, and a street and a place of
  the same name would join one interpolation range.
- One name per address with a kind byte, street winning over place.
  Rejected: an address naming both would lose its place, which the query
  could then never answer, and inference could no longer tell ends that
  share a street from ends that share only a place.
- Let a place hold every address naming it, street or not. Rejected: a
  place can hold several street numbering systems, so ends on North Road and
  South Road in one village would form a range, and an interior address on
  another street would split a place range.
- Require exactly one shared name across streets and places. Rejected: it
  refuses the common double tagging (`addr:street` and `addr:place` on both
  ends), where the street is the numbering parent.
- Anchor the step grid at the way's first node. Rejected: reversing the
  geometry would change which numbers exist.
- Read `addr:substreet` as a place. Not taken: it would lose its provenance,
  and a third name slot is not justified without measured use; substreet-only
  addresses stay unindexed.
