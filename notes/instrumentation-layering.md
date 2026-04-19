# Instrumentation layering

## Problem

1.0 needs probes everywhere that tell us whether optimizations actually
work. Today the instrumentation story has a specific gap: anything that
needs to fire per-element in a hot loop either doesn't exist or gets
reverted the moment it shows up in wall time. The comment at
[`src/commands/time_filter.rs:147-152`](../src/commands/time_filter.rs)
captures the last time we hit this:

> Don't re-add per-element `Instant::now()` here: Japan scale is 344 M
> elements and the time-source overhead alone doubled wall in an earlier
> iteration of this instrumentation.

Today's single shape covers the rest well enough:

- `crate::debug::emit_marker("TIMEFILTER_HISTORY_START")` - runtime-gated
  through a `OnceLock<Option<File>>` on `BROKKR_MARKER_FIFO`. One
  atomic-ish load plus an `Option` branch per call. Disappears at block
  granularity (~8000:1 amortization), would wreck a 344M-element loop.

For the per-element case we need a compile-time gate. The secondary
problem is that commands today grow ad-hoc groups of related counters
and per-element state inline in business logic, with no canonical home
when a per-element probe needs to share state with an aggregated flush.

TODO.md has a related open item: the `emit_marker` / `emit_counter` /
`emit_mallinfo2` sink is hard-wired to a Unix FIFO, so library consumers
(PyO3 bindings, progress UIs, structured logging) have no idiomatic way
to subscribe. The resolution there is to migrate the backend to the
`tracing` crate. That migration is a single-file change in
[`src/debug.rs`](../src/debug.rs) and does not depend on this plan.

This plan covers where call-site instrumentation lives and what gets
gated at compile time - the shape of `probes` modules, empty-twin
conventions, parallel-path composition. What any given command chooses
to instrument is a per-command decision driven by concrete optimization
questions, not prescribed here. Function-attribute instrumentation
(hotpath) is a separate mechanism and not part of this plan.

## Proposal: structured instrumentation through `probes` modules

Every command that has structured instrumentation (aggregated counter
flushes, per-element probes, grouped state) gets a `probes` module next
to it. Counters emitted as a group, mallinfo snapshots, per-element
probes, aggregated end-of-run stats - all of it goes through typed
wrappers in that module.

**Markers stay inline.** `crate::debug::emit_marker("FOO_START")` is
just a string literal at a call site; wrapping it in
`probes::foo_start()` adds a layer that duplicates the literal inside
the wrapper body without hiding anything load-bearing. The marker
strings ARE the ABI (they appear in `.brokkr/results.db`,
`--durations`, sidecar logs), and grep-for-string jumping straight from
sidecar output to the call site is a real workflow that wrapping
breaks. Standalone `emit_counter` calls (not part of a group) follow
the same rule.

What goes in `probes`:

- Per-element probes with cfg-gated empty twins.
- Aggregated emissions (e.g. walking a stats struct to emit six
  counters at once) where the grouping itself is the thing worth
  naming.
