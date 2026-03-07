# pbfhogg

Rust library and CLI for reading, writing, and transforming OpenStreetMap PBF files. Designed for planet-scale operations on normal hardware.

Applying a daily diff to an 18.8 GB North America extract (2.58 billion elements, 645K changes) takes 12 seconds and uses under 600 MB of RAM. 92% of blobs pass through as raw bytes — no decompression, no re-encoding. The same pipeline targets full planet files (~80 GB) on a 32 GB machine — validation in progress.

Developed on Linux, untested elsewhere. Production-relevant features (O_DIRECT, io_uring) are Linux-only.

## Features

- **Read** `.osm.pbf` files sequentially, in parallel (`par_map_reduce`), or with a 3-stage pipelined decoder
- **Write** valid `.osm.pbf` files with `HeaderBuilder`, `BlockBuilder`, and `PbfWriter` — dense node packing, delta encoding, configurable compression (none, zlib, zstd)
- **Blob passthrough** (`write_raw` / `copy_file_range`) for copying unmodified blobs during merge/cat — kernel-space copy eliminates userspace buffer overhead
- **Blob indexdata** — embeds element type + ID range + spatial bbox in BlobHeader for fast merge classification and spatial filtering without decompression
- **Blob tag index** — embeds per-blob tag key metadata in BlobHeader field 4; the pipeline skips decompression of blobs that provably lack required tag keys (e.g. `tags-filter highway=primary` skips all blobs without a `highway` key)
- **Configurable compression** — zlib (default), zstd, or none; zlib-rs for fast pure-Rust decompression and compression (no C dependencies)
- **O_DIRECT I/O** — optional `linux-direct-io` feature bypasses the page cache for planet-scale (80 GB+) reads and writes, preventing cache pollution on the host
- **io_uring writes** — optional `linux-io-uring` feature replaces the synchronous writer thread with io_uring `WriteFixed` and registered buffers for maximum throughput when I/O-bound

## Usage

```toml
[dependencies]
pbfhogg = "0.2"
```

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("input.osm.pbf")?;

// Check if the PBF declares sorted elements
if reader.header().is_sorted() {
    println!("PBF is sorted by type then ID");
}

reader.for_each(|element| {
    if let Element::Way(way) = element {
        // process way
    }
})?;
# Ok::<(), std::io::Error>(())
```

### Writing

```rust
use pbfhogg::write::block_builder::{HeaderBuilder, BlockBuilder};
use pbfhogg::write::writer::{PbfWriter, Compression};

// Build a sorted header with bounding box
let header_bytes = HeaderBuilder::new()
    .bbox(9.0, 54.0, 13.0, 58.0)
    .sorted()
    .build()?;
let mut writer = PbfWriter::to_path("output.osm.pbf".as_ref(), Compression::default(), &header_bytes)?;

// Add elements via BlockBuilder
let mut bb = BlockBuilder::new();
bb.add_node(1, 556_761_000, 125_683_000, &[("name", "Copenhagen")], None);
if let Some(bytes) = bb.take()? {
    writer.write_primitive_block(bytes)?;
}
writer.flush()?;
# Ok::<(), std::io::Error>(())
```

`HeaderBuilder::from_header(&existing_header)` copies bbox and replication metadata from an existing PBF header — useful for commands that transform data while preserving metadata.

## Read modes

| Method | Order | Sorted guarantee | Use case |
|--------|-------|-----------------|----------|
| `for_each` | File order | Yes — nodes arrive in ascending ID order when `header().is_sorted()` is `true` | Sequential processing, order-dependent consumers |
| `for_each_pipelined` | File order | Yes — same guarantee, with parallel decompression overlapping I/O | Fastest ordered read (production hot path) |
| `for_each_block_pipelined` | File order | Yes — blocks arrive in file order, consumer iterates elements | Consumers that need parallel processing per block (owned `PrimitiveBlock`) |
| `into_blocks_pipelined` | File order | Yes — same as above, but returns an `Iterator` | Loop control: early exit, zipping two files, interleaving work |
| `par_map_reduce` | Arbitrary | No — elements are distributed across rayon workers in unspecified order | Aggregation (counts, statistics) where order doesn't matter |

`for_each_block_pipelined` and `into_blocks_pipelined` deliver owned `PrimitiveBlock`s instead of individual elements. The consumer can send blocks to other threads for parallel processing, enabling overlapped I/O + decode + consumer parallelism without blocking the pipeline. `into_blocks_pipelined` runs the pipeline in a background thread and requires `R: 'static` (`ElementReader<FileReader>` satisfies this).

