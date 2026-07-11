# ADR-0007: Injected prepass wire extensions

## Status

Accepted, 2026-07-11.

## Decision

`add-locations-to-ways --inject-prepass` may emit two private, opt-in PBF
extensions alongside `LocationsOnWays`:

- BlobHeader field 5, `pbfhogg.WayMembers-v1`, contains a versioned bitmap
  of ways that belong to multipolygon or boundary relations. It is a
  deliberately cheap superset, not an exact relation plan.
- Way field 20, `pbfhogg.SharedNodePins-v1`, contains an LSB-first bitmap of
  reference positions that must be retained by downstream simplification.

Presence of the advertised metadata is its validity signal. Producers emit a
field-5 payload for every way blob, including an all-zero bitmap, and omit
field 20 when no bit is set. A pin means the node occurs in at least two
non-closure positions and resolved to an actual location. Missing references
remain unpinned, preserving the existing zero-coordinate sentinel behavior.

The extension is opt-in. Normal ALTW output remains compatible with existing
flows. Commands that rewrite way payloads do not preserve these extensions;
`HeaderBuilder::from_header` intentionally does not copy optional features,
and the command layer warns before dropping enrichment metadata.

## Consequences

Consumers can avoid an equivalent membership and shared-node prepass when
they opt into these versioned fields. Files emitted with the flag are private
enriched artifacts and are not suitable for readers that reject large PBF
BlobHeaders, including affected libosmium versions.
