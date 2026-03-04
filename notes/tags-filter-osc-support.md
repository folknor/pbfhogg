# tags-filter OSC support: preserving delete actions

## Problem

When `tags-filter` processes an OSC file in a diff pipeline, delete actions
are silently dropped because they carry no tags and never match any filter
expression. This breaks pipelines like:

```
derive-changes → tags-filter highway=* → merge
```

The filtered OSC loses all deletes, so stale `highway=*` elements are never
removed from the base PBF. osmium has the same limitation — their workaround
is to not use tags-filter on OSC files at all.

References: [osmium-tool#298](https://github.com/osmcode/osmium-tool/issues/298)

## Current state

`tags-filter` is PBF-in, PBF-out only (`src/commands/tags_filter.rs`, ~850
lines). It has no concept of deletes — it's purely additive.

The OSC parsing infrastructure (`CompactDiffOverlay` in `src/osc.rs`) is
mature and decoupled. It already stores deletes in separate HashSets
(`deleted_nodes`, `deleted_ways`, `deleted_relations`), separate from
created/modified elements stored in arena-indexed hash maps.

The OSC write path exists in `derive_changes.rs` (`write_osc` and helpers).

## Design

### CLI

```
pbfhogg tags-filter-osc <changes.osc.gz> -o <filtered.osc.gz> <expressions...>
```

Separate subcommand, not an extension of the existing `tags-filter`. Rationale:
the existing command is PBF-in/PBF-out with indexdata requirements, two-pass
reference expansion, and parallel blob processing — none of which apply to OSC
filtering. A separate command is simpler and avoids conditional logic sprawl.

No `--force`, no `--omit-referenced` (`-R`), no `--compression` — the input is
a small OSC, not a multi-GB PBF. Reference expansion doesn't apply (the OSC
doesn't contain the full way→node graph).

### Operation

1. Parse OSC into `CompactDiffOverlay` via existing `parse_osc_file()`
2. Iterate created/modified elements, apply tag matchers (same `matches_any()`
   logic as existing tags-filter)
3. Collect matching elements into output lists
4. Write gzipped OsmChange XML with three sections:
   - `<create>` / `<modify>`: only elements matching filter expressions
   - `<delete>`: **all** deletes from the input, unfiltered
5. Print stats: creates/modifies kept, creates/modifies dropped, deletes
   preserved

### Alternative: auto-forward deletes by default

The TODO suggests deletes could be auto-forwarded when input is OSC, since
dropping them is almost never what the user wants. This is the right default —
a `--drop-deletes` flag could suppress it for the rare case where someone wants
tag-filtered deletes too, but that's a future extension.

### Key insight: no base PBF needed

Unlike the existing `tags-filter` (which does two-pass reference expansion to
include way→node dependencies), OSC filtering doesn't need a base PBF. The OSC
is a self-contained change set — we're just pruning creates/modifies by tags
and passing deletes through. This dramatically simplifies the implementation.

## Implementation plan

### 1. New command: `tags_filter_osc.rs` (~150 lines)

```rust
pub fn tags_filter_osc(
    changes: &Path,
    output: &Path,
    expressions: &[TagExpression],
) -> Result<TagsFilterOscStats>
```

- Parse OSC via `parse_osc_file(changes)`
- Iterate `overlay.node_ids()`, `overlay.way_ids()`, `overlay.relation_ids()`
- For each element, check tags against expressions using existing matchers
- Collect matching elements
- Write filtered OSC (reuse `write_osc` pattern from derive_changes)
- Forward all `overlay.deleted_*` sets to the `<delete>` section

### 2. CLI entry point in `cli/src/main.rs` (~15 lines)

New subcommand variant:

```rust
TagsFilterOsc {
    /// OSC change file (.osc.gz)
    changes: PathBuf,
    #[command(flatten)]
    output: OutputArg,
    /// Tag filter expressions
    expressions: Vec<String>,
}
```

### 3. Reusable OSC write helpers

The `write_osc` function in `derive_changes.rs` writes gzipped OsmChange XML
from `Changes` structs containing `OwnedNode/OwnedWay/OwnedRelation`. For the
OSC filter, we need to write from `CompactDiffOverlay` elements instead.

Two options:
- **A)** Convert `CompactNodeRef`/`CompactWayRef`/`CompactRelationRef` to
  `OwnedNode`/`OwnedWay`/`OwnedRelation` and reuse `write_osc` as-is.
  Simple but allocates owned copies.
- **B)** Write a new `write_filtered_osc` that writes directly from compact
  refs. Zero-copy but duplicates XML writing logic.

Option A is simpler and the OSC is small (tens of thousands of elements, not
millions). The allocation overhead is negligible. Go with A.

### 4. Tag expression parsing

Already exists in `tags_filter.rs` as `parse_expressions()` → `Vec<TagExpression>`.
Currently `pub(crate)` — may need to be made accessible from the new module, or
the parsing can be shared via `super::tags_filter::parse_expressions()`.

## Estimated scope

- `src/commands/tags_filter_osc.rs`: ~150 lines (new file)
- `cli/src/main.rs`: ~15 lines (new subcommand + handler)
- `src/commands/mod.rs`: 1 line (module declaration)
- Shared code changes: minimal (expose `parse_expressions` if needed)

Total: ~170 lines of new code. No changes to existing commands.

## Not in scope

- Filtering deletes by tag (deletes carry no tags — would need base PBF lookup)
- Reference expansion for OSC (would need base PBF, defeats the purpose)
- Streaming OSC processing (OSCs fit comfortably in memory)
- Combining with merge in a single command