In debug builds, `for_each` and `for_each_pipelined` assert that node IDs are monotonically increasing when the sorted flag is set.

These methods are the `ElementReader` API for library consumers. The CLI commands mostly use `BlobReader` directly for blob-level operations (raw passthrough, file seeking, multi-pass scanning) where element-level decoding is unnecessary.

## CLI

pbfhogg includes a command-line toolkit for common OSM PBF operations:

```
pbfhogg inspect <file>                    Comprehensive file inspection (blocks, ordering, tagged counts)
pbfhogg inspect --indexed <file>          Check if PBF has blob-level indexdata (exit code 0/1)
pbfhogg inspect --nodes <file>            Analyze node coordinate statistics for FOR compression sizing
pbfhogg inspect tags <file>               Count tag key=value frequencies
pbfhogg check <file> --ids                Validate ID uniqueness and ordering
pbfhogg check <file> --refs               Validate referential integrity
pbfhogg cat <files...> -o <out>           Concatenate PBF files (-t node,way,relation to filter)
pbfhogg sort <file> -o <out>              Sort into standard order (nodes → ways → relations, by ID)
pbfhogg renumber <file> -o <out>          Renumber all element IDs sequentially, remapping cross-references
pbfhogg extract <file> -o <out> -b <bbox> Extract by bounding box (minlon,minlat,maxlon,maxlat)
pbfhogg extract <file> -o <out> -p <geo>  Extract by GeoJSON polygon
pbfhogg extract <file> -c <config>        Multi-extract from JSON config file
pbfhogg add-locations-to-ways <f> -o <o>  Embed node coordinates in ways
pbfhogg apply-changes <base> <osc> -o <o> Apply OSC diff to a sorted PBF file (--locations-on-ways)
pbfhogg merge-changes <oscs...> -o <out>  Merge multiple OSC files into one (--simplify to dedup)
pbfhogg diff <old> <new>                  Compare two PBFs by content equality (-v verbose, -c hide common)
pbfhogg diff <old> <new> --format osc     Generate OSC diff from two PBF snapshots
pbfhogg tags-filter <file> -o <out> <exp> Filter elements by tag expressions (PBF or OSC input)
pbfhogg getid <file> -o <out> <ids>       Extract elements by ID (e.g. n123 w456 r789)
pbfhogg getid <file> -o <out> --invert    Remove elements by ID
pbfhogg getparents <file> -o <out> <ids>  Find ways/relations referencing given IDs (reverse lookup)
pbfhogg time-filter <file> -o <out> <ts>  Filter history PBF to a snapshot at a timestamp
```

Inspect reports block breakdown by type (DenseNodes/Ways/Relations/Mixed) with compressed sizes, element counts with tagged node count, and **ordering analysis** — whether the file follows the standard nodes → ways → relations layout or has non-standard interleaving (with block ranges). On indexed PBFs, inspect uses an **index-only fast path** that reads only blob headers and skips all decompression — completing in ~36ms on a 473 MB file vs ~4s for full decode (109x speedup). Falls back to full decode on non-indexed PBFs or when `--locations` is requested. Optional flags: `--blocks` shows per-type distribution stats (min/max/median/p99 for elements-per-block and compressed size) plus a full per-block table; `--blocks=N` limits the table to the first and last N blocks; `--id-ranges` shows min/max element IDs per type with monotonicity checks; `--locations` reports locations-on-ways diagnostics (coverage percentage, coords-per-way percentiles); `--extended` runs a full scan for timestamp ranges, data bbox, metadata coverage, and ordering.

