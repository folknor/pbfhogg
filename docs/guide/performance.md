# Performance

## Read throughput

Count all 59M elements in Denmark extract (461 MB), best of 3 runs, fat LTO, on plantasjen at commit `90df51f`:

| Tool | Mode | Time | Notes |
|------|------|------|-------|
| **pbfhogg** | parallel | **0.31s** | `par_map_reduce` on all cores |
| osmpbf 0.3 | parallel | 0.53s | upstream crate, same API |
| **pbfhogg** | pipelined | **1.3s** | `for_each_pipelined`, preserves file order |
| Planetiler 0.10 | parallel | 2.0s | Java, `OsmInputFile` + thread pool |
| **pbfhogg** | sequential | 2.8s | `for_each` |
| **pbfhogg** | blobreader | 2.9s | `BlobReader` sequential decode |
| osmpbf 0.3 | sequential | 5.6s | upstream `for_each` |
| osmium 1.19 | cat to opl | 5.7s | `osmium cat -f opl -o /dev/null` |
| Planetiler 0.10 | sequential | 8.7s | Java, `OsmInputFile` single-threaded |

`par_map_reduce` is fastest when order does not matter. `for_each_pipelined` is the fastest ordered read and the production hot path.

## Write throughput

Decode all 59M elements then write through `BlockBuilder` + `PbfWriter` to `/dev/null`, on plantasjen at commit `def80d9`:

| Compression | Sync | Pipelined | Notes |
|-------------|------|-----------|-------|
| none | 6.2s | 6.2s | decode + wire-format serialization floor |
| zstd:3 | 8.1s | **6.2s** | pipelined hides compression cost |
| zlib:6 | 14.5s | **6.3s** | 2.3x speedup from parallel compression |

With pipelined writes, all compression modes converge to ~6.2s - the decode + wire-format serialization floor. All element types are encoded directly to protobuf wire format using reusable scratch buffers (no per-element allocation, no external protobuf dependencies).

## CLI command benchmarks

Denmark (487 MB indexed file, 59M elements), pbfhogg commit `53360af4`
against osmium 1.19.1 on bygg, 2026-11-09. Both tools read the same
indexed file (`pbf.indexed`) with default options, and each run starts
with that file (and the change file) evicted from the page cache.
Each figure is the median of three single runs, the tools alternating,
and is the tool process's own wall, without the bench's cache-eviction
preparation. Where defaults differ in what is promised, osmium is given
the flag that matches pbfhogg: `--fsync` on every command that writes a
PBF, and `--keep-member-nodes` on add-locations-to-ways. osmium has one
add-locations-to-ways mode, so both add-locations-to-ways rows time the
same osmium command against pbfhogg's two index types. The commands are
the `vs-osmium` bench, `benches/compare/vs-osmium.lua`.

The figures below were recorded with each tool's default compression
(zlib level 6 on both). The bench now pins that on both sides
explicitly on every row that writes a PBF (`--compression zlib:6` for
pbfhogg, `pbf_compression=zlib,pbf_compression_level=6` for osmium), so
a change of either tool's default cannot move the comparison. The
timings do not establish that the two tools' outputs are equivalent: a
successful exit proves the command ran. Equivalence is checked by a
separate, untimed invocation, `brokkr bench vs-osmium --row ROW
--validate-only yes`, which runs both tools on the row, decodes both
output PBFs with osmium to OPL including way locations, and compares the
two spools byte for byte. The policy is ordered entity equality: the
entities, their order, metadata, tags (in order), references and
locations must match; file headers and block encoding are excluded. The
three text-report rows (`inspect`, `inspect-tags`, `inspect-tags-way`)
print tool-specific report formats and refuse validation.
To rerun the table, `scripts/vs-osmium.sh PAIRS DATASET OSC` runs every
row as `PAIRS` alternating single-iteration pairs (the inspect row,
pbfhogg only), with `OSC` an explicit key in the dataset's OSC table for
the apply-changes row; all three arguments are required.

| Command | pbfhogg | osmium | speedup |
|---------|---------|--------|---------|
| inspect (indexdata) | **111 ms** | -- | answers from indexdata; no osmium counterpart |
| sort (already sorted, indexdata) | **305 ms** | 7681 ms | **25.2x** |
| apply-changes (indexdata) | **308 ms** | 4484 ms | **14.5x** |
| tags-filter -R w/highway=primary | **162 ms** | 360 ms | **2.2x** |
| tags-filter -R amenity=restaurant | **229 ms** | 802 ms | **3.5x** |
| cat --type way (raw passthrough) | **162 ms** | 1488 ms | **9.2x** |
| inspect tags (bare, every key) | **208 ms** | 1029 ms | **5.0x** |
| inspect tags --type way | **161 ms** | 389 ms | **2.4x** |
| getid (9 elements) | **119 ms** | 619 ms | **5.2x** |
| add-locations-to-ways (sparse) | **1401 ms** | 7486 ms | **5.3x** |
| add-locations-to-ways (external) | **1938 ms** | 7479 ms | **3.9x** |

