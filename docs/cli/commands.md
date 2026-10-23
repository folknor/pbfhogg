# pbfhogg CLI Reference

Version 0.5.0. Generated from `pbfhogg --help` output.

## Global flags

All commands support `-h, --help` and `-V, --version`.

## Exit codes

The full exit status contract, including stderr behavior, lives in the
[CLI reference](https://github.com/folknor/pbfhogg/blob/main/reference/cli-reference.md#exit-codes).

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
| `--indexed` | Check if PBF has blob-level indexdata (exit code 0 = yes, 1 = no). Reads the first data blob only; later blobs are not checked |
| `--nodes` | Analyze node coordinate statistics for FOR compression sizing |
| `-j, --jobs <N>` | Parallel worker threads for `--nodes`. `0` (default) auto-picks from `available_parallelism()`, `1` forces sequential, higher values cap the pool. **Requires `--nodes`**: no other inspect mode reads it, so passing it elsewhere is an error rather than a no-op |
| `--blocks [N]` | Show per-block distribution stats and optional block listing (N limits to first/last N blocks) |
| `--id-ranges` | Show min/max element IDs per type and monotonicity |
| `--locations` | Show locations-on-ways diagnostics |
| `--anomalies` | Show blocks outside inspect's per-type median thresholds, plus mixed blocks |
| `-e, --extended` | Extended scan: timestamp range, data bbox, metadata coverage, ordering |
| `-g, --get <KEY>` | Get a single value by key path (e.g. `header.bbox`, `data.timestamp.first`). Data and metadata paths enable extended stats automatically |
| `--json` | Machine-readable JSON output |
| `--show <TYPE_ID>` | Display a single element by ID (e.g. `n123` or `node/123`). Also accepts `w456` / `way/456` and `r789` / `relation/789`, including signed IDs. Uses indexdata to skip non-matching blobs, early exit on sorted PBFs. **A mode of its own**: mutually exclusive with every other `inspect` flag, which it used to accept and discard |
| `--distrust-metadata` | Decode every blob instead of reporting counts, ID ranges and per-block stats out of the blobs' own indexdata. Nothing validates indexdata against the payload it describes, so on a file you are inspecting because you suspect it, the fast path reports the file's claims rather than its contents. Scanning report only: rejected with `--indexed`, `--nodes`, and a `file.*` or `header.*` `--get` key |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata. **Requires `--nodes`**, the only mode that reads it |

#### inspect tags

Count tag key=value frequencies (subcommand of `inspect`).

| Flag | Description |
|------|-------------|
| `--min-count <N>` | Only show tags with at least this many occurrences [default: 1] |
| `-M, --max-count <N>` | Only show tags with at most this many occurrences |
| `-s, --sort <ORDER>` | Sort order: count-desc (default), count-asc, name-asc, name-desc |
| `-e, --expressions <FILE>` | Read tag expressions from file (one per line, # comments) |
| `-t, --type <TYPE>` | Filter by element type: node, way, or relation |
| `-j, --jobs <N>` | Parallel worker threads. `0` (default) auto-picks from `available_parallelism()`, `1` forces sequential, higher values cap the pool |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata (slower fallback path) |

`inspect tags` expressions use the same syntax as `tags-filter`, including
element-type prefixes. For example, `w/highway=primary` counts that match only
on ways; `--type` adds an overall element-type restriction.

Expressions select individual tags for counting. If a way has both
`highway=primary` and `name=Main Street`, the expression `w/highway` counts
only its highway tag. In `tags-filter` the same expression selects the whole
way; export also selects whole elements and accepts an omitted expression
filter as unrestricted.

Counts are exact. Memory grows with the number of distinct selected key/value
pairs and their string bytes, with no fixed byte cap or spill to disk. One
global map stores one key string per distinct key and one value string and
counter per distinct pair, plus hash-table capacity overhead. The blob
schedule, decoded blobs and in-flight per-blob count maps add transient
storage; input blobs are not limited to the writer's default element count.
The sorted result reserves one entry per distinct pair before applying
`--min-count` or `--max-count`, and retained entries each own a copy of the key.
During conversion that allocation overlaps the remaining map storage.
Consequently count thresholds do not bound memory, even when few rows are
printed. Restrict element types and tag expressions to reduce the selected
cardinality; neither a higher minimum count nor fewer workers bounds the
global map. No approximate counts or silent truncation are used.

### check

Validate PBF file integrity (IDs + referential integrity).

With no flags, runs the ID and referential integrity checks. Use `--ids`,
`--refs` or `--index` to select. `--index` is not part of the default pair: it
decompresses every blob in the file, so it is opt-in.

```
pbfhogg check [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `--ids` | Check ID uniqueness and ordering only |
| `--refs` | Check referential integrity only |
| `--index` | Verify blob-level metadata (`BlobHeader` indexdata and tagdata) against the blobs themselves. Decompresses every blob |
| `--full` | Full duplicate detection via bitmap (slower, more memory; applies to ID check) |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation; applies to ID check) |
| `--max-errors <N>` | Retain at most N violation details per check (0 = unlimited); scanning and total counts continue [default: 100] |
| `--check-relations` | Also check relation member references (applies to ref check) |
| `--show-ids` | Show IDs of missing objects, format: `n123 in w456` (applies to ref check) |
| `--json` | Machine-readable JSON output |
| `--quiet` | Exit-code only, no output. Not combinable with `--json` |
| `--direct-io` | Use O_DIRECT to bypass page cache |

For missing relation-to-relation members, reports unique missing IDs with occurrence count when they differ: `Missing relation members: 706 (777 references)`.

The ID check judges monotonicity by canonical OSM order (`0`, then negative IDs
by ascending absolute value, then positives) - the order `sort` produces. On a
positive-only file that is plain `i64` order.

#### `--index`: verifying blob-level metadata

`BlobHeader.indexdata` (element kind, ID range, element count, spatial bbox) and
`BlobHeader.tagdata` (the blob's tag key set) are hints a reader believes without
opening the blob: `cat --type` passes a blob through raw on the strength of the
kind, `tags-filter` skips whole blobs on the strength of the tag index, and the
spatial filter skips node blobs on the strength of the bbox. Nothing in the
format signs those claims. `check --index` decompresses every blob, re-derives
the metadata from the payload, and reports each disagreement with its blob
ordinal and byte offset.

It never routes on the metadata it is checking - no blob filter, no
`require_indexdata`, every OSMData blob decompressed - so there is no
`--distrust-metadata` flag to pass: distrust is the only mode it has.

A blob that carries no indexdata or no tagdata makes no claim and is not
reported; that is the ordinary state of a third-party PBF. Metadata that is
present but *unreadable* is a different matter and is reported: an indexdata of
an unrecognised length or version, a malformed tag index, or a tag index
attached to a blob whose element kinds are mixed (where no key set can be
complete). Ordinary reads treat all of those as "no metadata, decode the blob",
which is safe for a reader and useless in a verifier.

Two of the checks are containment rather than equality:

- A stored **bbox** must contain every coordinate in the blob. A wider box is
  sound (and is what a non-default `granularity` legitimately produces), an
  absent box is no claim at all, and only a box that is present and too small
  can make a spatial filter skip a blob it should have read.
- A stored **tag key set** that omits a key the blob carries makes `tags-filter`
  skip a blob holding matches; one that lists a key the blob does not carry only
  costs a needless decode. Both are reported, as distinct violations.

Kind, count and ID range are checked for exact equality, because every consumer
treats them as exact.

Exit 1 means mismatches were found; exit 2 means the check could not run.

The ref check reports element IDs it could not track (negative IDs; a positive
ID above the range sized from the file's own indexdata is still tracked) on their
own
`Untrackable element IDs` line, and as `untrackable_node_ids` /
`untrackable_way_ids` / `untrackable_relation_ids` under `--json`. References to
such an element are counted as missing, so a non-zero count means the
missing-reference numbers are an upper bound.

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
| `--dedupe` | K-way sorted merge with dedup (all inputs must be sorted). Rejects `--type` and `--clean`: the dedupe path is blob-level passthrough and implements neither |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O (only with `--dedupe`) |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

With several inputs the output header is reconciled across all of them rather
than copied from the first: the bbox is the union, and is omitted unless every
input declares one; each replication field is kept only when every input agrees
on it; `HistoricalInformation` is the OR. osmium writes a blank header for
multiple inputs, so keeping the agreed fields is a deliberate deviation.

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

> **0.4.0**

Re-encode a PBF with a configurable per-blob element cap. Element semantics, tags, refs, members, metadata, and DenseNodes encoding all round-trip; output is type-sorted and propagates `Sort.Type_then_ID` from the input header.

Primary use case: producing same-corpus-different-encoding pairs for blob-density measurement (Geofabrik's ~8 k/blob convention vs `planet.openstreetmap.org`'s ~228 k/blob), so commands with implicit blob-count scaling can be measured at controlled densities.

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

> **0.4.0**

Produce a valid-but-adversarial PBF by stripping properties or perturbing structure. Each flag composes; at least one is required. Used to produce inputs for benchmarking non-optimal code paths (`sort` overlap-rewrite, `add-locations-to-ways`, `--force` fallbacks).

```
pbfhogg degrade [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--unsort` | Clear `Sort.Type_then_ID`; perturb the element stream so at least one adjacent same-kind blob pair has overlapping IDs. Triggers `sort`'s overlap-rewrite path. |
| `--unsort-intra` | Clear `Sort.Type_then_ID`; leave one same-kind blob per kind with an internal ID-order inversion but non-overlapping blob ranges, the intra-blob shape `sort`'s overlap detector cannot see. Mutually exclusive with `--unsort`. |
| `--strip-locations` | Drop the `LocationsOnWays` header feature. Inline way-node coordinates are not preserved. |
| `--strip-indexdata` | Clear `BlobHeader.indexdata` on every OsmData blob. Forces commands into their `--force` / non-indexed fallback paths (`sort`, `getid`, `tags-filter`). Blob payloads are not decompressed. |
| `--strip-tagdata` | Clear `BlobHeader.tagdata` (the per-blob tag key index) on every OsmData blob, forcing `tags-filter`'s no-hint fallback path. Leaves `indexdata` intact. |
| `--corrupt-tagdata` | Replace `BlobHeader.tagdata` on every OsmData blob with a well-formed tag key index naming a key the blob does not carry. Parses cleanly and lies, so `tags-filter` skips blobs whose elements match - the adversarial case `--strip-tagdata` cannot produce, since a missing index is decoded rather than trusted. Mutually exclusive with `--strip-tagdata` and with any blob-re-encoding flag. |
| `--corrupt-indexdata` | Replace `BlobHeader.indexdata` on every OsmData blob with a well-formed index that overstates the blob (same kind, inflated count and max ID). Produces the file `inspect --distrust-metadata` exists for. Mutually exclusive with `--strip-indexdata` and with any blob-re-encoding flag. |
| `--corrupt-indexdata-kind` | Rotate the element kind declared by `BlobHeader.indexdata` on every OsmData blob (node to way to relation to node), leaving count and ID range honest. The kind is what per-kind blob schedules select on, so this makes a consumer skip a blob outright rather than misreport it. Mutually exclusive with `--strip-indexdata`; combinable with `--corrupt-indexdata`. |
| `--strip-bbox` | Clear `HeaderBlock.bbox` so the output declares no file-level extent. Header-only change; no OsmData blob is touched. Does not affect `extract --bbox`, which derives its region from the CLI argument and per-blob indexdata bboxes rather than the header. |
| `--drop-ids <N:SEED>` | Deterministically remove exactly N elements selected globally by kind, ID, and seed. Surviving references to removed elements intentionally dangle. |
| `--force` | Skip the indexdata precondition required by the decode path (falls back to scanning every blob for every kind; slower). |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

`--strip-indexdata`, `--strip-tagdata`, and/or `--strip-bbox` (with no `--unsort`/`--strip-locations`/`--drop-ids`) run as a header-and-blob-level passthrough: only the targeted field is cleared and every other header and blob-header field survives byte-for-byte, including `source`, custom optional features, replication metadata, and unknown/extension fields. Any combination involving `--unsort`, `--strip-locations`, or `--drop-ids` decodes elements and re-encodes via `BlockBuilder`. `--drop-ids` requires `N:SEED`, rejects zero N, and is reproducible for a given input and seed.

`--recompress` from the design doc remains deferred; so do configurable `--unsort` chaos modes (rotate / shuffle / reverse).

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
Relative `directory` and `polygon_file` values resolve from the config file's
directory. Relative `output` paths resolve from `directory`, or from the config
file's directory when it is omitted. `-d` rebases configured outputs under its
directory, preserving paths below the configured output directory; an absolute
output outside that root keeps only its filename.
These rules intentionally differ from `osmium extract`: relative `directory`
values use the config file's location here, and `-d` rebases absolute outputs
under the override instead of concatenating the two path strings.

```
pbfhogg extract [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file (required for single extract, omit with --config) |
| `-b, --bbox <BBOX>` | Bounding box: minlon,minlat,maxlon,maxlat |
| `-p, --polygon <FILE>` | Polygon GeoJSON file |
| `-c, --config <FILE>` | Multi-extract JSON config file |
| `-d, --directory <DIR>` | Rebase config outputs under this directory (only with --config) |
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

With `--input-kind osc` (or autodetected from `.osc`/`.osc.gz` extension), filters an OSC change file instead, always preserving deletes. PBF-only flags (`-R`, `-i`, `-t`, `-j`) are not valid in OSC mode (OSC parsing is single-threaded).

Expressions use osmium syntax: `highway=primary`, `amenity`, `w/building=yes`, etc.

An expression is `[nwr/]KEY[!]=VALUE` or `[nwr/]KEY`. Key and value are matched
by the same rules, following osmium's `get_string_matcher`:

| Form | Meaning |
|------|---------|
| `foo` | exact match |
| `foo,bar` | any one of the alternatives (both halves, keys included) |
| `foo*` | prefix match |
| `*foo`, `*foo*` | substring match (there is no separate suffix matcher) |
| `*` | matches anything |

Spaces around each half, and around each comma-list element, are stripped. The
expression splits at the first `=`; a `!` inverts the value match, which still
requires the key to match.

The `!` is tested on the raw key, before that space stripping, as osmium does.
So it marks an inversion only when it is the last character before the `=`:
`highway !=primary` inverts, while `highway ! = primary` and
`highway! = primary` name the literal keys `highway !` and `highway!`.

```
pbfhogg tags-filter [OPTIONS] --output <OUTPUT> <FILE> [EXPRESSIONS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--input-kind <KIND>` | Input kind override: `pbf` or `osc` (autodetect from extension by default) |
| `--distrust-metadata` | Do not skip blobs on their indexdata or tag index; decode every blob (PBF only). Nothing validates a blob's tag index against its payload, so a file whose index omits a key its elements carry silently loses those elements. Trades the skip speedup for an answer that depends only on element data, and waives the indexdata precondition |
| `-R, --omit-referenced` | Omit referenced objects (faster, single pass, direct matches only; PBF only) |
| `-i, --invert-match` | Invert match: exclude matching objects, keep non-matching (PBF only) |
| `-t, --remove-tags` | Remove tags from referenced objects not directly matched (use without -R; PBF only) |
| `-e, --expressions <FILE>` | Read filter expressions from file (one per line, # comments) |
| `-j, --jobs <N>` | Worker-pool size for the parallel classify phases (PBF only). `0` (default) uses rayon's `available_parallelism()`. Two-pass mode only: combining `-j` with `-R` is an error since the single-pass path uses the pipelined reader, not the parallel classify path |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### export

> **0.5.0**

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

Compare two PBF files and show differences. Uses content equality (coordinates, tags, refs, members) rather than version/timestamp ordering - deterministic regardless of metadata completeness (see [DEVIATIONS](/cli/deviations#diff-content-equality-vs-version-ordering)).

With `--format osc`, generates an OSC diff file instead of text output. Text-only flags (`-c`, `-v`, `-s`, `-q`, `-t`) are not valid with `--format osc`. OSC-only flags (`--increment-version`, `--update-timestamp`) are not valid with `--format text`.

```
pbfhogg diff [OPTIONS] <OLD> <NEW>
```

| Flag | Description |
|------|-------------|
| `--format <FORMAT>` | Output format: `text` (default) or `osc` |
| `-c, --suppress-common` | Hide unchanged elements (text only) |
| `-v, --verbose` | Show detailed changes for modified elements (text only). Always takes the sequential path regardless of `-j` |
| `-s, --osmium-summary` | Print osmium-style summary (left/right/same/different counts) to stderr instead of the default pbfhogg-format summary (text only). The pbfhogg-format summary always fires on stderr unless `--quiet` - this flag only swaps the format |
| `-q, --quiet` | Exit-code only, suppress output (text only) |
| `-o, --output <FILE>` | Write output to file (required for `--format osc`) |
| `-t, --type <TYPE>` | Filter by element type (text only) |
| `-j, --jobs <N>` | Parallel shard count for the block-pair merge. `0` (default) auto-picks from available cores; `1` restores the sequential, scratch-free path; higher values partition the ID space across that many worker threads. Applies to both `--format text` and `osc`; requires both inputs indexed. **Rejected with `-v/--verbose`**, which is sequential-only - an explicit `-j` there used to be accepted and discarded. The parallel path writes shard temp files, removed on completion; scratch demand varies by input. See `reference/performance.md` for measurements |
| `--increment-version` | Bump version of deleted elements by 1 (osc only) |
| `--update-timestamp` | Set delete timestamp to current time (osc only) |
| `--direct-io` | Use O_DIRECT to bypass page cache |

With `--format osc`, produces a lossless roundtrip - applying the derived OSC to the old PBF reproduces the new PBF exactly (see [DEVIATIONS](/cli/deviations#derive-changes-lossless-delete-roundtrip)).

Inputs holding any negative ID always take the sequential block-pair path, whatever `-j` says: the parallel planner's shard boundaries are raw numeric comparisons, which do not order negative IDs the way the merge does.

### digest

Print a digest of a PBF's element content. Two files the command accepts get the same digest exactly when `diff --suppress-common` reports them identical (barring a 128-bit hash collision), whatever their encoding: compression, blob boundaries, dense or plain nodes, string-table order, header fields and indexdata presence do not change it, and neither does metadata, which diff ignores.

```
pbfhogg digest [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `--direct-io` | Use O_DIRECT to bypass page cache |

Output, on stdout:

```
xxh128 v1 <32 hex digits>
node <count> <32 hex digits>
way <count> <32 hex digits>
relation <count> <32 hex digits>
```

The first line names the algorithm and the canonical-form version; the version changes whenever the content definition or its serialization does, so a stored digest is only comparable with one of the same version. The per-kind lines digest each kind alone, so a mismatch shows which kind differs.

The command refuses, exiting non-zero and printing no digest line, any input on which diff's answer is not a function of element content:

- no `OsmHeader` as the first blob, a second `OsmHeader`, or a blob of unknown type;
- a header that does not claim `Sort.Type_then_ID`;
- a data block that is mixed-kind (or classified as one kind while holding another), kinds out of node, way, relation order, or IDs that do not strictly ascend within a kind (duplicates included);
- indexdata on some data blobs and not others, or indexdata whose kind, minimum ID, maximum ID or count disagrees with its block, or on an empty block;
- a tag, role or metadata user name whose string-table index does not resolve.

A refusal is the safe answer for a cache keyed on the digest: fall back to the real comparison. The promise is "refuses input failing these checks", not "refuses every corrupt file": a file cut off exactly at a blob boundary is a valid shorter file.

### getid

Extract or remove elements by ID. By default, keeps only the listed IDs. With `--invert`, removes the listed IDs and keeps everything else.

Typed IDs accept `n123` or `node/123` (node), `w456` or `way/456` (way), and
`r789` or `relation/789` (relation). Bare numeric IDs are accepted only when
`--default-type` is supplied. Without it, a bare ID is an error. This also
applies to IDs in `--id-file` text input.

```
pbfhogg getid [OPTIONS] --output <OUTPUT> <FILE> [IDS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--invert` | Invert selection: remove listed IDs instead of keeping them |
| `-r, --add-referenced` | Include referenced nodes of matching ways (two-pass; not with `--invert`) |
| `-t, --remove-tags` | Remove tags from referenced objects (use with -r; not with `--invert`) |
| `--verbose-ids` | Print requested IDs, then report which were not found (not with `--invert`). Both lists go to stderr. The found set is collected on the write path, so this costs no extra pass over the output |
| `-i, --id-file <FILE>` | Read IDs from text file (one per line) |
| `-I, --id-osm-file <FILE>` | Read IDs from an OSM/PBF file (all element IDs are collected) |
| `--default-type <TYPE>` | Type for bare numeric IDs: node, way, relation. No type is assumed when this option is omitted. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

`--verbose-ids` is an exhaustive diagnostic with no output cap. Requested
IDs are space-separated on one line per kind; missing IDs are printed one
per line. With `-I`, every ID from that input participates, so large inputs
can produce very large stderr output. The missing-ID report also collects
three vectors, one per kind, requiring O(M) additional ID storage for M
missing objects. Omit `--verbose-ids` when that output and storage are unwanted;
the normal command summary remains available.

### getparents

Find ways/relations referencing given IDs (reverse lookup).

ID syntax and the optional `--default-type` behavior are the same as for
`getid`: bare numeric IDs require an explicit default type.

```
pbfhogg getparents [OPTIONS] --output <OUTPUT> <FILE> [IDS]...
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `-s, --add-self` | Also include the queried objects themselves in the output |
| `-i, --id-file <FILE>` | Read IDs from text file (one per line) |
| `-I, --id-osm-file <FILE>` | Read IDs from an OSM/PBF file (all element IDs are collected) |
| `--default-type <TYPE>` | Type for bare numeric IDs: node, way, relation. No type is assumed when this option is omitted. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

### add-locations-to-ways

Embed node coordinates in ways. Two index strategies:

- **sparse** (default) - Rank-indexed flat mmap array. Pre-allocates `referenced.total_count() * 8` bytes; workers store `(lat, lon)` at byte offset `IdSet::rank_if_set(node_id) << 3` via atomic stores. Working-set behavior depends on the available page cache; see `reference/performance.md` for measurements.
- **external** - Double radix permutation via 4-stage pipeline. Bounded memory with high scratch-disk demand. Requires sorted PBF (Sort.Type\_then\_ID) and indexdata. See `reference/performance.md` for measured resource use.

`--index-type dense` was removed; see `reference/performance.md` for measured comparisons. `dense` survives as a hidden value (absent from `--help` and from completion) purely so passing it errors with a pointer to `sparse` rather than a bare "invalid value".

By default, untagged nodes not referenced by a relation are dropped from output.

```
pbfhogg add-locations-to-ways [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--index-type <TYPE>` | Node index type: `sparse` (default), `external`, or `auto` (runtime routing; see `reference/pipeline.md`) |
| `--keep-untagged-nodes` | Keep all untagged nodes in output |
| `--inject-prepass` | Emit the opt-in `pbfhogg.WayMembers-v1` and `pbfhogg.SharedNodePins-v1` metadata for downstream reuse |
| `--ignore-missing-nodes` | Accept ways whose node references are absent from the input, writing those vertices as `(0, 0)`. Without it the run fails and no output is written: `(0, 0)` is Null Island, a real coordinate, so a placeholder vertex cannot be told apart from genuine data by any reader. Matches osmium in name and in default. |
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
| `--locations-on-ways` | Preserve way-node locations through the merge (requires base PBF with LocationsOnWays). Ways the OSC creates or modifies get locations from the merged nodes; base ways the OSC does not touch keep their existing locations, even when the OSC moves or deletes a node they reference. Rerun `add-locations-to-ways` on the output for a full refresh |
| `--ignore-missing-nodes` | With `--locations-on-ways`, permit unresolved references to retain the legacy `(0,0)` fallback instead of failing |
| `-j, --jobs <N>` | Worker-pool size for the descriptor-first pipeline. `0` (default) uses the `nproc - 2` heuristic (leaves two cores for the scanner + drain threads, minimum 2). An explicit `-j 1` is refused |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed even if input lacks indexdata |
| `--allow-version-regression` | Accept a change whose `version` is lower than the version already held for that object. Off by default: feeding diffs out of sequence order is refused rather than silently downgrading objects to an older state. A delete leaves a versioned tombstone, so a stale change after a delete is refused too. An equal version is always accepted and the later change wins, so overlapping diffs and regional-extract deletes at the held version apply. Elements that claim no `version` are unaffected |
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
| `--allow-version-regression` | Requires `--simplify`, the mode that resolves conflicts; passing it alone is a usage error. Accepts a later change whose `version` is lower than one seen earlier in input order. Off by default, so listing the input files out of sequence order is an error rather than a silent downgrade. An equal version is always accepted and the later change wins |
| `-j, --jobs <N>` | Parallel worker threads. `1` forces sequential, `0` (default) auto-picks from `available_parallelism()`, higher values cap the worker pool. Affects both the per-input parse fan-out and the simplify path's chunk fan-out |

### build-geocode-index

Build a reverse geocoding index from a PBF file. Produces a set of binary files (S2 cell index, address points, street segments, admin boundaries, string pool) that can be memory-mapped for sub-millisecond reverse geocoding queries.

Requires an indexed PBF (generated by `pbfhogg cat`) unless `--allow-unindexed` is set. The output directory must be new or empty, or hold an existing geocode index and `--overwrite` must be set; any other existing path is refused because publication replaces the whole directory. The output path itself cannot be a symlink. On Linux, an existing mount-point output directory is refused before the build.

```
pbfhogg build-geocode-index [OPTIONS] --output-dir <DIR> <FILE>
```

| Flag | Description |
|------|-------------|
| `--output-dir <DIR>` | Output directory for index files |
| `--street-level <N>` | S2 cell level for streets/addresses; must exceed `--coarse-level` and fit the builder limit. Default comes from the builder configuration |
| `--coarse-level <N>` | Fallback cell level for rural areas; must be below `--street-level` and fit the builder limit. Default comes from the builder configuration |
| `--admin-level <N>` | S2 cell level for admin boundaries, within the builder's work limit. Default comes from the builder configuration |
| `--max-admin-vertices <N>` | Douglas-Peucker cap for local admin polygons; levels 5, 4, 3, and 2 retain up to 2x, 4x, 8x, and 16x this value. Default comes from the builder configuration |
| `--search-radius <M>` | Fine-level max search distance in meters. Default comes from the builder configuration |
| `--coarse-search-radius <M>` | Coarse-level max search distance in meters. Default comes from the builder configuration |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--overwrite` | Replace an existing index in `--output-dir` after the new build succeeds. Does not bypass the indexdata requirement |
| `--allow-unindexed` | Accept input without blob-level indexdata (slower fallback path). Does not replace an existing index |
| `--force` | Legacy shorthand equivalent to `--overwrite --allow-unindexed` |

A node whose coordinates fall outside the geographic domain fails the whole
build. Incomplete extracts remain usable: missing way nodes and missing admin
member ways contribute no geometry; admin boundary candidates with a missing,
unparseable, or unsupported level or no name are skipped; relation members
whose roles do not describe outer or inner rings are ignored (an empty role
counts as outer). The PBF replication sequence is stored as a u32 in the
geocode header. A missing value is stored as zero, and a present value outside
the u32 range fails the build rather than being wrapped to a different source
version.

The three cell levels are validated before any input is read; a violation exits
2 with an explicit message containing the builder's current upper bound.
Street and coarse levels are limited by the per-segment cell-covering budget,
whose sample count doubles at each level. Admin level is limited by the
polygon flood-fill budget, whose cell count grows by a factor of four at each
level. `--coarse-level` must be strictly below `--street-level`. Readers still
accept indexes written at levels up to the S2 format limit of 30.

Every build writes a complete generation in a sibling staging directory and
publishes it only after the build and smoke test succeed; `--overwrite` (or
legacy `--force`) lets that publication replace an existing index. Failed
builds leave the existing index intact. Publication moves the old directory aside, moves
the new one into place, then removes the old files. Already-open readers keep
their mapped inodes and remain usable. The entire output directory is replaced;
use a dedicated directory for the index. Allow disk space for both generations.

Publication uses two renames, so it is not crash-atomic and there is a short
pathname gap. Serialize publication with new reader opens and other builds;
queries on readers already open need no coordination. An interrupted publication
may leave the old index in `.geocode-build-*/old` beside the output directory.
SIGKILL or power loss during a build can also leave an incomplete
`.geocode-build-*` sibling because process cleanup does not run. There is no
automatic sweep: a directory may belong to a concurrent build, and `old` may
hold the only recoverable prior index. After confirming no builder is active,
inspect abandoned staging directories and preserve or restore any `old` index
before removing scratch. A mount table change after the Linux preflight can
still make the final rename fail; on other platforms mount points are not
preflighted and the final rename may fail after the build.
External writers must also replace files rather than modify mapped inodes:
reader open-time string validation cannot protect against truncation, which
can fault before a string check can return an error.

A build that finds nothing queryable at all fails rather than publishing an
empty index. Indexes holding only some record kinds, such as an
admin-boundaries extract, still build.

Outputs a set of binary files. See `reference/performance.md` for measured build times, index size, and peak memory.
