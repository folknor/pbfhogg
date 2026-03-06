# osmium-tool feature parity tracker

Comparison of osmium-tool commands and flags against pbfhogg. Based on
osmium-tool installed version as of 2026-03-04.

## Command mapping

| osmium command | pbfhogg equivalent | Notes |
|---|---|---|
| `add-locations-to-ways` | `add-locations-to-ways` | Have it |
| `apply-changes` | `merge` | Different name, same operation |
| `cat` | `cat` | Have it |
| `changeset-filter` | — | Missing (niche) |
| `check-refs` | `check-refs` | Have it |
| `create-locations-index` | — | Missing (pbfhogg builds indexes in-memory) |
| `derive-changes` | `derive-changes` | Have it |
| `diff` | `diff` | Have it |
| `export` | — | Missing (GeoJSON/GeoJSONSeq/PG) |
| `extract` | `extract` | Have it |
| `fileinfo` | `inspect` | Different name, different approach |
| `getid` | `getid` | Have it |
| `getparents` | `getparents` | Have it |
| `merge` | — | Missing (merge multiple sorted PBFs with dedup by version) |
| `merge-changes` | `--simplify` | Implemented (`merge-changes`, plus optional simplification by last change per object) |
| `query-locations-index` | — | Missing (paired with create-locations-index) |
| `removeid` | `removeid` | Have it |
| `renumber` | — | Missing (reassign IDs starting from 1) |
| `show` | — | Missing (pretty-print to terminal with pager) |
| `sort` | `sort` | Have it |
| `tags-count` | `tags-count` | Have it |
| `tags-filter` | `tags-filter` | Have it |
| `time-filter` | `time-filter` | Have it |

## pbfhogg-only commands (no osmium equivalent)

- `is-indexed` — check if PBF has blob-level indexdata
- `node-stats` — coordinate statistics for FOR compression sizing
- `verify ids` — ID uniqueness and ordering validation
- `verify refs` — referential integrity (wraps check-refs with JSON/quiet modes)
- `verify all` — run all verification checks
- `bench-read` / `bench-write` / `bench-merge` — internal benchmarking harnesses

## Missing commands

### High priority (pipeline workflows)

- **`merge` (multi-PBF)** — merge multiple sorted PBFs into one, deduplicating
  by highest version. osmium's `merge` is distinct from `apply-changes`: it
  combines N sorted PBFs, not base+diff. Used for recombining extracts.
  Already in TODO.md with upstream refs.

### Medium priority (useful but not pipeline-critical)

- **`renumber`** — reassign node/way/relation IDs starting from configurable
  base (default 1,1,1). Useful for anonymizing data, reducing ID space for
  testing, and preparing data for tools that struggle with high IDs. Flags:
  `--start-id`, `--object-type`, `--index-directory` (persist renumbering map).
- **`export`** — export to GeoJSON, GeoJSONSeq, or PG text format. Large scope:
  geometry assembly, index types, config file, attribute selection. Probably
  out of scope for pbfhogg's core mission.

### Low priority

- **`changeset-filter`** — filter changeset files by user, time, bbox, etc.
  Changesets are a niche use case outside core PBF processing.
- **`create-locations-index` / `query-locations-index`** — build and query
  persistent on-disk node location indexes. pbfhogg builds indexes in-memory
  (DenseMmapIndex). Only needed if external tools want to share a node index.
- **`show`** — pretty-print PBF contents to terminal via pager. `inspect`
  covers the metadata case; element-level display would need OPL or debug
  format output.

## Missing flags on existing commands

### `add-locations-to-ways`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `-n, --keep-untagged-nodes` | `--keep-untagged-nodes` | Have it |
| `-i, --index-type` | — | N/A (DenseMmapIndex only, by design) |
| `--index-type-neg` | — | N/A (DenseMmapIndex only, by design) |
| `--keep-member-nodes` | — | N/A (always-on, see DEVIATIONS.md) |
| `--ignore-missing-nodes` | — | N/A (always-on, see DEVIATIONS.md) |

### `cat`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `-t, --object-type` | `-t, --type` | Have it |
| `-c, --clean` | `-c, --clean` | Have it (per-attribute: version, changeset, timestamp, uid, user) |
| `--buffer-data` | — | N/A (pipelined writer handles this differently) |

### `check-refs`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `-r, --check-relations` | `--check-relations` | Have it |
| `-i, --show-ids` | `--show-ids` | Have it (format: `n123 in w456`, each occurrence, stdout) |

### `derive-changes`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `--increment-version` | `--increment-version` | Have it |
| `--keep-details` | — | N/A (niche, only useful for debugging deleted objects) |
| `--update-timestamp` | `--update-timestamp` | Have it (sets delete timestamp to current wall-clock time) |

### `diff`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `-c, --suppress-common` | `-c, --suppress-common` | Have it |
| `-t, --object-type` | `-t, --type` | Have it |
| `--ignore-changeset` | `--ignore-changeset` | Have it (compatibility flag, already ignored by default) |
| `--ignore-uid` | `--ignore-uid` | Have it (compatibility flag, already ignored by default) |
| `--ignore-user` | `--ignore-user` | Have it (compatibility flag, already ignored by default) |
| `-q, --quiet` | `-q, --quiet` | Have it |
| `-o, --output` | `-o, --output` | Have it |
| `-s, --summary` | `-s, --summary` | Have it (left/right/same/different counts on stderr) |

