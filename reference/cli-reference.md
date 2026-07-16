# pbfhogg CLI Reference

Version 0.5.0. Generated from `pbfhogg --help` output.

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
| `-j, --jobs <N>` | Parallel worker count for `--nodes` (only). `0` auto-picks from `available_parallelism()`, `1` forces sequential, higher values cap the pool. Other inspect modes ignore this flag. |
| `--blocks [N]` | Show per-block distribution stats and optional block listing (N limits to first/last N blocks) |
| `--id-ranges` | Show min/max element IDs per type and monotonicity |
| `--locations` | Show locations-on-ways diagnostics |
| `--anomalies` | Show only anomalous blocks (<50% or >150% of median, plus mixed blocks) |
| `-e, --extended` | Extended scan: timestamp range, data bbox, metadata coverage, ordering |
| `-g, --get <KEY>` | Get a single value by key path (e.g. `header.bbox`, `data.timestamp.first`) |
| `--json` | Machine-readable JSON output |
| `--show <TYPE_ID>` | Display a single element by ID (e.g. `n123`, `w456`, `r789`). Uses indexdata to skip non-matching blobs, early exit on sorted PBFs |
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
| `-j, --jobs <N>` | Parallel worker count. `0` auto-picks from `available_parallelism()`, `1` forces sequential, higher values cap the pool. |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata (slower fallback path) |

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
| `--json` | Machine-readable JSON output |
| `--quiet` | Exit-code only, no output |
| `--direct-io` | Use O_DIRECT to bypass page cache |

For missing relation-to-relation members, reports unique missing IDs with occurrence count when they differ: `Missing relation members: 706 (777 references)`.

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

### repack

Re-encode a PBF with a configurable per-blob element cap. Element semantics, tags, refs, members, metadata, and DenseNodes encoding all round-trip; output is type-sorted and propagates `Sort.Type_then_ID` from the input header.

Primary use case: producing same-corpus-different-encoding pairs for blob-density measurement (Geofabrik's ~8 k/blob convention vs `planet.openstreetmap.org`'s ~228 k/blob), so commands with implicit blob-count scaling (`HeaderWalker`-based paths in particular) can be measured at controlled densities.

```
pbfhogg repack [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--elements-per-blob <N>` | Per-blob element cap [default: 8000]. `8000` matches the osmium / Geofabrik convention; pass a larger value to approximate `planet.openstreetmap.org`-style packing. Must be > 0. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

Growing (e.g. europe 8 k -> 64 k) coalesces elements across input-blob boundaries, so caps larger than the input blob size fire correctly. On a coalescing shrink the output blob count is not the general `ceil(elements / cap)`: each input blob whose element count is not a multiple of the cap emits its tail as its own possibly-under-cap block (the deliberate trade that keeps output ID-monotonic across coalesced boundaries). When the input header declares `LocationsOnWays`, the output re-advertises it and every inline way-node coordinate round-trips exactly; the two `pbfhogg.*` prepass features (`WayMembers-v1`, `SharedNodePins-v1`) are dropped with a warning.

### degrade

Produce a valid-but-adversarial PBF by stripping properties or perturbing structure. Each flag composes; at least one is required. Used to produce inputs for benchmarking non-optimal code paths (`sort` overlap-rewrite, `add-locations-to-ways`, `--force` fallbacks).

