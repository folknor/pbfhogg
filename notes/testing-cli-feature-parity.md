# pbfhogg to brokkr: feature requests

This file is the consolidated set of brokkr-side feature requests
that have come out of the pbfhogg testing reorg (see
`notes/testing.md`, 2026-04-25). It is intended to be delivered
directly to the brokkr maintainer as a self-contained handoff.

Each request is independent; implement in any order. The pbfhogg
side has working fallbacks for everything except request 1 (test
profiles), so this is not a blocker - but every item simplifies
pbfhogg test infrastructure and removes a piece of in-tree
workaround.

## Requests at a glance

1. **Validation profiles via Rust module paths.** Map `cargo test`
   shapes to named profiles defined in `brokkr.toml`, using
   `mod tier2`, `mod platform`, `mod serial` as the annotation
   surface. Replaces the current "either run `brokkr check` or
   `cargo test -- --ignored` and hope" model. Largest item; unlocks
   the entire tiering story.

2. **Feature-aware test sweeps that rebuild the CLI binary.**
   `cargo test -p pbfhogg --no-default-features` does not rebuild
   `pbfhogg-cli` with matching features. Result: consumer-sweep
   CLI tests invoke an all-features binary and silently miss the
   feature-off behaviour they were meant to pin. Add a CLI build
   step per sweep with matching features.

3. **`brokkr verify <command> --input <path>`.** Accept an
   explicit fixture path so pbfhogg-built pathological fixtures
   (handcrafted overlapping-blob PBFs, etc.) can be cross-validated
   against osmium / osmosis / osmconvert. Currently `brokkr verify`
   only takes datasets from the configured `[datasets.<region>.*]`
   tables.

4. **`verify_merge` delete-set tolerance.** Osmium and pbfhogg
   disagree on apply-changes delete semantics (version-based vs
   unconditional). Element-strict diff in `verify_merge.rs` flags
   osmium-only elements as failures even when those elements are
   in the input OSC's delete set. Add the carve-out so legitimate
   semantic differences are not reported as cross-validation
   regressions.

---

## 1. Validation profiles via Rust module paths

### Problem

`brokkr check` is becoming too broad as the in-project test suite
grows. Today every test in `tests/*.rs` either runs by default
(via `cargo test`) or is `#[ignore]`d and only runs under
`cargo test -- --ignored`. That binary switch does not capture
the actual cost classes that matter to a developer.

The pbfhogg-side reorg classifies tests by **runtime cost** in a
5-tier ladder (see `notes/testing.md` > "Validation tiers" for the
full development contract):

| Tier | Cost | When | Mechanism |
|---|---|---|---|
| 1. Fast contracts | seconds | Every edit | `brokkr check` default profile |
| 2. Command slice | tens of seconds | While working on that command | `brokkr check --profile <cmd>` |
| 3. Full in-project | minutes | Before merge | `brokkr check --profile full` |
| 4. Scale/perf | hours | Performance work, release | `brokkr bench`, `brokkr suite` |
| 5. External cross-validation | host-dependent | Release gate | `brokkr verify` |

Tiers 1-3 are the brokkr profiles this request is about. Tiers 4
and 5 are existing brokkr commands (`bench` / `suite` / `verify`)
and are out of scope for the profile system. They are listed
because they are the remaining cost classes a release passes
through and the profile system needs to compose with them
(`brokkr check --profile full && brokkr bench && brokkr verify` is
the full release gate).

Two orthogonal cross-cutting markers also need brokkr support:
**platform** (host-specific tests for `--direct-io`, `--io-uring`,
MEMLOCK requirements) and **serial** (tests that need
`--test-threads=1`). Both can sit at any tier; they describe a
configuration overlay, not a runtime cost.

`#[ignore]` stays as the libtest mechanic for tests that must
never run accidentally; it is no longer the tier label.

### Proposed annotation surface

Use ordinary Rust **module paths** as the marker. No custom
attribute macros, no pbfhogg-specific config. `cargo test
<substring>::` and `--skip <substring>::` already match on module
paths, so the brokkr implementation can translate profile
selection into ordinary cargo arguments.