Extract supports three strategies: `--simple` (single pass, fast, may have dangling refs), complete-ways (default, two passes, all way nodes included), and `--smart` (three passes, completes multipolygon/boundary relations — all member ways and their nodes are included even if outside the region). Multi-extract mode (`--config`) reads a JSON file defining multiple extract regions and writes them in a single pass.

Tags-filter uses OR semantics across expressions. In default mode (without `-R`), it resolves matched relation members transitively: member ways, member nodes, and nested member relations are included (with cycle-safe recursion), and node refs of included ways are pulled in. With `-R`, only directly matched elements are emitted. Supports both PBF and OSC input (autodetected from content or overridden with `--input-kind osc`); in OSC mode, deletes are always preserved.

Apply-changes with `--locations-on-ways` preserves and updates inline way-node coordinates through OSC diffs, eliminating the need to re-run `add-locations-to-ways` after each merge. Requires a sorted base PBF with `LocationsOnWays` (bootstrap with `add-locations-to-ways` once). Surviving base ways forward existing coordinates as raw bytes; OSC ways look up node coordinates from a sparse index built from the diff and base PBF.

Add-locations-to-ways uses a file-backed mmap index (8 bytes/slot, direct addressing by node ID). The index is backed by a temporary file that the kernel pages in/out under memory pressure, so it works from country-scale to planet-scale without OOM.

Getid supports `--add-referenced` to include way node refs (two-pass), `--id-file` / `--id-osm-file` to read IDs from files, and `--default-type` for bare numeric IDs. `--invert` reverses the selection (remove listed IDs, keep everything else).

**Input assumption:** pbfhogg assumes canonical OSM snapshot data (Geofabrik extracts or planet files) with unique element IDs. Custom or malformed PBFs with duplicate IDs are not validated and may produce incorrect output. In particular, `add-locations-to-ways` indexes nodes by ID — duplicate node IDs will silently overwrite earlier coordinates with later ones.

Commands that benefit from blob-level indexdata (`apply-changes`, `sort`, `add-locations-to-ways`, `extract` complete/smart, `tags-filter`, `getid`, `cat --type`, `inspect tags --type`, `inspect --nodes`) will error if the input PBF lacks indexdata. Pass `--force` to proceed anyway (slower). Generate an indexed PBF with `pbfhogg cat input.osm.pbf -o indexed.osm.pbf` — the passthrough path adds indexdata automatically without re-compressing blobs, using minimal memory. The `--type` filtered path also embeds indexdata but does full decode and re-encode.

Indexdata generation via passthrough cat (commit `69a127f`):

| Dataset | Size | Buffered | `--direct-io` | Overhead |
|---------|------|----------|---------------|----------|
| Planet | 87 GB | **497s** (8m17s) | 520s (+5%) | +0.5% file size |
| Denmark | 461 MB | **2.8s** | — | — |

Buffered I/O wins here — sequential single-file passthrough benefits from page cache prefetch. `--direct-io` adds alignment overhead without the concurrent read/write pattern that makes it faster for merge.

All write commands accept `--compression` to control blob compression: `none`, `zlib` (default), `zstd`, or with explicit level (`zlib:9`, `zstd:19`).

## Performance

Read throughput — count all 59M elements in Denmark extract (461 MB), best of 3 runs, fat LTO (commit `90df51f`):

