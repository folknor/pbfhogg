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
flows. Commands that rewrite way payloads do not preserve these extensions.
The output-header policy (`docs/cli/commands.md` "Output header")
declares them on a command's output only where every way blob is copied whole
from a single input with nothing added, and warns when it stops declaring
them. The fields are authoritative only in a file whose header declares them:
a splice path (apply-changes) can leave stale field-5 or field-20 bytes in a
file that no longer declares the feature, and a reader that forces their
parsing there reads bytes nobody vouches for.

## Consequences

Consumers can avoid an equivalent membership and shared-node prepass when
they opt into these versioned fields. Files emitted with the flag are private
enriched artifacts and are not suitable for readers that reject large PBF
BlobHeaders, including affected libosmium versions.