```
pbfhogg degrade [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--unsort` | Clear `Sort.Type_then_ID`; perturb the element stream so at least one adjacent same-kind blob pair has overlapping IDs (one per kind that has more than `block-cap + 1` elements). Triggers `sort`'s overlap-rewrite path. |
| `--unsort-intra` | Clear `Sort.Type_then_ID`; leave one same-kind blob per kind with an internal ID-order inversion but non-overlapping blob ranges - the intra-blob shape `sort`'s overlap detector cannot see. Mutually exclusive with `--unsort`. |
| `--strip-locations` | Drop the `LocationsOnWays` header feature. Inline way-node coordinates are not preserved; downstream `add-locations-to-ways` runs see a redundancy-free starting point. |
| `--strip-indexdata` | Clear `BlobHeader.indexdata` on every OsmData blob. Forces commands into their `--force` / non-indexed fallback paths (`sort`, `getid`, `tags-filter`). Blob payloads are not decompressed. |
| `--strip-tagdata` | Clear `BlobHeader.tagdata` (the per-blob tag key index) on every OsmData blob, forcing `tags-filter`'s no-hint fallback path. Leaves `indexdata` intact - a tagdata-stripped file is still indexed. |
| `--strip-bbox` | Clear `HeaderBlock.bbox` (field 1). Header-only change; no OsmData blob is touched. Exercises `inspect`'s bbox handling (`extract_header_metadata`, the `inspect --get header.bbox` fast path) and downstream/external-consumer tolerance of a file with no declared extent. Does not affect `extract --bbox`, which derives its region from the CLI argument and prunes via per-blob `indexdata` bboxes rather than reading the header. |
| `--drop-ids <N:SEED>` | Deterministically remove exactly N elements selected globally by kind, ID, and seed. Surviving references to removed elements intentionally dangle. |
| `--force` | Skip the indexdata precondition required by the decode path (falls back to scanning every blob for every kind; slower). |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

**Implementation paths:** `--strip-indexdata`, `--strip-tagdata`, and/or `--strip-bbox` (with no `--unsort`/`--strip-locations`/`--drop-ids`) run as a header-and-blob-level passthrough: with no `--generator`/`--output-header` override, the input `HeaderBlock` payload is forwarded field-for-field via a surgical wire-level field stripper, with only the bbox (field 1) removed under `--strip-bbox`, and raw OsmData frames are reframed by copying the original `BlobHeader` through byte-for-byte with only `indexdata` (field 2) and/or `tagdata` (field 4) cleared as targeted. This preserves every other header and blob-header field byte-for-byte - `source`, custom optional features, a non-default `writingprogram`, replication metadata, `WayMembers-v1`, and unknown/extension fields all survive - rather than losing them to a `HeaderBuilder` rebuild. When a header override (`--generator`/`--output-header`) is present, the header is instead rebuilt through `HeaderBuilder` with the override applied (and the bbox omitted under `--strip-bbox`). Any combination involving `--unsort`, `--strip-locations`, or `--drop-ids` decodes elements, re-encodes via `BlockBuilder`, and frames with `indexdata=None` when `--strip-indexdata` composes; `--strip-bbox` on this path clears the bbox from the rebuilt output header. `--drop-ids` requires `N:SEED`, rejects zero N, and is reproducible for a given input and seed.

`--recompress` from the design doc remains deferred. The `--unsort` perturbation is the minimum-viable swap that triggers `sort`'s overlap detector; configurable chaos modes (rotate / shuffle / reverse) are deferred.

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
| `-j, --jobs <N>` | Worker-pool size for the parallel classify phases. `0` (default) uses rayon's `available_parallelism()`. Two-pass mode only: the single-pass `-R` path uses the pipelined reader and ignores `-j` (CLI rejects the combination). |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### export

Stream a PBF to GeoJSON. Tagged nodes become Points. Tagged ways become
LineStrings, or Polygons when they are closed and satisfy the built-in area
rules. Untagged nodes and ways are skipped. Relation features are not emitted.
Way export requires the input header to declare `LocationsOnWays`; `--type
node` works without it.

The default `geojsonseq` format writes one Feature object per newline with no
RFC 8142 record-separator byte. `geojson` writes one FeatureCollection.

