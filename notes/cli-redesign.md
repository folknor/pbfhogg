# CLI Redesign Plan

Consolidate 22 commands down to 14. Fix overlapping commands, remove
near-synonyms, keep the surface flat with self-documenting names.

## Final command list

| # | Command | Absorbs |
|---|---------|---------|
| 1 | `inspect` | `is-indexed`, `node-stats`, `tags-count` |
| 2 | `check` | `verify *`, `check-refs` |
| 3 | `cat` | `merge-pbf` (via `--dedupe`) |
| 4 | `sort` | — |
| 5 | `renumber` | — |
| 6 | `extract` | — |
| 7 | `tags-filter` | `tags-filter-osc` (via `--input-kind`) |
| 8 | `getid` | `removeid` (via `--invert`) |
| 9 | `getparents` | — |
| 10 | `add-locations-to-ways` | — |
| 11 | `time-filter` | — |
| 12 | `diff` | `derive-changes` (via `--format osc`) |
| 13 | `apply-changes` | old `merge` (apply OSC to PBF) |
| 14 | `merge-changes` | — |

## Removed commands

| Old command | Replacement |
|---|---|
| `is-indexed` | `inspect --indexed` (exit code 0/1) |
| `node-stats` | `inspect --nodes` |
| `tags-count` | `inspect tags [EXPR...]` |
| `check-refs` | `check --refs` |
| `verify ids` | `check --ids` |
| `verify refs` | `check --refs` |
| `verify all` | `check` (default when no flag given) |
| `removeid` | `getid --invert` |
| `tags-filter-osc` | `tags-filter --input-kind osc` (autodetect fallback) |
| `derive-changes` | `diff --format osc` |
| `merge` (apply changes) | `apply-changes` |
| `merge-pbf` | `cat --dedupe` |

## Interface details

### inspect

```
pbfhogg inspect <FILE>                              # metadata summary
pbfhogg inspect <FILE> --blocks [N]                 # block distribution
pbfhogg inspect <FILE> --extended                   # deep scan
pbfhogg inspect <FILE> --indexed                    # exit code 0/1
pbfhogg inspect <FILE> --nodes                      # coordinate statistics
pbfhogg inspect <FILE> --get <KEY>                  # single value lookup
pbfhogg inspect <FILE> --json                       # machine-readable output
pbfhogg inspect tags <FILE> [EXPR...]               # tag counting (subcommand)
pbfhogg inspect tags <FILE> --type way --sort count-desc --min-count 100
```

`tags` is a subcommand (not a flag) because it has its own positional
expressions, `--min-count`, `--max-count`, `--sort`, `--type`, `-e`.

### check

```
pbfhogg check <FILE>                                # run all checks (default)
pbfhogg check <FILE> --ids                          # ID uniqueness/ordering only
pbfhogg check <FILE> --refs                         # referential integrity only
pbfhogg check <FILE> --refs --check-relations       # include relation members
pbfhogg check <FILE> --refs --show-ids              # show missing refs
pbfhogg check <FILE> --ids --full                   # bitmap duplicate detection
pbfhogg check <FILE> --json                         # machine-readable output
pbfhogg check <FILE> --quiet                        # exit code only
```

### diff

```
pbfhogg diff <OLD> <NEW>                            # human-readable text diff
pbfhogg diff <OLD> <NEW> --summary                  # counts on stderr
pbfhogg diff <OLD> <NEW> --quiet                    # exit code only
pbfhogg diff <OLD> <NEW> --format osc -o out.osc    # produce OSC output
pbfhogg diff <OLD> <NEW> --format osc --increment-version --update-timestamp
```

`--format text` is the default. `--format osc` enables `--increment-version`
and `--update-timestamp` flags. No `derive` alias.

### cat --dedupe

```
pbfhogg cat -o out.pbf a.pbf b.pbf                   # concatenate (existing)
pbfhogg cat -o out.pbf a.pbf b.pbf --dedupe           # sorted k-way merge with dedup
```

Two distinct modes:

1. **`cat` (default)** -- pure concatenation. Reads inputs sequentially, writes
   all elements. No ordering requirement, no dedup.
2. **`cat --dedupe`** -- sorted k-way merge with dedup by `(type, id)`. Requires
   all inputs to be sorted (Type_then_ID). Hard error on unsorted input.
   Tie-break: **last-seen wins** (later file in argument order takes priority,
   matching osmium merge semantics).

No `merge-pbf` command. First release ships with `cat --dedupe` only.

### apply-changes

```
pbfhogg apply-changes -o out.pbf base.pbf changes.osc
pbfhogg apply-changes -o out.pbf base.pbf changes.osc --locations-on-ways
pbfhogg apply-changes -o out.pbf base.pbf changes.osc --io-uring
```

Replaces old `merge` command. Name says what it does.

### getid --invert

```
pbfhogg getid -o out.pbf in.pbf n123 w456            # include by ID
pbfhogg getid -o out.pbf in.pbf --invert n123 w456   # exclude by ID
```

### tags-filter input detection

```
pbfhogg tags-filter -o out.pbf in.pbf highway=primary       # PBF mode
pbfhogg tags-filter -o out.osc in.osc highway=primary        # OSC mode (autodetect)
pbfhogg tags-filter -o out.osc --input-kind osc in.osc ...   # explicit override
```

OSC mode always preserves deletes. `-R`, `--invert-match`, `--remove-tags`
only apply in PBF mode.

## Resolved decisions

### check flag interaction

- `--ids --refs` together: run both (same as bare `check` with no flags).
- `--show-ids` without `--refs`: hard error. Only meaningful for ref checking.
- `--check-relations` without `--refs`: hard error. Same reason.
- `--full` without `--ids`: hard error. Only meaningful for ID duplicate detection.

### inspect --indexed semantics

Exit code is the primary interface (0 = indexed, 1 = not). Additionally:
- Default: prints a human-readable line ("Indexed: yes" / "Indexed: no").
- With `--json`: emits `{"indexed": true}` (or false).
- Combinable with other flags: `--indexed --json` works, `--indexed --blocks`
  works (shows both).

### tags-filter input detection

Three-tier detection, highest priority first:
1. `--input-kind pbf|osc` -- authoritative override, always wins.
2. Content sniffing -- read first bytes: PBF has a known binary blob header,
   OSC is XML (`<?xml` or `<osmChange`). For gzipped inputs, decompress the
   first chunk and sniff the decompressed bytes.
3. Extension fallback -- `.osc`/`.osc.gz` = OSC, `.pbf` = PBF. Only used
   when sniffing can't decide (shouldn't happen in practice).

When reading from stdin, `--input-kind` is required (hard error without it).
No content to sniff before committing to a parse path, and no extension.

### cat --dedupe unsorted behavior

Hard error only. No `--force-unsorted` slow fallback. Sorting+dedup in memory
is not viable at planet scale. If inputs are unsorted, run `pbfhogg sort` on
each one first, then `cat --dedupe`. Two explicit steps > hidden memory bomb.

### Incompatible flag validation

All commands must validate flag combinations at parse time, before any I/O.
Hard errors for incompatible combinations:

**check:**
- `--show-ids` without `--refs`
- `--check-relations` without `--refs`
- `--full` without `--ids`

**diff:**
- `--increment-version` without `--format osc`
- `--update-timestamp` without `--format osc`
- `--suppress-common`, `--verbose`, `--type` with `--format osc` (text-only flags)

**tags-filter (OSC mode):**
- `-R` / `--omit-referenced` in OSC mode
- `-i` / `--invert-match` in OSC mode
- `-t` / `--remove-tags` in OSC mode

These should be covered by unit tests on the CLI argument parser (clap
conflicts/requires), not just runtime checks in command logic.

### Breaking-change policy

Not applicable. No prior public release exists. This is v1 -- ship the clean
surface with no aliases, no deprecation shims, no backwards-compatibility.

## Design principles

