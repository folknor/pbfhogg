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

## Embedding pbfhogg

The library never mutates process state, reads the environment or writes to stderr on its own (ADR-0016). Every command entry point takes a `&pbfhogg::host::Host`, which has no default: the caller states each policy the CLI used to decide for itself. `Host`'s public fields, with the CLI's choice for each:

| Field | What it decides | The CLI's choice |
|-------|-----------------|------------------|
| `process: Arc<dyn ProcessActions>` | Process-wide actions, invoked at the exact point a command needs them: `cross_thread_alloc_pool` before a pool that allocates on one thread and frees on another, `fd_allowance(wanted)` before a run that holds many descriptors, answering how many it may open on top of those already open (`FdAllowance::host_grant` for a share the host reserves across concurrent calls) | `cross_thread_alloc_pool` caps glibc's malloc arenas at 2 (`process::cap_malloc_arenas`); `fd_allowance` raises the soft `RLIMIT_NOFILE` to the hard cap and subtracts the descriptors already open (`process::raise_nofile_to_hard_cap`) |
| `resources: Arc<dyn Resources>` | Thread counts and memory budgets, probed where each command sizes them | `MachineProbe` (`available_parallelism`, `MemAvailable` from `/proc/meminfo`, on Linux capped by the visible cgroup v2 `memory.max - memory.current` headroom) |
| `diagnostics: Diagnostics` | Where progress lines, warnings, notes and summaries go, one event per line | A sink that prints each event as one stderr line, with `Warning:` before warnings and `Note:` before notes |
| `page_cache: PageCachePolicy` | Whether commands drop their input files' pages behind the read head | `Evict` |
| `executor: Executor` | Which rayon pool runs the library's parallel work and writer framing | `Ambient` |
| `cancel: Cancel` | The run's cancellation token. `Cancel::cancel` (from any thread) makes the run stop at its next checkpoint - a blob read or dispatched, a pool result received, a stage boundary, an output write, the publication decision - remove its staging and scratch, publish nothing and return `ErrorKind::Cancelled` (`Error::is_cancelled`). Tripping is permanent: give each independent run a fresh `Cancel::new()` | A fresh token per process, tripped by the first SIGINT or SIGTERM; exits 130 or 143 (ADR-0019) |

`NoProcessActions` leaves the process alone, `Fixed` supplies fixed resource values, `Diagnostics::discard()` drops every event, and `PageCachePolicy::Keep` leaves the page cache alone - the shape a long-running host process usually wants. Concurrent runs in one process can be kept apart: give each run its own `Diagnostics` and its events stay its own, and each run owns its own scratch directory, so two runs never share a scratch file. The diagnostic-only metric totals behind the sidecar counters (writer, reader pipeline, add-locations-to-ways) are process-wide and mix across concurrent runs.

Scratch is not a `Host` policy: the entry points that write scratch (`altw::add_locations_to_ways`, `altw::external::external_join`, `diff::diff`, `diff::derive::derive_changes`, `geocode_index::builder::build_geocode_index`) take a `pbfhogg::scratch::RunScratch` by value. `RunScratch::create(parent, &host)` checks that `parent` exists and is writable, makes the run's `.pbfhogg-run-*` directory in it, and removes the directory when the guard drops (a removal that fails is reported through `host.diagnostics` with the path left behind). Create it before opening any input, so a bad scratch location fails first. `scratch::find_stale(dir)` returns a typed `StaleScan` of what crashed runs left in a directory and `scratch::report_stale` renders it as warnings; nothing is ever deleted, and a `StaleKind::OutputBackup` entry can be the only copy of a previous output. The scratch parent and the path to it are trusted: they must stay stable for the run.

The CLI's arena cap is a `mallopt(M_ARENA_MAX, 2)` call, which overrides `GLIBC_TUNABLES=glibc.malloc.arena_max` for the rest of the process. An embedding caller that wants the tunable to stand should not call `cap_malloc_arenas`.

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
| `sparse` (default) | Depends on the referenced-node working set | `referenced_count * 8` bytes | no | When the working set fits available page cache; see `reference/performance.md` for measurements |
| `external` | Bounded memory | High scratch-disk demand; see `reference/performance.md` for measurements | yes + indexdata | Sorted, indexed input when the sparse working set is too large |
| `auto` | Runtime routing by input eligibility and estimated store/page-cache budget | varies | no (unsorted or unindexed input routes to sparse) | Selects a backend at runtime |

**sparse** is a rank-indexed flat mmap array (~8 bytes per referenced node). Builds a referenced-id IdSet in pass 1, then writes locations in pass 2 indexed by rank within that set. Works on any PBF. With `--inject-prepass`, that first pass also builds a second IdSet over the same node-ID range (it must remember which referenced nodes are shared, not just referenced), so its set memory can approach double the default's.

**external** uses a four-stage ID- and slot-position-bucketed join with bounded memory. Requires sorted input with indexdata. See `reference/performance.md` for measured scratch-disk needs.

**auto** uses sparse for unsorted or unindexed input. For sorted, indexed input,
it uses external when the estimated store exceeds a fixed share of the
page-cache budget; if either estimate is unavailable, it uses external. The
full routing rule is documented in `reference/pipeline.md`; the selected
backend is reported to stderr. Explicit `--index-type sparse|external`
bypasses auto routing.

A previous `dense` mode (a direct-mapped mmap array indexed by node ID) was removed. See `reference/performance.md` for measured comparisons.

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

This runs a 4-pass pipeline: admin boundary relations, referenced node collection, node+way fused scan, and bucketed S2 cell assignment. The output is a set of memory-mappable binary files for fast reverse geocoding queries.

Build time and memory use depend on the input and host. See `reference/performance.md` for measurements.

The index can be queried from Rust using the `geocode-reader` feature:

```toml
[dependencies]
pbfhogg = { version = "0.5", default-features = false, features = ["geocode-reader"] }
```

`Reader::query` and `Reader::candidates` return a `Result`: a non-finite or
out-of-range latitude or longitude is a typed `QueryError` rather than a
silently wrong cell lookup.

The on-disk format is version 3. Readers reject older versions outright with a
message naming `build-geocode-index` as the remedy, so an index built by an
earlier release must be rebuilt rather than migrated - the index is a directory
of regenerable derived files, not a document.
