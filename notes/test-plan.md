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
| Feature-missing error paths | **untested** | `"--direct-io requires the linux-direct-io feature"` |
| CLI binary invocation | **none** | All tests call library API directly |

### Remaining test gaps

Priority 2 (error paths):
- [ ] Build without `linux-direct-io`, pass `direct_io: true` — expect error
- [ ] Build without `linux-io-uring`, pass `io_uring: true` — expect error

Priority 3 (CLI integration):
- [ ] Invoke `pbfhogg` binary via `std::process::Command` for key commands
- [ ] Verify `--direct-io` flag accepted/rejected based on compiled features

## 3. Cross-validation (brokkr verify)

Each verify command should run with both buffered and direct-io to exercise both
write paths against reference tools. All default to `--dataset denmark --variant indexed`.

**Prerequisite:** brokkr verify commands need `--direct-io` support added (see
"Brokkr changes needed" at end of document).

| Command | Buffered | `--direct-io` | Notes |
|---------|----------|---------------|-------|
| `verify sort` | [ ] | [ ] | vs osmium sort |
| `verify cat` | [ ] | [ ] | type filters vs osmium cat |
| `verify extract` | [ ] | [ ] | simple/complete/smart vs osmium |
| `verify tags-filter` | [ ] | [ ] | 3 expressions vs osmium |
| `verify getid-removeid` | [ ] | [ ] | getid + invert vs osmium |
| `verify add-locations-to-ways` | [ ] | [ ] | vs osmium |
| `verify check-refs` | [ ] | [ ] | vs osmium check-refs |
| `verify merge` | [ ] | [ ] | vs osmium/osmosis/osmconvert |
| `verify derive-changes` | [ ] | [ ] | diff --format osc roundtrip |
| `verify diff` | [ ] | [ ] | diff summary vs osmium diff |

Note: verify commands must run **one at a time**, never in parallel.

## 4. Command × I/O mode matrix

Commands that accept I/O flags, with the modes that need testing:

| Command | buffered | `--direct-io` | `--uring` | `--force` (raw PBF) |
|---------|----------|---------------|-----------|---------------------|
| cat (passthrough) | [x] | [ ] | n/a | [ ] |
| cat --type | [ ] | [ ] | n/a | [ ] |
| cat --dedupe | [ ] | [ ] | [ ] | [ ] |
| sort | [ ] | [ ] | [ ] | [ ] |
| apply-changes | [ ] | [ ] | [ ] | [ ] |
| extract --simple | [ ] | [ ] | n/a | [ ] |
| extract --smart | [ ] | [ ] | n/a | [ ] |
| add-locations-to-ways | [ ] | [ ] | n/a | [ ] |
| tags-filter | [ ] | [ ] | n/a | [ ] |
| getid | [ ] | [ ] | n/a | [ ] |
| getparents | [ ] | [ ] | n/a | n/a |
| renumber | [ ] | [ ] | n/a | n/a |
| time-filter | [ ] | [ ] | n/a | n/a |
| diff | [ ] | [ ] | n/a | n/a |
| inspect | [ ] | [ ] | n/a | [ ] |
| check | [ ] | [ ] | n/a | n/a |

`--uring` is only available on: cat --dedupe, sort, apply-changes.

## 5. Scale testing

| Dataset | Size | Commands to test |
|---------|------|-----------------|
| Denmark | 461 MB | All commands (fast iteration) |
| Germany | 4.5 GB | merge (direct-io crossover point) |
| North America | 18.8 GB | merge buffered vs uring (uring wins 12-20%) |
| Planet | 87 GB | cat passthrough, merge (extrapolated ~47s uring+none) |

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

## Brokkr changes needed

The following changes are needed in brokkr before section 3 is executable:

**Add `--direct-io` flag to all verify subcommands that produce output PBFs.**

Currently none of the 10 `brokkr verify` subcommands accept `--direct-io`. The flag
should be forwarded to the underlying pbfhogg command so the write path uses O_DIRECT.

Commands that write PBFs and need the flag:
- `verify sort` — passes to `pbfhogg sort`
- `verify cat` — passes to `pbfhogg cat`
- `verify extract` — passes to `pbfhogg extract`
- `verify tags-filter` — passes to `pbfhogg tags-filter`
- `verify getid-removeid` — passes to `pbfhogg getid`
- `verify add-locations-to-ways` — passes to `pbfhogg add-locations-to-ways`
- `verify merge` — passes to `pbfhogg apply-changes`
- `verify derive-changes` — passes to `pbfhogg diff --format osc`

Commands that don't write PBFs (no flag needed):
- `verify check-refs` — read-only comparison
- `verify diff` — summary output only

The flag should require building pbfhogg with `--features linux-direct-io`. The verify
command should pass `--direct-io` through to the pbfhogg CLI invocation. The comparison
logic (diff against osmium output) stays the same — only the write path changes.
