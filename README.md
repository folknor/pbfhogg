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
  <a href="https://docs.rs/pbfhogg"><img src="https://img.shields.io/docsrs/pbfhogg" alt="docs.rs"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License"></a>
</p>

Rust library and CLI for reading, writing, and transforming OpenStreetMap PBF
files. Planet-scale workflows are a design target; see
[reference/performance.md](reference/performance.md) for measured inputs and
resource use.

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

Runtime, memory and scratch-disk needs at planet scale vary by command and
input snapshot. See [reference/performance.md](reference/performance.md) for
measured command runs and [reference/performance-history.md](reference/performance-history.md)
for optimization history.

History-shaped input (per-element version history and visibility, consumed by `time-filter` on multi-version files) is functional but deliberately outside this validation surface: no history dataset is configured or benchmarked, so the measurement record does not cover it.

Per-command phase breakdowns are in [reference/performance.md](reference/performance.md); per-command optimization arcs and retired phase breakdowns at older architectures are in [reference/performance-history.md](reference/performance-history.md). Note that recorded results always track the latest git head and may not match the released version.

The goal for pbfhogg 1.0 is that every CLI command must be planet-scale safe on a 32GB RAM host (28-ish free GB.)

## Usage

```toml
[dependencies]
pbfhogg = "0.5"
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
use pbfhogg::host::Executor;

let header_bytes = HeaderBuilder::new()
    .bbox(9.0, 54.0, 13.0, 58.0)
    .sorted()
    .build()?;
let mut writer = PbfWriter::to_path(
    "output.osm.pbf".as_ref(),
    Compression::default(),
    &header_bytes,
    Executor::Ambient,
)?;

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
pbfhogg repack <file> -o <out>            Re-encode at a configurable --elements-per-blob N cap
pbfhogg degrade <file> -o <out>           Adversarial PBF generator
pbfhogg renumber <file> -o <out>          Renumber all IDs sequentially, remap cross-references
pbfhogg extract <file> -o <out> -b <bbox> Extract by bounding box
pbfhogg extract <file> -o <out> -p <geo>  Extract by GeoJSON polygon
pbfhogg extract <file> --config <config>  Multi-extract from JSON config
pbfhogg add-locations-to-ways <f> -o <o>  Embed node coordinates in ways
pbfhogg apply-changes <base> <osc> -o <o> Apply OSC diff (--locations-on-ways)
pbfhogg merge-changes <oscs...> -o <out>  Merge multiple OSC files (--simplify)
pbfhogg diff <old> <new>                  Compare two PBFs (-v verbose, --format osc)
pbfhogg digest <file>                     Encoding-independent digest of element content
pbfhogg tags-filter <file> -o <out> <exp> Filter by tag expressions (PBF or OSC input)
pbfhogg getid <file> -o <out> <ids>       Extract elements by ID (--invert to remove)
pbfhogg getparents <file> -o <out> <ids>  Find ways/relations referencing given IDs
pbfhogg time-filter <file> -o <out> <ts>  Filter history PBF to a timestamp
pbfhogg build-geocode-index <f> -o <dir>  Build reverse geocoding index
```

All write commands accept `--compression` (`none`, `zlib`, `zstd`, or with level: `zlib:9`). Default is `zlib:6` for osmium interop. For internal pipelines that do not need osmium/JOSM compatibility, `zstd:1` can help when output compression limits the pipeline. The effect depends on the command and input; see [reference/performance.md](reference/performance.md) for measured comparisons. Commands that benefit from indexdata will error without it - pass `--force` to proceed (slower), or generate indexed PBFs with `pbfhogg cat input.osm.pbf -o indexed.osm.pbf`.

**Applying accumulated dailies:** when several OSC diffs have piled up (say a week of dailies), squash them with `merge-changes` first and run `apply-changes` once on the result, instead of applying each diff in turn:

```
pbfhogg merge-changes day1.osc.gz day2.osc.gz ... day7.osc.gz -o week.osc.gz
pbfhogg apply-changes planet.osm.pbf week.osc.gz -o updated.osm.pbf
```

One merge plus one apply avoids rewriting the whole base PBF for each daily diff. Adding `--simplify` keeps only the last change per object across the merged diffs, so the single apply can also touch fewer elements than the individual applies would in sequence. See [reference/performance.md](reference/performance.md) for measured comparisons.

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