```
pbfhogg export [OPTIONS] <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Write to a guarded file instead of stdout |
| `--format <FORMAT>` | `geojsonseq` (default) or `geojson` |
| `--type <TYPE>` | Export only `node` or `way` |
| `-e, --expressions <FILE>` | Read tag expressions from a file, one per line |
| `--properties <KEYS>` | Comma-separated whitelist of tag property keys |
| `--bbox <BBOX>` | `min_lon,min_lat,max_lon,max_lat`; ways match by vertex containment only, so crossing or enclosing geometry without an inside vertex is omitted |
| `--metadata` | Add available `@version`, `@timestamp`, `@changeset`, `@uid`, `@user`, and `@visible` properties |

Every feature includes `@id` and `@type`. Metadata timestamps are RFC 3339 UTC
strings. Tags that collide with emitted reserved property names are omitted.
Polygon exterior rings are closed and counterclockwise. Ways with invalid
geometry are skipped and reported in the stderr summary.

### diff

Compare two PBF files and show differences. Uses content equality (coordinates, tags, refs, members) rather than version/timestamp ordering - deterministic regardless of metadata completeness (see [DEVIATIONS](../DEVIATIONS.md#diff-content-equality-vs-version-ordering)).

With `--format osc`, generates an OSC diff file instead of text output. Text-only flags (`-c`, `-v`, `-s`/`--osmium-summary`, `-q`, `-t`) are not valid with `--format osc`. OSC-only flags (`--increment-version`, `--update-timestamp`) are not valid with `--format text`.

```
pbfhogg diff [OPTIONS] <OLD> <NEW>
```

| Flag | Description |
|------|-------------|
| `--format <FORMAT>` | Output format: `text` (default) or `osc` |
| `-c, --suppress-common` | Hide unchanged elements (text only) |
| `-v, --verbose` | Show detailed changes for modified elements (text only) |
| `-s, --osmium-summary` | Print osmium-style summary (`Summary: left=N right=N same=N different=N`) on stderr instead of the default pbfhogg-format summary. Both formats fire on stderr unless `--quiet` (text only) |
| `-q, --quiet` | Exit-code only, suppress output (text only) |
| `-o, --output <FILE>` | Write output to file (required for `--format osc`) |
| `-t, --type <TYPE>` | Filter by element type (text only) |
| `-j, --jobs <N>` | Parallel shard count. `0` (default) auto-picks from `available_parallelism()`; `1` restores the sequential, scratch-free path; higher values partition the ID space across N worker threads. Applies to both text and `--format osc`; parallel sharding requires both inputs to be indexed, and `-v/--verbose` always uses the sequential path. The parallel path writes shard temp files next to the output (planet scale: ~30 GB text, ~45 GB osc XML), removed on completion. |
| `--increment-version` | Bump version of deleted elements by 1 (osc only) |
| `--update-timestamp` | Set delete timestamp to current time (osc only) |
| `--direct-io` | Use O_DIRECT to bypass page cache |

With `--format osc`, produces a lossless roundtrip - applying the derived OSC to the old PBF reproduces the new PBF exactly (see [DEVIATIONS](../DEVIATIONS.md#derive-changes-lossless-delete-roundtrip)).

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

Embed node coordinates in ways. Two index strategies:

- **sparse** (default) - Rank-indexed flat mmap array. Pre-allocates `referenced.total_count() * 8` bytes; workers store `(lat, lon)` at byte offset `IdSet::rank_if_set(node_id) << 3` via atomic stores. Fast at small / medium scale; survives Europe at ~6 minutes on a 27 GB-RAM host. Likely thrashes at planet (working set exceeds free page cache) - use `external` for planet.
- **external** - Double radix permutation via 4-stage pipeline. Bounded memory (~8.7 GB measured peak anon at planet). The only mode that survives at planet on memory-constrained hosts. Requires sorted PBF (Sort.Type\_then\_ID) and indexdata. Uses ~224 GB temp disk at planet.

`--index-type dense` was removed - sparse rank-indexed flat dominated dense at every measured scale (japan dense 51.6 s vs sparse 11.9 s). Passing `--index-type dense` errors with a pointer to `sparse`.

By default, untagged nodes not referenced by a relation are dropped from output.

```
pbfhogg add-locations-to-ways [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--index-type <TYPE>` | Node index type: `sparse` (default), `external`, or `auto` (scale-aware: sparse unless the input is sorted+indexed and the estimated node store exceeds ~80 % of available RAM) |
| `--keep-untagged-nodes` | Keep all untagged nodes in output |
| `--inject-prepass` | Emit the opt-in `pbfhogg.WayMembers-v1` and `pbfhogg.SharedNodePins-v1` metadata into the output header, readable back via `Blob::way_members` / `Way::shared_node_pins` |
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
| `-j, --jobs <N>` | Worker-pool size for the descriptor-first pipeline. `0` (default) uses the `nproc - 2` heuristic (leaves two cores for scanner + drain, min 1). Pass a specific N to measure scaling or constrain CPU use. |
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
| `-j, --jobs <N>` | Parallel worker threads. `1` forces sequential, `0` (default) auto-picks from `available_parallelism()`, higher values cap the pool. Affects both the per-input parse fan-out (capped by input count) and the simplify path's chunk fan-out. |

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
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed without indexdata / overwrite existing index |

Outputs 19 binary files. Denmark (465 MB PBF): ~7s, 172 MB index. Europe (32.4 GB): 524s (8.7 min), 7.5 GB RSS. Planet (87 GB): 1,255s (20.9 min), 29.5 GB peak RSS (pass-1.5 transient).

---

## I/O capability matrix

Which commands support `--direct-io` (O_DIRECT bypass of page cache) and `--io-uring`
(io_uring async writes), and which paths are affected.

| Command | Reads PBF | Writes PBF | `--direct-io` | `--io-uring` | Notes |
|---------|:---------:|:----------:|:-------------:|:------------:|-------|
| inspect | Yes | - | Yes | - | Read-only |
| inspect tags | Yes | - | Yes | - | Read-only |
| inspect --nodes | Yes | - | Yes | - | Read-only |
| check --ids | Yes | - | Yes | - | Read-only |
| check --refs | Yes | - | Yes | - | Read-only |
| cat | Yes | Yes | Yes (R+W) | - | Passthrough + filtered paths |
| cat --dedupe | Yes | Yes | Yes (R+W) | Yes | Via merge-pbf path |
| sort | Yes | Yes | Yes (R+W) | Yes | |
| repack | Yes | Yes | Yes (R+W) | Yes | Re-encode at configurable elements-per-blob cap |
| degrade | Yes | Yes | Yes (R+W) | Yes | Adversarial PBF generator (--unsort / --strip-locations / --strip-indexdata / --drop-ids) |
| renumber | Yes | Yes | Yes (R+W) | - | |
| extract | Yes | Yes | Yes (R+W) | - | All strategies |
| tags-filter | Yes | Yes | Yes (R+W) | - | Both single-pass and two-pass |
| export | Yes | No | No | - | GeoJSON output to stdout or a regular file |
| getid | Yes | Yes | Yes (R+W) | - | Including --add-referenced |
| getid --invert | Yes | Yes | Yes (R+W) | - | |
| getparents | Yes | Yes | Yes (R+W) | - | |
| time-filter | Yes | Yes | Yes (R+W) | - | |
| add-locations-to-ways | Yes | Yes | Yes (R+W) | - | All index types |
| apply-changes | Yes | Yes | Yes (R+W) | Yes | Production merge path |
| diff | Yes | - | Yes | - | Read-only (text output) |
| diff --format osc | Yes | - | Yes | - | Read-only (OSC XML output) |
| derive-changes (via `diff --format osc`) | Yes | - | Yes | - | Read-only (OSC XML output) |
| build-geocode-index | Yes | - | Yes | - | Binary index output, not PBF |
| merge-changes | - | - | - | - | OSC-only, no PBF I/O |

**R+W** = `--direct-io` affects both PBF reads (via `BlobReader`) and PBF writes (via `PbfWriter`).
**`--io-uring`** requires the `linux-io-uring` compile-time feature and sufficient `RLIMIT_MEMLOCK`.
