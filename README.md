<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/public/pbfhogg-logo-text-dark.svg">
    <img src="docs/public/pbfhogg-logo-text.svg" width="300" alt="pbfhogg">
  </picture>
  <br>
  <em>Fast OpenStreetMap PBF reader and writer for Rust</em>
</p>

<p align="center">
  <a href="https://crates.io/crates/pbfhogg"><img src="https://img.shields.io/crates/v/pbfhogg" alt="crates.io"></a>
  <img src="https://img.shields.io/badge/rust-1.87+-orange?logo=rust" alt="MSRV 1.87">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License"></a>
</p>

---

Rust library and CLI for reading, writing, and transforming OpenStreetMap PBF files. **The full planet (87 GB, 11.6B elements) processes on a 32 GB machine - every command, bounded memory, no compromises.**

Developed on Linux, untested elsewhere.

Built with LLMs. See [LLM.md](LLM.md).

## Features

- **Read** `.osm.pbf` files sequentially, in parallel (`par_map_reduce`), or with a 3-stage pipelined decoder
- **Write** valid `.osm.pbf` files with `HeaderBuilder`, `BlockBuilder`, and `PbfWriter` - dense node packing, delta encoding, configurable compression (none, zlib, zstd)
- **Blob passthrough** for copying unmodified blobs during merge/cat - kernel-space copy eliminates userspace buffer overhead
- **Blob indexdata** - embeds element type + ID range + spatial bbox in BlobHeader for O(1) blob classification without decompression
- **Blob tag index** - per-blob tag key metadata enables skipping decompression of blobs that provably lack required tag keys
- **O_DIRECT I/O** - optional `linux-direct-io` feature bypasses the page cache for planet-scale reads and writes
- **io_uring writes** - optional `linux-io-uring` feature for maximum throughput when I/O-bound

## Planet scale

Every command runs on the full planet on normal hardware. Measured on **plantasjen** (AMD Ryzen 9 5900X, 32 GB DDR4, NVMe SSD), ~28 GB available. Input: 87 GB indexed planet PBF, 11.6B elements.

| Command | Wall | Peak anon RSS | Notes |
|---------|------|---------------|-------|
| `cat --type way` (raw passthrough) | 44s | **10 MB** | zero decompression, indexdata blob filter |
| `getid --invert` | 1m23s | 102 MB | raw-frame passthrough for non-intersecting blobs |
| `cat` (indexdata generation) | 8m17s | ~200 MB | rewrites BlobHeader without re-compressing |
| `tags-filter -R highway=primary` | 52s | 688 MB | single-pass (`--omit-referenced`), parallel classify |
| `getid` | 1m6s | 833 MB | multi-blob ID resolution via indexdata |
| `check --refs` | **1m12s** | **2.17 GB** | referential integrity over 11.6B elements |
| `check --ids --full` | **1m33s** | **2.22 GB** | monotonicity + type-order + per-type duplicate detection over 11.6B elements |
| `apply-changes` (daily diff, zlib) | 12m33s | ~1.8 GB | 3.4M-change daily diff, 86% rewrite |
| `renumber` | 3m14s | **3.3 GB** | wire-format rewriters, shared atomic IdSetDense |
| `extract --smart` (Europe bbox) | 4m42s | 11.17 GB | three-pass, multipolygon-complete |
| `build-geocode-index` | 20m55s | 29.5 GB | reverse geocoding index, S2 cells (pass-1.5 transient peak) |
| `add-locations-to-ways --index-type external` | 11m38s | 17.2 GB | rank-bucketed counting sort → per-blob delta-varint coord payloads, ~246 GB temp disk |

Per-command phase breakdowns and optimization history are in [reference/performance.md](reference/performance.md). Note that recorded results always track the latest git head and may not match the released version.

## Usage

```toml
[dependencies]
pbfhogg = "0.2"
```

```rust
use pbfhogg::{ElementReader, Element};

let reader = ElementReader::from_path("input.osm.pbf")?;

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

let header_bytes = HeaderBuilder::new()
    .bbox(9.0, 54.0, 13.0, 58.0)
    .sorted()
    .build()?;
let mut writer = PbfWriter::to_path("output.osm.pbf".as_ref(), Compression::default(), &header_bytes)?;

let mut bb = BlockBuilder::new();
bb.add_node(1, 556_761_000, 125_683_000, [("name", "Copenhagen")], None);
if let Some(bytes) = bb.take()? {
    writer.write_primitive_block(bytes)?;
}
writer.flush()?;
# Ok::<(), std::io::Error>(())
```

### Read modes

| Method | Order | Use case |
|--------|-------|----------|
| `for_each` | File order | Sequential processing, order-dependent consumers |
| `for_each_pipelined` | File order | Fastest ordered read (parallel decompression) |
| `for_each_block_pipelined` | File order | Consumers that need owned `PrimitiveBlock` for parallel processing |
| `into_blocks_pipelined` | File order | Loop control: early exit, zipping two files |
| `par_map_reduce` | Arbitrary | Aggregation where order doesn't matter |

