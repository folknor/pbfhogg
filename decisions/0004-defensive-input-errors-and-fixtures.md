# ADR-0004: Defensive input handling via boundary errors + fixture coverage

Date: 2026-04-24
Status: Accepted

## Decision

When a site reads a producer-controlled field that drives a
pre-allocation, a pointer-arithmetic step, or a structural
assumption, verify it at the boundary and surface a specific error
naming the offending field and blob. `debug_assert!` is fine for
per-element invariants where release-mode cost would be visible; it
is *not* fine for per-blob or one-shot invariants where the
release-mode failure mode is silent data corruption.

When a new defensive check lands, a regression test exercising the
malformation pattern lands in the same commit if feasible; otherwise
an entry is added to `tests/cluster2_defensive_input.rs`'s TODO list
for the byte-level fixture helper that would make the test writable.

## Alternatives considered

- **(a) Defend every read.** Safest but most expensive. Per-element
  checks on hot paths would measurably regress planet-scale runs,
  and most of the value is captured by the (b) subset - hot-path
  reads that are currently unchecked are typically either already
  defended upstream or are of a type (e.g. tag string table
  lookups) where a defensive check is as expensive as the decode
  itself. Rejected: too much cost, too little marginal defensiveness
  over (b).
- **(b) alone, no fixtures.** Fixes the five current findings but
  doesn't solve the audit-problem - the NEXT five unspotted
  sites get found only when a user reports a panic or silent bad
  output. Rejected: no CI coverage for the pattern.
- **(c) alone, no runtime changes.** Leaves known failure modes
  live in the field until a user triggers them. Rejected: fixtures
  without fixes is just "document how it breaks today."
- **(d) hybrid** *[chosen]* - fixes the five runtime holes now,
  starts building the fixture infrastructure so subsequent audits
  benefit from CI coverage.