### `extract`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `-b, --bbox` | `-b, --bbox` | Have it |
| `-p, --polygon` | `-p, --polygon` | Have it |
| `-s, --strategy` | `--simple`, `--smart` | Have it (different syntax) |
| `-c, --config` | — | Missing (multi-extract from config file) |
| `-H, --with-history` | — | N/A (current-snapshot tool, no history file support) |
| `--set-bounds` | `--set-bounds` | Have it (opt-in, writes bbox to output header) |
| `--clean` | `--clean` | Have it (per-attribute: version, changeset, timestamp, uid, user) |
| `-S, --option` | — | Missing (strategy-specific options) |

### `getid`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `-r, --add-referenced` | `-r, --add-referenced` | Have it |
| `-i, --id-file` | `-i, --id-file` | Have it |
| `-I, --id-osm-file` | `-I, --id-osm-file` | Have it (scans all element IDs, additive with -i and CLI args) |
| `-H, --with-history` | — | N/A (current-snapshot tool, no history file support) |
| `-t, --remove-tags` | `-t, --remove-tags` | Have it (strips tags from referenced-only nodes, use with -r) |
| `--verbose-ids` | `--verbose-ids` | Have it (prints requested IDs and reports missing) |
| `--default-type` | `--default-type` | Have it |

### `getparents`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `--add-self` | `--add-self` | Have it |
| `-i, --id-file` | `-i, --id-file` | Have it |
| `-I, --id-osm-file` | `-I, --id-osm-file` | Have it |
| `--default-type` | `--default-type` | Have it |

### `merge` (apply-changes)

| osmium flag | pbfhogg | Status |
|---|---|---|
| (base + changes) | base + changes | Have it |
| `--redact` | — | N/A (requires history file support) |
| `-H, --with-history` | — | N/A (current-snapshot tool, no history file support) |
| `--locations-on-ways` | `--locations-on-ways` | Have it |

### `removeid`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `-i, --id-file` | `-i, --id-file` | Have it |
| `-I, --id-osm-file` | `-I, --id-osm-file` | Have it (scans all element IDs, additive with -i and CLI args) |
| `--default-type` | `--default-type` | Have it |

### `sort`

No missing flags. osmium sort has no command-specific options either.

### `tags-count`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `-t, --object-type` | `-t, --type` | Have it |
| `-m, --min-count` | `--min-count` | Have it |
| `-e, --expressions` | `-e, --expressions` | Have it (one per line, `#` comments, additive with CLI args) |
| `-M, --max-count` | `-M, --max-count` | Have it |
| `-s, --sort` | `-s, --sort` | Have it (count-desc/asc, name-asc/desc, plus shortcuts) |
| expressions (positional) | expressions (positional) | Have it (optional key/value filter, same syntax as tags-filter) |

### `tags-filter`

| osmium flag | pbfhogg | Status |
|---|---|---|
| `-R, --omit-referenced` | `-R, --omit-referenced` | Have it |
| expressions (positional) | expressions (positional) | Have it |
| `-e, --expressions` | `-e, --expressions` | Have it (one per line, `#` comments, additive with CLI args) |
| `-i, --invert-match` | `-i, --invert-match` | Have it (inverts match: keep non-matching, exclude matching) |
| `-t, --remove-tags` | `-t, --remove-tags` | Have it (strips tags from referenced-only objects, use without -R) |

### `inspect` (fileinfo)

| osmium flag | pbfhogg | Status |
|---|---|---|
| `--blocks` | `--blocks` | Have it |
| `--id-ranges` | `--id-ranges` | Have it |
| `--locations` | `--locations` | Have it |
| `-e, --extended` | `-e, --extended` | Have it (timestamp range, data bbox, objects ordered, metadata coverage; auto-enables --id-ranges) |
| `-g, --get` | `-g, --get` | Have it (dot-path key accessor for scripting; auto-enables -e for data.*/metadata.* keys) |
| `-j, --json` | `--json` | Have it |
| `-c, --crc` | — | N/A (niche, not useful for PBF processing) |

## Common flags pbfhogg does not have

These appear on nearly every osmium command but have no pbfhogg equivalent:

- **`-F, --input-format`** — osmium supports PBF, XML, OPL, O5M. pbfhogg is
  PBF-only (by design).
- **`-f, --output-format`** — same. pbfhogg outputs PBF only (except
  derive-changes which outputs OSC).
- **`-v, --verbose`** — osmium has per-command verbose mode. pbfhogg has no
  verbosity control.
- **`--progress / --no-progress`** — progress bars. pbfhogg has none.
- **`--fsync`** — call fsync after writing. pbfhogg does not offer this.
- **`-O, --overwrite`** — osmium refuses to overwrite by default. pbfhogg
  always overwrites.
- **`--generator`** — set generator string in output header.
- **`--output-header`** — set arbitrary output header fields.