```rust
// File-root tests are Tier 1 by default.
#[test]
fn sort_basic_cli_contract() {}

mod tier2 {
    #[test]
    fn sort_many_blob_boundaries() {}
}

mod tier3 {
    #[test]
    fn sort_large_fixture_roundtrip() {}
}

mod platform {
    #[test]
    fn sort_direct_io_alignment() {}
}

mod serial {
    #[test]
    #[ignore = "run through brokkr profile serial/fault"]
    fn injected_write_failure_is_atomic() {}
}
```

`#[ignore]` stays a libtest execution mechanic for tests that
must never run accidentally (serial-only, platform-only on
unsupported hosts). It is no longer the tier label.

### Proposed `brokkr.toml` shape

```toml
[test]
default_package = "pbfhogg"
default_profile = "tier1"

[test.sweeps.all]
features = "all"
build_packages = ["pbfhogg-cli"]

[test.sweeps.consumer]
no_default_features = true
features = ["commands"]
build_packages = ["pbfhogg-cli"]

[test.profiles.tier1]
description = "Fast edit loop used by brokkr check (tier 1)"
sweeps = ["all", "consumer"]
skip = ["tier2::", "tier3::", "platform::", "serial::"]
include_ignored = false

# Tier 2 = command slice. One profile per command surface, each
# extending tier1 with that command's `mod tier2 { ... }` cell.
# Add similar profiles for `extract`, `add-locations-to-ways`,
# `apply-changes`, etc., as those command surfaces grow tier-2
# matrices.
[test.profiles.sort]
description = "Tier 2: expanded sort command tests"
extends = "tier1"
tests = ["cli_sort"]
skip = ["platform::", "serial::"]

[test.profiles.full]
description = "Tier 3: all in-project correctness tests"
sweeps = ["all"]
skip = ["platform::"]
include_ignored = true

[test.profiles.platform]
description = "Platform-sensitive tests"
sweeps = ["all"]
only = ["platform::"]
include_ignored = true
env = { BROKKR_TEST_PLATFORM = "1" }

[test.profiles.serial]
description = "Serial/fault-injection tests"
sweeps = ["all"]
only = ["serial::"]
include_ignored = true
test_threads = 1
```

### Proposed command surface

```text
brokkr check                          # tier 1 (default profile)
brokkr check --profile sort           # tier 2: one command family
brokkr check --profile full           # tier 3: full in-project sweep
brokkr bench                          # tier 4: scale/perf (existing command)
brokkr verify                         # tier 5: external reference checks (existing command)
```

Tier 2 is delivered through per-command profiles (`sort`,
`extract`, `add-locations-to-ways`, ...) - or `--command <name>`
sugar that resolves to one. The underlying mechanism should
remain profile selection so non-pbfhogg projects can define their
own slices. There is no separate `tier2` profile because "all
command slices at once" is approximately tier 3 and is already
covered by `--profile full`.

### Translation to cargo/libtest

The brokkr implementation should translate each profile into
ordinary cargo / libtest arguments:

- `--test <name>` to limit to specific binaries.
- Substring filters (positional args to `cargo test`) and
  `--skip <substring>` for the `only` / `skip` lists.
- `--include-ignored` driven by the profile flag.
- `--test-threads=N` driven by `test_threads`.
- Feature flags from the named sweep.
- Environment variables.
- Prerequisite-tool checks (e.g. `command -v osmium`).
- Explicit CLI binary builds (see request 2).

This keeps the model transparent to other Rust projects instead
of baking pbfhogg internals into brokkr.

### What this replaces

Today's pbfhogg side uses two crude levers in place of profiles:

- `#[ignore]` to keep slow / serial / platform tests out of the
  default sweep.
- Per-test cfg gates (`#[cfg(feature = "linux-direct-io")]`) plus
  hand-rolled stderr string matching to skip on unsupported
  hosts.

Both work, but they bury the cost-class information. With profiles,
the file structure tells brokkr how to schedule the test, and the
test author does not have to read CLAUDE.md to know how to mark a
slow test.

---

## 2. CLI binary feature parity in test sweeps

### Problem

