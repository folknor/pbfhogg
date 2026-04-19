# Source tree restructure

Two-stage reorganization of `src/` into a coherent package layout.

## Stage 1: library package layout

### Problem

`src/` outside `src/commands/` grew organically. Some pieces are folders (`read/`, `write/`, `geocode_index/`), some are flat top-level files (`osc.rs` 1506 lines, `blob_index.rs` 1225, `geo.rs`, `error.rs`, `debug.rs`, `reorder_buffer.rs`), and a chunk of genuinely shared infrastructure is misfiled inside `src/commands/` (`id_set_dense.rs`, `tag_expr.rs`, `external_radix.rs`, `node_scanner.rs`, `way_scanner.rs`, `elements_pbf.rs`, `elements_xml.rs`, `stream_merge.rs`). The shared files are imported by multiple commands and by `read/`, `geocode_index/`, etc.

Goal: domain-driven packages (Java standard library style), each package flat inside, files grouped by what they are FOR rather than by their size. File size does not drive folder creation; only domain coherence does.

Constraint: Stage 1 must land before Stage 2. Otherwise the command-folder migration would touch the same imports twice, or freeze the misfiled shared infrastructure in place under `src/commands/`.

### Plan

1. **Inventory pass (subagent fan-out).** Main conversation lists the in-scope files and assigns batches. Spawn parallel foreground subagents, each given a batch of source files (read-only, no shell). For each file the subagent produces:
   - One-paragraph summary of what the file does.
   - List of `pub` / `pub(crate)` items that form its API surface.
   - Inbound dependencies: which other project files import from it (`grep` for the module name).
   - Outbound dependencies: what this file imports from elsewhere in the project.
   - Current line count.

   Files in scope: all `.rs` under `src/` except command-specific files in `src/commands/`. Explicit exception: the misfiled shared infrastructure files inside `src/commands/` listed above ARE in scope for Stage 1.

2. **Consolidation.** Main conversation merges subagent reports into a single inventory document at `notes/source-tree-inventory.md`. One row per file: path, one-line summary, API items, inbound / outbound deps, line count.

3. **Categorization.** With the user, decide the final package list and assign each file to a package. Candidates to consider (not pre-decided): `osc/`, `index/` (blob index plus geocode index?), `format/` or `wire/`, `tags/`, plus the existing `read/` and `write/`. The inventory drives the grouping; do not anchor on a candidate list before reading the inventory.

4. **Move sequencing.** One package landed per commit. Pure move plus module wiring, no behavior changes. Preserve `#[cfg_attr(feature = "hotpath", hotpath::measure)]`, marker, and counter coverage verbatim. Run `brokkr check` after each move. Update `notes/` cross-references when line numbers shift substantially.

### Target structure (agreed 2026-04-19)

After Stage 1, `src/` outside `src/commands/` has the following shape. File counts and line counts reference the inventory at `notes/source-tree-inventory.md`.

#### Top-level flat (foundational, no relocation)

| File | Lines | Notes |
|------|------:|-------|
| `src/lib.rs` | 177 | crate root |
| `src/error.rs` | 162 | error types |
| `src/debug.rs` | 111 | profiler markers / counters |
| `src/reorder_buffer.rs` | 118 | sequence reorder utility |
| `src/geo.rs` | 643 | geometry; promote to `src/geo/` package later if GeoJSON adds enough geometry helpers to warrant splitting |

#### Top-level flat (lifted or renamed)

| Target | Origin | Lines | Reason |
|--------|--------|------:|--------|
| `src/blob_meta.rs` | `src/blob_index.rs` | 1225 | renamed. The file is misleadingly named "index": it actually holds per-blob metadata embedded in BlobHeader extras (`BlobBbox`, `BlobIndex`, `BlobFilter`, `scan_block_ids`, `scan_block_tags`). |
| `src/idset.rs` | `src/commands/id_set_dense.rs` | 816 | lifted out of `commands/`; renamed. Pbfhogg has only the chunked-sparse variant (no `IdSetSmall` like osmium), so the `_dense` qualifier is unused. |
| `src/owned.rs` | `src/commands/elements_pbf.rs` | 316 | lifted out of `commands/`; renamed. After the OSC bundling, the `_pbf` qualifier is redundant. The file is the owned/mutable form of PBF elements used as a scratchpad between decode and re-encode (sort, merge_pbf, time_filter). |
| `src/tag_expr.rs` | `src/commands/tag_expr.rs` | 284 | lifted out of `commands/` (consumed by `tags_count`, `tags_filter`, `tags_filter_osc`). |

#### Existing packages (no relocation; internal splits deferred)