1. **Flat surface** -- every command is one word (or hyphenated phrase) at the
   top level. No namespace indirection to memorize.
2. **Self-documenting names** -- `add-locations-to-ways`, `apply-changes`,
   `merge-changes` say what they do. No abbreviations.
3. **Consolidate duplicates, not reorganize** -- the problem was overlapping
   commands, not the flat structure. Fix the overlaps, keep the ergonomics.
4. **Explicit modes over magic** -- `--format osc`, `--input-kind osc`,
   `--dedupe` instead of auto-detection from file extensions.
5. **No aliases, no deprecation** -- first release, clean surface. No
   backwards-compatibility shims.

## Implementation plan

Vertical slices — each commit removes the old entrypoint, adds the new
flag/subcommand, and migrates tests. Functionality never disappears. Every
slice ends with `brokkr check` passing.

### Phase 1a: Sequential slices (inspect absorptions, must be ordered)

| # | Slice | Old | New |
|---|-------|-----|-----|
| 1 | inspect absorbs is-indexed | `is-indexed` | `inspect --indexed` |
| 2 | inspect absorbs node-stats | `node-stats` | `inspect --nodes` |
| 3 | inspect absorbs tags-count | `tags-count` | `inspect tags` subcommand |

These three all modify the `Inspect` Command variant and `run_inspect`, so
they must be sequential.

### Phase 1b: Parallel library work (slices 4-9)

After slice 3 lands, slices 4-9 are independent at the library level. Sub-
agents write library-side code in parallel, targeting the integration contracts
below. Then main.rs integration happens sequentially.

**Integration queue order** (by conflict risk, highest first):

| Order | Slice | Old | New |
|---|---|---|---|
| 1st | 9: rename merge | `Merge` | `ApplyChanges` |
| 2nd | 4: check absorbs check-refs + verify | `CheckRefs`, `Verify *` | `Check --ids --refs` |
| 3rd | 6: diff absorbs derive-changes | `DeriveChanges` | `diff --format osc` |
| 4th | 7: cat absorbs merge-pbf | `MergePbf` | `cat --dedupe` |
| 5th | 8: tags-filter absorbs tags-filter-osc | `TagsFilterOsc` | `tags-filter --input-kind` |
| 6th | 5: getid absorbs removeid | `Removeid` | `getid --invert` |

Slice 10 (merge-changes): no change needed, keep as-is.

**Integration gate:** after each main.rs integration, run `brokkr check` plus
one smoke invocation of the affected command before moving to the next slice.

### Per-slice checklist

Each slice (sequential or integrated from parallel work):

1. Remove old clap variant(s) from `Command` enum
2. Add new flag/subcommand to target `Command` variant
3. Update `run_*` dispatch function
4. Update or remove library re-exports in `src/lib.rs` and `src/commands/mod.rs`
5. Migrate tests
6. Update `notes/cli-reference.md` for affected command
7. Gate: `brokkr check` + smoke invocation of the command

### Integration contracts (slices 4-9)

Each contract defines: the exact new enum shape in main.rs, the library
function signature the sub-agent must target, and the expected help text.

---

**Slice 4: check absorbs check-refs + verify**

Enum shape (replaces `CheckRefs`, `Verify { VerifyCommand }`):
```rust
/// Validate PBF file integrity
Check {
    /// Input PBF file
    file: PathBuf,
    /// Check ID uniqueness and ordering
    #[arg(long)]
    ids: bool,
    /// Check referential integrity
    #[arg(long)]
    refs: bool,
    /// Also check relation member references (requires --refs)
    #[arg(long, requires = "refs")]
    check_relations: bool,
    /// Show IDs of missing objects (requires --refs)
    #[arg(long, requires = "refs")]
    show_ids: bool,
    /// Full duplicate detection via bitmap (requires --ids)
    #[arg(long, requires = "ids")]
    full: bool,
    /// Filter by element type for ID check (comma-separated: node, way, relation)
    #[arg(short = 't', long = "type")]
    type_filter: Option<String>,
    /// Stop after N violations per check (0 = unlimited)
    #[arg(long, default_value = "100")]
    max_errors: usize,
    /// Machine-readable JSON output
    #[arg(long)]
    json: bool,
    /// Exit-code only, no output
    #[arg(long, conflicts_with = "json")]
    quiet: bool,
    #[command(flatten)]
    io: DirectIoArg,
},
```