Under the CLI-decoupled test reorg (see `notes/testing.md`)
integration tests in `tests/cli_*.rs` drive the compiled `pbfhogg`
binary via `CliInvoker` (a `std::process::Command` wrapper).
Today `brokkr test` runs two sweeps:

- **all-features** - `cargo test --release --all-features -p pbfhogg`
- **consumer**     - `cargo test --release --no-default-features --features commands -p pbfhogg`

The `-p pbfhogg` restricts the build target to the library crate.
`pbfhogg-cli` is a separate workspace member; cargo does not
rebuild it for these invocations, and the binary at
`target/release/pbfhogg` remains whatever features it was last
built with. In practice that is always all-features, because
`brokkr check` runs `cargo test --all-features` at the workspace
level before any consumer-sweep test fires.

**Consequence:** in the consumer sweep, the test crate's
`#[cfg(feature = "linux-direct-io")]` and the CLI binary's actual
`--direct-io` support are decoupled. The test is gated by the
library's feature set; the binary it invokes is governed by
whatever was built last.

### Symptoms observed

1. **`sort_direct_io_feature_missing_error` /
   `sort_io_uring_feature_missing_error`** (deleted from
   `cli_sort.rs` 2026-04-24).
   Tests were cfg-gated on `not(feature = "linux-direct-io")` etc,
   intending to run only in the consumer sweep where the feature
   is off. In practice they invoked the all-features binary, which
   accepted `--direct-io` and produced sorted output, failing the
   `assert_failure` assertion. Deleted; left a pointer comment in
   `tests/cli_sort.rs`.
2. **Latent: `sort_overlapping_blobs_direct_io` and `_uring`**.
   These are positive tests gated on `cfg(feature = "linux-*")`,
   which happens to line up today because the CLI binary is a
   superset. On a fresh checkout where the CLI binary was built
   in consumer mode first, they would invoke a binary that does
   not support the flag, and fail with an unrelated error.
3. **General:** any future CLI test that cares about the CLI
   binary's feature surface has the same latent hazard.

### Proposed fix

Each test sweep must rebuild the CLI binary with matching feature
flags. Two shapes:

**Option A: workspace-level invocation.**

Replace `-p pbfhogg` with a workspace test invocation that
propagates features to dependent crates:

```text
cargo test --release --workspace --no-default-features --features pbfhogg/commands
cargo test --release --workspace --all-features
```

Cargo's feature unifier would then rebuild `pbfhogg-cli` with the
library features it is configured to propagate
(`cli/Cargo.toml`'s `linux-direct-io = ["pbfhogg/linux-direct-io"]`,
etc.). Risk: feature unification rules at the workspace level
surface more complexity; `--features pbfhogg/commands` syntax is
required at the workspace boundary because unqualified feature
names must exist in every member.

**Option B: explicit second build step.**

Before each `cargo test -p pbfhogg ...` invocation, run a matching
`cargo build -p pbfhogg-cli` step with the same feature selection:

```text
cargo build --release -p pbfhogg-cli --all-features
cargo test  --release -p pbfhogg     --all-features

cargo build --release -p pbfhogg-cli --no-default-features
cargo test  --release -p pbfhogg     --no-default-features --features commands
```

Incremental-rebuild cost is near zero after the first pass.
Explicit and easy to reason about; no workspace-feature unification
complexity.

**Recommended: B.** The explicit rebuild is cheap, self-documenting,
and avoids edge cases in cargo's workspace feature resolver.

### Profile interaction

The validation-profile design in request 1 should own this rebuild
behavior. A profile sweep that runs CLI tests should declare both
the library test feature set and the matching CLI binary build:

```toml
[test.sweeps.all]
features = "all"
build_packages = ["pbfhogg-cli"]

[test.sweeps.consumer]
no_default_features = true
features = ["commands"]
build_packages = ["pbfhogg-cli"]
```

When brokkr executes a sweep, it should build each listed binary
package with the same feature selection before invoking the
profile's test steps. That keeps `CliInvoker` tests honest without
requiring each individual test to understand cargo workspace
feature resolution.

### What this unblocks

Once brokkr guarantees binary/library feature parity per sweep:

