# pbfhogg CLI Reference

Version 0.5.0 (plus unreleased 0.6.0 changes). Generated from
`pbfhogg --help` output.

Every enumerated flag value (`--compression`, `--type`, `--index-type`,
`--clean`, `inspect tags --sort`, the `bench-*` modes) is parsed once at
the argument boundary, so `--help` lists the accepted values and shell
completion offers them.

## Global flags

All commands support `-h, --help` and `-V, --version`.

## Exit codes

Every command uses the same three-band contract. `1` is reserved for a
command that ran to completion and whose answer is negative; a command
that could not run at all exits `2`, so a script can branch on the
answer without swallowing failures.

| Code | Meaning |
|------|---------|
| `0` | Success. |
| `1` | The command ran and its answer is negative. Only four commands can produce it: `diff` (differences found), `inspect --indexed` (no blob-level indexdata), `inspect --show` (element not in the file), and `check` (integrity violations). Nothing is printed to stderr for it beyond the command's normal output. |
| `2` | The command failed: missing or unreadable input, an invalid argument combination, a malformed PBF, an I/O error. `Error: ...` goes to stderr. Argument-parsing errors from clap also land here. |

Before 0.6.0 every one of these outcomes exited `1`, so "the files
differ" and "the file does not exist" were indistinguishable. Callers
that branch on `1` for the semantic answer are unaffected; callers that
treat any non-zero as failure are unaffected. Only a caller that read
`1` as "the command failed" needs to widen to `!= 0`.

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
| `-j, --jobs <N>` | Parallel worker count for `--nodes`. `0` auto-picks from `available_parallelism()`, `1` forces sequential, higher values cap the pool. **Requires `--nodes`**: no other inspect mode reads it, so passing it elsewhere is an error rather than a no-op. |
| `--blocks [N]` | Show per-block distribution stats and optional block listing (N limits to first/last N blocks) |
| `--id-ranges` | Show min/max element IDs per type and monotonicity |
| `--locations` | Show locations-on-ways diagnostics |
| `--anomalies` | Show only anomalous blocks (<50% or >150% of median, plus mixed blocks) |
| `-e, --extended` | Extended scan: timestamp range, data bbox, metadata coverage, ordering |
| `-g, --get <KEY>` | Get a single value by key path (e.g. `header.bbox`, `data.timestamp.first`). Not combinable with `--json`. |
| `--json` | Machine-readable JSON output. Not combinable with `--nodes` (node stats have no JSON form) or `--get`. |
| `--show <TYPE_ID>` | Display a single element by ID (e.g. `n123`, `w456`, `r789`). Uses indexdata to skip non-matching blobs, early exit on sorted PBFs. **A mode of its own**: mutually exclusive with every other `inspect` flag (`--indexed`, `--nodes`, `-j`, `--blocks`, `--id-ranges`, `--locations`, `--anomalies`, `-e`, `--get`, `--json`, `--force`), which it used to accept and discard. |
| `--distrust-metadata` | Decode every blob instead of reporting counts, ID ranges and per-block stats out of the blobs' own `indexdata`. Nothing validates indexdata against the payload it describes, so on a file you are inspecting *because* you suspect it, the default fast path reports the file's claims rather than its contents. Applies to the scanning report only: rejected alongside `--indexed`, `--nodes` and a `header.*` `--get` key, none of which consult indexdata contents. |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata. **Requires `--nodes`**: it is threaded only into the node-stats scan, so passing it to another mode is an error rather than a no-op. |

#### inspect tags

Count tag key=value frequencies (subcommand of `inspect`).

