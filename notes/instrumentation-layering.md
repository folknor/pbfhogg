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

Two existing shapes cover the rest well enough:

1. `crate::debug::emit_marker("TIMEFILTER_HISTORY_START")` - runtime-gated
   through a `OnceLock<Option<File>>` on `BROKKR_MARKER_FIFO`. One
   atomic-ish load plus an `Option` branch per call. Disappears at block
   granularity (~8000:1 amortization), would wreck a 344M-element loop.
2. `#[cfg_attr(feature = "hotpath", hotpath::measure)]` - per-function,
   compile-time, zero cost when off.

TODO.md has a related open item: the `emit_marker` / `emit_counter` /
`emit_mallinfo2` sink is hard-wired to a Unix FIFO, so library consumers
(PyO3 bindings, progress UIs, structured logging) have no idiomatic way
to subscribe. The resolution there is to migrate the backend to the
`tracing` crate. That migration is a single-file change in
[`src/debug.rs`](../src/debug.rs) and does not depend on this plan.

This plan is the per-element cfg-gated probe shape, plus the small
amount of structure around it.

## Proposal: `probes` modules for structured probes only

When a probe has command-specific structure - a cfg-gated per-element
check, an aggregation state that only exists in one command, a group of
related counters flushed together - it goes in a `probes` module next to
the command. Simple markers and one-off counters keep calling
`crate::debug::emit_marker` / `emit_counter` directly.

### The shape that earns a probe module

Per-element cfg-gated probe: empty twin keeps the call site clean.

```rust
// src/commands/time_filter/probes.rs
#[cfg(feature = "instrument")]
#[inline(always)]
pub fn per_element(stats: &mut PerElementStats, el: &Element) {
    stats.tick(el);
}

#[cfg(not(feature = "instrument"))]
#[inline(always)]
pub fn per_element(_: &mut PerElementStats, _: &Element) {}
```

Call site:

```rust
probes::per_element(&mut per_el_stats, &element);
```

LLVM drops the empty twin to nothing on default builds. `_`-prefixed
params kill unused-variable warnings inside the probe module, where the
cfg split belongs. No `#[cfg]` at the business-logic call site.

Aggregated multi-counter emission is the other case that earns a
wrapper. Today `time_filter.rs:109-114` emits six counters inline; a
`probes::stats(&stats, is_history)` function that walks the struct is
easier to keep in sync than six copy-paste call sites and makes the
grouping legible.

### The shape that does *not* earn a wrapper

```rust
crate::debug::emit_marker("TIMEFILTER_HISTORY_START");
// ...
crate::debug::emit_marker("TIMEFILTER_HISTORY_END");
```

Wrapping these in `probes::history_start()` / `probes::history_end()`
adds an extra indirection and a typed name that duplicates the string,
without hiding anything load-bearing. The marker strings are the stable
ABI (they appear in `.brokkr/results.db`, in brokkr `--durations`
output, in historical sidecar logs); renaming a wrapper function is
just as breaking as renaming the string, and the backend migration to
`tracing` changes `emit_marker`'s body either way. Leave these in
business logic.

## When each tier applies

| Tier            | Granularity             | Mechanism                                                    | Cost when off         |
|-----------------|-------------------------|--------------------------------------------------------------|-----------------------|
| marker          | per phase               | `crate::debug::emit_marker("...")` inline                    | 1 OnceLock load       |
| counter         | per batch / block       | `crate::debug::emit_counter("...", n)` inline, or probe wrapper if grouped | 1 OnceLock load |
| mallinfo        | per phase boundary      | `crate::debug::emit_mallinfo2("...")` inline                 | 1 OnceLock load + syscall guarded by env var |
| hotpath         | per function            | `#[cfg_attr(feature = "hotpath", hotpath::measure)]`         | 0                     |
| per-element     | per element in hot loop | probe fn in command's `probes` module, `#[cfg(not)]` empty twin | 0 (inlined away)   |

Rule of thumb: if the probe fires more than once per block, it belongs
behind a cargo feature and therefore in a `probes` module. Once per
phase or block, inline the emit call - the OnceLock cost is amortized,
adding a wrapper adds noise without adding information.

## Module layout

Start as a nested `mod probes` at the bottom of the command file. No
new file, no directory conversion:

```rust
// at the end of src/commands/time_filter.rs
mod probes {
    use super::{Element, PerElementStats};

    #[cfg(feature = "instrument")]
    #[inline(always)]
    pub(super) fn per_element(stats: &mut PerElementStats, el: &Element) {
        stats.tick(el);
    }

    #[cfg(not(feature = "instrument"))]
    #[inline(always)]
    pub(super) fn per_element(_: &mut PerElementStats, _: &Element) {}
}
```