- Restore the two `feature_missing_error` tests in `cli_sort.rs`
  (negative tests: `--direct-io` must fail with a clear message
  when the feature is absent), but place them in the platform
  tier (`mod platform`) rather than the default tier 1 sweep.
- Same pattern becomes safe for every other command with
  feature-gated flags: `cli_apply_changes.rs`, `cli_altw.rs`,
  `cli_cat.rs`, etc.
- The positive `_direct_io` / `_uring` tests
  (`sort_overlapping_blobs_*` and equivalents) become genuinely
  correct instead of accidentally-correct. They still need
  deterministic skip handling for unsupported filesystems,
  kernels, and MEMLOCK limits - covered on the pbfhogg side by
  `CliOutput::is_o_direct_unsupported` /
  `is_uring_unsupported` predicates that match the CLI's actual
  error strings.

### Fallback without brokkr changes

If the brokkr change is deferred, the invariant "feature-off
build errors cleanly instead of silently falling back" is
restored on the pbfhogg side with inline unit tests inside
`src/commands/mod.rs`:

```rust
#[cfg(all(test, not(feature = "linux-direct-io")))]
mod feature_gate_tests {
    use super::*;

    #[test]
    fn writer_for_cli_rejects_direct_io_without_feature() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.pbf");
        let result = writer_for_cli(
            &path,
            Compression::default(),
            &[],
            /* direct_io */ true,
            /* io_uring  */ false,
        );
        let err = result.expect_err("must error without linux-direct-io");
        assert!(err.to_string().contains("direct-io"));
    }

    // And symmetric cases for io_uring in writer_for_cli +
    // writer_for_apply_changes.
}
```

Four inline tests, ~40 lines total, no infrastructure changes.
Covers the library-side invariant but does not exercise the
clap-parse to library-call route through the CLI binary - which
is exactly the surface request 2 makes testable.

---

## 3. `brokkr verify <command> --input <path>`

### Problem

`brokkr verify <command>` cross-validates pbfhogg against external
tools (osmium / osmosis / osmconvert) on a configured dataset
selected by `--dataset <region>` (and `--variant`). The dataset
list comes from `[datasets.<region>.*]` tables in `brokkr.toml`,
which point at real-world OSM PBFs (Denmark, Europe, planet, etc.).

Some pbfhogg cross-validations need a **handcrafted fixture**
that no real-world dataset exercises. The clearest example today:
`pbfhogg sort` has an overlap-rewrite path that triggers when
adjacent node blobs have overlapping ID ranges. Real OSM PBFs
arrive sorted from upstream, so `brokkr verify sort
--dataset denmark` cannot exercise the overlap-rewrite path.
A pathological fixture (built by `tests/cli_sort.rs::write_unsorted_overlapping_pbf`)
does, but that fixture is constructed in pbfhogg test code, not
checked in as a binary.

The pbfhogg-side test reorg explicitly offloads external
cross-validation to `brokkr verify` (see
`notes/testing.md` > "External cross-validation"). Without
`--input <path>`, the offload is incomplete: the only osmium
checks brokkr can run are the ones that fit the configured
real-data shape.

### Proposed shape

Add `--input <path>` to every `brokkr verify <command>` subcommand
that today takes `--dataset`. Mutually exclusive with `--dataset`.
When `--input` is present, the harness:

- Skips the dataset-resolution step.
- Treats `<path>` as the canonical input PBF.
- Runs the same pbfhogg + reference-tool + diff steps it would run
  for a real dataset.

```text
brokkr verify sort --input target/test-fixtures/overlapping.osm.pbf
brokkr verify add-locations-to-ways --input fixtures/multi-blob-ways.pbf
```

### Companion: pbfhogg-side fixture builders

pbfhogg will add small `examples/` binaries (or an `xtask`-style
helper) that produce fixtures on demand:

```text
cargo run --release --example overlapping_fixture -- \
    --output target/test-fixtures/overlapping.osm.pbf
```

The verify profile invokes these before calling
`brokkr verify ... --input ...`. The fixture artifacts are
gitignored; the builder is checked in as code.

### Migration path for existing in-tree osmium tests

Two in-tree tests today shell out to osmium:

