# ADR-0021: A broken internal invariant fails in release builds

Date: 2026-11-03
Status: Accepted
Supersedes 0005

## Decision

An internal invariant, one that holds by construction of pbfhogg's own
code and that no input can make false, is checked in release builds, never
only under `debug_assert!`. Each site gets the first of these that fits:

1. **Make the type carry it.** Where a type, a constructor that owns its
   precondition, an enum that cannot express the illegal combination, a
   const generic with a compile-time check, a safe slice in place of
   unchecked pointer arithmetic, or a const assert can make the violation
   unrepresentable, do that and leave no runtime check.
2. **Return `Error::internal`** where the function already returns
   `Result` and the failure ends the run without leaving corrupted state
   that anything reuses, checked before the write it guards.
3. **Panic in release** otherwise (`assert!`, or one inlined branch onto a
   `#[cold]` helper with a static message on a per-element path). Never
   panic in `Drop`: a panic during unwinding aborts the process and hides
   the original failure, so a drop path settles its invariant structurally.
4. **Drop the check** when it restates what the lines above it already
   make true, with a comment saying why it holds, so it is not re-added.

Anything input can make false is not an internal invariant: it is an input
refusal under ADR-0004, never a panic in any build. `debug_assert!` is kept
only where a matched planet A/B shows the release check costs more than
noise on normal input, and the site's comment records that measurement.
Checks that guard a library caller's API discipline rather than pbfhogg's
own state (a writer dropped without its flush) are not invariants and stay
test-only. This applies to every production site; reconsider the
debug-only exception only for a site with such a measurement.

## Alternatives considered

- **Keep ADR-0005's default of `debug_assert!` plus a comment.**
  Rejected: a debug assertion is compiled out of every run that sees real
  data (every bench, verify and planet run is a release build), so it only
  ever fires on small test fixtures, and a release build carries on past
  the violation and writes a wrong answer. An audit of every site found
  several "invariants" that were input-reachable checks living only in
  debug builds, and at least one release panic and one silent wrong output
  behind them.
- **Release `assert!` at every site.** Rejected as the default: where the
  function returns `Result`, an `Error::internal` fails the run the same
  way without a panic, and where a type can carry the invariant, a runtime
  check of any kind is the weaker form.
- **`debug_assert!` wherever the check sits on a per-element path.**
  Rejected: measured on planet, the per-element checks (the lockstep ends
  of the parallel-array iterators, one branch per element walk) cost
  nothing measurable; cost is a measurement, not a presumption from where
  the check sits.
- **Promote every check to an input refusal.** Rejected: an internal
  invariant broken is a pbfhogg bug, not bad input, and dressing it as a
  refusal misleads the user about what to fix; ADR-0004 keeps refusals for
  what input can actually cause.