| Package | Files | Notes |
|---------|------:|-------|
| `src/read/` | 12 | unchanged |
| `src/write/` | 9 | unchanged |
| `src/geocode_index/` | 11 | unchanged |

Internal restructuring of these packages (e.g. splitting the oversized `read/blob.rs`, `write/block_builder.rs`, `write/writer.rs`, `geocode_index/reader.rs`) is out of scope for Stage 1.

#### New packages

**`src/osc/`** unifies the OSM change-format domain:

| File | Origin | Lines | Role |
|------|--------|------:|------|
| `osc/parse.rs` | `src/osc.rs` | 1506 | OSC input parser (`CompactDiffOverlay`, `parse_osc_file`, `load_all_diffs`) |
| `osc/write.rs` | `src/commands/elements_xml.rs` | 420 | OSC XML output: writers + private owned element types (`OwnedNode/Way/Relation`, equality, `write_*_xml`) |
| `osc/merge_join.rs` | `src/commands/stream_merge.rs` | 920 | merge-join driving `diff` and `derive_changes` (which produce OSC output) |

Bundling rationale: the three files form the OSC read / write / merge-join story. The `stream_merge → elements_xml` coupling that Stage 1 step 2 flagged is resolved by making them siblings inside the same package. The owned XML element types stay as private implementation detail of `osc/write.rs` rather than getting their own "elements" file.

**`src/scan/`** holds lightweight wire-format scanners that bypass full PrimitiveBlock construction:

| File | Origin | Lines | Role |
|------|--------|------:|------|
| `scan/node.rs` | `src/commands/node_scanner.rs` | 110 | extract `(id, lat, lon)` tuples from DenseNodes |
| `scan/way.rs` | `src/commands/way_scanner.rs` | 392 | extract way IDs / refs, with a geocode-tagged variant for street / building / interp classification |

#### Stays in `src/commands/` (deferred to Stage 2)

- `src/commands/external_radix.rs` (76) - altw-only consumer, not actually shared. Will fold into `src/commands/altw/` during Stage 2 rather than lift to a top-level package.

#### Type renames

- `IdSetDense` → `IdSet` (drop the unused `_dense` suffix at the same time as the file rename).

#### TypeFilter relocation (decided 2026-04-19)

`src/commands/mod.rs` defines `TypeFilter` (a "node | way | relation" element-type filter) which is imported by `src/commands/tag_expr.rs`. Decision: **bundle into `src/owned.rs`** alongside the owned PBF element types - element-type filter conceptually fits where the element types live.

#### Move sequencing

One package per commit, pure move + module wiring + import-path updates, no behavior changes. Subagents handle the per-move file edits and import-path sweeps (the rule against subagent edits is suspended for this work). Main conversation orchestrates: announces each move, runs `brokkr check`, commits.

Order (safest first, then by dependency relief):

1. `blob_index.rs → src/blob_meta.rs` - pure top-level rename
2. `commands/id_set_dense.rs → src/idset.rs` + type `IdSetDense → IdSet` - lifts misfiled infra; clears 22 inbound deps including `read/indexed.rs`, so `read/` becomes self-contained
3. `commands/{node_scanner,way_scanner}.rs → src/scan/{node,way}.rs` - sibling pair lift
4. `commands/elements_pbf.rs → src/owned.rs` - rename + lift; creates the file `TypeFilter` will land in
5. `commands/tag_expr.rs → src/tag_expr.rs` + `TypeFilter` lift into the now-existing `src/owned.rs`
6. OSC bundling: `osc.rs` + `commands/elements_xml.rs` + `commands/stream_merge.rs → src/osc/{parse,write,merge_join}.rs` - most complex (three files coalesce + internal type movements); do last

### Done when

The non-commands portion of `src/` matches the target structure above. No misfiled shared infrastructure remains inside `src/commands/` (with the explicit exception of `external_radix.rs`). `brokkr check` is clean. Stage 2 can begin against a stable library API surface.

## Stage 2: commands as per-folder units

### Problem

After Stage 1, `src/commands/` contains only CLI command code. Each command should sit in its own folder regardless of current size, including small ones like `renumber.rs` (1.1K). Uniformity has more value than per-command size optimization, and folders give every command a natural growth path. The mega `src/commands/mod.rs` (1295 lines) collapses into thin CLI dispatch once each command's surface lives in its own folder.

Plan shape mirrors Stage 1: inventory pass via subagents (one batch per command grouping, summarizing what each command file does and what it shares with neighbors), consolidate into a per-command planning document, then one folder migration per commit under the existing rules at `TODO.md:36-46`. Drafted in detail after Stage 1 lands and the library API surface is stable.
