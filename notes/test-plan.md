# Pre-release Test Plan

Comprehensive testing plan for all CLI commands across feature flag permutations.

## Feature matrix

Four compile-time configurations:

| Config | Features | Purpose |
|--------|----------|---------|
| **default** | `commands` | Library users, cross-platform |
| **linux** | `commands` + `linux-direct-io` + `linux-io-uring` | Production CLI on Linux |
| **minimal** | (none) | Library-only, no commands |
| **minimal-linux** | `linux-direct-io` + `linux-io-uring` | Feature-gated I/O paths without commands |

## 1. Compilation tests

Every config must compile cleanly (clippy + build):

```
brokkr check                                          # default features
brokkr check --features linux-direct-io,linux-io-uring
brokkr check --no-default-features
brokkr check --no-default-features --features linux-direct-io,linux-io-uring
```

These catch issues like the `VecDeque` import and unclosed delimiter in `uring_writer.rs`
that hid behind feature gates until we enabled `linux-io-uring`.

## 2. Unit + integration tests

Run `tests/` suite under each config:

```
brokkr check                                          # default
brokkr check --features linux-direct-io               # adds direct-io roundtrip tests
brokkr check --features linux-direct-io,linux-io-uring
brokkr check --no-default-features --features linux-direct-io,linux-io-uring  # minimal-linux
brokkr check -- --ignored                             # all ignored tests (see below)
```

### Ignored tests

All three ignored tests must pass before release:

| Test | File | Why ignored | Notes |
|------|------|-------------|-------|
| `roundtrip_denmark` | `tests/roundtrip_real.rs:187` | 54s, needs Denmark data | Full PBF roundtrip |
| `sorted_flag_but_unsorted_nodes_panics` | `tests/read_paths.rs:445` | Needs `debug_assertions`; broken since nightly 1.95 | Debug sorted-flag assertion |
| `merge_cross_validate_osmium` | `tests/merge.rs:1176` | Needs osmium installed + data files | Cross-validates merge against osmium |

Note: `sorted_flag_but_unsorted_nodes_panics` requires a debug build (`cargo test` without
`--release`). If the nightly 1.95 regression (debug_assertions off in test builds) persists
on the release toolchain, document it as a known issue.

### Current test gaps

| Area | Status | Notes |
|------|--------|-------|
| `linux-direct-io` write roundtrip | 2 tests | `roundtrip_direct_io`, `roundtrip_pipelined_direct_io` |
| `linux-io-uring` | 2 tests | `sort_overlapping_blobs_uring`, `merge_basic_create_modify_delete_uring` |
| `--direct-io` in commands | 4 tests | sort, merge, cat, add-locations-to-ways |
| `--uring` in commands | 2 tests | sort, merge |
| Feature-missing error paths | 2 tests | `sort_direct_io_feature_missing_error`, `sort_io_uring_feature_missing_error` |
| CLI binary invocation | 7 tests | `cli/tests/cli.rs`: version, help, cat, sort, inspect, check, feature-gated flags |

### Remaining test gaps

Priority 2 (error paths): **DONE**
- [x] Build without `linux-direct-io`, pass `direct_io: true` — expect error
- [x] Build without `linux-io-uring`, pass `io_uring: true` — expect error

Priority 3 (CLI integration): **DONE**
- [x] Invoke `pbfhogg` binary via `std::process::Command` for key commands
- [x] Verify `--direct-io` flag accepted/rejected based on compiled features

## 3. Cross-validation (brokkr verify)

Each verify command should run with both buffered and direct-io to exercise both
write paths against reference tools. All default to `--dataset denmark --variant indexed`.