<!-- BENCH:START -->
| Tool | Mode | Time | Notes |
|------|------|------|-------|
| **pbfhogg** | parallel | **0.31s** | `par_map_reduce` on all cores |
| osmpbf 0.3 | parallel | 0.53s | upstream crate, same API |
| **pbfhogg** | pipelined | **1.3s** | `for_each_pipelined`, preserves file order |
| Planetiler 0.10 | parallel | 2.0s | Java, `OsmInputFile` + thread pool |
| **pbfhogg** | sequential | 2.8s | `for_each` |
| **pbfhogg** | blobreader | 2.9s | `BlobReader` sequential decode |
| osmpbf 0.3 | sequential | 5.6s | upstream `for_each` |
| osmium 1.19 | cat → opl | 5.7s | `osmium cat -f opl -o /dev/null` |
| Planetiler 0.10 | sequential | 8.7s | Java, `OsmInputFile` single-threaded |
<!-- BENCH:END -->

Write throughput — decode all 59M elements then write through `BlockBuilder` + `PbfWriter` to `/dev/null` (commit `def80d9`):

| Compression | Sync | Pipelined | Notes |
|-------------|------|-----------|-------|
| none | 6.2s | 6.2s | decode + wire-format serialization floor |
| zstd:3 | 8.1s | **6.2s** | pipelined hides compression cost |
| zlib:6 | 14.5s | **6.3s** | 2.3x speedup from parallel compression |

With pipelined writes, all compression modes converge to ~6.2s — the decode + wire-format serialization floor. All element types are encoded directly to protobuf wire format using reusable scratch buffers (no per-element allocation, no external protobuf dependencies). `Compression::None` on erofs is the target production config.

CLI commands — Denmark (487 MB, 59M elements, commit `23862d1`, add-locations `46f7388`, inspect `fc76dfb`):

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| inspect (indexdata) | **0.036s** | — | **109x** vs full decode |
| sort (sorted, indexdata) | **0.14s** | 11.6s | **83x** |
| apply-changes (indexdata + zlib) | **2.7s** | 7.2s | **2.7x** |
| tags-filter w/highway=primary -R | **0.24s** | 0.56s | **2.3x** |
| tags-filter amenity=restaurant -R | **0.58s** | 1.19s | **2.1x** |
| cat --type way (indexdata) | **1.1s** | 2.22s | **2.0x** |
| inspect tags --type way (indexdata) | **0.34s** | 0.59s | **1.7x** |
| getid (9 elements) | **0.53s** | 0.83s | **1.6x** |
| add-locations-to-ways | **6.5s** | 12.1s | **1.9x** |

Filter commands (cat, tags-filter, inspect tags, getid) use parallel element processing — each rayon thread owns a `BlockBuilder` and processes decoded blocks in parallel, then results are written sequentially. PBFs with blob-level indexdata skip decompression of irrelevant blob types; PBFs with tagdata additionally skip blobs that provably lack required tag keys. Apply-changes uses blob passthrough (zero decode for unmodified blobs). Sort uses streaming sweep merge — for sorted inputs with indexdata, blobs pass through as raw bytes; unsorted inputs use blob-level permutation. add-locations-to-ways uses parallel node index building (batch-and-dispatch to rayon) and blob passthrough for unchanged node/relation blobs on indexed PBFs.

Extract — Japan (2.4 GB, 344M elements, Tokyo bbox, commit `23862d1`):

| Strategy | pbfhogg | osmium | ratio |
|----------|---------|--------|-------|
| simple | 12.9s | **7.2s** | 1.79x |
| complete-ways | 13.3s | **11.0s** | 1.21x |
| smart | 14.9s | **13.4s** | 1.11x |

Extract uses pipelined parallel decoding with metadata skipping in scan-only passes. Smart Pass 2 (way dependency resolution) iterates only way groups, skipping all node and relation blocks. Complete-ways and smart are within 10-16% of osmium. Simple uses a single-pass fast path on sorted inputs (`Sort.Type_then_ID`) — classify and write in one file scan — and falls back to two passes on unsorted inputs. Spatial blob filtering skips decompression of node blobs outside the extract region when indexdata is present.

Apply-changes with `--locations-on-ways` — Denmark (501 MB with LocationsOnWays, daily diff, commit `e7bbfa2`):