| Flag | Description |
|------|-------------|
| `--min-count <N>` | Only show tags with at least this many occurrences [default: 1] |
| `-M, --max-count <N>` | Only show tags with at most this many occurrences |
| `-s, --sort <ORDER>` | Sort order: count-desc (default), count-asc, name-asc, name-desc. `count` and `name` are accepted as aliases for `count-desc` and `name-asc`. |
| `-e, --expressions <FILE>` | Read tag expressions from file (one per line, # comments) |
| `-t, --type <TYPE>` | Filter by element type: node, way, or relation |
| `-j, --jobs <N>` | Parallel worker count. `0` auto-picks from `available_parallelism()`, `1` forces sequential, higher values cap the pool. |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed even if input lacks indexdata (slower fallback path) |

### check

Validate PBF file integrity (IDs + referential integrity).

Validate PBF integrity along three independent axes, selected by flag. With no
flag, the ID and referential integrity checks both run.

`--index` is a selector like the other two but is deliberately **not** in the
default set: it decompresses every blob in the file, and folding that into a
bare `check` would multiply the cost of the command everyone already runs.
Passing it suppresses the default pair; combine it with `--ids` / `--refs` to
run more than one.

```
pbfhogg check [OPTIONS] <FILE>
```

| Flag | Description |
|------|-------------|
| `--ids` | Check ID uniqueness and ordering only |
| `--refs` | Check referential integrity only |
| `--index` | Verify blob-level metadata against the blobs. Decompresses every blob |
| `--full` | Full duplicate detection via bitmap (slower, more memory; applies to ID check) |
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation; applies to ID check) |
| `--max-errors <N>` | Stop after N violations (0 = unlimited) [default: 100] |
| `--check-relations` | Also check relation member references (applies to ref check) |
| `--show-ids` | Show IDs of missing objects, format: `n123 in w456` (applies to ref check) |
| `--json` | Machine-readable JSON output |
| `--quiet` | Exit-code only, no output. Not combinable with `--json`. |
| `--direct-io` | Use O_DIRECT to bypass page cache |

