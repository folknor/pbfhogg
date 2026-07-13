# ADR-0010: Fix GeoJSON export framing and area semantics

Date: 2026-07-13
Status: Accepted

## Decision

`pbfhogg export` defaults to newline-delimited GeoJSON Feature objects with
no record-separator byte, while `--format geojson` emits a wrapped
FeatureCollection. Every feature carries numeric `@id` and string `@type`
properties. OSM tags that collide with emitted identity or metadata names
are suppressed, and the first occurrence of a duplicate source tag wins.

A closed way is a Polygon only when it has at least four references, is not
tagged `area=no`, and is tagged either `area=yes` or with one of these keys:
`building`, `landuse`, `natural`, `leisure`, `amenity`, `boundary`, or
`waterway`. Polygon geometry must contain a closed exterior ring of at least
four positions and is emitted counterclockwise. Invalid way geometry is
skipped and counted. These rules intentionally do not claim osmium parity,
as recorded in `DEVIATIONS.md`.

## Alternatives considered

- RFC 8142 record separators were rejected because the primary consumers are
  line-oriented Unix tools and osmium's `geojsonseq` output is newline-only.
- Osmium's configurable area rule set was rejected for the first version in
  favor of a small, stable rule that can later become configurable.
- Omitting identity properties was rejected because stable feature identity
  is needed for joining and round-tripping exported data.
- Preserving colliding source tags was rejected because duplicate JSON object
  names have ambiguous consumer behavior.
- Emitting malformed or clockwise polygon rings was rejected because RFC 7946
  gives downstream consumers clear validity and winding expectations.