Library target: existing `pbfhogg::check_refs::check_refs()` and
`pbfhogg::verify_ids::verify_ids()` — no signature changes. The CLI dispatch
decides which to call based on `--ids`/`--refs` flags (default: both).

Remove: `Command::CheckRefs`, `Command::Verify`, `VerifyCommand` enum,
`run_check_refs`, `run_verify_ids`, `run_verify_refs`, `run_verify_all`.
Add: `run_check` that dispatches to both/either based on flags.

Help text:
```
pbfhogg check [OPTIONS] <FILE>
  Validate PBF file integrity (IDs + referential integrity by default)
```

---

**Slice 5: getid absorbs removeid**

Enum shape (modifies existing `Getid`, removes `Removeid`):
```rust
Getid {
    // ... all existing fields ...
    /// Invert: exclude matching IDs instead of including them
    #[arg(long)]
    invert: bool,
    // ... rest unchanged ...
},
```

Library target: existing `pbfhogg::getid::getid()` and
`pbfhogg::getid::removeid()`. CLI dispatch calls `removeid()` when
`--invert` is set, `getid()` otherwise. No library signature changes.

Remove: `Command::Removeid`, `run_removeid`.
Modify: `run_getid` to handle `--invert`.

Help text:
```
pbfhogg getid [OPTIONS] --output <OUTPUT> <FILE> [IDS]...
  Extract elements by ID (or exclude with --invert)
```

---

**Slice 6: diff absorbs derive-changes**

Enum shape (modifies existing `Diff`, removes `DeriveChanges`):
```rust
Diff {
    // ... all existing fields ...
    /// Output format: text (default) or osc
    #[arg(long, default_value = "text")]
    format: DiffFormat,
    /// Bump version of deleted elements by 1 (--format osc only)
    #[arg(long)]
    increment_version: bool,
    /// Set delete timestamp to current time (--format osc only)
    #[arg(long)]
    update_timestamp: bool,
    // ... rest unchanged ...
},
```

New enum:
```rust
#[derive(Clone, Copy, ValueEnum)]
enum DiffFormat {
    Text,
    Osc,
}
```

Library target: existing `pbfhogg::diff::diff()` and
`pbfhogg::derive_changes::derive_changes()`. CLI dispatch calls
`derive_changes()` when format is Osc, `diff()` when Text.
No library signature changes.

Remove: `Command::DeriveChanges`, `run_derive_changes`.
Modify: `run_diff` to handle `--format osc`.

Help text:
```
pbfhogg diff [OPTIONS] <OLD> <NEW>
  Compare two PBF files (text diff or OSC output)
```

---

**Slice 7: cat absorbs merge-pbf**

Enum shape (modifies existing `Cat`, removes `MergePbf`):
```rust
Cat {
    // ... all existing fields ...
    /// Sorted k-way merge with dedup (requires sorted inputs)
    #[arg(long)]
    dedupe: bool,
    #[command(flatten)]
    uring: UringArg,
    // ... rest unchanged ...
},
```

Library target: existing `pbfhogg::merge_pbf::merge_pbf()`. CLI dispatch
calls `merge_pbf()` when `--dedupe` is set, existing `cat()` otherwise.
No library signature changes.

Remove: `Command::MergePbf`, `run_merge_pbf`.
Modify: `run_cat` to handle `--dedupe`.

Help text:
```
pbfhogg cat [OPTIONS] --output <OUTPUT> <FILES>...
  Concatenate PBF files (--dedupe for sorted k-way merge with dedup)
```

---

**Slice 8: tags-filter absorbs tags-filter-osc**

