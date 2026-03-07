# Pre-release Test Plan

Comprehensive testing plan for all CLI commands across feature flag permutations.

## Feature matrix

Three compile-time configurations:

| Config | Features | Purpose |
|--------|----------|---------|
| **default** | `commands` | Library users, cross-platform |
| **linux** | `commands` + `linux-direct-io` + `linux-io-uring` | Production CLI on Linux |
| **minimal** | (none) | Library-only, no commands |

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
brokkr check -- --ignored                             # roundtrip_denmark (~54s)
```

### Current test gaps

| Area | Status | Notes |
|------|--------|-------|
| `linux-direct-io` write roundtrip | 2 tests | `roundtrip_direct_io`, `roundtrip_pipelined_direct_io` |
| `linux-io-uring` | **0 tests** | No uring tests exist |
| `--direct-io` in commands | **untested** | merge/sort/cat/extract all hardcode `direct_io: false` |
| `--uring` in commands | **untested** | merge/sort all hardcode `io_uring: false` |
| Feature-missing error paths | **untested** | `"--direct-io requires the linux-direct-io feature"` |
| CLI binary invocation | **none** | All tests call library API directly |

### Recommended new tests

Priority 1 (correctness):
- [ ] `merge` with `direct_io: true` — verify output matches buffered
- [ ] `merge` with `io_uring: true` — verify output matches buffered
- [ ] `sort` with `direct_io: true`
- [ ] `sort` with `io_uring: true`
- [ ] `cat` passthrough with `direct_io: true` — verify indexdata added
- [ ] `add-locations-to-ways` with `direct_io: true`

Priority 2 (error paths):
- [ ] Build without `linux-direct-io`, pass `direct_io: true` — expect error
- [ ] Build without `linux-io-uring`, pass `io_uring: true` — expect error

Priority 3 (CLI integration):
- [ ] Invoke `pbfhogg` binary via `std::process::Command` for key commands
- [ ] Verify `--direct-io` flag accepted/rejected based on compiled features

## 3. Cross-validation (brokkr verify)

Each verify command should run with both buffered and direct-io. All default to
`--dataset denmark --variant indexed`.

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
    include:
      - features: "--features linux-direct-io,linux-io-uring"
        run_ignored: true                     # roundtrip_denmark
```

Each matrix entry runs: `cargo clippy` + `cargo test` with the given features.
