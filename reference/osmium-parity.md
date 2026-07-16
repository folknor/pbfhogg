# Migrating from osmium-tool to pbfhogg

This guide maps osmium-tool commands and flags to their pbfhogg equivalents.
If you're already familiar with osmium, you should feel at home - most commands
have the same names and flags, with a few consolidations that reduce the total
command count.

## Quick reference

| osmium | pbfhogg | What changed |
|--------|---------|--------------|
| `cat` | `cat` | Same |
| `sort` | `sort` | Same |
| `extract` | `extract` | `--strategy` replaced by `--simple` / `--smart` flags |
| `tags-filter` | `tags-filter` | Also handles OSC input (see below) |
| `getid` | `getid` | Same |
| `getparents` | `getparents` | Same |
| `renumber` | `renumber` | Same |
| `add-locations-to-ways` | `add-locations-to-ways` | Same |
| `time-filter` | `time-filter` | Same |
| `diff` | `diff` | Content equality instead of version ordering (see [DEVIATIONS](../DEVIATIONS.md#diff-content-equality-vs-version-ordering)) |
| `apply-changes` | `apply-changes` | Same |
| `merge-changes` | `merge-changes` | Same |
| `export` | `export` | Fixed area rule and property model, not osmium's configurable export rules - see `export: area detection and property model` below |
| `fileinfo` | `inspect` | Different name, more capabilities |
| `check-refs` | `check --refs` | Consolidated into `check` |
| `merge` | `cat --dedupe` | Consolidated into `cat` |
| `derive-changes` | `diff --format osc` | Consolidated into `diff` |
| `removeid` | `getid --invert` | Consolidated into `getid` |
| `tags-count` | `inspect tags` | Consolidated into `inspect` |

## export: area detection and property model

osmium's `export` reads a user-supplied JSON config that can override
per-element geometry type, property inclusion, and area classification.
pbfhogg's `export` has no config file - the rules are fixed:

- **Area detection** (`src/commands/export/geometry.rs::is_area_way`,
  `is_area_tags`): a way becomes a `Polygon` only if it is closed
  (`first ref == last ref`, at least 4 refs) AND its tags pass the area
  heuristic - `area=no` always disqualifies; otherwise `area=yes` or the
  presence of any of `building`, `landuse`, `natural`, `leisure`,
  `amenity`, `boundary`, `waterway` (`AREA_KEYS`) qualifies. Every other
  way (open, or closed but untagged for area) becomes a `LineString`.
  Polygon exterior rings are re-wound counterclockwise
  (`write_way_geometry`'s signed-area check) and always explicitly
  closed in the output, regardless of whether the source way repeats
  its first node ID. A closed way with fewer than 3 geometrically
  distinct positions is rejected as invalid geometry, not silently
  emitted as a degenerate polygon.
- **Property model** (`src/commands/export/properties.rs::write_properties`):
  every feature gets `@id` and `@type`. `--metadata` adds `@version`,
  `@timestamp` (RFC 3339 UTC), `@changeset`, `@uid`, `@user`, `@visible`
  when present on the source element. Remaining OSM tags are emitted as
  properties in source order, first-occurrence-wins on a duplicate key,
  filtered by `--properties` (whitelist) when given, and skipped
  entirely if the key collides with a reserved `@`-prefixed name (the
  tag is dropped, not renamed or errored).

There is no equivalent to osmium's config-driven per-element overrides;
every input is classified by the same fixed rule.

## Consolidated commands

pbfhogg merges several osmium commands that do closely related things. The
underlying functionality is identical - only the CLI entrypoint changed.

### check-refs is now `check`

pbfhogg's `check` command validates both ID integrity and referential integrity
in a single pass by default.

```
# osmium
osmium check-refs input.pbf
osmium check-refs -r input.pbf

# pbfhogg
pbfhogg check input.pbf                          # runs both ID + ref checks
pbfhogg check input.pbf --refs                   # ref check only (same as osmium check-refs)
pbfhogg check input.pbf --refs --check-relations # same as osmium check-refs -r
pbfhogg check input.pbf --refs --show-ids        # show missing refs (n123 in w456)
pbfhogg check input.pbf --ids                    # ID uniqueness/ordering only
pbfhogg check input.pbf --json                   # machine-readable output
```

**Counting difference:** For missing relation-to-relation members, osmium reports
the number of broken references (occurrences), while pbfhogg reports unique
missing IDs with the occurrence count in parentheses when they differ:
`Missing relation members: 706 (777 references)`. Both tools find the same
set of missing IDs.

### removeid is now `getid --invert`

```
# osmium
osmium removeid -o out.pbf input.pbf n123 w456

# pbfhogg
pbfhogg getid --invert -o out.pbf input.pbf n123 w456
```

All ID source flags (`-i`, `-I`, `--default-type`) work in both normal and
inverted mode. The getid-only flags (`-r`, `-t`, `--verbose-ids`) are not
available with `--invert`.

### derive-changes is now `diff --format osc`

```
# osmium
osmium derive-changes old.pbf new.pbf -o changes.osc

# pbfhogg
pbfhogg diff --format osc old.pbf new.pbf -o changes.osc
pbfhogg diff --format osc old.pbf new.pbf -o changes.osc --increment-version
pbfhogg diff --format osc old.pbf new.pbf -o changes.osc --update-timestamp
```

Text diff (the default `--format text`) and OSC diff have separate flag sets.
Text-only flags (`-c`, `-v`, `-s`, `-q`, `-t`) are rejected with `--format osc`,
and vice versa for `--increment-version` / `--update-timestamp`.

**Lossless deletes:** pbfhogg's `diff --format osc` produces a perfect roundtrip
- applying the derived OSC to the old PBF reproduces the new PBF exactly.
osmium's `derive-changes` can lose deletes when the deleted element is absent
from both input files (see [DEVIATIONS](../DEVIATIONS.md#derive-changes-lossless-delete-roundtrip)).

### merge is now `cat --dedupe`

osmium's `merge` (sorted k-way merge with dedup) is `cat --dedupe` in pbfhogg.
Plain `cat` remains concatenation without dedup, just like osmium's `cat`.

```
# osmium
osmium merge -o merged.pbf a.pbf b.pbf c.pbf

# pbfhogg
pbfhogg cat --dedupe -o merged.pbf a.pbf b.pbf c.pbf
```

All inputs must be sorted. Unsorted input is a hard error - run `pbfhogg sort`
first if needed.

### tags-count is now `inspect tags`

```
# osmium
osmium tags-count input.pbf highway
osmium tags-count -t way -s count-desc --min-count 100 input.pbf

# pbfhogg
pbfhogg inspect tags input.pbf highway
pbfhogg inspect tags input.pbf -t way -s count-desc --min-count 100
```

### tags-filter-osc is now `tags-filter --input-kind osc`

pbfhogg auto-detects OSC vs PBF input by content sniffing and file extension,
so in most cases you don't need to specify `--input-kind` at all.

```
# osmium (separate command)
osmium tags-filter -o out.osc changes.osc.gz highway=primary

# pbfhogg (same command, auto-detected)
pbfhogg tags-filter -o out.osc changes.osc.gz highway=primary

# explicit override if needed
pbfhogg tags-filter --input-kind osc -o out.osc changes.osc.gz highway=primary
```

OSC mode always preserves deletes. PBF-only flags (`-R`, `-i`, `-t`) are
rejected in OSC mode.

### fileinfo is now `inspect`

```
# osmium
osmium fileinfo input.pbf
osmium fileinfo -e input.pbf
osmium fileinfo -g header.bbox input.pbf
osmium fileinfo -j input.pbf

# pbfhogg
pbfhogg inspect input.pbf
pbfhogg inspect -e input.pbf
pbfhogg inspect -g header.bbox input.pbf
pbfhogg inspect --json input.pbf
```

pbfhogg's `inspect` also includes capabilities not in osmium:

- `--indexed` - check if a PBF has blob-level indexdata (exit code 0/1)
- `--nodes` - coordinate statistics for compression analysis
- `--anomalies` - highlight unusual blocks

## Flag differences

Most flags are identical between osmium and pbfhogg. Here are the differences
worth knowing about.

### Different flag names

| osmium | pbfhogg | Commands |
|--------|---------|----------|
| `-t, --object-type` | `-t, --type` | cat, diff, inspect tags, check |
| `--strategy simple\|complete_ways\|smart` | `--simple` / `--smart` (default: complete) | extract |
| `-p, --polygon` (accepts `.poly`, GeoJSON, or OSM file) | `-p, --polygon` (GeoJSON only: bare Polygon/MultiPolygon geometry, Feature, or FeatureCollection - first feature only; Osmosis `.poly` files are not accepted) | extract |

### Flags pbfhogg doesn't have

These osmium flags have no pbfhogg equivalent:

| Flag | Reason |
|------|--------|
| `-F, --input-format` | pbfhogg is PBF-only by design |
| `-f, --output-format` | PBF output only (except `diff --format osc` and `merge-changes`) |
| `-v, --verbose` | No per-command verbosity control |
| `--progress` / `--no-progress` | No progress bars |
| `-O, --overwrite` | pbfhogg always overwrites |
| `--fsync` | Always enabled (no flag needed) |
| `-H, --with-history` | Current-snapshot tool, no history file support |
| `--buffer-data` | Pipelined writer handles buffering internally |
| `--index-type` | `sparse` (default), `external` (bounded memory, sequential I/O, only mode that survives at planet on memory-constrained hosts), `auto` (scale-aware: sparse unless the input is sorted+indexed and the estimated node store exceeds ~80 % of available RAM; see reference/pipeline.md). Different valid values from osmium's `-i, --index-type`. |

### Flags only pbfhogg has

| Flag | Commands | Purpose |
|------|----------|---------|
| `--force` | Most commands | Run without indexdata (slower fallback) |
| `--direct-io` | Most commands | Bypass page cache via O_DIRECT |
| `--io-uring` | apply-changes, cat --dedupe, sort | io_uring output I/O |
| `--compression` | Most write commands | `none`, `zlib` (default), `zstd`, with optional level (`zlib:9`) |
| `--generator` | Most write commands | Set writing program name in output header |
| `--output-header` | Most write commands | Set replication header fields |
| `--json` | inspect, check | Machine-readable output |
| `--index-type` | add-locations-to-ways | `sparse` (default), `external`, `auto` (different values from osmium's `-i`) |

## Commands pbfhogg doesn't have

| osmium command | Status |
|----------------|--------|
| `changeset-filter` | Not planned. Changeset processing is a niche use case. |
| `create-locations-index` / `query-locations-index` | Not needed. pbfhogg builds indexes in-memory via anonymous mmap. |
| `show` | Implemented via `inspect --show <TYPE_ID>` |

## apply-changes: permissive missing-element semantics (parity)

Both tools silently tolerate every missing-element edge case in
`apply-changes`. This is positive parity, not a deviation - users
migrating from osmium can rely on the same behaviour. No flag is
required in either tool.

| OSC op | Element state in base | Outcome (both tools) |
|---|---|---|
| `<create>` on existing ID | present | Silent overwrite (treated as modify). Base record is replaced with the OSC record. |
| `<modify>` on absent ID | absent | Silent insert (treated as create). OSC record is written. |
| `<delete>` on absent ID | absent | Silent no-op. |
| way/relation ref to absent node | absent from base AND from OSC | pbfhogg under `--locations-on-ways`: `(0, 0)` sentinel coord; missing-node count is reported in the summary as `loc_missing` (derived post-hoc from `needed - resolved`, not per-site incremented). pbfhogg without `--locations-on-ways`: the ref is written bare, no coordinate lookup. osmium with `--locations-on-ways`: identical `(0, 0)` fallback via `location_index.get_noexcept` + `if (location)` guard; no warning. |

**Rationale (applies to both projects).** The motivating workload is
incremental-extract - region-extracted base PBF plus a full-planet
daily OSC, then re-extract by bbox. Such pipelines routinely
reference OSC elements (nodes outside the region, ways whose refs
extend outside the region) that are not in the base. Failing by
default would force every such user to pass an opt-out flag that is
the right behaviour in virtually all cases.

The `(0, 0)` coordinate sentinel under `--locations-on-ways` is
consistent with the Null Island convention used elsewhere in pbfhogg
(see [CORRECTNESS.md](../CORRECTNESS.md) "Null Island ambiguity in
dense mmap index"); ways referencing nodes exactly at Null Island
are indistinguishable from ways referencing absent nodes. This
affects zero real-world nodes (nearest land ~570 km).

**pbfhogg implementation anchors:**

- Upsert semantics: `src/commands/apply_changes/rewrite_block.rs`
  (the walker treats `diff.get_node/way/relation(id)` hits as
  replacements regardless of whether the ID was in base).
- Delete no-ops: arise naturally - the `deleted_nodes` /
  `deleted_ways` / `deleted_relations` sets are only consulted while
  walking base elements, so a delete of an absent ID has nothing to
  skip.
- `(0, 0)` fallback under `--locations-on-ways`:
  `src/commands/apply_changes/element_writes.rs` (search
  `locations.push((0, 0))`).
- `loc_missing` counter: defined in
  `src/commands/apply_changes/stats.rs`, computed in
  `src/commands/apply_changes/rewrite.rs` via
  `loc_stats_pre.2.saturating_sub(extracted)`, and reported in the
  summary line by `MergeStats::print_summary`.

**osmium implementation anchors:**

- `osmium apply-changes` runs `copy_first_with_id` plus
  `std::set_union` over reverse-version-sorted inputs to dedup by
  ID and merge. The write is gated on `obj.visible()`.
- `<delete>` entries arrive from the XML input format with
  `visible=false`, so `copy_first_with_id` skips them on absent IDs
  with no error path.
- `update_nodes_if_way` looks up coordinates via
  `location_index.get_noexcept(...)` followed by an
  `if (location) set_location(...)` guard, so missing nodes fall
  through silently.

No `throw`, `std::cerr` warning, or validation is raised on any of
the four scenarios in the osmium apply-changes code path.

**Test coverage (pbfhogg):** `tests/apply_changes_invariants.rs`
pins the three non-ALTW scenarios as regression anchors:
`modify_on_missing_id_silently_inserts`,
`delete_on_missing_id_is_noop`,
`create_on_existing_id_overwrites_base`. The ALTW `(0, 0)` fallback
is exercised end-to-end by the Denmark byte-equal cross-validation
against osmium (`brokkr verify apply-changes`).

## Indexdata

pbfhogg can embed blob-level indexdata into PBF files, enabling fast
passthrough for commands like `apply-changes`, `sort`, and `extract`. This
has no osmium equivalent.

To create an indexed PBF:

```
pbfhogg cat input.osm.pbf -o indexed.osm.pbf
```

Commands that benefit from indexdata will error if it's missing. Use `--force`
to run anyway (slower). Check if a file is indexed:

```
pbfhogg inspect --indexed input.pbf    # exit code 0 = indexed, 1 = not
```
