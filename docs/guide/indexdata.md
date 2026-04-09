# Indexdata

## What is indexdata?

pbfhogg embeds additional metadata in BlobHeader fields that standard PBF readers silently skip (per protobuf wire format rules for unknown fields). This metadata enables commands to classify and filter blobs without decompressing them, which is the key to pbfhogg's performance on large files.

There are two types of embedded metadata:

### Indexdata (BlobHeader field 2)

A 42-byte fixed-size blob per BlobHeader containing:

- **Element type** (`ElemKind`) — whether the blob contains nodes, ways, or relations
- **ID range** — minimum and maximum element ID in the blob
- **Spatial bounding box** — min/max latitude and longitude in decimicrodegrees (`i32` coordinates)

This enables O(1) blob classification. For example, `apply-changes` determines which blobs are affected by a changeset by comparing ID ranges, passing through ~92% of blobs as raw bytes without decompression.

### Tagdata (BlobHeader field 4)

A variable-length blob containing the set of unique tag key strings present in the blob. Wire format: version byte (`0x01`) + key count (`u16` LE) + repeated `[key_len (u16 LE) + key_bytes]`.

This enables `tags-filter` and filtered reads to skip decompression of blobs that provably lack required tag keys. For example, `tags-filter highway=primary` skips all blobs without a `highway` key.

## Generating indexed PBFs

Use `pbfhogg cat` without a `--type` flag:

```sh
pbfhogg cat input.osm.pbf -o indexed.osm.pbf
```

The passthrough path adds indexdata via decompress+scan without re-compressing blobs. Memory usage is minimal and the file size overhead is under 0.5%.

Indexdata generation timings (commit `69a127f`):

| Dataset | Size | Buffered | `--direct-io` |
|---------|------|----------|---------------|
| Planet | 87 GB | **497s** (8m17s) | 520s (+5%) |
| Denmark | 461 MB | **2.8s** | -- |

Buffered I/O wins for this workload — sequential single-file passthrough benefits from page cache prefetch. `--direct-io` adds alignment overhead without the concurrent read/write pattern that makes it faster for merge.

When `cat` is invoked with a `--type` flag (e.g., `cat -t way`), it also embeds indexdata but does full decode and re-encode of every block.

## Which commands require indexdata?

These commands will error if the input PBF lacks indexdata:

- `apply-changes` — blob classification for merge
- `sort` — blob-level permutation for sorted inputs
- `add-locations-to-ways` — parallel node index building, blob passthrough
- `extract` (complete-ways and smart strategies) — spatial blob filtering
- `tags-filter` — skips blobs lacking required tag keys
- `getid` — skips blobs whose ID range has no intersection with requested IDs
- `cat --type` — skips non-matching blob types entirely
- `inspect tags --type` — type-filtered tag counting
- `inspect --nodes` — node coordinate analysis
- `build-geocode-index` — multi-pass pipeline with type filtering

## The --force flag

Pass `--force` to any command to skip the indexdata check and proceed with a raw (non-indexed) PBF:

```sh
pbfhogg sort raw-input.osm.pbf -o sorted.osm.pbf --force
```

Commands will work but use slower fallback paths — they must decompress every blob to determine its contents instead of reading the blob header metadata.

The recommended workflow is to generate an indexed PBF once with `pbfhogg cat`, then use it for all subsequent operations.

## How it works

The PBF format structures data as a sequence of blobs, each containing a `BlobHeader` followed by compressed data. pbfhogg writes two additional fields into the `BlobHeader`:

1. After writing a `PrimitiveBlock` to a blob, `PbfWriter` scans the serialized bytes to extract element type, ID range, and bounding box, then writes this summary as field 2 of the `BlobHeader`.
2. Tag keys are collected from the block's string table and written as field 4 of the `BlobHeader`.

On the read side, `BlobReader` parses these fields from the `BlobHeader` before touching the compressed blob data. Commands use this metadata to decide whether to decompress a blob, pass it through as raw bytes, or skip it entirely.

## Compatibility

Indexdata and tagdata are transparent extensions. The protobuf wire format specification requires parsers to silently skip unknown fields, so any standard PBF reader (osmium, osm2pgsql, Planetiler, etc.) can read indexed PBFs without modification. No `optional_features` header declaration is added.

PBFs from other tools (without indexdata) are fully supported by pbfhogg — they just don't benefit from the fast classification paths. Blobs without tagdata always pass tag filters (conservative behavior).

## Checking for indexdata

Use `inspect --indexed` to check whether a PBF has blob-level indexdata:

```sh
pbfhogg inspect --indexed denmark.osm.pbf
```

Exit code 0 means indexed, exit code 1 means not indexed. This is useful in scripts to decide whether to run `cat` first.