| Command | Buffered | `--direct-io` | Notes |
|---------|----------|---------------|-------|
| `verify sort` | [x] | [x] | vs osmium sort |
| `verify cat` | [x] | [x] | type filters vs osmium cat |
| `verify extract` | [x] | [x] | simple/complete/smart vs osmium |
| `verify tags-filter` | [x] | [x] | 3 expressions vs osmium |
| `verify getid-removeid` | [x] | [x] | getid + invert vs osmium |
| `verify add-locations-to-ways` | [x] | [x] | vs osmium |
| `verify check-refs` | [x] | [x] | was known diff — fixed: reports unique IDs + occurrence count, verified against osmium |
| `verify merge` | [x] | [x] | vs osmium/osmosis/osmconvert |
| `verify derive-changes` | **known diff** | **osmium bug** | [libosmium#405](https://github.com/osmcode/libosmium/issues/405): osmium rejects large BlobHeaders (indexdata). Fixed upstream, not released. |
| `verify diff` | **known diff** | **osmium bug** | same libosmium#405 — not a pbfhogg issue |

Note: verify commands must run **one at a time**, never in parallel.

## 4. Command × I/O mode matrix

Commands that accept I/O flags, with the modes that need testing:

| Command | buffered | `--direct-io` | `--io-uring` | `--force` (raw PBF) |
|---------|----------|---------------|-----------|---------------------|
| cat (passthrough) | [x] | [x] | n/a | [x] |
| cat --type | [x] | [x] | n/a | [x] |
| cat --dedupe | [x] | [x] | [x] | [x] |
| sort | [x] | [x] | [x] | [x] |
| apply-changes | [x] | [x] | [x] | [x] |
| extract --simple | [x] | [x] | n/a | [x] |
| extract --smart | [x] | [x] | n/a | [x] |
| add-locations-to-ways | [x] | [x] | n/a | [x] |
| tags-filter | [x] | [x] | n/a | [x] |
| getid | [x] | [x] | n/a | [x] |
| getparents | [x] | [x] | n/a | n/a |
| renumber | [x] | [x] | n/a | n/a |
| time-filter | [x] | [x] | n/a | n/a |
| diff | [x] | [x] | n/a | n/a |
| inspect | [x] | [x] | n/a | [x] |
| check | [x] | [x] | n/a | n/a |

All 54 cells tested on Denmark dataset (commit `4f69912`). All pass.

`--uring` is only available on: cat --dedupe, sort, apply-changes.

## 5. Scale testing

| Dataset | Size | Commands to test | Status |
|---------|------|-----------------|--------|
| Denmark | 461 MB | All commands (fast iteration) | [x] All pass (section 4) |
| Germany | 4.5 GB | merge buffered + direct-io | [x] Both pass |
| North America | 18.8 GB | merge buffered + io-uring | [x] Both pass, identical element counts |
| Planet | 87 GB | cat passthrough | [x] Pass (50816 blobs) |
| Planet | 87 GB | cat --type | **OOM** (SIGKILL on 30 GB host, known issue) |
| Planet | 87 GB | merge | Skipped (no planet OSC diff available) |

### Ignored tests

| Test | Status |
|------|--------|
| `roundtrip_denmark` | [x] Pass |
| `merge_cross_validate_osmium` | [x] Pass (88.9s) |
| `sorted_flag_but_unsorted_nodes_panics` | **FAIL** (known: nightly 1.95 debug_assertions regression) |

## 6. CI matrix (future GitHub Actions)

```yaml
strategy:
  matrix:
    features:
      - ""                                    # default
      - "--no-default-features"               # minimal
      - "--features linux-direct-io,linux-io-uring"  # full linux
      - "--no-default-features --features linux-direct-io,linux-io-uring"  # minimal-linux
    include:
      - features: "--features linux-direct-io,linux-io-uring"
        run_ignored: true                     # roundtrip_denmark, merge_cross_validate_osmium
```

Each matrix entry runs: `cargo clippy` + `cargo test` with the given features.

## Brokkr changes done

- [x] `--direct-io` flag added to all verify subcommands that produce output PBFs
- [x] `--package` / `-p` flag added to `brokkr check`