The cfg split stays local to the command. Call sites use
`probes::per_element(...)` as shown earlier.

Promote to a separate `probes.rs` (or `probes/` submodule) only when
the module earns it - when it grows past ~50 lines, acquires its own
types, or the command itself is already split across a directory and
wants probes co-located per sub-file. Same rule used for any other
submodule extraction in the tree.

One cargo feature, `instrument`, gates every per-element probe in the
tree. Per-command features (`timefilter-per-element`, `extract-per-way`,
...) were considered and rejected: enabling a probe in extract while
running time_filter costs nothing at runtime (the probe only fires
inside extract's hot loop), while per-command features would multiply
cargo's build cache entries and force brokkr and the user to track
N feature names instead of one flag. If a future workload ever needs
finer-grained control, split `instrument` into sub-features at that
point, named after the specific thing they gate.

No shared `src/probes/` tree. If a second command ever needs the same
helper, extract it at that point, named after what it actually does.

## Brokkr integration

Brokkr already orchestrates cargo feature flips via `--hotpath` and
`--alloc`. Add a matching boolean flag: `brokkr <cmd> --instrument`
rebuilds with `--features instrument` and runs once. The flag appears
in `.brokkr/results.db` as a run axis (greppable with `brokkr results
--grep instrument`).

Mirrors the existing `--hotpath` / `--alloc` shape one-for-one, so the
user's mental model is "each measurement mode is a boolean flag," not
"hotpath and alloc are flags but probes needs an argument."

## Coverage (separate from this plan)

Several long-running pipelines have uneven marker coverage at stage
boundaries (TODO.md already calls this out). Fixing that is an
orthogonal coverage sweep with `emit_marker` / `emit_counter` calls
inline at the right places - no `probes` module required unless the
probe has structure per the rule above. Worth doing but not blocked by
this plan and not part of its rollout.

## Rollout

1. **Land the `probes` module pattern on one command.** Time-filter is
   the obvious starting point - already carries the load-bearing
   wall-time comment, small enough to add a per-element probe in one
   commit. Add `src/commands/time_filter/probes.rs` with the per-element
   empty-twin probe and a `stats()` aggregator for the six existing
   counters. Existing `emit_marker` calls in `time_filter.rs` stay
   inline.
2. **Add `--instrument` (boolean) to brokkr**, rebuilds with
   `--features instrument` the same way `--hotpath` and `--alloc` do.
3. **Document the pattern and the "what earns a probe module" rule in
   CLAUDE.md** under the architecture / instrumentation section.
   Include the tier table.
4. **Add per-element probes as concrete questions arise.** Don't
   pre-instrument speculatively - each probe module is a maintenance
   surface.

The `tracing` backend migration (TODO.md) is a separate item, tracked
separately, unblocked either way.

## Non-goals

- Replacing or wrapping the existing `emit_marker` / `emit_counter`
  call sites that are fine as-is. Bureaucratic indirection with no
  information gain.
- A grand `src/probes/` tree that knows about every command. Per-command
  modules only; shared helpers only when a second consumer appears.
- Proc macros or DSLs over the probe module. Inline-empty fns fit on
  one screen per command.
- Runtime gating of per-element probes via atomic flags. Measurably
  worse than cfg gating (load + branch per call in the hot path vs zero
  instructions) and brokkr rebuilds per mode anyway.

## Risks and mitigations

- **Signature drift between real and empty twin.** Each probe module
  gets a `#[cfg(test)] mod probe_sigs` that constructs both fn pointers,
  forcing the empty variant to match the active variant under
  `cargo test --features instrument`.
- **Discoverability of cfg'd probes.** Plain `grep` doesn't see empty
  twins. Mitigated by (a) co-locating probes next to the command and
  (b) a top-level doc comment on each probe module listing what it
  gates. CI can also build with `--features instrument` as a smoke
  check so a cfg'd probe that stops compiling is caught immediately.

## Decision points still open

- **Single `probes.rs` vs `probes/` submodule per command.** Fine to
  start with a single file; split when the first command has 3+
  per-element probes.
- **Where `PerElementStats`-shaped state lives - inside the probe
  module (feature-gated struct, ZST when off) or as a genuine field on
  the command's state touched only by probes.** Leaning the former so
  disabling the feature removes the field entirely, but decide per
  command based on how entangled the state is with the hot loop.
