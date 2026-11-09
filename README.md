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
  <img src="https://img.shields.io/badge/rust-1.99+-orange?logo=rust" alt="MSRV 1.99">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License"></a>
</p>

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

Every command listed below runs on the full planet on normal hardware, within 28 GB of RAM. Measured on bygg: AMD Ryzen 9 9950X3D2 (16c/32t), 30.5 GiB RAM, one NVMe SSD for input, output and scratch. Input: the 2026-02-23 indexed planet PBF (87 GB, 11.6B elements). Each row is one stored brokkr cell; the commit is on the row.

<!-- Rows marked "pending" await a fresh bygg cell; the cell list and the
reasons are in .plans/readme-refresh.md. -->

| Command | Wall | Peak anon RSS | Commit |
|---------|------|---------------|--------|
| `add-locations-to-ways --index-type external` | pending | pending | pending |
| `apply-changes` (OSC-only daily diff, `zstd:1`) | pending | pending | pending |
| `apply-changes --locations-on-ways` (daily diff, base keeps untagged nodes) ¹ | pending | pending | pending |
| `build-geocode-index` | pending | pending | pending |
| `cat` (indexdata generation) | pending | pending | pending |
| `cat --clean version` | pending | pending | pending |
| `cat --dedupe` (the planet merged with itself) | pending | pending | pending |
| `cat --type way` (raw passthrough) | pending | pending | pending |
| `check --ids` (streaming, default) | pending | pending | pending |
| `check --ids --full` | pending | pending | pending |
| `check --refs` | pending | pending | pending |
| `diff -j 16` (two independent 47-day-apart planets, text) | pending | pending | pending |
| `diff --format osc -j 16` (two independent 47-day-apart planets) | pending | pending | pending |
| `extract --complete` (Europe bbox) | pending | pending | pending |
| `extract --simple` (Europe bbox) | pending | pending | pending |
| `extract --smart` (Europe bbox) | pending | pending | pending |
| `getid` | pending | pending | pending |
| `getid --invert` | pending | pending | pending |
| `getparents` | pending | pending | pending |
| `inspect` | pending | pending | pending |
| `inspect --extended` | pending | pending | pending |
| `inspect --nodes` | pending | pending | pending |
| `inspect tags` (key counts, as `osmium tags-count`) | pending | pending | pending |
| `merge-changes` (1-OSC daily) | pending | pending | pending |
| `merge-changes` (7-OSC, ~1 week of dailies) | pending | pending | pending |
| `multi-extract --simple -c` (5 regions, Europe bbox) | pending | pending | pending |
| `multi-extract --smart -c` (5 regions, Europe bbox) | pending | pending | pending |
| `renumber` | pending | pending | pending |
| `sort` (already-sorted input) | pending | pending | pending |
| `tags-filter` (default two-pass, `w/highway=primary`) | pending | pending | pending |
| `tags-filter -i w/highway=primary` (invert-match) | pending | pending | pending |
| `tags-filter -R highway=primary` | pending | pending | pending |
| `time-filter` (snapshot, cutoff 2024-01-01) | pending | pending | pending |

¹ On a base made with `add-locations-to-ways --keep-untagged-nodes`, which the flag needs; on a base without its untagged nodes most of the diff's way references go unresolved and the command refuses. See [reference/performance.md](reference/performance.md).

`add-locations-to-ways` defaults to `--index-type sparse`, whose coordinate store at planet scale is larger than the page cache of a 28 GB host; the row above is `--index-type external`, the mode for planet on such a host.

The largest temp files go to the output's parent directory (or `--tmp-dir`): `add-locations-to-ways --index-type external` (hundreds of GB at planet) and `diff --format osc` (its change XML: compressed, about the output's size, for a `.osc.gz` from indexed inputs; plain and several times larger otherwise). A text `diff` writes none at any `-j`: its output goes out as it is produced.