- Grouped probe state that only exists inside one command (a
  `PerElementStats`-shaped struct the command's probes tick).

Why group these:

- One file per command lists the command's structured probes. Grep the
  `probes` module, see the per-element shape and the aggregated
  flushes.
- A cfg-gated probe is easier to read and maintain when its empty twin
  sits right next to the active variant.
- The `tracing` backend migration is backend-wide (single-file in
  `src/debug.rs`); it does not depend on wrapping call sites.

Cost: one probe module per instrumented command. Low maintenance
surface, paid once per probe.

## What gets cfg-gated

Cfg-gate only probes whose per-call cost would distort wall or RSS
measurements. Today that is per-element probes in hot loops. Everything
else - markers, singleton counters, mallinfo, per-phase aggregated
emissions - stays always-compiled and runtime-gated by FIFO presence,
same as today.

The reason is measurement integrity: `brokkr --bench` needs to measure
the same binary users ship. If we cfg-gated cheap probes, the default
build would have different wall/RSS from the measurement build, and we
would need a second measurement path to validate shipped targets.
Phase-boundary markers cost one OnceLock load per phase - below noise -
so compile-time gating them buys nothing and costs measurement fidelity.

One cargo feature, `instrument`, gates every per-element probe in the
tree. Per-command features (`timefilter-per-element`, `extract-per-way`,
...) were considered and rejected: enabling a probe in extract while
running time_filter costs nothing at runtime (the probe only fires
inside extract's hot loop), while per-command features would multiply
cargo's build cache entries and force brokkr and the user to track N
feature names instead of one flag. If a future workload needs
finer-grained control, split `instrument` into sub-features at that
point, named after the specific thing they gate.

## Shape of a probes module

```rust
// at the end of src/commands/time_filter.rs, or as a sibling file
mod probes {
    use super::TimeFilterStats;
    use crate::debug;

    // Aggregated emission: walks the struct, emits the group.
    pub(super) fn flush_stats(stats: &TimeFilterStats, is_history: bool) {
        debug::emit_counter("timefilter_versions_seen", stats.versions_seen as i64);
        debug::emit_counter("timefilter_versions_before_cutoff", stats.versions_before_cutoff as i64);
        debug::emit_counter("timefilter_elements_written", stats.elements_written as i64);
        // ...
        debug::emit_counter("timefilter_is_history_path", i64::from(is_history));
    }

    // Per-element probe state. Empty struct when the feature is off so
    // the hot-loop local declaration carries no runtime state.
    #[cfg(feature = "instrument")]
    #[derive(Default)]
    pub(super) struct PerElementStats {
        pub nodes: u64,
        pub ways: u64,
        // ...
    }

    #[cfg(not(feature = "instrument"))]
    #[derive(Default)]
    pub(super) struct PerElementStats {}

    // Cfg-gated per-element: empty twin on default builds.
    #[cfg(feature = "instrument")]
    #[inline(always)]
    pub(super) fn per_element(stats: &mut PerElementStats, el: &crate::Element<'_>) {
        // increment fields based on kind
    }

    #[cfg(not(feature = "instrument"))]
    #[inline(always)]
    pub(super) fn per_element(_: &mut PerElementStats, _: &crate::Element<'_>) {}
}
```

Call sites (markers stay inline, probe calls go through the module):

```rust
crate::debug::emit_marker("TIMEFILTER_HISTORY_START");
// ...
probes::per_element(&mut per_el_stats, &element);
// ...
probes::flush_stats(&stats, is_history);
crate::debug::emit_marker("TIMEFILTER_HISTORY_END");
```

LLVM drops empty twins to nothing on default builds. `_`-prefixed
params kill unused-variable warnings inside the probe module, where the
cfg split belongs. No `#[cfg]` at the business-logic call site.

## Tiers

| Tier        | Granularity              | In `probes` module | Cfg-gated          | Cost when not measuring                  |
|-------------|--------------------------|--------------------|--------------------|------------------------------------------|
| marker      | per phase                | no (inline)        | no                 | 1 OnceLock load                          |
| counter     | per batch / block        | no (inline)        | no                 | 1 OnceLock load                          |
| mallinfo    | per phase boundary       | no (inline)        | no                 | 1 OnceLock load + syscall guarded by env |
| aggregated  | per phase (struct flush) | yes                | no                 | N OnceLock loads, still amortized        |
| per-element | per element in hot loop  | yes                | yes (`instrument`) | 0 (empty twin inlined away)              |

Rule: a probe earns a `probes` module entry when it has structure - a
cfg split, grouped emission, or command-specific state. Plain
`emit_marker` / `emit_counter` calls with a single string argument stay
inline.

Cfg-gate only when the per-call cost shows up in wall or RSS.

## Conventions every command follows

These rules keep the pattern consistent across the tree and make the
ZST semantics load-bearing - so the cost-when-off claim actually holds
once a second or third command adopts the pattern.

**Module placement.** Nested `mod probes` at the bottom of the command
file. Promote to sibling `probes.rs` (or `probes/` submodule) when the
module grows past ~50 lines, acquires 3+ per-element probes, or the
command itself is split across a directory. Same rule used for any
other submodule extraction in the tree.

**Probe contents.** Only structured stuff: cfg-gated per-element probes
with their state, and aggregated counter flushes over grouped state
(walking a struct to emit a related group of counters). Markers and
singleton `emit_counter` calls stay inline in business logic.

**ZST shape for cfg-gated state.** Under `#[cfg(not(feature =
"instrument"))]`, the state struct is `struct Foo {}` (empty struct) -
**not** `struct Foo;` (unit struct). The empty struct is still a ZST
and `#[derive(Default)]` works without triggering
`clippy::default_constructed_unit_structs`.

**Parallel composition via `merge`.** Any per-element stats struct
used on a parallel path has a cfg-gated `merge(&mut self, other:
&Self)` method:

```rust
#[cfg(feature = "instrument")]
impl PerElementStats {
    pub(super) fn merge(&mut self, other: &Self) {
        self.nodes += other.nodes;
        // ...
    }
}

#[cfg(not(feature = "instrument"))]
impl PerElementStats {
    #[inline(always)]
    pub(super) fn merge(&mut self, _: &Self) {}
}
```

Workers produce their own `PerElementStats`; the caller folds worker
instances into a main one before `flush_per_element`. Under
`not(instrument)` the merge compiles to nothing and the tuple-tail ZST
does not change the worker's return layout.

**Call-site lifecycle.**

```rust
crate::debug::emit_marker("FOO_START");
let mut stats = probes::PerElementStats::default();
// ... hot loop or parallel work ...
probes::per_element(&mut stats, &element);           // sequential
// or: stats.merge(&worker_stats);                   // after rayon collect
probes::flush_per_element(&stats);
crate::debug::emit_marker("FOO_END");
```

**Naming.** `PerElementStats` for the state, `per_element` for the
probe tick, `flush_per_element` for the aggregated emission -
consistent across commands so readers know the shape at a glance.
Always-on aggregated flushes (e.g. the `TimeFilterStats` end-of-run
flush) are named after their state type's domain (`flush_stats`).

**Verification when wiring a parallel path.** The first command that
wires per-element probes into a parallel path verifies the ZST
composition claim empirically: `brokkr --bench` (without
`--instrument`) must land within noise of the baseline immediately
before the probes were added. That claim carries forward to subsequent
commands unless they change the pattern.

**No allow-clippy gymnastics at call sites.** `#[allow(...)]` for
cast/wrap lints lives inside the probes module next to the emission,
not at callers.

No shared `src/probes/` tree. If a second command ever needs the same
helper, extract it at that point, named after what it actually does.

## Brokkr integration

Brokkr already orchestrates cargo feature flips via `--hotpath` and
`--alloc`. Add `--instrument` as an **additive flag** (not a mode):
rebuilds with `--features instrument` alongside whatever measurement
mode is running. Composes with `--bench`, `--hotpath`, `--alloc`.
Appears in `.brokkr/results.db` as a run axis (greppable with `brokkr
results --grep instrument`).

Why additive, not a mode: the existing modes are mutually exclusive
measurements. `--instrument` is orthogonal - it changes the binary's
instrumentation surface; any measurement mode can run against an
instrumented binary or a release binary.

Default `brokkr --bench` measures the same binary users ship (no
feature flip). `brokkr --bench --instrument` measures an instrumented
binary with per-element probes live. Wall/RSS targets are validated
without `--instrument`; investigation questions run with it.

## Coverage (separate from this plan)

Several long-running pipelines have uneven marker coverage at stage
boundaries (TODO.md already calls this out). Fixing that is an
orthogonal coverage sweep - inline `emit_marker` calls at the right
places. Worth doing but not blocked by this plan and not part of its
rollout.

## Rollout

1. **Land the `probes` module pattern on one command.** Time-filter is
   the obvious starting point - already carries the load-bearing
   wall-time comment, small enough to add the full pattern in one
   commit. Move the grouped end-of-run `emit_counter` block into a
   `probes::flush_stats` aggregator. Add a cfg-gated per-element probe
   with empty twin. Marker calls stay inline at phase boundaries.
2. **Add `--instrument` (additive flag) to brokkr**, composes with the
   existing measurement modes.
3. **Document the pattern in CLAUDE.md** under the architecture /
   instrumentation section. Include the tier table.
4. **Roll out to other commands as concrete questions arise.** Don't
   pre-instrument speculatively - each `probes` module is a maintenance
   surface. The coverage sweep above is separately tracked.

The `tracing` backend migration (TODO.md) is a separate item, tracked
separately, unblocked either way.

## Non-goals

- A grand `src/probes/` tree that knows about every command.
  Per-command modules only; shared helpers only when a second consumer
  appears.
- Proc macros or DSLs over the probe module. Inline fns fit on one
  screen per command.
- Runtime gating of per-element probes via atomic flags. Measurably
  worse than cfg gating (load + branch per call in the hot path vs
  zero instructions) and brokkr rebuilds per mode anyway.
- Hotpath (function-attribute) instrumentation. Separate mechanism,
  tracked separately.

## Risks and mitigations

- **Signature drift between real and empty twin.** Caught by the call
  site - business logic passes real arguments, so if the empty variant's
  signature diverges the compiler fails at the caller. No dedicated
  test needed.
- **Discoverability of cfg'd probes.** Plain `grep` does not see empty
  twins. Mitigated by (a) co-locating probes next to the command and
  (b) a top-level doc comment on each probe module listing what it
  gates. Build with `--features instrument` as a smoke check so a
  cfg'd probe that stops compiling is caught immediately.

## Decision points still open

- **Single `probes.rs` vs `probes/` submodule per command.** Fine to
  start with a nested `mod probes` in the command file; split when the
  first command has 3+ per-element probes or ~50+ lines of probe code.
- **Where `PerElementStats`-shaped state lives - inside the probe
  module (feature-gated struct, ZST when off) or as a field on the
  command's state touched only by probes.** Leaning the former so
  disabling the feature removes the field entirely, but decide per
  command based on how entangled the state is with the hot loop.
