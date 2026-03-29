# pbfhogg CLI Reference

Version 0.2.0. Generated from `pbfhogg --help` output.

## Global flags

All commands support `-h, --help` and `-V, --version`.

## Common flags

These flags appear on most commands that produce PBF output:

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file path |
| `--compression <COMPRESSION>` | Blob compression: `none`, `zlib` (default), `zstd`, or with level (`zlib:9`, `zstd:19`) |
| `--direct-io` | Use O_DIRECT to bypass page cache (requires `linux-direct-io` feature) |
| `--force` | Proceed even if input lacks indexdata (slower fallback path) |
| `--generator <GENERATOR>` | Override the writing program name in the output header |
| `--output-header <KEY=VALUE>` | Set output header fields (repeatable). Keys: `osmosis_replication_timestamp`, `osmosis_replication_sequence_number`, `osmosis_replication_base_url` |

---

## Commands

### inspect

Inspect PBF file: metadata, block breakdown, ordering analysis.

On indexed PBFs, uses an index-only fast path that reads blob headers without decompression (~36ms on 473 MB vs ~4s for full decode).

```
pbfhogg inspect [OPTIONS] <FILE>
pbfhogg inspect tags [OPTIONS] <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `--indexed` | Check if PBF has blob-level indexdata (exit code 0 = yes, 1 = no) |
| `--nodes` | Analyze node coordinate statistics for FOR compression sizing |
| `--blocks [N]` | Show per-block distribution stats and optional block listing (N limits to first/last N blocks) |
| `--id-ranges` | Show min/max element IDs per type and monotonicity |
| `--locations` | Show locations-on-ways diagnostics |
| `--anomalies` | Show only anomalous blocks (<50% or >150% of median, plus mixed blocks) |
| `-e, --extended` | Extended scan: timestamp range, data bbox, metadata coverage, ordering |
| `-g, --get <KEY>` | Get a single value by key path (e.g. `header.bbox`, `data.timestamp.first`) |
| `--json` | Machine-readable JSON output |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata (for `--nodes`) |

#### inspect tags

Count tag key=value frequencies (subcommand of `inspect`).

| Flag | Description |
|------|-------------|
| `--min-count <N>` | Only show tags with at least this many occurrences [default: 1] |
| `-M, --max-count <N>` | Only show tags with at most this many occurrences |
| `-s, --sort <ORDER>` | Sort order: count-desc (default), count-asc, name-asc, name-desc |
| `-e, --expressions <FILE>` | Read tag expressions from file (one per line, # comments) |
| `-t, --type <TYPE>` | Filter by element type: node, way, or relation |

### check

Validate PBF file integrity (IDs + referential integrity).

With no flags, runs both ID and referential integrity checks. Use `--ids` or `--refs` to run only one.

```
pbfhogg check [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `--ids` | Check ID uniqueness and ordering only |
| `--refs` | Check referential integrity only |
| `--full` | Full duplicate detection via bitmap (slower, more memory; applies to ID check) |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation; applies to ID check) |
| `--max-errors <N>` | Stop after N violations (0 = unlimited) [default: 100] |
| `--check-relations` | Also check relation member references (applies to ref check) |
| `--show-ids` | Show IDs of missing objects, format: `n123 in w456` (applies to ref check) |

For missing relation-to-relation members, reports unique missing IDs with occurrence count when they differ: `Missing relation members: 706 (777 references)`.
| `--json` | Machine-readable JSON output |
| `-q, --quiet` | Exit-code only, no output |
| `--direct-io` | Use O_DIRECT to bypass page cache |

---

### cat

Concatenate PBF files with optional type filtering. Embeds blob-level indexdata and tagdata automatically.

With `--dedupe`, merges multiple sorted PBF files with blob-level passthrough and exact-duplicate deduplication.