| Pipeline | pbfhogg | osmium | speedup |
|----------|---------|--------|---------|
| apply-changes `--locations-on-ways` | **3.9s** | 8.3s | **2.1x** |
| apply-changes + ALTW (separate) | 2.7s + 6.5s = 9.2s | 4.3s + 9.5s = 13.8s | — |

The `--locations-on-ways` flag replaces a two-step pipeline (apply-changes then add-locations-to-ways) with a single command. Surviving base ways forward raw coordinate bytes without decode; OSC ways look up node coordinates from a sparse index. Zero overhead when the flag is off.

Apply-changes at scale — single-pass 4-phase batch pipeline with O(log n) inline upsert assignment, reader thread read-ahead, and passthrough coalescing (commit `a6ebbfe`):

| Dataset | Config | Time | vs osmium |
|---------|--------|------|-----------|
| Japan (2.4 GB, 43K diff) | indexdata + zlib | **3.0s** | **15x** faster |
| Germany (4.5 GB, 146K diff) | buffered + zlib | **5.3s** | — |
| Germany (4.5 GB, 146K diff) | buffered + none | **3.4s** | — |
| N. America (18.8 GB, 645K diff) | buffered + zlib | **17.3s** | — |
| N. America (18.8 GB, 645K diff) | buffered + none | **14.9s** | — |
| N. America (18.8 GB, 645K diff) | io_uring + zlib | **15.2s** | — |
| N. America (18.8 GB, 645K diff) | io_uring + none | **11.9s** | — |

At Japan scale, osmium takes 36.6s for the same operation (9-15x slower) because it decodes and re-encodes every element. pbfhogg passes ~92% of blobs through as raw bytes without decompression, using blob-level indexdata for O(1) classification. The single-pass pipeline overlaps reader I/O, parallel classification, parallel rewrite of touched blobs, and pipelined writes — passthrough blobs are coalesced into large buffers and moved into the writer channel with zero copy. Adaptive in-flight memory bounding keeps RSS under 600 MB even at North America scale (18.8 GB).

All CLI commands are cross-validated against osmium on Denmark (`brokkr verify`). cat, tags-filter, add-locations-to-ways, and getid produce byte-identical output. diff --format osc produces a correct roundtrip (apply derived OSC back to old = new, 59.1M elements identical) while osmium's derived OSC loses 1243 delete directives. extract has expected differences in relation inclusion criteria across all three strategies (99.99% node/way match; smart: pbfhogg includes more way-referenced nodes, osmium includes more relations). diff has a 14-element discrepancy out of 59.1M due to different version comparison semantics.

System: plantasjen — AMD Ryzen 9 5900X (12c/24t), 32 GB DDR4, NVMe SSD (input/output) + HDD (build artifacts), Linux 6.18.

Measured with `brokkr bench`. Cross-validated with `brokkr verify`.

## O_DIRECT I/O

Planet-scale operations read and write 80 GB+, polluting the entire page cache and evicting useful data from co-resident processes. The `linux-direct-io` feature adds O_DIRECT read and write paths that bypass the page cache entirely.

```toml
[dependencies]
pbfhogg = { version = "0.2", features = ["linux-direct-io"] }
```

CLI usage (all commands support `--direct-io`):

```
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --direct-io
pbfhogg extract input.osm.pbf -o output.osm.pbf -b 12.4,55.6,12.7,55.8 --direct-io
pbfhogg sort input.osm.pbf -o output.osm.pbf --direct-io
```

Library usage:

```rust
use pbfhogg::write::writer::{PbfWriter, Compression};
use pbfhogg::{BlobReader, ElementReader};

// O_DIRECT reads
let reader = BlobReader::open("input.osm.pbf", true)?;
let reader = ElementReader::open("input.osm.pbf", true)?;

// O_DIRECT writes (parallel compression)
let writer = PbfWriter::to_path_direct(path, compression, &header_bytes)?;
```

