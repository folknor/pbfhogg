# Indexdata

## What is indexdata?

pbfhogg embeds additional metadata in BlobHeader fields that standard PBF readers silently skip (per protobuf wire format rules for unknown fields). This metadata enables commands to classify and filter blobs without decompressing them, which is the key to pbfhogg's performance on large files.

There are two types of embedded metadata:

### Indexdata (BlobHeader field 2)

A 42-byte fixed-size blob per BlobHeader containing:

- **Element type** (`ElementKind`) - whether the blob contains nodes, ways, or relations
- **ID range** - minimum and maximum element ID in the blob
- **Spatial bounding box** - min/max latitude and longitude in decimicrodegrees (`i32` coordinates)

The leading byte is a format version. Three are read: v1 (`0x01`, 26 bytes, no
bbox), v2 (`0x02`, 42 bytes, "no bbox" spelled as four zero words) and v3
(`0x03`, 42 bytes, current). v3 differs from v2 only in how an absent bbox is
encoded - as an inverted, unconstructible box rather than as zeros, which are
also the exact bbox of a blob at (0, 0). New files are written as v3; existing
v1 and v2 files keep working unchanged. The reverse direction degrades
gracefully: a pbfhogg release older than the v3 bump does not recognise `0x03`
and treats a v3 file as unindexed - output stays correct, but on the slower
full-decode paths (or with `--force` where indexdata is required).

This enables O(1) blob classification. For example, `apply-changes` determines which blobs are affected by a changeset by comparing ID ranges, passing through ~92% of blobs as raw bytes without decompression.

### Tagdata (BlobHeader field 4)

A variable-length blob containing the set of unique tag key strings present in the blob. Wire format: version byte (`0x01`) + key count (`u16` LE) + repeated `[key_len (u16 LE) + key_bytes]`.

This enables `tags-filter`, `tags-count` and filtered reads to skip decompression of blobs that provably lack required tag keys. For example, `tags-filter highway=primary` skips all blobs without a `highway` key.

## Verifying indexdata