Enum shape (modifies existing `TagsFilter`, removes `TagsFilterOsc`):
```rust
TagsFilter {
    // ... all existing fields ...
    /// Input kind override: pbf or osc (required for stdin, autodetect otherwise)
    #[arg(long = "input-kind")]
    input_kind: Option<InputKind>,
    // ... rest unchanged ...
},
```

New enum:
```rust
#[derive(Clone, Copy, ValueEnum)]
enum InputKind {
    Pbf,
    Osc,
}
```

Library target: existing `pbfhogg::tags_filter::tags_filter()` and
`pbfhogg::tags_filter_osc::tags_filter_osc()`. CLI dispatch calls the
appropriate one based on detected input kind. No library signature changes.

Remove: `Command::TagsFilterOsc`, `run_tags_filter_osc`.
Modify: `run_tags_filter` to handle OSC mode.

Help text:
```
pbfhogg tags-filter [OPTIONS] --output <OUTPUT> <FILE> [EXPRESSIONS]...
  Filter elements by tag expressions (PBF or OSC input)
```

---

**Slice 9: rename merge → apply-changes**

Enum shape (rename only):
```rust
/// Apply OSC diffs to a PBF file
ApplyChanges {
    // ... identical fields to current Merge ...
},
```

Library target: existing `pbfhogg::merge::merge()`. No changes at all.
Pure rename of the CLI entrypoint.

Remove: `Command::Merge`.
Add: `Command::ApplyChanges` (identical fields).
Rename: `run_merge` → `run_apply_changes`.

Help text:
```
pbfhogg apply-changes [OPTIONS] --output <OUTPUT> <BASE> <CHANGES>
  Apply OSC diffs to a PBF file
```

---

### Parallelism plan

```
Sequential:  Slice 1 → Slice 2 → Slice 3
             (all modify Inspect in main.rs)

Parallel:    Sub-agents write library-side code for slices 4-9
             (independent files, no main.rs changes)
             Sub-agents must NOT run shell commands.

Sequential:  Integrate into main.rs in queue order:
             9 → 4 → 6 → 7 → 8 → 5
             Gate after each: brokkr check + smoke test

Sequential:  Phase 2 (validation) → Phase 3 (docs)
```

### Phase 2: Cross-cutting validation (after all slices land)

New behavior that doesn't exist in any old command.

1. Incompatible flag validation (clap conflicts/requires) for `check`, `diff`,
   `tags-filter` — with unit tests on the argument parser.
2. Three-tier input detection for `tags-filter`: `--input-kind` > content
   sniffing (with gzip first-chunk decompression) > extension fallback.
   `--input-kind` required for stdin.
3. `inspect --indexed` exit-code semantics + `--json` interaction.

### Phase 3: Documentation (hard gate before release)

1. Regenerate `notes/cli-reference.md`.
2. Update `notes/osmium-parity.md` command mapping table.
3. Update `CLAUDE.md` references to old command names.
4. Update `TODO.md`.
5. Update `README.md`.
6. Verify all `brokkr verify` and `brokkr bench commands` references are
   current (should already be done per-slice, this is the final audit).

## Implementation notes

- `cat --dedupe` reuses the `merge-pbf` k-way sorted merge codepath. The flag
  changes the algorithm entirely -- not concatenation with a post-hoc dedup
  pass. Requires sorted inputs; hard error otherwise. Tie-break: last-seen
  wins (later argument takes priority).
- No `merge-pbf` command ships. First release has `cat --dedupe` only.
- `inspect tags` is a clap subcommand under `inspect`, not a flag. This keeps
  the positional expression args clean.
- `check` with no flags runs both `--ids` and `--refs` (equivalent to old
  `verify all`).
- `tags-filter` input detection follows the three-tier priority in Resolved
  decisions: `--input-kind` > content sniffing (with gzip decompression) >
  extension fallback. `--input-kind` required for stdin. OSC mode is a
  restricted subset (no `-R`, no `--invert-match`, no `--remove-tags`).