O_DIRECT requires a real filesystem (not tmpfs). Wall time is unchanged at country scale (CPU-bound on zlib compression) — the benefit is cache hygiene at planet scale.

## io_uring I/O

The `linux-io-uring` feature replaces the synchronous writer thread with an io_uring submission loop using `WriteFixed` and pre-registered page-aligned buffers.

```toml
[dependencies]
pbfhogg = { version = "0.2", features = ["linux-io-uring"] }
```

CLI usage:

```
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --io-uring
```

Library usage:

```rust
use pbfhogg::write::writer::{PbfWriter, Compression};

let writer = PbfWriter::to_path_uring(path, Compression::None, &header_bytes)?;
```

Requires Linux 5.1+ and sufficient `RLIMIT_MEMLOCK` (16 MB for the default 64-buffer pool). If the limit is too low, the error message will suggest `ulimit -l unlimited`.

At North America scale (18.8 GB), io_uring + `Compression::None` is 20% faster than buffered writes (11.9s vs 14.9s). At country scale (Denmark 465 MB, Japan 2.4 GB), buffered writes keep up — io_uring overhead dominates when the page cache absorbs everything. The crossover is around 4-5 GB input size.

## Compression

All write commands support `--compression` to control blob compression:

```
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --compression none
pbfhogg sort input.osm.pbf -o output.osm.pbf --compression zstd
pbfhogg cat input.osm.pbf -o output.osm.pbf --compression zlib:9
```

| Value | Description |
|-------|-------------|
| `none` | No compression. Fastest writes, largest files. Ideal for intermediate files or erofs storage (where the filesystem handles compression). |
| `zlib` | Zlib level 6 (default). Standard PBF compression, compatible with all tools. |
| `zlib:LEVEL` | Zlib with explicit level (0-9). Higher = smaller + slower. |
| `zstd` | Zstandard level 3. Better compression ratio and faster decompression than zlib. |
| `zstd:LEVEL` | Zstandard with explicit level. |

## BlobHeader extensions

pbfhogg embeds additional metadata in BlobHeader fields that standard PBF readers silently skip (per protobuf wire format rules for unknown fields).

**Field 2 (indexdata)**: 42-byte fixed-size blob containing element type (`ElemKind`), ID range (min/max), and spatial bounding box (decimicrodegree `i32` coordinates). Used by apply-changes and sort for O(1) blob classification without decompression.

**Field 4 (tagdata)**: Variable-length blob containing the set of unique tag key strings present in the blob. Wire format: version byte (`0x01`) + key count (`u16` LE) + repeated `[key_len (u16 LE) + key_bytes]`. Used by `tags-filter` and any filtered read to skip decompression of blobs that provably lack required tag keys. Blobs without tagdata (files from other tools) always pass the filter (conservative).

Both fields are written automatically by `PbfWriter` and preserved during sort/apply-changes passthrough. No `optional_features` header declaration is added — these are transparent extensions.

## Correctness

See [CORRECTNESS.md](CORRECTNESS.md) for parser/encoder edge cases and data representation limits accepted by design, and [DEVIATIONS.md](DEVIATIONS.md) for intentional behavioral differences from osmium.

## Acknowledgements

pbfhogg started as a fork of [osmpbf](https://github.com/b-r-u/osmpbf/) by Thomas Brüggemann, which provided the foundation for PBF reading in Rust. The write path, pipelined decoder, blob passthrough, and CLI were built on top of that foundation.

[osmium-tool](https://osmcode.org/osmium-tool/) and [libosmium](https://osmcode.org/libosmium/) by Jochen Topf are the reference implementation for OSM PBF tooling. pbfhogg's CLI commands were designed to cover the same use cases, and all output is cross-validated against osmium. The osmium documentation and source code were invaluable for understanding PBF semantics, edge cases in extract strategies, and the OSC change file format.

## License

Licensed under the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0).