```
pbfhogg cat [OPTIONS] --output <OUTPUT> <FILES>...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation) |
| `-c, --clean <ATTR>` | Strip metadata attribute (repeatable: version, timestamp, changeset, uid, user) |
| `--dedupe` | K-way sorted merge with dedup (all inputs must be sorted) |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O (only with `--dedupe`) |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### sort

Sort PBF into standard order (nodes, ways, relations, each by ascending ID). For already-sorted inputs with indexdata, blobs pass through as raw bytes.

```
pbfhogg sort [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### renumber

Renumber all element IDs sequentially, remapping cross-references (way node refs, relation member refs).

```
pbfhogg renumber [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-s, --start-id <ID>` | Starting ID(s): single value or comma-separated node,way,relation [default: 1] |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### extract

Extract elements within a geographic region (bounding box or polygon).

Three strategies: `--simple` (single pass, fast, may have dangling refs), complete-ways (default, two passes, all way nodes included), `--smart` (three passes, completes multipolygon/boundary relations).

Supports multi-extract via `--config` with a JSON config file specifying multiple regions.

```
pbfhogg extract [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file (required for single extract, omit with --config) |
| `-b, --bbox <BBOX>` | Bounding box: minlon,minlat,maxlon,maxlat |
| `-p, --polygon <FILE>` | Polygon GeoJSON file |
| `-c, --config <FILE>` | Multi-extract JSON config file |
| `-d, --directory <DIR>` | Output directory override (only with --config) |
| `-s, --simple` | Simple strategy (single pass) |
| `--smart` | Smart strategy (three passes, complete relations) |
| `--set-bounds` | Write the extract region bounding box to the output header |
| `--clean <ATTR>` | Strip metadata attribute (repeatable: version, timestamp, changeset, uid, user) |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### tags-filter

Filter elements by tag expressions. Default mode resolves relation members transitively (matched relations pull in member ways, nodes, and nested relations). With `-R`, only directly matched elements are emitted.

With `--input-kind osc` (or autodetected from `.osc`/`.osc.gz` extension), filters an OSC change file instead, always preserving deletes. PBF-only flags (`-R`, `-i`, `-t`) are not valid in OSC mode.

Expressions use osmium syntax: `highway=primary`, `amenity`, `w/building=yes`, etc.

```
pbfhogg tags-filter [OPTIONS] --output <OUTPUT> <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--input-kind <KIND>` | Input kind override: `pbf` or `osc` (autodetect from extension by default) |
| `-R, --omit-referenced` | Omit referenced objects (faster, single pass, direct matches only; PBF only) |
| `-i, --invert-match` | Invert match: exclude matching objects, keep non-matching (PBF only) |
| `-t, --remove-tags` | Remove tags from referenced objects not directly matched (use without -R; PBF only) |
| `-e, --expressions <FILE>` | Read filter expressions from file (one per line, # comments) |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### diff

Compare two PBF files and show differences. Uses content equality (coordinates, tags, refs, members) rather than version/timestamp ordering — deterministic regardless of metadata completeness (see [DEVIATIONS](DEVIATIONS.md#diff-content-equality-vs-version-ordering)).

With `--format osc`, generates an OSC diff file instead of text output. Text-only flags (`-c`, `-v`, `-s`, `-q`, `-t`) are not valid with `--format osc`. OSC-only flags (`--increment-version`, `--update-timestamp`) are not valid with `--format text`.

```
pbfhogg diff [OPTIONS] <OLD> <NEW>
```

| Flag | Description |
|------|-------------|
| `--format <FORMAT>` | Output format: `text` (default) or `osc` |
| `-c, --suppress-common` | Hide unchanged elements (text only) |
| `-v, --verbose` | Show detailed changes for modified elements (text only) |
| `-s, --summary` | Show summary on stderr (text only) |
| `-q, --quiet` | Exit-code only, suppress output (text only) |
| `-o, --output <FILE>` | Write output to file (required for `--format osc`) |
| `-t, --type <TYPE>` | Filter by element type (text only) |
| `--increment-version` | Bump version of deleted elements by 1 (osc only) |
| `--update-timestamp` | Set delete timestamp to current time (osc only) |

With `--format osc`, produces a lossless roundtrip — applying the derived OSC to the old PBF reproduces the new PBF exactly (see [DEVIATIONS](DEVIATIONS.md#derive-changes-lossless-delete-roundtrip)).
| `--ignore-changeset` | Compatibility flag (already ignored by content-equality mode) |
| `--ignore-uid` | Compatibility flag (already ignored by content-equality mode) |
| `--ignore-user` | Compatibility flag (already ignored by content-equality mode) |
| `--direct-io` | Use O_DIRECT to bypass page cache |

### getid

Extract or remove elements by ID. By default, keeps only the listed IDs. With `--invert`, removes the listed IDs and keeps everything else.

IDs use type prefixes: `n123` (node), `w456` (way), `r789` (relation).

```
pbfhogg getid [OPTIONS] --output <OUTPUT> <FILE> [IDS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--invert` | Invert selection: remove listed IDs instead of keeping them |
| `-r, --add-referenced` | Include referenced nodes of matching ways (two-pass; not with `--invert`) |
| `-t, --remove-tags` | Remove tags from referenced objects (use with -r; not with `--invert`) |
| `--verbose-ids` | Print requested IDs and report which were not found (not with `--invert`) |
| `-i, --id-file <FILE>` | Read IDs from text file (one per line) |
| `-I, --id-osm-file <FILE>` | Read IDs from an OSM/PBF file (all element IDs are collected) |
| `--default-type <TYPE>` | Default type for bare numeric IDs: node, way, relation |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### getparents

Find ways/relations referencing given IDs (reverse lookup).

```
pbfhogg getparents [OPTIONS] --output <OUTPUT> <FILE> [IDS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-s, --add-self` | Also include the queried objects themselves in the output |
| `-i, --id-file <FILE>` | Read IDs from text file (one per line) |
| `-I, --id-osm-file <FILE>` | Read IDs from an OSM/PBF file (all element IDs are collected) |
| `--default-type <TYPE>` | Default type for bare numeric IDs: node, way, relation |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### add-locations-to-ways

Embed node coordinates in ways. Three index strategies:

- **dense** (default) — Direct-mapped mmap array. Fastest when the working set fits in RAM. At planet scale (~16 GB touched), requires ~30+ GB free memory to avoid page cache thrashing.
- **sparse** — Planetiler-inspired chunk-indexed sparse array. Bounded memory (~540 MB). Slower than dense at all scales. No temp disk needed. Works on any PBF.
- **external** — Double radix permutation via 4-stage pipeline. Bounded memory (~1.6 GB). 2.8x faster than dense at Europe scale. Requires sorted PBF (Sort.Type\_then\_ID) and indexdata. Uses ~112 GB temp disk at Europe, ~224 GB at planet.

By default, untagged nodes not referenced by a relation are dropped from output.

```
pbfhogg add-locations-to-ways [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--index-type <TYPE>` | Node index type: `dense` (default), `sparse`, `external`, or `auto` (external if sorted+indexed, dense otherwise) |
| `--keep-untagged-nodes` | Keep all untagged nodes in output |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### time-filter

Filter a history PBF to a snapshot at a given timestamp.

```
pbfhogg time-filter [OPTIONS] --output <OUTPUT> <FILE> <TIMESTAMP>
```

The timestamp can be UNIX seconds or RFC3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`).

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

---

### apply-changes

Apply an OSC diff to a sorted PBF file. Uses blob passthrough -- unmodified blobs are copied as raw bytes without decompression.

```
pbfhogg apply-changes [OPTIONS] --output <OUTPUT> <BASE> <CHANGES>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--locations-on-ways` | Preserve and update way-node locations through the merge (requires base PBF with LocationsOnWays) |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### merge-changes

Merge multiple OSC files into one OSC file.

```
pbfhogg merge-changes [OPTIONS] --output <OUTPUT> <CHANGES>...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--simplify` | Keep only the last change per object (type + id) |

### build-geocode-index

Build a reverse geocoding index from a PBF file. Produces a set of binary files (S2 cell index, address points, street segments, admin boundaries, string pool) that can be memory-mapped for sub-millisecond reverse geocoding queries.

Requires an indexed PBF (generated by `pbfhogg cat`). The output directory must not already exist unless `--force` is set.

```
pbfhogg build-geocode-index [OPTIONS] --output-dir <DIR> <FILE>
```

| Flag | Description |
|------|-------------|
| `--output-dir <DIR>` | Output directory for index files |
| `--street-level <N>` | S2 cell level for streets/addresses [default: 17] |
| `--coarse-level <N>` | Fallback cell level for rural areas [default: 14] |
| `--admin-level <N>` | S2 cell level for admin boundaries [default: 10] |
| `--max-admin-vertices <N>` | Douglas-Peucker vertex cap per admin polygon [default: 500] |
| `--search-radius <M>` | Fine-level max search distance in meters [default: 75] |
| `--coarse-search-radius <M>` | Coarse-level max search distance in meters [default: 1000] |
| `--force` | Proceed without indexdata / overwrite existing index |

Outputs 19 binary files. Denmark (465 MB PBF): ~7s, 172 MB index. Europe (32.4 GB): 568s (9.5 min), 7.5 GB RSS. Planet (87 GB): 1,346s (22.4 min), 17.8 GB RSS.

---

## I/O capability matrix

Which commands support `--direct-io` (O_DIRECT bypass of page cache) and `--io-uring`
(io_uring async writes), and which paths are affected.

| Command | Reads PBF | Writes PBF | `--direct-io` | `--io-uring` | Notes |
|---------|:---------:|:----------:|:-------------:|:------------:|-------|
| inspect | Yes | — | Yes | — | Read-only |
| inspect tags | Yes | — | Yes | — | Read-only |
| inspect --nodes | Yes | — | Yes | — | Read-only |
| check --ids | Yes | — | Yes | — | Read-only |
| check --refs | Yes | — | Yes | — | Read-only |
| cat | Yes | Yes | Yes (R+W) | — | Passthrough + filtered paths |
| cat --dedupe | Yes | Yes | Yes (R+W) | Yes | Via merge-pbf path |
| sort | Yes | Yes | Yes (R+W) | Yes | |
| renumber | Yes | Yes | Yes (R+W) | — | |
| extract | Yes | Yes | Yes (R+W) | — | All strategies |
| tags-filter | Yes | Yes | Yes (R+W) | — | Both single-pass and two-pass |
| getid | Yes | Yes | Yes (R+W) | — | Including --add-referenced |
| getid --invert | Yes | Yes | Yes (R+W) | — | |
| getparents | Yes | Yes | Yes (R+W) | — | |
| time-filter | Yes | Yes | Yes (R+W) | — | |
| add-locations-to-ways | Yes | Yes | Yes (R+W) | — | All index types |
| apply-changes | Yes | Yes | Yes (R+W) | Yes | Production merge path |
| diff | Yes | — | Yes | — | Read-only (text output) |
| diff --format osc | Yes | — | Yes | — | Read-only (OSC XML output) |
| derive-changes (via `diff --format osc`) | Yes | — | Yes | — | Read-only (OSC XML output) |
| build-geocode-index | Yes | — | Yes | — | Binary index output, not PBF |
| merge-changes | — | — | — | — | OSC-only, no PBF I/O |

**R+W** = `--direct-io` affects both PBF reads (via `BlobReader`) and PBF writes (via `PbfWriter`).
**`--io-uring`** requires the `linux-io-uring` compile-time feature and sufficient `RLIMIT_MEMLOCK`.
