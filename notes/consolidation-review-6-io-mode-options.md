# Consolidation Review #6: I/O Mode Options Normalization

## Verdict: NOT WORTH IT

## What Actually Exists

Two option structs with identical fields:

| Struct | File | Fields |
|--------|------|--------|
| `SortOptions` | `src/commands/sort.rs:148-154` | `compression`, `direct_io`, `io_uring`, `sqpoll`, `force` |
| `MergeOptions` | `src/commands/merge.rs:1069-1075` | `compression`, `direct_io`, `io_uring`, `sqpoll`, `force` |

Field-for-field identical: 5 fields, same types (`Compression`, `bool`, `bool`, `bool`, `bool`), same names.

## All other commands use loose function parameters

| Command | `compression` | `direct_io` | `io_uring` | `sqpoll` | `force` |
|---------|:---:|:---:|:---:|:---:|:---:|
| `sort` | yes | yes | yes | yes | yes |
| `merge` | yes | yes | yes | yes | yes |
| `cat` | yes | yes | -- | -- | yes |
| `extract` | yes | yes | -- | -- | yes |
| `tags_filter` | yes | yes | -- | -- | yes |
| `add_locations_to_ways` | yes | yes | -- | -- | yes |
| `getid` | yes | yes | -- | -- | yes |
| `removeid` | yes | yes | -- | -- | -- |
| `tags_count` | -- | yes | -- | -- | yes |
| `node_stats` | -- | yes | -- | -- | -- |
| `derive_changes` | -- | yes | -- | -- | -- |
| `diff` | -- | yes | -- | -- | -- |
| `check_refs` | -- | yes | -- | -- | -- |
| `inspect` | -- | yes | -- | -- | -- |
| `is_indexed` | -- | yes | -- | -- | -- |
| `verify_ids` | -- | yes | -- | -- | -- |

Only `sort` and `merge` support `io_uring`/`sqpoll`. Only these two use `writer_from_header_bytes` (I/O mode-aware writer constructor). All other writing commands use `writer_from_header` or `PbfWriter::to_path` directly (always basic buffered I/O).

## How CLI Args Map to Internal Options

The CLI side already has well-factored shared arg structs via clap `#[command(flatten)]`:
- `CompressionArg` (1 field: `compression: String`)
- `DirectIoArg` (1 field: `direct_io: bool`)
- `ForceArg` (1 field: `force: bool`)
- `UringArg` (2 fields: `io_uring: bool`, `sqpoll: bool`)

Each command flattens the relevant subset. The `run_*` functions destructure into loose parameters and pass individually. For `sort` and `merge`, they reconstruct `SortOptions`/`MergeOptions` from the loose args.

## Behavioral Differences

None. Both structs are destructured identically:
```rust
let SortOptions { compression, direct_io, io_uring, sqpoll, force } = *opts;
let MergeOptions { compression, direct_io, io_uring, sqpoll, force } = *opts;
```

Both pass `(direct_io, io_uring, sqpoll)` to the same `writer_from_header_bytes` function identically.

## Assessment

**Lines saved:** Merging into a shared `IoModeOptions` struct would eliminate ~7 lines of struct definition and save zero behavioral complexity.

**Why this is cosmetic:**

1. Only two consumers exist. A shared type for two users is not a meaningful abstraction.
2. The CLI side is already properly factored via `flatten`.
3. Other commands do not support `io_uring`/`sqpoll` and could not use the shared struct without ignoring fields.
4. `SortOptions` and `MergeOptions` are self-documenting at call sites.
5. If a third command gains io_uring support, adding the struct takes 30 seconds.

**Risk:** Minimal but nonzero. Both structs are `pub`. Downstream consumers (nidhogg, elivagar) that call `merge()` or `sort()` directly would need import changes.

## Recommendation: NOT WORTH IT

Purely cosmetic duplication. The 12 lines of redundant struct definition are clearer and more maintainable than a premature abstraction. Not worth the churn.
