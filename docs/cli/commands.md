# pbfhogg CLI Reference

Version 0.5.0. Generated from `pbfhogg --help` output.

## Global flags

All commands support `-h, --help` and `-V, --version`.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success. |
| `1` | The command ran and its answer is negative. Only `diff` (differences found), `inspect --indexed` (no indexdata), `inspect --show` (element not found) and `check` (integrity violations) produce it. |
| `2` | The command failed: missing or unreadable input, an invalid argument combination, a malformed PBF, an I/O error. `Error: ...` goes to stderr; clap's usage errors also exit `2`. |

Before 0.6.0 both meanings shared `1`, so "the files differ" and "the file
does not exist" were indistinguishable. `1` kept its meaning, so callers
branching on the semantic answer, or on any non-zero for failure, are
unaffected.

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
| `-j, --jobs <N>` | Parallel worker threads for `--nodes`. `0` (default) auto-picks from `available_parallelism()`, `1` forces sequential, higher values cap the pool. **Requires `--nodes`**: no other inspect mode reads it, so passing it elsewhere is an error rather than a no-op |
| `--blocks [N]` | Show per-block distribution stats and optional block listing (N limits to first/last N blocks) |
| `--id-ranges` | Show min/max element IDs per type and monotonicity |
| `--locations` | Show locations-on-ways diagnostics |
| `--anomalies` | Show only anomalous blocks (<50% or >150% of median, plus mixed blocks) |
| `-e, --extended` | Extended scan: timestamp range, data bbox, metadata coverage, ordering |
| `-g, --get <KEY>` | Get a single value by key path (e.g. `header.bbox`, `data.timestamp.first`) |
| `--json` | Machine-readable JSON output |
| `--show <TYPE_ID>` | Display a single element by ID (e.g. `n123`, `w456`, `r789`). Uses indexdata to skip non-matching blobs, early exit on sorted PBFs. **A mode of its own**: mutually exclusive with every other `inspect` flag, which it used to accept and discard |
| `--distrust-metadata` | Decode every blob instead of reporting counts, ID ranges and per-block stats out of the blobs' own indexdata. Nothing validates indexdata against the payload it describes, so on a file you are inspecting because you suspect it, the fast path reports the file's claims rather than its contents. Scanning report only: rejected with `--indexed`, `--nodes`, and a `header.*` `--get` key |
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
| `--max-errors <N>` | Stop after N violations (0 = unlimited) [default: 100] |
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