The largest speedups come from blob passthrough (sort, apply-changes, cat --type) where pbfhogg avoids decompressing and re-compressing unmodified blobs entirely. The commands that answer from indexdata (inspect, inspect tags --type way, getid) spend most of their cold time walking the blob headers. Each header's position comes from the one before it, so on a cold cache the walk reads ahead in 1 MiB windows on files of small blobs like this one, rather than paying one device latency per blob; a walk whose headers are already cached reads them alone.

## Extract benchmarks

Japan (2.4 GB, Tokyo bbox), indexed PBFs, on plantasjen at commit
`cadc3e6`. Best of three runs (or single-sample where noted in the source):
[reference/performance/extract.md, Extract](https://github.com/folknor/pbfhogg/blob/main/reference/performance/extract.md#extract).

| Strategy | pbfhogg |
|----------|---------|
| simple | **3.8s** |
| complete-ways | **3.7s** |
| smart | **4.7s** |

Simple extract uses a 3-phase barrier pipeline with parallel classification and raw frame passthrough. Complete-ways and smart use multi-pass parallel pread classification. Spatial blob filtering skips decompression of node blobs outside the extract region when indexdata is present.

## Apply-changes at scale

`apply-changes` uses blob-level indexdata to classify blobs and passes
unmodified blobs through without decompression. Runtime and memory depend
on the input and change set; see the commit- and host-pinned measurements in
[reference/performance/README.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance/README.md).

## Apply-changes with LocationsOnWays

Denmark (501 MB with LocationsOnWays, daily diff), on plantasjen at commit `e7bbfa2`:

| Pipeline | pbfhogg | osmium | speedup |
|----------|---------|--------|---------|
| apply-changes `--locations-on-ways` | **3.9s** | 8.3s | **2.1x** |
| apply-changes + ALTW (separate) | 2.7s + 6.5s = 9.2s | 4.3s + 9.5s = 13.8s | -- |

These figures predate the inline-location refresh (commit `81f3fbe4`):
at `e7bbfa2` the flag did not update ways the diff left alone, so a node
the diff moved left them stale, and the run did less work than it does
now. Since the refresh, the flag replaces the two-step pipeline with a
single command, provided the base was made with
`add-locations-to-ways --keep-untagged-nodes`; see
[reference/performance/README.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance/README.md) for measurements of the current code.

## Planet-scale pipeline

Runtime and resource use depend on the command, input snapshot, encoding, and
host. See [reference/performance/README.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance/README.md) for measured command runs and
[reference/performance/README.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance/README.md) for optimization history.

## Performance tips

### Use indexed PBFs

Generate an indexed PBF once with `pbfhogg cat`, then use it for all subsequent operations. Commands on indexed PBFs skip decompression of irrelevant blobs entirely, which is where the largest speedups come from.

### Choose the right ALTW index type

`sparse` is fastest while the referenced nodes' working set fits the page
cache; past that, `external` trades heavy scratch-disk use for bounded
memory, and `auto`, the default, picks between them at run time (staying on
sparse where it cannot read the host's memory). When to use each
is in [Advanced Topics](./advanced.md#add-locations-to-ways-index-types),
and measured comparisons between the strategies are in
[Performance](https://github.com/folknor/pbfhogg/blob/main/reference/performance/README.md).

### O_DIRECT for planet-scale I/O

Planet-scale operations read and write 80 GB+, polluting the entire page cache. The `--direct-io` flag bypasses the page cache entirely. Wall time is typically unchanged at country scale (CPU-bound) - the benefit is cache hygiene at planet scale and avoiding eviction of useful data from co-resident processes.

O_DIRECT wins for concurrent read/write patterns (merge). For sequential single-file passthrough (`cat`), buffered I/O is actually faster because the page cache prefetch helps.

### io_uring for large writes

The `--io-uring` flag replaces the synchronous writer thread with io_uring `WriteFixed`. Compare it with buffered writes on the intended workload; see the commit- and host-pinned measurements in
[reference/performance/README.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance/README.md).

### Compression choice

With pipelined writes (the production path), compression is dispatched to rayon and all modes converge to the decode + serialization floor. The choice mainly affects file size and downstream read speed:

- `none` - fastest writes, largest files, ideal for intermediate files or erofs storage
- `zlib` - standard PBF compression, compatible with all tools
- `zstd` - better ratio and faster decompression, but not all consumers support it yet

## System

The CLI command table was measured on bygg, AMD Ryzen 9 9950X3D2 (16c/32t), at commit `53360af4`. The read throughput, write throughput, extract and apply-changes with LocationsOnWays tables were measured on plantasjen, AMD Ryzen 9 5900X (12c/24t), 32 GB DDR4, NVMe SSD, at the older commits each table names; they have not been re-measured since. Measured with brokkr, and cross-validated with `brokkr verify` for every command it compares against osmium (`inspect tags` is not among them).
