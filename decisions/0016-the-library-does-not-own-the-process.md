# ADR-0016: The library does not own the process

Status: Accepted

## Context

Embedded callers share process state and must isolate concurrent runs. Path
choices are only known inside the library, so the CLI cannot reproduce policy
by applying flags before calling it.

## Decision

Process state and shared-host policy belong to the caller: allocator settings,
descriptor limits, input page-cache eviction, the rayon global pool, framing
panic disposition, environment access, warnings and stderr. The library calls
explicit caller-supplied hooks at the original program points. Scratch file
eviction remains library scratch management.

Per-call resources derived from the host are supplied through `Host`, which has
no default. Fixed pool sizes and byte budgets remain constants. `Host` has
public fields without `non_exhaustive`; adding a field breaks struct literals,
which is accepted before 1.0.

## Alternatives

Grant flags leave policy inside the library. CLI pre-application cannot know
header-dependent paths. A global logging facade cannot isolate runs. Framing
`catch_unwind` would take panic disposition away from the pool owner. A
per-allocation scratch counter adds unnecessary work; use a per-operation token.

## Consequences

The CLI supplies legacy process actions at the original points. Library callers
choose their process actions and resources explicitly. Diagnostics, cache
policy, execution and scratch placement belong to each invocation.