`--full` and `-t/--type` apply to the ID check and are rejected under
`--refs` alone; `--check-relations` and `--show-ids` apply to the ref
check and are rejected under `--ids` alone. Violations exit `1`, not
`2` - see [Exit codes](#exit-codes).

For missing relation-to-relation members, reports unique missing IDs with occurrence count when they differ: `Missing relation members: 706 (777 references)`.

The ID check judges monotonicity by canonical OSM order - `0`, then the
negative IDs by ascending absolute value, then the positives - which is the
order `sort` produces. On a positive-only file (every production PBF) that is
plain `i64` order; on a file carrying negative IDs the two differ, and the
canonical one is the definition.

#### `--index`: the metadata trust boundary

`BlobHeader.indexdata` (element kind, ID range, element count, spatial bbox) and
`BlobHeader.tagdata` (the blob's tag key set) are hints every fast path in this
crate believes without opening the blob. `cat --type` passes a blob through raw
on the strength of the declared kind, `renumber` assigns IDs from the declared
count, `tags-filter` skips whole blobs on the strength of the tag index, and the
spatial filter skips node blobs on the strength of the bbox. Nothing in the
format signs any of it, and `degrade --corrupt-indexdata` /
`--corrupt-indexdata-kind` / `--corrupt-tagdata` exist precisely to manufacture
files whose hints are well-formed and false.

`check --index` is the other side of that boundary. It decompresses every
OSMData blob, re-derives the metadata from the payload with `scan_block_ids`
and `scan_block_tags`, and reports each disagreement with the blob's ordinal and
byte offset.

Those are the same two scanners the writer's passthrough path uses, so on a
re-framed blob the check cannot catch a bug *in the scanner itself* - it can
only catch metadata that has since diverged from its payload. Against the
ordinary write path it is genuinely independent: `BlockBuilder` derives its
index incrementally as elements are added and never rescans the bytes it
emitted, so a disagreement between the two is a real finding either way.

**There is no `--distrust-metadata` flag on it, by design.** A verifier that
consulted the thing it is verifying in order to decide what to look at would be
worthless, so the command installs no blob filter, never calls
`require_indexdata`, and decompresses every blob unconditionally - including
blobs whose stored index claims a kind nobody asked about. Distrust is the only
mode it has.

Absence is not a claim. A blob with no indexdata, or no tagdata, is not
reported; that is the ordinary state of any third-party PBF, and of every v1
(26-byte) index, which has no bbox field at all.

**Unreadable is not the same as absent.** Indexdata whose length, version byte
or element-kind byte the reader does not recognise is reported as a violation
even though every *reader* collapses it to "no index" and decodes the blob.
The safe reading is right for a reader and wrong for a verifier: a file that
claims something it cannot back up is not a file that claims nothing.
Malformed tagdata is reported the same way, and so is a well-formed tagdata
attached to a blob this crate refuses to index at all - a group mixing element
kinds, where any key set is necessarily incomplete because the scanner sees
only the first element field, and `tags-filter` would skip the blob on the
strength of it.

Two checks are containment rather than equality:

| Field | Test | Why |
|-------|------|-----|
| kind, count, min_id, max_id | exact equality | every consumer treats them as exact |
| bbox | stored must **contain** the blob's coordinates | the scan rounds outward through `granularity`/`lat_offset`/`lon_offset`, so a stored box narrower than the scan is the only shape that can make a spatial filter skip a blob it should have read. A wider box is sound; an absent box is no claim |
| tag key set | stored must contain every key the blob carries | an omitted key makes `tags-filter` skip a blob holding matches. A key listed but not present is sound - it only costs a decode - and is reported as a separate, distinct violation |

Exit `1` means mismatches were found, exit `2` that the check could not run.

The ref check reports element IDs it could not track - negative IDs, or IDs
above the range it sized from the file's own indexdata - as a separate
`Untrackable element IDs` line and as `untrackable_node_ids` /
`untrackable_way_ids` / `untrackable_relation_ids` under `--json`. References
to such an element are counted as missing, so a non-zero count means the
missing-reference numbers are an upper bound. `--ids --full` reports the same
situation as an `id_out_of_range` violation.

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
| `-t, --type <TYPE>` | Filter by element type (comma-separated: node, way, relation). Not valid with `--dedupe`. |
| `-c, --clean <ATTR>` | Strip metadata attribute (repeatable: version, timestamp, changeset, uid, user). Not valid with `--dedupe`. |
| `--dedupe` | K-way sorted merge with dedup (all inputs must be sorted). Rejects `--type` and `--clean`: the dedupe path is blob-level passthrough and implements neither. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O. Only valid with `--dedupe` - the plain `cat` path has no io_uring writer, and passing it there is an error. |
| `--force` | Proceed even if input lacks indexdata |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

**Multi-input output header.** With one input the input header is carried
through. With several, the output header is reconciled across all of them so
it can only ever understate: the bbox is the union, and is omitted entirely
unless every input declares one (an input of unknown extent makes the union
unknown); each replication field (timestamp, sequence number, base URL) is
kept only when every input agrees on it, and dropped otherwise;
`HistoricalInformation` is the OR. osmium writes a wholly blank header for
multiple inputs - the same honesty rule, applied bluntly - so keeping the
agreed fields is a deliberate deviation. Previously all three cat paths took
the header from the first input alone, which advertised a bbox that did not
cover the output's own data.

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
| `--corrupt-tagdata` | Replace `BlobHeader.tagdata` on every OsmData blob with a well-formed tag key index naming one key (`pbfhogg:degrade:no-such-key`) the blob does not carry. The result parses cleanly and lies, so `tags-filter` skips blobs whose elements match and emits a short answer. This is the adversarial case `--strip-tagdata` cannot produce: a *missing* index is decoded rather than trusted, so stripping only exercises the slow path. Mutually exclusive with `--strip-tagdata`, and with every blob-re-encoding flag (which would rebuild the index correctly). `tags-filter --distrust-metadata` is what reads such a file correctly. |
| `--corrupt-indexdata` | Replace `BlobHeader.indexdata` on every OsmData blob with a well-formed index that overstates the blob: same element kind, `count` and `max_id` inflated by 1,000,000. The kind is preserved deliberately, so per-kind classify schedules still route correctly and the lie is confined to the counts. Produces the file `inspect` reports from without decoding anything - see `inspect --distrust-metadata`. Mutually exclusive with `--strip-indexdata` and with every blob-re-encoding flag. |
| `--corrupt-indexdata-kind` | Rotate the element *kind* declared by `BlobHeader.indexdata` on every OsmData blob - node to way to relation to node - leaving `min_id`, `max_id`, `count` and the bbox honest. The kind is what per-kind blob schedules (`build_classify_schedule`) select on, so unlike `--corrupt-indexdata` this is the corruption that makes a consumer skip a blob *outright* rather than misreport it, which is why the two are separate flags. It is what makes `tags-filter --distrust-metadata` testable through relation closure and way-dependency collection rather than only through the direct-match pass. Mutually exclusive with `--strip-indexdata`, combinable with `--corrupt-indexdata`, and rejected alongside every blob-re-encoding flag. |
| `--strip-bbox` | Clear `HeaderBlock.bbox` (field 1). Header-only change; no OsmData blob is touched. Exercises `inspect`'s bbox handling (`extract_header_metadata`, the `inspect --get header.bbox` fast path) and downstream/external-consumer tolerance of a file with no declared extent. Does not affect `extract --bbox`, which derives its region from the CLI argument and prunes via per-blob `indexdata` bboxes rather than reading the header. |
| `--drop-ids <N:SEED>` | Deterministically remove exactly N elements selected globally by kind, ID, and seed. Surviving references to removed elements intentionally dangle. |
| `--force` | Skip the indexdata precondition required by the decode path (falls back to scanning every blob for every kind; slower). |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--generator` | Override writing program name |
| `--output-header <K=V>` | Set output header fields (repeatable) |

**Implementation paths:** `--strip-indexdata`, `--strip-tagdata`, and/or `--strip-bbox` (with no `--unsort`/`--strip-locations`/`--drop-ids`) run as a header-and-blob-level passthrough: with no `--generator`/`--output-header` override, the input `HeaderBlock` payload is forwarded field-for-field via a surgical wire-level field stripper, with only the bbox (field 1) removed under `--strip-bbox`, and raw OsmData frames are reframed by copying the original `BlobHeader` through byte-for-byte with only `indexdata` (field 2) and/or `tagdata` (field 4) cleared as targeted. This preserves every other header and blob-header field byte-for-byte - `source`, custom optional features, a non-default `writingprogram`, replication metadata, `WayMembers-v1`, and unknown/extension fields all survive. A `HeaderBuilder` rebuild carries only the fields the builder models - it does keep `source` and replication metadata, but drops the optional-feature list and any unknown/extension field. When a header override (`--generator`/`--output-header`) is present, the header is instead rebuilt through `HeaderBuilder` with the override applied (and the bbox omitted under `--strip-bbox`). Any combination involving `--unsort`, `--strip-locations`, or `--drop-ids` decodes elements, re-encodes via `BlockBuilder`, and frames with `indexdata=None` when `--strip-indexdata` composes; `--strip-bbox` on this path clears the bbox from the rebuilt output header. `--drop-ids` requires `N:SEED`, rejects zero N, and is reproducible for a given input and seed.

`degrade --block-cap` and `getparents --full-scan-min-blobs` are hidden
(`hide = true`) test instruments: they override an internal threshold so
the suite can fire a code path without building a full-scale fixture.
They are not part of the supported surface and may change or disappear
without a deprecation cycle. `--full-scan-min-blobs` is nevertheless
range-checked at the CLI boundary (1 to 1,000,000,000): the library entry
point behind it takes the value raw, and 0 is not a threshold - it pins the
FullScan arm unconditionally.

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

#### Expression syntax

An expression is `[nwr/]KEY[!]=VALUE` or `[nwr/]KEY`. The optional `nwr/`
prefix restricts the expression to element types. Key and value are each
matched by the same rules, which follow
`osmium-tool/src/util.cpp::get_string_matcher`:

| Form | Meaning |
|------|---------|
| `foo` | exact match |
| `foo,bar` | any one of the comma-separated alternatives (**both halves**, keys included) |
| `foo*` | prefix match |
| `*foo`, `*foo*` | substring match (osmium has no separate suffix matcher) |
| `*` | matches anything |

Spaces around each half, and around each comma-list element, are stripped, so
`highway = primary` from an `--expressions` file names the key `highway`. The
expression is split at the **first** `=`; a `!` inverts the value match, which
still requires the key to match (`highway!=primary` selects elements that have
a `highway` tag whose value is not `primary`, not elements without the tag).

The `!` is tested on the raw key, *before* the space stripping above, exactly
as `get_tag_matcher` does. It therefore counts as an inversion marker only when
it is the last character before the `=`. `highway !=primary` inverts;
`highway ! = primary` and `highway! = primary` do not, and name the literal
keys `highway !` and `highway!` instead. This is a corner worth stating rather
than discovering, because both spellings parse without error and select a
different set of elements than the reader intended.

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
| `--distrust-metadata` | Do not skip blobs on their `indexdata` or tag index; decode every blob. Blob-level tag-index skipping is a trust boundary with nothing on the other side of it - nothing validates a blob's tag index against its payload - so a file whose index omits a key its elements carry silently loses those elements from the output. This flag trades the skip speedup for an answer that depends only on element data, and waives the indexdata precondition (`--force` is not needed alongside it). Not valid in OSC mode, which has no blobs. `degrade --corrupt-tagdata` produces the file this flag exists for. |
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
| `-j, --jobs <N>` | Parallel shard count. `0` (default) auto-picks from `available_parallelism()`; `1` restores the sequential, scratch-free path; higher values partition the ID space across N worker threads. Applies to both text and `--format osc`; parallel sharding requires both inputs to be indexed. **Rejected with `-v/--verbose`**, which is sequential-only - an explicit `-j` there used to be accepted and discarded. The parallel path writes shard temp files next to the output (planet scale: ~30 GB text, ~45 GB osc XML), removed on completion. |
| `--increment-version` | Bump version of deleted elements by 1 (osc only) |
| `--update-timestamp` | Set delete timestamp to current time (osc only) |
| `--direct-io` | Use O_DIRECT to bypass page cache |

With `--format osc`, produces a lossless roundtrip - applying the derived OSC to the old PBF reproduces the new PBF exactly (see [DEVIATIONS](../DEVIATIONS.md#derive-changes-lossless-delete-roundtrip)).

Inputs holding any negative ID always take the sequential block-pair path, whatever `-j` says: the parallel planner's shard boundaries are raw numeric comparisons, which do not order negative IDs the way the merge does.

### digest

Print a digest of a PBF's element content. Two files the command accepts get the same digest exactly when `diff --suppress-common` reports them identical (barring a 128-bit hash collision), whatever their encoding: compression, blob boundaries, dense or plain nodes, string-table order, header fields and indexdata presence do not change it, and neither does metadata, which diff ignores. Element content is defined once, in `src/content.rs`, for both commands.

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
| `--verbose-ids` | Print requested IDs, then report which were not found (not with `--invert`). Both lists go to stderr. The found set is collected on the write path, so this costs no extra pass over the output. |
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
- **external** - Double radix permutation via 4-stage pipeline. Bounded memory (~8.7 GB measured peak anon at planet). The only mode that survives at planet on memory-constrained hosts. Requires sorted PBF (Sort.Type\_then\_ID) and indexdata. Uses ~256 GB peak temp disk at planet.

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
| `--inject-prepass` | Emit the opt-in `pbfhogg.WayMembers-v1` and `pbfhogg.SharedNodePins-v1` metadata into the output header, readable back via `Blob::way_members` / `Way::shared_node_pins` |
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
| `-j, --jobs <N>` | Worker-pool size for the descriptor-first pipeline. `0` (default) uses the `nproc - 2` heuristic (leaves two cores for scanner + drain, min 1). Pass a specific N to measure scaling or constrain CPU use. |
| `--compression` | Blob compression [default: zlib] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--io-uring` | Use io_uring for output I/O |
| `--force` | Proceed even if input lacks indexdata |
| `--allow-version-regression` | Accept a change whose `version` does not advance past the version already held for that object. Off by default: precedence is by version, not by position, so applying the same OSC twice or feeding diffs out of sequence order is an error naming both versions rather than a silent downgrade. Deletes count -- a delete leaves a versioned tombstone, so a later stale create or modify is refused too. Elements claiming no `version` are unaffected. See DEVIATIONS.md. |
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
| `--allow-version-regression` | Only meaningful with `--simplify`, which is the mode that resolves conflicts. Accepts a later change whose `version` does not advance past one seen earlier in input order. Off by default, so listing the inputs out of sequence order is an error rather than a silent downgrade. Default (non-simplify) mode preserves the full stream in input order and resolves nothing, so the flag has no effect there. See DEVIATIONS.md. |
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
| `--street-level <N>` | S2 cell level for streets/addresses, 0-19 [default: 17] |
| `--coarse-level <N>` | Fallback cell level for rural areas, 0-19 [default: 14] |
| `--admin-level <N>` | S2 cell level for admin boundaries, 0-19 [default: 10] |
| `--max-admin-vertices <N>` | Douglas-Peucker cap for local admin polygons; levels 5, 4, 3, and 2 retain up to 2x, 4x, 8x, and 16x this value [default: 500] |
| `--search-radius <M>` | Fine-level max search distance in meters [default: 75] |
| `--coarse-search-radius <M>` | Coarse-level max search distance in meters [default: 1000] |
| `--direct-io` | Use O_DIRECT to bypass page cache |
| `--force` | Proceed without indexdata / overwrite existing index |

### S2 level bounds

All three cell levels are validated before any input is read, and a violation
exits 2 with an explicit message:

- Each level must be in `0..=19`. The upper bound is 19, not the S2 format
  limit of 30, because cell covering samples each way segment at a fraction of
  the cell width for the level: the per-segment sample count doubles with every
  level, and past 19 a single long or antimeridian-crossing segment exceeds the
  builder's per-segment budget of 2^24 samples. Accepting such a level would
  produce a build that never finishes rather than one that is merely slow.
- `--coarse-level` must be strictly less than `--street-level`. The coarse
  index is the fallback for rural queries and must be the coarser of the two.

The *reader* is deliberately looser and accepts levels up to the S2 limit of
30, since reading does no cell covering: an index written at a higher level by
another producer stays readable.

### Overwriting an existing index

A geocode index is a directory of 19 files, and there is no atomic replace for
a non-empty directory. `--force` therefore invalidates rather than swaps: it
removes the header file so that a build which dies partway leaves a directory
that fails to open, rather than one that opens and answers from a mixture of
two vintages.

Invalidation happens only after every read-only check on the input has passed -
config validation, the indexdata requirement, the PBF header, and the relation
and referenced-node passes. A bad input path, a missing file or a malformed PBF
leaves the existing index untouched.

A build that finds nothing queryable at all - no address points, no streets, no
interpolation ways and no admin polygons - fails rather than publishing an empty
index. Indexes containing only some of those kinds, such as an admin-boundaries
extract, are legitimate and still build.

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
| check --index | Yes | - | Yes | - | Read-only; decompresses every blob |
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