The ref check reports element IDs it could not track (negative IDs, or IDs above
the range sized from the file's own indexdata) on their own
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
| `-j, --jobs <N>` | Parallel shard count for the block-pair merge. `0` (default) auto-picks from available cores; `1` restores the sequential, scratch-free path; higher values partition the ID space across that many worker threads. Applies to both `--format text` and `osc`; requires both inputs indexed. **Rejected with `-v/--verbose`**, which is sequential-only - an explicit `-j` there used to be accepted and discarded. The parallel path writes shard temp files (planet scale: ~30 GB text, ~45 GB osc XML), removed on completion |
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
| `--verbose-ids` | Print requested IDs, then report which were not found (not with `--invert`). Both lists go to stderr. The found set is collected on the write path, so this costs no extra pass over the output |
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
- **external** - Double radix permutation via 4-stage pipeline. Bounded memory (~8.7 GB measured peak anon at planet). The only mode that survives at planet on memory-constrained hosts. Requires sorted PBF (Sort.Type\_then\_ID) and indexdata. Uses ~112 GB temp disk at Europe, ~256 GB at planet.

`--index-type dense` was removed - sparse rank-indexed flat dominated dense at every measured scale (japan dense 51.6 s vs sparse 11.9 s). `dense` survives as a hidden value (absent from `--help` and from completion) purely so passing it errors with a pointer to `sparse` rather than a bare "invalid value".

By default, untagged nodes not referenced by a relation are dropped from output.

```
pbfhogg add-locations-to-ways [OPTIONS] --output <OUTPUT> <FILE>
```

| Flag | Description |
|------|-------------|
| `-o, --output <FILE>` | Output file |
| `--index-type <TYPE>` | Node index type: `sparse` (default), `external`, or `auto` (scale-aware: sparse unless the input is sorted+indexed and the estimated node store exceeds ~80 % of available RAM) |
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
| `--locations-on-ways` | Preserve and update way-node locations through the merge (requires base PBF with LocationsOnWays) |
| `--ignore-missing-nodes` | With `--locations-on-ways`, permit unresolved references to retain the legacy `(0,0)` fallback instead of failing |
| `-j, --jobs <N>` | Worker-pool size for the descriptor-first pipeline. `0` (default) uses the `nproc - 2` heuristic (leaves two cores for the scanner + drain threads, min 1) |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed even if input lacks indexdata |
| `--allow-version-regression` | Accept a change whose `version` does not advance past the version already held for that object. Off by default: applying the same OSC twice, or feeding diffs out of sequence order, is refused rather than silently downgrading objects to an older state. A delete leaves a versioned tombstone, so a stale change after a delete is refused too. Elements that claim no `version` are unaffected |
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
| `--allow-version-regression` | Only meaningful with `--simplify`, the mode that resolves conflicts. Accepts a later change whose `version` does not advance past one seen earlier in input order. Off by default, so listing the input files out of sequence order is an error rather than a silent downgrade |
| `-j, --jobs <N>` | Parallel worker threads. `1` forces sequential, `0` (default) auto-picks from `available_parallelism()`, higher values cap the worker pool. Affects both the per-input parse fan-out and the simplify path's chunk fan-out |

### build-geocode-index

Build a reverse geocoding index from a PBF file. Produces a set of binary files (S2 cell index, address points, street segments, admin boundaries, string pool) that can be memory-mapped for sub-millisecond reverse geocoding queries.

Requires an indexed PBF (generated by `pbfhogg cat`). The output directory must not already exist unless `--force` is set.

```
pbfhogg build-geocode-index [OPTIONS] --output-dir <DIR> <FILE>
```

| Flag | Description |
|------|-------------|
| `--output-dir <DIR>` | Output directory for index files |
| `--street-level <N>` | S2 cell level for streets/addresses, 0-19 [default: 17] |
| `--coarse-level <N>` | Fallback cell level for rural areas, 0-19 [default: 14] |
| `--admin-level <N>` | S2 cell level for admin boundaries, 0-19 [default: 10] |
| `--max-admin-vertices <N>` | Douglas-Peucker cap for local admin polygons; levels 5, 4, 3, and 2 retain up to 2x, 4x, 8x, and 16x this value [default: 500] |
| `--search-radius <M>` | Fine-level max search distance in meters [default: 75] |
| `--coarse-search-radius <M>` | Coarse-level max search distance in meters [default: 1000] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed without indexdata / overwrite existing index |

The three cell levels are validated before any input is read; a violation exits
2 with an explicit message. Each must be in `0..=19`, and `--coarse-level` must
be strictly below `--street-level`. The upper bound is 19 rather than the S2
limit of 30 because the per-segment cell-covering cost doubles with every
level, and beyond 19 a single long segment exceeds the builder's sampling
budget. Readers still accept indexes written at levels up to 30.

`--force` invalidates the existing index rather than swapping it: a geocode
index is a directory of 19 files with no atomic replace, so the header is
removed first and a build that dies partway leaves a directory that fails to
open instead of one that answers from two vintages. Invalidation happens only
after every read-only check on the input has passed, so a bad input path leaves
the existing index untouched.

A build that finds nothing queryable at all fails rather than publishing an
empty index. Indexes holding only some record kinds, such as an
admin-boundaries extract, still build.

Outputs 19 binary files. Denmark (465 MB PBF): ~7s, 172 MB index. Europe (32.4 GB): 524s (8.7 min), 7.5 GB RSS. Planet (87 GB): 1,255s (20.9 min), 29.5 GB peak RSS (pass-1.5 transient).
