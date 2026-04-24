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

No blanket sweep: each site gets its own reading and its own
disposition. The ~28 % first-round mis-call rate on the original
sweep makes "text-only" patches unacceptable for this cluster too.

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

## Consequences

The 10 findings resolved as follows:

1. **`write/copy_range.rs:63-96`** - **docs-only.** The
   `copy_range_fallback` uses position-based `write_all`; the
   parallel writer has its own pwrite sibling
   (`parallel_writer::copy_range_fallback_pwrite`). A module doc
   comment records the single-thread-out_fd constraint so a future
   reader picking up `copy_range_fallback` for parallel use is
   routed to the right primitive.
2. **`renumber/wire_rewrite.rs:271,276,480,486`** -
   **`debug_assert`** at each of the four `val_start - 1` sites.
   Each site matches on a protobuf field number (1, 8, 1, 9);
   the assertion verifies the tag byte has the continuation bit
   clear (tag < 0x80), i.e. the field number fits in 4 bits. Any
   future PBF schema extension that adds a field >= 16 the
   rewriter needs to splice would trip the assertion in test
   rather than produce corrupt output silently.
3. **`apply_changes/streaming.rs:420-445`** - **no code change.**
   The load-bearing invariant ("`get_node` must return creates
   AND modifies") is already documented at
   `apply_changes/node_locations.rs:50-55`. Locking it with a
   test (modify a node in OSC, verify way refs pick up fresh
   coords) is better than a `debug_assert` here because the
   invariant lives at a different call site than the site at
   risk. Deferred to the `-j N` parity / coord-freshness test
   shape in `TODO.md` > "Release prep > Test-shape gaps".
4. **`altw/external/stage2.rs:67-72`** - **`debug_assert` +
   `saturating_sub`.** The underflow on `unique_nodes < NUM_BUCKETS`
   is masked today by the early-continue at :355, but replacing
   the raw subtraction with `saturating_sub` makes the safety
   local to the site, and a `debug_assert!(bucket_rank_start <=
   bucket_rank_end)` above it documents the invariant the
   early-continue upholds.
5. **`renumber/mod.rs:325-328`** - **`debug_assert` near the
   constant.** `STAGE2D_WORKERS = 6` is a file-local `const`.
   Adding `debug_assert!(STAGE2D_WORKERS >= 1)` at the `remove(0)`
   call site ties the panic to the constant, so a future tweak
   to 0 trips the assertion with a clear message rather than
   producing a `Vec::remove(0) on empty` panic from deep in the
   call stack.
6. **`reorder_buffer.rs:21-33`** - **no change.** The design-doc
   comment at `reorder_buffer.rs:21-28` already explains why
   `push`'s asserts are panics: seqs originate from `enumerate()`,
   a stale/duplicate seq is a programming error, and the panic
   surfaces loudly via `join().map_err(...)?`. This is option (2):
   the invariant is already loudly enforced and documented. No
   change beats adding a redundant `debug_assert`.
7. **`read/blob.rs:670-681`** - **real fix** (not just insurance).
   `seek_raw` does not reset `last_blob_ok`; an iteration that
   errored, then successfully `seek_raw`s, still returns `None`
   from `next()`. This is a live correctness bug, not latent.
   Fix: set `self.last_blob_ok = true` on seek success, plus a
   regression test in `tests/read_paths.rs`.
8. **`write/writer.rs:108-145`** - **docs-only.** The hang
   scenario (uring thread stuck in `register_buffers` on a buggy
   kernel without sending `init_tx` or dropping it) is on
   `init_rx.recv()`, not `handle.join()` as the TODO originally
   said. Picking a `recv_timeout` value is fraught: too short
   kills slow-init on a loaded host, too long doesn't help the
   kernel-bug case. A module comment records the scenario; adding
   a timeout is a future-work item if a buggy kernel is ever
   observed in practice.
9. **`geocode_index/reader.rs:800-831`** - **dropped from the
   triage.** O(n²) dedup via linear `Vec::contains` on `seen`. `n`
   is the number of admin polygons containing a single point (~10
   for dense urban, < 20 for any realistic point). Not a latent
   invariant; a perf footgun only if someone queries with deeply
   nested synthetic admin overlap. Tracked in `TODO.md` as a
   query-API perf note, not here.
10. **`geocode_index/builder/pass2.rs:295-304`** - **dropped from
    the triage.** Integer-divide centroid for ways spanning the
    antimeridian produces a wrong-hemisphere centroid. Buildings
    that cross ±180 don't exist in OSM. Documented as a
    coordinate-math limitation (not a correctness bug). Moved to
    `DEVIATIONS.md` if the surface matters; otherwise left as-is.

Net code changes:
- 6 `debug_assert!` additions (one each in items 2's four sites
  collapsed to one pattern, one in item 4, one in item 5).
- 1 real fix (item 7: `last_blob_ok` reset on `seek_raw`
  success + regression test).
- 2 doc-only comments (items 1, 8).
- 2 items dropped from the triage (9, 10) and rehomed in
  `TODO.md` / `DEVIATIONS.md` as appropriate.

**No user-visible behavior change.** `debug_assert` is off in
release; the real fix (item 7) corrects a stuck-iteration bug
that production inputs do not trigger. **Performance:** no
hot-path cost in release; `debug_assert` is free.

**Triage rule for new latent-invariant findings:** default to
disposition (1), `debug_assert` + one-line comment. Escalate to
(2) docs-only only when the invariant isn't expressible as a
local check. Drop to (3) only when the finding turns out not to
be a latent invariant in the first place (perf, pathological
input).

## Cross-references

- ADR-0002 - negative-ID policy (partial pattern: `debug_assert`
  at planner entry).
- ADR-0003 - error-path hygiene (sibling shared-primitive shape).
- ADR-0004 - defensive input errors (hard-error sibling of this
  decision's debug-only posture, distinguished by where the
  untrusted-input boundary is).
- `TODO.md` - the per-site finding list and file:line pins.