## CLI

```
pbfhogg inspect <file>                    File inspection (blocks, ordering, counts)
pbfhogg inspect --indexed <file>          Check if PBF has indexdata (exit code 0/1)
pbfhogg inspect tags <file>               Tag key=value frequencies
pbfhogg check <file> --ids                Validate ID uniqueness and ordering
pbfhogg check <file> --refs               Validate referential integrity
pbfhogg cat <files...> -o <out>           Concatenate PBFs (-t node,way,relation to filter)
pbfhogg sort <file> -o <out>              Sort into standard order (nodes, ways, relations by ID)
pbfhogg renumber <file> -o <out>          Renumber all IDs sequentially, remap cross-references
pbfhogg extract <file> -o <out> -b <bbox> Extract by bounding box
pbfhogg extract <file> -o <out> -p <geo>  Extract by GeoJSON polygon
pbfhogg extract <file> -c <config>        Multi-extract from JSON config
pbfhogg add-locations-to-ways <f> -o <o>  Embed node coordinates in ways
pbfhogg apply-changes <base> <osc> -o <o> Apply OSC diff (--locations-on-ways)
pbfhogg merge-changes <oscs...> -o <out>  Merge multiple OSC files (--simplify)
pbfhogg diff <old> <new>                  Compare two PBFs (-v verbose, --format osc)
pbfhogg tags-filter <file> -o <out> <exp> Filter by tag expressions (PBF or OSC input)
pbfhogg getid <file> -o <out> <ids>       Extract elements by ID (--invert to remove)
pbfhogg getparents <file> -o <out> <ids>  Find ways/relations referencing given IDs
pbfhogg time-filter <file> -o <out> <ts>  Filter history PBF to a timestamp
pbfhogg build-geocode-index <f> -d <dir>  Build reverse geocoding index
```

All write commands accept `--compression` (`none`, `zlib`, `zstd`, or with level: `zlib:9`). Default is `zlib:6` for osmium interop. For internal pipelines that don't need osmium/JOSM compatibility, `zstd:1` is a substantial wall-time win - measured ≈ −10 % on Europe `add-locations-to-ways --index-type external` (419 s → 379 s) by relieving consumer/compression saturation in stage 4, at similar output size. Commands that benefit from indexdata will error without it - pass `--force` to proceed (slower), or generate indexed PBFs with `pbfhogg cat input.osm.pbf -o indexed.osm.pbf`.

See [docs/cli/commands.md](docs/cli/commands.md) for detailed command documentation, [docs/guide/advanced.md](docs/guide/advanced.md) for O_DIRECT, io_uring, and index type details.

## Performance

Read throughput - 59M elements in Denmark (461 MB), best of 3 (commit `90df51f`):

| Tool | Mode | Time |
|------|------|------|
| **pbfhogg** | parallel | **0.31s** |
| osmpbf 0.3 | parallel | 0.53s |
| **pbfhogg** | pipelined | **1.3s** |
| Planetiler 0.10 | parallel | 2.0s |
| **pbfhogg** | sequential | 2.8s |
| osmpbf 0.3 | sequential | 5.6s |
| osmium 1.19 | cat → opl | 5.7s |

CLI commands vs osmium - Denmark (487 MB, commit `6fc1283`):

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| sort (sorted, indexdata) | **0.7s** | 11.6s | **17x** |
| apply-changes (indexdata) | **0.6s** | 7.2s | **12x** |
| cat --type way (raw passthrough) | **0.24s** | 2.22s | **9.3x** |
| extract --smart (Tokyo bbox, Japan) | **4.7s** | 13.4s | **2.9x** |
| tags-filter highway=primary -R | **0.2s** | 0.56s | **2.8x** |
| add-locations-to-ways | **9.7s** | 12.1s | **1.2x** |

All CLI commands are cross-validated against osmium on Denmark (`brokkr verify`). See [reference/osmium-parity.md](reference/osmium-parity.md) for the full comparison matrix, [DEVIATIONS.md](DEVIATIONS.md) for intentional behavioral differences, and [CORRECTNESS.md](CORRECTNESS.md) for parser/encoder edge cases. Detailed per-command benchmarks and phase breakdowns are in [reference/performance.md](reference/performance.md).

System: AMD Ryzen 9 5900X (12c/24t), 32 GB DDR4, NVMe SSD, Linux 6.18. Measured with `brokkr bench`.

## Acknowledgements

pbfhogg started as a fork of [osmpbf](https://github.com/b-r-u/osmpbf/) by Thomas Bruggemann. [osmium-tool](https://osmcode.org/osmium-tool/) and [libosmium](https://osmcode.org/libosmium/) by Jochen Topf are the reference implementation - pbfhogg's CLI covers the same use cases, cross-validated against osmium using [brokkr](https://github.com/folknor/brokkr).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