Neither field is signed or checked by the format, and every fast path above
believes them, as do the commands that require sorted input of the header's
`Sort.Type_then_ID` flag. A file whose claims are false is outside what
pbfhogg promises to handle: commands may produce wrong output from it and
report success. pbfhogg's own writer produces honest metadata, and `cat`
derives it from the payload for every blob it indexes (a blob that already
carries indexdata is passed through as it is). For a file from elsewhere,
`pbfhogg check --index --ids` is the admission test: `--index` decompresses
every blob and diffs what it finds against what the header claims, and
`--ids` checks ID and type order; either exits `1` on a disagreement. It has
gaps: element-kind order is not checked on a file without indexdata, and the
header's feature flags (`LocationsOnWays` and the like) are not checked at
all. See [check](/cli/commands#check), and the
[indexdata trust model](https://github.com/folknor/pbfhogg/blob/main/reference/indexdata-trust.md)
for which command believes which claim, what a false one costs, and what no
check covers.

## Generating indexed PBFs

Use `pbfhogg cat` without a `--type` flag:

```sh
pbfhogg cat input.osm.pbf -o indexed.osm.pbf
```

The passthrough path adds indexdata via decompress+scan without re-compressing blobs. Memory usage is minimal and the file size overhead is under 0.5%.

Indexdata generation timings:

| Dataset | Size | Time | Notes |
|---------|------|------|-------|
| Denmark | 461 MB | **2.8s** | commit `69a127f`, buffered |
| Planet | 87 GB | **86.5s** (1m26s) | commit `aee7727`, buffered, post-regression-fix |

Buffered I/O wins for this workload - sequential single-file passthrough benefits from page cache prefetch. `--direct-io` adds alignment overhead without the concurrent read/write pattern that makes it faster for merge.

When `cat` is invoked with a `--type` flag (e.g., `cat -t way`), an indexed blob of the selected kind is copied raw on the strength of its indexdata, one of another kind is skipped, and a blob without indexdata (under `--force`) is decoded and re-encoded with fresh indexdata.

## Which commands require indexdata?

These commands will error if the input PBF lacks indexdata:

- `apply-changes` - blob classification for merge
- `sort` - blob-level permutation for sorted inputs
- `add-locations-to-ways` - parallel node index building, blob passthrough
- `extract` (complete-ways and smart strategies) - spatial blob filtering
- `tags-filter` - skips blobs lacking required tag keys
- `getid` - skips blobs whose ID range has no intersection with requested IDs
- `cat --type` (with or without `--clean`) - skips non-matching blob types entirely (except `--type node,way,relation --clean`, which skips nothing and does not refuse)
- `cat --dedupe` - blob-level overlap planning and merging
- `repack` and `degrade` (except the metadata-only `--strip-indexdata` path) - per-kind blob schedules
- `inspect tags --type` - type-filtered tag counting
- `inspect --nodes` - node coordinate analysis
- `build-geocode-index` - multi-pass pipeline with type filtering (escape differs, see below)
- `renumber` - per-blob element counts for ID assignment (no escape, see below)

The escape, and whether a command refuses at all, is decided per command:

- `build-geocode-index` refuses unindexed input unless `--allow-unindexed` is
  passed. Its `--force` is a legacy shorthand that grants both
  `--allow-unindexed` and `--overwrite`.
- `renumber` has no escape. It reads each blob's element count from indexdata
  to assign new IDs without a decode pass, so its header scan refuses the
  input if any data blob lacks indexdata.
- `getparents` and `check --ids` do not refuse a wholly unindexed input:
  `getparents` uses indexdata to choose a scan strategy, and `check --ids`
  warns that unindexed input is scanned more slowly.
- `tags-filter` asks for indexdata only when it would use it: not with
  `--invert` or `--distrust-metadata`.
- `cat --clean` without `--type` does not refuse: it decodes every blob
  exactly once whether or not it is indexed, so indexdata would save it
  nothing.

Except for renumber's scan, the check reads only the first data blob's
header. It is not a whole-file validation: a file whose first data blob is
indexed passes it even if later blobs are not. Such a partially indexed file
(indexdata on some data blobs and not others, in either order) is refused by
every command except `cat`, `check --index` and `inspect`, once its reads
have met both kinds of blob, with the missing-indexdata error located at the
first data blob without indexdata and this advice:

```sh
pbfhogg cat input.osm.pbf -o indexed.osm.pbf
```

`--force` does not lift that refusal, and it holds whether or not a command
would have used the indexdata: `getid`, `getparents`, `tags-filter` (with
`-R`, `--invert-match` and `--distrust-metadata` too), `time-filter` and
`export` refuse it like the rest. If the first data blob is the unindexed
one, a command that asks for indexdata refuses the file as unindexed first;
with `--force` it then stops at the partial refusal. `pbfhogg cat` indexes
every blob of such a file, and every `cat` mode runs on it: `--type`,
`--clean` and `--dedupe` decode or scan each blob that lacks indexdata, so
their output is the same as from the file fully indexed. `check --index`
reports the file as a violation, and `inspect` describes it.

## The --force flag

For the other commands in the list above, pass `--force` to skip the indexdata
check and proceed with a raw (non-indexed) PBF:

```sh
pbfhogg sort raw-input.osm.pbf -o sorted.osm.pbf --force
```

Commands will work but use slower fallback paths - they must decompress every blob to determine its contents instead of reading the blob header metadata. `--force` is not a universal escape: see the exceptions above.

The recommended workflow is to generate an indexed PBF once with `pbfhogg cat`, then use it for all subsequent operations.

## How it works

The PBF format structures data as a sequence of blobs, each containing a `BlobHeader` followed by compressed data. pbfhogg writes two additional fields into the `BlobHeader`:

1. After writing a `PrimitiveBlock` to a blob, `PbfWriter` scans the serialized bytes to extract element type, ID range, and bounding box, then writes this summary as field 2 of the `BlobHeader`.
2. Tag keys are collected from the block's string table and written as field 4 of the `BlobHeader`.

On the read side, `BlobReader` parses these fields from the `BlobHeader` before touching the compressed blob data. Commands use this metadata to decide whether to decompress a blob, pass it through as raw bytes, or skip it entirely.

## Compatibility

Indexdata and tagdata are transparent extensions. The protobuf wire format specification requires parsers to silently skip unknown fields, so any standard PBF reader (osmium, osm2pgsql, Planetiler, etc.) can read indexed PBFs without modification. No `optional_features` header declaration is added.

PBFs from other tools (without indexdata) are fully supported by pbfhogg - they just don't benefit from the fast classification paths. Blobs without tagdata always pass tag filters (conservative behavior).

## Checking for indexdata

Use `inspect --indexed` to check whether a PBF has blob-level indexdata:

```sh
pbfhogg inspect --indexed denmark.osm.pbf
```

Exit code 0 means indexed, exit code 1 means not indexed. This is useful in scripts to decide whether to run `cat` first.

The answer comes from the first data blob only, the same probe the commands
above use: a file whose first data blob carries indexdata reports indexed even
if later blobs do not, and a file with no data blobs reports indexed (no blob
lacks indexdata). `check --index` reports a partially indexed file as a
violation.