History-shaped input (per-element version history and visibility, consumed by `time-filter` on multi-version files) is functional but deliberately outside this validation surface: no history dataset is configured or benchmarked, and the table above says nothing about it.

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
pbfhogg inspect tags <file>               Tag key frequencies (key=value with an expression)
pbfhogg check <file> --ids                Validate ID uniqueness and ordering
pbfhogg check <file> --refs               Validate referential integrity
pbfhogg check <file> --locations          Validate inline way locations against node records
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
pbfhogg digest <file>                     Encoding-independent digest of header and elements (--form content)
pbfhogg tags-filter <file> -o <out> <exp> Filter by tag expressions (PBF or OSC input)
pbfhogg getid <file> -o <out> <ids>       Extract elements by ID (--invert to remove)
pbfhogg getparents <file> -o <out> <ids>  Find ways/relations referencing given IDs
pbfhogg time-filter <file> -o <out> <ts>  Filter history PBF to a timestamp
pbfhogg build-geocode-index <f> -o <dir>  Build reverse geocoding index
```

All write commands accept `--compression` (`none`, `zlib`, `zstd`, or with level: `zlib:9`). Default is `zlib:6` for osmium interop (`renumber`: `zlib:1`). For internal pipelines that don't need osmium/JOSM compatibility, `zstd:1` can be a substantial wall-time win where output compression is what limits the pipeline - measured ≈ −14 % on Europe `add-locations-to-ways --index-type external` (270.8 s zlib:6 at `0dc8ae1` → 233.3 s zstd:1 at `4fc8e35`, UUID `e2fba1bf`) by relieving consumer/compression saturation in stage 4, at similar output size. **This does not generalise by dataset size:** the same command at planet scale is *not* compression-bound, and `zstd:1` buys ≤5 % of the streaming phase there for ~80 % less compression CPU, at ~4 % larger output (2026-07-14, UUIDs `0e9d93cc` vs `ed5dd6c5`). Measure your own pipeline before assuming the Europe number transfers. Commands that benefit from indexdata will error without it - pass `--force` to proceed (slower), or generate indexed PBFs with `pbfhogg cat input.osm.pbf -o indexed.osm.pbf`.

**Applying accumulated dailies:** when several OSC diffs have piled up (say a week of dailies), squash them with `merge-changes` first and run `apply-changes` once on the result, instead of applying each diff in turn:

```
pbfhogg merge-changes day1.osc.gz day2.osc.gz ... day7.osc.gz -o week.osc.gz
pbfhogg apply-changes planet.osm.pbf week.osc.gz -o updated.osm.pbf
```

One merge plus one apply is much cheaper than N applies: each `apply-changes` pass rewrites the whole base PBF, while the squash itself is cheap (see the 7-OSC `merge-changes` row in the table above). Adding `--simplify` keeps only the last change per object across the merged diffs, so the single apply can also touch fewer elements than the individual applies would in sequence.

See [docs/cli/commands.md](docs/cli/commands.md) for detailed command documentation, [docs/guide/advanced.md](docs/guide/advanced.md) for O_DIRECT, io_uring, index types and embedding the library (`Host`, scratch).

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

CLI commands vs osmium 1.19.1 - Denmark (487 MB indexed), bygg, both tools starting with their inputs evicted from the page cache, median of three alternating single runs (brokkr script bench `vs-osmium`, run by `scripts/vs-osmium.sh 3`; the full table and method are in the [performance guide](docs/guide/performance.md)):

<!-- pending: one vs-osmium campaign at a commit that carries the
header-walk prefetch; see .plans/readme-refresh.md. -->

| Command | pbfhogg | osmium | speedup | Commit |
|---------|---------|--------|---------|--------|
| cat --type way (raw passthrough) | pending | pending | pending | pending |
| sort (already sorted, indexdata) | pending | pending | pending | pending |
| apply-changes (indexdata) | pending | pending | pending | pending |
| add-locations-to-ways (sparse) | pending | pending | pending | pending |
| add-locations-to-ways (external) | pending | pending | pending | pending |
| tags-filter -R w/highway=primary | pending | pending | pending | pending |
| tags-filter -R amenity=restaurant | pending | pending | pending | pending |
| inspect tags (every key) | pending | pending | pending | pending |
| inspect tags --type way | pending | pending | pending | pending |
| getid (9 elements) | pending | pending | pending | pending |

Library mode - the same work called through the library API from the `pbfhogg-harness` executable, against the CLI running the equivalent command, measured by `brokkr library SCENARIO --bench N` on bygg (each process verified; the CLI arm timed from its command interval, inputs evicted before each arm):

<!-- pending: no `brokkr library --bench` row is stored yet; see
.plans/readme-refresh.md. -->

| Scenario | Dataset | Library | CLI | Commit |
|----------|---------|---------|-----|--------|
| `isolation-cat` (one `cat` in a fresh process) | Denmark | pending | pending | pending |
| `isolation-cat` | planet | pending | pending | pending |
| `repeat-cat` (two `cat` calls in one process, one after another) | Denmark | pending | - | pending |
| `concurrent-cat` (two `cat` calls at once, one descriptor pool) | generated | pending | - | pending |
| `fixed-resources-executor` (`cat --clean` and `cat --type` on a caller-built pool) | Denmark | pending | - | pending |

All CLI commands are cross-validated against osmium on Denmark (`brokkr verify`); `apply-changes --locations-on-ways` runs on a base that keeps its untagged nodes, with pbfhogg's output held to `check --locations`. See [reference/osmium-parity.md](reference/osmium-parity.md) for the full comparison matrix, [DEVIATIONS.md](DEVIATIONS.md) for intentional behavioral differences, and [CORRECTNESS.md](CORRECTNESS.md) for parser/encoder edge cases. Detailed per-command benchmarks and phase breakdowns are in [reference/performance.md](reference/performance.md).

System: the planet, osmium and library tables on bygg, AMD Ryzen 9 9950X3D2 (16c/32t), 30.5 GiB RAM, NVMe SSD; the read-throughput table on plantasjen, AMD Ryzen 9 5900X (12c/24t), 32 GB DDR4, NVMe SSD. Measured with brokkr.

## Acknowledgements

pbfhogg started as a fork of [osmpbf](https://github.com/b-r-u/osmpbf/) by Thomas Bruggemann. [osmium-tool](https://osmcode.org/osmium-tool/) and [libosmium](https://osmcode.org/libosmium/) by Jochen Topf are the reference implementation - pbfhogg's CLI covers the same use cases, cross-validated against osmium using [brokkr](https://github.com/folknor/brokkr).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
