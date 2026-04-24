# CLI binary feature parity in `brokkr test`

## Problem

Under the CLI-decoupled test reorg (see [`testing.md`](testing.md))
integration tests in `tests/cli_*.rs` drive the compiled `pbfhogg`
binary via `CliInvoker`. `brokkr test` runs two sweeps:

- **all-features** — `cargo test --release --all-features -p pbfhogg`
- **consumer**     — `cargo test --release --no-default-features --features commands -p pbfhogg`

The `-p pbfhogg` restricts the build target to the library crate.
`pbfhogg-cli` is a separate workspace member; cargo does not rebuild
it for these invocations, and the binary at `target/release/pbfhogg`
remains whatever features it was last built with. In practice that
is always all-features, because `brokkr check` runs
`cargo test --all-features` at the workspace level before any
consumer-sweep test fires.

**Consequence:** in the consumer sweep, the test crate's
`#[cfg(feature = "linux-direct-io")]` and the CLI binary's actual
`--direct-io` support are decoupled. The test is gated by the
library's feature set; the binary it invokes is governed by whatever
was built last.

## Symptoms observed

1. **`sort_direct_io_feature_missing_error` / `sort_io_uring_feature_missing_error`** (deleted from `cli_sort.rs` 2026-04-24).
   Tests were cfg-gated on `not(feature = "linux-direct-io")` etc,
   intending to run only in the consumer sweep where the feature is
   off. In practice they invoked the all-features binary, which
   accepted `--direct-io` and produced sorted output, failing the
   `assert_failure` assertion. Deleted; left a pointer comment in
   `tests/cli_sort.rs`.
2. **Latent: `sort_overlapping_blobs_direct_io` and `_uring`**.
   These are positive tests gated on `cfg(feature = "linux-*")`,
   which happens to line up today because the CLI binary is a
   superset. On a fresh checkout where the CLI binary was built in
   consumer mode first, they would invoke a binary that doesn't
   support the flag, and fail with an unrelated error.
3. **General:** any future CLI test that cares about the CLI
   binary's feature surface has the same latent hazard.

## Proposed fix (brokkr side)

Each test sweep must rebuild the CLI binary with matching feature
flags. Two shapes:

**Option A: workspace-level invocation.**

Replace `-p pbfhogg` with a workspace test invocation that
propagates features to dependent crates:

```
cargo test --release --workspace --no-default-features --features pbfhogg/commands
cargo test --release --workspace --all-features
```

Cargo's feature unifier would then rebuild `pbfhogg-cli` with the
library features it's configured to propagate
(`cli/Cargo.toml`'s `linux-direct-io = ["pbfhogg/linux-direct-io"]`,
etc.). Risk: feature unification rules at the workspace level
surface more complexity; `--features pbfhogg/commands` syntax is
required at the workspace boundary because unqualified feature
names must exist in every member.

**Option B: explicit second build step.**

Before each `cargo test -p pbfhogg ...` invocation, run a matching
`cargo build -p pbfhogg-cli` step with the same feature selection:

```
cargo build --release -p pbfhogg-cli --all-features
cargo test  --release -p pbfhogg     --all-features

cargo build --release -p pbfhogg-cli --no-default-features
cargo test  --release -p pbfhogg     --no-default-features --features commands
```

Incremental-rebuild cost is near zero after the first pass. Explicit
and easy to reason about; no workspace-feature unification
complexity.

Recommended: **B**. The explicit rebuild is cheap, self-documenting,
and avoids edge cases in cargo's workspace feature resolver.

## What this unblocks

Once brokkr guarantees binary/library feature parity per sweep:

- Restore the two `feature_missing_error` tests in `cli_sort.rs`
  (negative tests: `--direct-io` must fail with a clear message when
  the feature is absent).
- Same pattern becomes safe for every other command with
  feature-gated flags: `cli_apply_changes.rs`, `cli_altw.rs`,
  `cli_cat.rs`, etc.
- The positive `_direct_io` / `_uring` tests (`sort_overlapping_blobs_*`
  and equivalents) become genuinely correct instead of
  accidentally-correct.

## Fallback without brokkr changes

If the brokkr change is deferred, the invariant "feature-off build
errors cleanly instead of silently falling back" can be restored with
inline unit tests inside `src/commands/mod.rs`:

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
clap-parse → library-call route through the CLI binary.
