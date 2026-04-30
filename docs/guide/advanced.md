# Advanced Topics

## Indexdata

pbfhogg embeds additional metadata in BlobHeader fields: element type, ID range, spatial bounding box, and tag key sets. Standard PBF readers silently skip these fields per protobuf wire format rules.

This metadata enables commands to skip decompression of irrelevant blobs entirely. For example, `apply-changes` classifies blobs in O(1) without decompressing them, passing through ~92% of blobs as raw bytes. `tags-filter` skips blobs that provably lack required tag keys.

Generate an indexed PBF:

```sh
pbfhogg cat input.osm.pbf -o indexed.osm.pbf
```

Without a `--type` flag, `cat` adds indexdata via decompress+scan without re-compressing blobs. Memory usage is minimal. Planet (87 GB) completes in ~8 minutes with under 0.5% file size overhead.

Commands that benefit from indexdata: `apply-changes`, `sort`, `add-locations-to-ways`, `extract` (complete/smart), `tags-filter`, `getid`, `cat --type`, `inspect tags --type`, `inspect --nodes`, and `build-geocode-index`.

## O_DIRECT for planet-scale I/O

Planet-scale operations read and write 80 GB+, polluting the entire page cache and evicting useful data from co-resident processes. The `linux-direct-io` feature bypasses the page cache entirely.

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --direct-io
```

O_DIRECT requires a real filesystem (not tmpfs). Wall time is typically unchanged at country scale (CPU-bound) - the benefit is cache hygiene at planet scale. For sequential single-file passthrough (`cat`), buffered I/O is actually faster because the page cache prefetch helps. `--direct-io` wins for concurrent read/write patterns like merge.

## io_uring writes

The `linux-io-uring` feature replaces the synchronous writer thread with io_uring `WriteFixed` and pre-registered page-aligned buffers. Requires Linux 5.1+ and sufficient `RLIMIT_MEMLOCK` (16 MB for the default 64-buffer pool).

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --io-uring
```

At North America scale (18.8 GB), io_uring + `--compression none` is 20% faster than buffered writes (11.9s vs 14.9s). Below ~4 GB input size, buffered writes keep up - io_uring overhead dominates when the page cache absorbs everything.

## Compression modes

All write commands accept `--compression`:

| Value | Description |
|-------|-------------|
| `none` | No compression. Fastest writes, largest files. Ideal for intermediate files or erofs storage. |
| `zlib` | Zlib level 6 (default). Standard PBF compression, compatible with all tools. |
| `zlib:LEVEL` | Zlib with explicit level (0-9). Higher = smaller + slower. |
| `zstd` | Zstandard level 3. Better ratio and faster decompression than zlib. |
| `zstd:LEVEL` | Zstandard with explicit level. |

With pipelined writes (the production path), compression is dispatched to rayon and all modes converge to the decode + serialization floor. The choice mainly affects file size and downstream read speed.

Zlib uses `zlib-rs` (pure Rust). No C compiler needed.

## Add-locations-to-ways index types

`add-locations-to-ways` embeds node coordinates in ways. It supports two index strategies (plus `auto`) via `--index-type`:

```sh
pbfhogg add-locations-to-ways input.osm.pbf -o output.osm.pbf --index-type external
```

| Type | Memory | Disk | Sorted required | Best for |
|------|--------|------|-----------------|----------|
| `sparse` (default) | ~540 MB + IdSet/rank index | `referenced_count * 8` bytes (japan 2 GB, europe ~29 GB) | no | Small to europe scale; survives europe at ~6 minutes on a 27 GB-RAM host |
| `external` | ~8.7 GB | ~256 GB (planet) | yes + indexdata | Planet-scale, the only mode that survives at planet on memory-constrained hosts |
| `auto` | varies | varies | external if sorted+indexed, else sparse | Recommended default |

**sparse** is a rank-indexed flat mmap array (~8 bytes per referenced node). Builds a referenced-id IdSet in pass 1, then writes locations in pass 2 indexed by rank within that set. Works on any PBF.

**external** uses a rank-bucketed counting sort with parallel stages and bounded memory. Requires sorted input with indexdata. ~256 GB temp disk at planet scale.

A previous `dense` mode (a direct-mapped mmap array indexed by node ID) was removed: the rank-indexed flat sparse layout dominated dense at every measured scale (japan 4.3x faster) and worked in regimes dense did not (europe survives where dense OOMs).

## Multi-extract

Extract multiple regions in a single pass using a JSON config file:

```sh
pbfhogg extract input.osm.pbf -c regions.json
```

The config file defines multiple extract regions, each with a name, output path, and bounding box or polygon. All regions are extracted in one pass over the input, which is much faster than running separate extracts.

Use `-d` to override the output directory for all extracts:

```sh
pbfhogg extract input.osm.pbf -c regions.json -d /data/extracts/
```

## Tags-filter with and without -R

Without `-R` (default mode), `tags-filter` resolves matched relation members transitively: member ways, member nodes, nested member relations are included, and node refs of included ways are pulled in. This requires multiple passes but gives complete, usable output.

With `-R` (omit-referenced), only directly matched elements are emitted. This is a single pass and significantly faster, but the output may have dangling references.

```sh
# Full resolution (default) - complete output
pbfhogg tags-filter denmark.osm.pbf -o highways.osm.pbf "highway=primary"

# Direct matches only - faster but may have dangling refs
pbfhogg tags-filter denmark.osm.pbf -o highways.osm.pbf -R "highway=primary"
```

Tags-filter also supports OSC input (autodetected from extension, or override with `--input-kind osc`). In OSC mode, delete directives are always preserved.

## Build-geocode-index

Build a reverse geocoding index from a PBF file:

```sh
pbfhogg build-geocode-index denmark.osm.pbf --output-dir geocode-index/
```

This runs a 4-pass pipeline: admin boundary relations, referenced node collection, node+way fused scan, and bucketed S2 cell assignment. The output is a set of 19 memory-mappable binary files for fast reverse geocoding queries.

Europe: ~10 minutes, 7.5 GB RSS. Planet: ~22 minutes, 18 GB RSS.

The index can be queried from Rust using the `geocode-reader` feature:

```toml
[dependencies]
pbfhogg = { version = "0.3", default-features = false, features = ["geocode-reader"] }
```
