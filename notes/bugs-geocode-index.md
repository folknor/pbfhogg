# Bug hunt: geocode index (`src/geocode_index/`)

Hunt date 2026-08-12. Single Opus hunter, scope: format.rs, builder/, reader.rs,
mod.rs. Findings only, unverified.

## What the hunter would rewrite rather than patch

Status: deferred by `decisions/0015-defer-s2-admin-region-coverer.md` until the
selected S2 implementation supplies polygon and loop regions usable by
`RegionCoverer`, or pbfhogg adopts one that does.

The admin-interior, `cover_segment` and `max-admin-vertices` findings are one
problem wearing three hats: admin coverage is built from ad-hoc sampling and a
guessed seed instead of a real S2 region coverer.
`research/traccar-geocoder/` is cited as the precedent and the reader's own
comment admits the interior flag is untrustworthy because of this - the PIP-skip
optimisation is disabled, permanently, to compensate. Replacing `cover_segment`
plus centroid flood-fill with `s2::RegionCoverer` over a proper loop/polygon
region would fix under-coverage, kill the duplicate emissions, make the interior
flag sound (re-enabling the PIP skip), remove the level-hardcoded step table, and
let the DP cap be a metric tolerance instead of a vertex count. That is the
high-payoff structural change in this scope; everything else here is repair work.

Round 1 bounded `cover_segment`'s work rather than rewriting it: the 256-step
cap that silently under-covered is gone, and the unbounded sampling that
replaced it is now bounded by capping the configurable S2 levels at 19, derived
from a per-segment budget of 2^24 samples and the widest separation two legal
coordinates can have. That is a stopgap with a proof attached, not a fix. The
sampler is still sampling; a `RegionCoverer` would make the level cap
unnecessary, because an exact traversal's cost scales with the cells actually
covered rather than with the cell width.

Round 2 additionally taught the sampler about the antimeridian: it unwraps the
far endpoint around the near one before stepping, so a segment from 179.9E to
179.9W is sampled as the 0.2-degree segment it is instead of walking 359.8
degrees the wrong way around the globe. `MAX_SEGMENT_SPAN_DEG` of 405 is now a
loose over-estimate rather than a tight bound - the true worst case after
unwrapping is about 255 degrees - which is conservative in the safe direction
and was left alone deliberately. A `RegionCoverer` would retire the constant
along with the sampler.

## Cross-reference: Douglas-Peucker topology (open, and worst here)

`notes/bugs-spatial-locations.md`, "Ring simplification can change polygon
topology", is a spatial-locations finding whose worst consumer is this scope:
the geocode reader runs point-in-polygon against exactly the rings DP
simplified, so a self-intersection introduced by simplification resolves a
query to the wrong administrative area. That entry carries the precise
producer, knob, consumer, blast radius and the missing fixture. It is listed
here so a later round of *this* document does not rediscover it from the
reader's end.
