<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/public/pbfhogg-logo-text-dark.svg">
    <img src="docs/public/pbfhogg-logo-text.svg" width="300" alt="pbfhogg">
  </picture>
  <br>
  <em>Fast OpenStreetMap PBF reader and writer for Rust</em>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/rust-stable-orange?logo=rust" alt="Rust">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License"></a>
</p>

---

Rust library and CLI for reading, writing, and transforming OpenStreetMap PBF files. **The full planet (87 GB, 11.6B elements) processes on a 32 GB machine — every command, bounded memory, no compromises.** See the [Planet scale](#planet-scale) table below for per-command wall time and peak RSS.

Developed on Linux, untested elsewhere. Production-relevant features (O_DIRECT, io_uring) are Linux-only.

Built with LLMs. See [LLM.md](LLM.md).

## Features

- **Read** `.osm.pbf` files sequentially, in parallel (`par_map_reduce`), or with a 3-stage pipelined decoder
- **Write** valid `.osm.pbf` files with `HeaderBuilder`, `BlockBuilder`, and `PbfWriter` — dense node packing, delta encoding, configurable compression (none, zlib, zstd)
- **Blob passthrough** (`write_raw` / `copy_file_range`) for copying unmodified blobs during merge/cat — kernel-space copy eliminates userspace buffer overhead
- **Blob indexdata** — embeds element type + ID range + spatial bbox in BlobHeader for fast merge classification and spatial filtering without decompression
- **Blob tag index** — embeds per-blob tag key metadata in BlobHeader field 4; the pipeline skips decompression of blobs that provably lack required tag keys (e.g. `tags-filter highway=primary` skips all blobs without a `highway` key)
- **Configurable compression** — zlib (default), zstd, or none; zlib-rs for fast pure-Rust decompression and compression (no C dependencies)
- **O_DIRECT I/O** — optional `linux-direct-io` feature bypasses the page cache for planet-scale (80 GB+) reads and writes, preventing cache pollution on the host
- **io_uring writes** — optional `linux-io-uring` feature replaces the synchronous writer thread with io_uring `WriteFixed` and registered buffers for maximum throughput when I/O-bound

## Planet scale

Every pbfhogg CLI command is designed to run on the full planet on normal hardware. Measured on **plantasjen** (AMD Ryzen 9 5900X, 32 GB DDR4, NVMe SSD, Linux 6.18), ~28 GB available memory at run start. Input: 87 GB indexed planet PBF (92 GB raw), 11.6B elements. Peak anon RSS excludes file-backed mmap (kernel-reclaimable page cache), so it represents the actual pressure the process puts on system memory.

Sorted by peak memory — the hardest operations still fit in 32 GB with room to spare.

| Command | Wall | Peak anon RSS | Notes |
|---------|------|---------------|-------|
| `cat --type way` (raw passthrough) | 44s | **10 MB** | zero decompression, indexdata blob filter |
| `getid --invert` | 1m23s | 102 MB | raw-frame passthrough for non-intersecting blobs |
| `cat` (indexdata generation, passthrough) | 8m17s | ~200 MB ‡ | rewrites BlobHeader without re-compressing |
| `tags-filter highway=primary` | 52s | 688 MB | two-pass, parallel classify |
| `getid` | 1m12s | 833 MB | multi-blob ID resolution via indexdata |
| `check --refs` | 20m54s | 1.73 GB | full referential integrity over 11.6B elements |
| `apply-changes` (daily diff, buffered+none) | 8m35s | ~1.8 GB ‡ | 3.4M-change daily diff, 86% rewrite fraction |
| `apply-changes` (daily diff, buffered+zlib) | 12m42s | ~1.8 GB ‡ | same + recompression |
| `extract --smart` (Europe bbox) | 4m39s | 11.17 GB † | three-pass, multipolygon-complete |
| `build-geocode-index` | 22m26s | 14.59 GB | reverse geocoding index, S2 cells |
| `add-locations-to-ways --index-type external` | 24m22s | **16.67 GB** | double-radix permutation, ~112 GB temp disk |
| `renumber --mode external`       | 18m11s | **7.32 GB** | wire-format rewriter (pass 1 + stage 2d splice), parallel pwrite (stage 2c), 256-bucket radix partition |

† Single-sample `--bench 1` measurement with Europe bbox. See [notes/parallel-classify-regression.md](notes/parallel-classify-regression.md) for the investigation that validated the 32 GB host ceiling. \
‡ Older runs without sidecar profiler; peak RSS stated from investigation notes.

Not yet measured on planet, all pending tonight's overnight bench suite: `sort`, `inspect`, `getparents`, `extract --simple` / `--complete`, `multi-extract`, `diff`, `diff --format osc`, `merge-changes` (multi-OSC, 7-file range), and fresh sidecar-profiled runs for `cat` indexdata generation and `apply-changes`. `time-filter` stays unmeasured — it needs a history PBF we don't have. `add-locations-to-ways --index-type dense` is expected to thrash on ≤32 GB hosts (~30+ GB mmap working set) — use `external` instead. **`renumber --mode inmem`** (the default) remains **not planet-safe** — the in-memory `FxHashMap` architecture requires ~278 GB at planet scale. Users needing planet-scale renumber should pass `--mode external`.

Full commit hashes, sidecar UUIDs, and phase breakdowns are in [reference/performance.md](reference/performance.md).

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
bb.add_node(1, 556_761_000, 125_683_000, [("name", "Copenhagen")], None);
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
pbfhogg build-geocode-index <f> --output-dir <d>  Build reverse geocoding index (S2 cells, mmap-ready)
```

Inspect reports block breakdown by type (DenseNodes/Ways/Relations/Mixed) with compressed sizes, element counts with tagged node count, and **ordering analysis** — whether the file follows the standard nodes → ways → relations layout or has non-standard interleaving (with block ranges). On indexed PBFs, inspect uses an **index-only fast path** that reads only blob headers and skips all decompression — completing in ~36ms on a 473 MB file vs ~4s for full decode (109x speedup). Falls back to full decode on non-indexed PBFs or when `--locations` is requested. Optional flags: `--blocks` shows per-type distribution stats (min/max/median/p99 for elements-per-block and compressed size) plus a full per-block table; `--blocks=N` limits the table to the first and last N blocks; `--id-ranges` shows min/max element IDs per type with monotonicity checks; `--locations` reports locations-on-ways diagnostics (coverage percentage, coords-per-way percentiles); `--extended` runs a full scan for timestamp ranges, data bbox, metadata coverage, and ordering.

Extract supports three strategies: `--simple` (single pass, fast, may have dangling refs), complete-ways (default, two passes, all way nodes included), and `--smart` (three passes, completes multipolygon/boundary relations — all member ways and their nodes are included even if outside the region). Multi-extract mode (`--config`) reads a JSON file defining multiple extract regions and writes them in a single pass.

Tags-filter uses OR semantics across expressions. In default mode (without `-R`), it resolves matched relation members transitively: member ways, member nodes, and nested member relations are included (with cycle-safe recursion), and node refs of included ways are pulled in. With `-R`, only directly matched elements are emitted. Supports both PBF and OSC input (autodetected from content or overridden with `--input-kind osc`); in OSC mode, deletes are always preserved.

Apply-changes with `--locations-on-ways` preserves and updates inline way-node coordinates through OSC diffs, eliminating the need to re-run `add-locations-to-ways` after each merge. Requires a sorted base PBF with `LocationsOnWays` (bootstrap with `add-locations-to-ways` once). Surviving base ways forward existing coordinates as raw bytes; OSC ways look up node coordinates from a sparse index built from the diff and base PBF.

Add-locations-to-ways supports three index strategies via `--index-type`: `dense` (default) uses a file-backed mmap index (8 bytes/slot, direct addressing by node ID) — fastest when the working set fits in RAM. `sparse` uses a Planetiler-inspired chunk-indexed array with batched sorted lookups — ~540 MB RAM + compact on-disk values file. `external` uses a double radix permutation with bounded memory and all sequential I/O — best for memory-constrained hosts where dense thrashes. Requires sorted PBF input and ~300 GB temp disk at planet scale. Planet (87 GB): 24 min, 17 GB peak anon, 3.9x faster than dense (96 min).

Getid supports `--add-referenced` to include way node refs (two-pass), `--id-file` / `--id-osm-file` to read IDs from files, and `--default-type` for bare numeric IDs. `--invert` reverses the selection (remove listed IDs, keep everything else).

**Input assumption:** pbfhogg assumes canonical OSM snapshot data (Geofabrik extracts or planet files) with unique element IDs. Custom or malformed PBFs with duplicate IDs are not validated and may produce incorrect output. In particular, `add-locations-to-ways` indexes nodes by ID — duplicate node IDs will silently overwrite earlier coordinates with later ones.

Commands that benefit from blob-level indexdata (`apply-changes`, `sort`, `add-locations-to-ways`, `extract` complete/smart, `tags-filter`, `getid`, `cat --type`, `inspect tags --type`, `inspect --nodes`, `build-geocode-index`) will error if the input PBF lacks indexdata. Pass `--force` to proceed anyway (slower). Generate an indexed PBF with `pbfhogg cat input.osm.pbf -o indexed.osm.pbf` — the passthrough path adds indexdata automatically without re-compressing blobs, using minimal memory. The `--type` filtered path also embeds indexdata but does full decode and re-encode.

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

CLI commands — Denmark (487 MB, 59M elements, commit `6fc1283`, osmium from `23862d1`):

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| inspect (indexdata) | **0.1s** | — | index-only fast path |
| sort (sorted, indexdata) | **0.7s** | 11.6s | **17x** |
| apply-changes (indexdata + zlib) | **0.6s** | 7.2s | **12x** |
| tags-filter w/highway=primary -R | **0.2s** | 0.56s | **2.8x** |
| tags-filter amenity=restaurant -R | **0.5s** | 1.19s | **2.4x** |
| cat --type way (raw passthrough) | **0.24s** | 2.22s | **9.3x** |
| inspect tags --type way (indexdata) | **0.4s** | 0.59s | **1.5x** |
| getid (9 elements) | **0.6s** | 0.83s | **1.4x** |
| add-locations-to-ways (dense) | **9.9s** | 12.1s | **1.2x** |
| add-locations-to-ways (external) | **9.7s** | 12.1s | **1.2x** |

Filter commands use parallel element processing — each rayon thread owns a `BlockBuilder` and processes decoded blocks in parallel, then results are written sequentially. `cat --type` uses raw frame passthrough for matching blobs (zero decompression) and skips non-matching blobs entirely via indexdata. `getid --invert` passes through blobs whose ID range has no intersection with requested IDs as raw frames. `getid` (include mode) skips decompression of blobs whose ID range doesn't intersect the requested IDs. PBFs with blob-level indexdata skip decompression of irrelevant blob types; PBFs with tagdata additionally skip blobs that provably lack required tag keys. Apply-changes uses blob passthrough (zero decode for unmodified blobs). Sort uses streaming sweep merge — for sorted inputs with indexdata, blobs pass through as raw bytes; unsorted inputs use blob-level permutation. add-locations-to-ways uses parallel node index building (batch-and-dispatch to rayon) and blob passthrough for unchanged node/relation blobs on indexed PBFs.

Extract — Japan (2.4 GB, 344M elements, Tokyo bbox, commit `cadc3e6`):

| Strategy | pbfhogg | osmium | ratio |
|----------|---------|--------|-------|
| simple | **3.8s** | 7.2s | **1.9x faster** |
| complete-ways | **3.7s** | 11.0s | **3.0x faster** |
| smart | **4.7s** | 13.4s | **2.9x faster** |

Simple extract uses a 3-phase barrier pipeline with parallel classification and raw frame passthrough — each phase (nodes, ways, relations) classifies blobs in parallel then writes matching raw frames via pread workers. No decode+re-encode for matching blobs. Complete-ways and smart pass 1 uses three-phase parallel pread classification (nodes, ways, relations) via a reusable `parallel_classify_phase` helper. Smart pass 2 (way dependency resolution) also uses `parallel_classify_phase`, replacing the old sequential BlobReader scan. Pass 2 write (and pass 3 for smart) uses pread-from-workers write passes with full PrimitiveBlock lifecycle per worker. Spatial blob filtering skips decompression of node blobs outside the extract region when indexdata is present.

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

All CLI commands are cross-validated against osmium on Denmark (`brokkr verify`). cat, tags-filter, add-locations-to-ways, and getid produce byte-identical output. diff --format osc produces a correct roundtrip (apply derived OSC back to old = new, 59.1M elements identical) while osmium's derived OSC loses 1243 delete directives — root cause traced to an upstream bug in `osmium-tool/src/command_derive_changes.cpp:184`, where the dual-iterator merge walk compares raw `int64` IDs (`it2->id() != it1->id()`) without type discrimination, silently dropping any deleted object whose numeric ID happens to coincide with a different-type object at the merge-walk boundary between type sections. The fix is comparing `(type, id)` tuples instead. extract has expected differences in relation inclusion criteria across all three strategies (99.99% node/way match; smart: pbfhogg includes more way-referenced nodes, osmium includes more relations). diff has a 14-element discrepancy out of 59.1M due to different version comparison semantics. renumber has a 306-element discrepancy out of 59.1M (all in relation member lists) due to different orphan-reference handling: pbfhogg preserves the old id for refs to objects not in the input, while osmium assigns new sequential ids past the last valid range — see [DEVIATIONS.md](DEVIATIONS.md).

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

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