- `tests/merge.rs::merge_cross_validate_osmium` - real Denmark
  data, same input shape `brokkr verify merge` already supports.
  Retire-able once the delete-set tolerance from request 4
  lands.
- `tests/cli_sort.rs::sort_cross_validate_osmium` - handcrafted
  fixture, currently `#[ignore = "external"]` as an escape
  hatch. Retire-able once `--input <path>` from this request
  lands plus the pbfhogg-side `examples/overlapping_fixture.rs`
  is built.

After both requests ship, the in-tree osmium-shelling test
surface goes to zero. Future osmium / osmosis / osmconvert
checks are added as `verify_<command>.rs` modules in brokkr,
with `--input` for the fixture-based ones.

---

## 4. `verify_merge` delete-set tolerance

### Problem

`pbfhogg merge` (apply-changes) and osmium `apply-changes`
disagree on a corner of OSC delete semantics:

- **Osmium** uses **version-based deletes**: a `<delete>`
  element is only applied if the version in the OSC matches
  the version in the base. If the base has a newer version,
  the delete is silently skipped and the element survives in
  the output.
- **pbfhogg / osmosis / osmconvert** use **unconditional
  deletes**: every `<delete>` ID is removed from the output
  regardless of version.

Both behaviours are defensible (the OSC spec is ambiguous
here), and pbfhogg's choice matches the majority of the OSC
toolchain. But it means a strict element-by-element diff
between pbfhogg's and osmium's outputs over the same base + OSC
will surface delta entries that are **not bugs**: every
version-mismatched delete shows up as "element present in
osmium, missing from pbfhogg".

### What the in-tree test does today

`tests/merge.rs::merge_cross_validate_osmium` (lines 1271-1295,
present at the time of writing; on the migration list per
request 3) handles this carve-out explicitly:

```rust
// Elements in osmium but not pbfhogg should be in the OSC delete set.
// osmium uses version-based deletes; pbfhogg/osmosis/osmconvert delete unconditionally.
for id in &missing_n {
    if !diff.deleted_nodes.contains(id) {
        eprintln!("FAIL: node {id} missing but NOT in delete set");
        failures += 1;
    }
}
// (and the same for ways, relations)
```

The test parses the input OSC into a `diff` struct with
`deleted_nodes` / `deleted_ways` / `deleted_relations` sets,
then treats osmium-only elements as expected if and only if
their ID appears in the matching delete set.

### What `verify_merge.rs` should do

`brokkr/src/pbfhogg/verify_merge.rs` runs both tools and calls
`harness.diff_pbfs(&pbfhogg_out, &osmium_out)` to compare the
outputs. If `diff_pbfs` is element-strict, it will report
every osmium-only delete as a failure - the same false-positive
class the in-tree test specifically carved out.

The fix is one of:

1. **Tolerance hook in `diff_pbfs`.** Let the caller pass an
   "expected one-sided diff" set, e.g.:

   ```rust
   let expected_left_only = parse_osc_deletes(osc_path);
   harness.diff_pbfs_with_tolerance(
       &pbfhogg_out,
       &osmium_out,
       DiffTolerance {
           left_only_ids: expected_left_only,
           // (further tolerances can grow here)
       },
   )?;
   ```

   The harness emits a diff and silently drops entries whose
   ID is in the tolerance set; everything else is a failure.

2. **Per-verify carve-out.** Keep `diff_pbfs` strict and put the
   delete-set logic inside `verify_merge.rs`:

   ```rust
   let report = harness.diff_pbfs_report(&pbfhogg_out, &osmium_out)?;
   let osc_deletes = parse_osc_deletes(osc_path);
   let unexplained = report
       .right_only
       .into_iter()
       .filter(|id| !osc_deletes.contains(id))
       .collect::<Vec<_>>();
   if !unexplained.is_empty() {
       return Err(...);
   }
   ```

Option 1 generalizes - other commands may grow similar
semantic carve-outs - but it is a bigger surface change.
Option 2 is local and keeps `diff_pbfs` simple. Either works.

### Migration trigger

Once this lands, `tests/merge.rs::merge_cross_validate_osmium`
becomes redundant with `brokkr verify merge` and gets retired
on the pbfhogg side. Until then it stays as an in-tree
escape-hatch test.
