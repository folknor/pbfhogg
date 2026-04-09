# Migrating from osmium-tool to pbfhogg

This guide maps osmium-tool commands and flags to their pbfhogg equivalents.
If you're already familiar with osmium, you should feel at home — most commands
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
| `diff` | `diff` | Content equality instead of version ordering (see [DEVIATIONS](DEVIATIONS.md#diff-content-equality-vs-version-ordering)) |
| `apply-changes` | `apply-changes` | Same |
| `merge-changes` | `merge-changes` | Same |
| `fileinfo` | `inspect` | Different name, more capabilities |
| `check-refs` | `check --refs` | Consolidated into `check` |
| `merge` | `cat --dedupe` | Consolidated into `cat` |
| `derive-changes` | `diff --format osc` | Consolidated into `diff` |
| `removeid` | `getid --invert` | Consolidated into `getid` |
| `tags-count` | `inspect tags` | Consolidated into `inspect` |

## Consolidated commands

pbfhogg merges several osmium commands that do closely related things. The
underlying functionality is identical — only the CLI entrypoint changed.

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
— applying the derived OSC to the old PBF reproduces the new PBF exactly.
osmium's `derive-changes` can lose deletes when the deleted element is absent
from both input files (see [DEVIATIONS](DEVIATIONS.md#derive-changes-lossless-delete-roundtrip)).

### merge is now `cat --dedupe`

osmium's `merge` (sorted k-way merge with dedup) is `cat --dedupe` in pbfhogg.
Plain `cat` remains concatenation without dedup, just like osmium's `cat`.

```
# osmium
osmium merge -o merged.pbf a.pbf b.pbf c.pbf

# pbfhogg
pbfhogg cat --dedupe -o merged.pbf a.pbf b.pbf c.pbf
```

All inputs must be sorted. Unsorted input is a hard error — run `pbfhogg sort`
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

- `--indexed` — check if a PBF has blob-level indexdata (exit code 0/1)
- `--nodes` — coordinate statistics for compression analysis
- `--anomalies` — highlight unusual blocks

## Flag differences

Most flags are identical between osmium and pbfhogg. Here are the differences
worth knowing about.

### Different flag names

| osmium | pbfhogg | Commands |
|--------|---------|----------|
| `-t, --object-type` | `-t, --type` | cat, diff, inspect tags, check |
| `--strategy simple\|complete_ways\|smart` | `--simple` / `--smart` (default: complete) | extract |

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
| `--index-type` | `dense` (default), `sparse`, `external` (bounded memory, 3.9x faster than dense at planet), `auto` (external if sorted+indexed). Different valid values from osmium's `-i, --index-type`. |

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
| `--index-type` | add-locations-to-ways | `dense`, `sparse`, `external`, `auto` (different values from osmium's `-i`) |

## Commands pbfhogg doesn't have

| osmium command | Status |
|----------------|--------|
| `export` | Planned (GeoJSON export design exists, not yet implemented) |
| `changeset-filter` | Not planned. Changeset processing is a niche use case. |
| `create-locations-index` / `query-locations-index` | Not needed. pbfhogg builds indexes in-memory via anonymous mmap. |
| `show` | Implemented via `inspect --show <TYPE_ID>` |

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
