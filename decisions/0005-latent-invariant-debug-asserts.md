# ADR-0005: Cheap insurance for latent-only invariants

Date: 2026-04-24
Status: Accepted

## Decision

For an invariant that is correct under current callers, constants,
and production inputs but would regress silently on a future refactor
or pathological input, each site is read in context and the
disposition is one of three:

1. **Add `debug_assert!` + one-line invariant comment** if the
   invariant is local (at most a small code region), silent on
   violation, and the assertion is tight enough to catch the class
   of refactor the author worried about.
2. **Add a doc comment only** if the invariant is structural (e.g.
   "single-thread use only" on a helper with no caller-visible
   mechanism to enforce it), or if the panic-on-violation is
   already loud (existing `.expect`, already-panicking `assert!`,
   documented in a load-bearing comment).
3. **Drop from the cluster** if the finding is not actually a
   latent invariant. Perf degradation on pathological sizes and
   known-accepted coordinate-math limitations get recorded
   elsewhere: perf notes in `reference/` or
   `notes/`, limitations in `DEVIATIONS.md`. They don't become
   `debug_assert`s because violating them isn't a correctness bug.

Default to disposition (1). Escalate to (2) only when the invariant
isn't expressible as a local check. Drop to (3) only when the
finding turns out not to be a latent invariant in the first place
(perf, pathological input). No blanket sweep: each site gets its
own reading and its own disposition.

## Alternatives considered

- **(a) Comments only, accept regression risk.** Rejected because
  the comments were already in place at most of the identified
  sites and did not prevent the findings from being flagged again
  in the sweep. Adding a single line of `debug_assert` above the
  comment converts "silent corruption" to "test-suite failure"
  for zero release-build cost.
- **(c) Promote everything to runtime `Err`.** Rejected because
  release cost accumulates (one check per blob, per way, per
  relation at some sites) and ADR-0004 already handles the
  adversarial-input boundary. These invariants are internal;
  runtime enforcement is over-scoped.
- **(d) No policy.** Rejected for the same reason as ADR-0003's
  (a) vs (b) split: inconsistency across commands is a real audit
  cost, and a shared posture lets future findings of the same
  shape be disposed of by matching the pattern rather than
  re-arguing the question.
