# ADR-0015: Defer the S2 admin region coverer rewrite

Date: 2026-08-20
Status: Accepted

## Decision

Keep the bounded segment cover plus boundary-adjacent interior flood fill for
admin indexing until the Rust S2 dependency can represent an OSM multipolygon
as a `Region` without a second geometry implementation. Do not enable the
reader's interior-cell point-in-polygon bypass while this design remains.
Revisit this decision when the selected S2 crate supplies polygon and loop
regions usable by `RegionCoverer`, or when pbfhogg adopts a different S2
implementation that does. The rewrite must then ship with antimeridian, holes,
disconnected components, and independent coverage fixtures before removing the
level 19 build cap or trusting the interior flag.

## Alternatives considered

- Implement a local `Region` over pbfhogg's polygon representation now. Rejected
  because containment, cell intersection, holes, and antimeridian behavior would
  become a second geometry engine whose correctness burden is larger than the
  bounded producer it replaces.
- Cover only polygon edges with `RegionCoverer` and retain the flood fill.
  Rejected because it would not make the interior flag sound or remove the
  guessed interior construction, so it does not deliver the structural benefit.
- Port the Traccar implementation directly. Rejected because its geometry and S2
  APIs do not match the Rust crate, and a transliteration would hide rather than
  eliminate the semantic gap.
