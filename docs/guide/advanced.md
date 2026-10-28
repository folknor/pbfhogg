# Advanced Topics

## Indexdata

pbfhogg embeds additional metadata in BlobHeader fields: element type, ID range, spatial bounding box, and tag key sets. Standard PBF readers silently skip these fields per protobuf wire format rules.

This metadata enables commands to skip decompression of irrelevant blobs entirely. For example, `apply-changes` classifies blobs in O(1) without decompressing them and passes the blobs a diff does not touch through as raw bytes. `tags-filter` skips blobs that provably lack required tag keys.

Generate an indexed PBF:

```sh
pbfhogg cat input.osm.pbf -o indexed.osm.pbf
```

Without a `--type` flag, `cat` adds indexdata via decompress+scan without re-compressing blobs. Memory usage is minimal and the size overhead is small; see [Performance](./performance) for measurements.

Commands that benefit from indexdata: `apply-changes`, `sort`, `add-locations-to-ways`, `extract` (complete/smart), `tags-filter`, `getid`, `cat --type`, `inspect tags --type`, `inspect --nodes`, and `build-geocode-index`.

## O_DIRECT for planet-scale I/O

Planet-scale operations read and write 80 GB+, polluting the entire page cache and evicting useful data from co-resident processes. The `linux-direct-io` feature bypasses the page cache entirely.

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --direct-io
```

O_DIRECT requires a real filesystem (not tmpfs). The intended benefit is cache hygiene at planet scale, not wall time; whether it is faster or slower than buffered I/O depends on the command and the host, so measure on yours.

## io_uring writes

The `linux-io-uring` feature replaces the synchronous writer thread with io_uring `WriteFixed` and pre-registered page-aligned buffers. Requires Linux 5.1+ and sufficient `RLIMIT_MEMLOCK` (16 MB for the default 64-buffer pool).

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --io-uring
```

Whether it beats buffered writes depends on output size and host: when the page cache absorbs the whole output, buffered writes keep up. See [Performance](./performance).

## Embedding pbfhogg

The library never mutates process state, reads the environment or writes to stderr on its own (ADR-0016). Every command entry point takes a `&pbfhogg::host::Host`, which has no default: the caller states each policy the CLI used to decide for itself. `Host`'s public fields, with the CLI's choice for each:

| Field | What it decides | The CLI's choice |
|-------|-----------------|------------------|
| `process: Arc<dyn ProcessActions>` | Process-wide actions, invoked at the exact point a command needs them: `cross_thread_alloc_pool` before a pool that allocates on one thread and frees on another, `fd_allowance(wanted)` before a run that holds many descriptors, answering how many it may open on top of those already open (`FdAllowance::host_grant` for a share the host reserves across concurrent calls) | `cross_thread_alloc_pool` caps glibc's malloc arenas at 2 (`process::cap_malloc_arenas`); `fd_allowance` raises the soft `RLIMIT_NOFILE` to the hard cap and subtracts the descriptors already open (`process::raise_nofile_to_hard_cap`) |
| `resources: Arc<dyn Resources>` | Thread counts and memory budgets, probed where each command sizes them | `MachineProbe` (`available_parallelism`, `MemAvailable` from `/proc/meminfo`, on Linux capped by the visible cgroup v2 `memory.max - memory.current` headroom) |
| `diagnostics: Diagnostics` | Where progress lines, warnings, notes and summaries go, one event per line | A sink that prints each event as one stderr line, with `Warning:` before warnings and `Note:` before notes |
| `page_cache: PageCachePolicy` | Whether commands drop their input files' pages behind the read head | `Evict` |
| `executor: Executor` | Which rayon pool runs the library's parallel work and writer framing | `Ambient` |
| `invocation: Invocation` | The run itself: its cancellation token and its counters. `Invocation::cancel` (from any thread) makes the run stop at its next checkpoint - a blob read or dispatched, a pool result received, a stage boundary, an output write, the publication decision - remove its staging and scratch, publish nothing and return `ErrorKind::Cancelled` (`Error::is_cancelled`). Every pass of the run records its engine counters (`pipeline_*`, `ordered_pool_*`, `classify_unordered_*`, `writer_*`, `fadvise_failures`) into it, readable with `Invocation::for_each_counter`. Phase markers and counter samples go to the invocation's `debug::InstrumentSink`, if it was built with `Invocation::with_sink`; `Invocation::new()` emits nothing. Tripping is permanent and the counters only grow: give each independent run a fresh `Invocation::new()`. The `hotpath` and `hotpath-alloc` profiling features are outside this: their state is process-wide (and `hotpath-alloc` installs nothing itself; the CLI sets the counting global allocator), so their reports mean one run per process | A fresh token per process, tripped by the first SIGINT or SIGTERM (exits 130 or 143, ADR-0019), with a `debug::MarkerFifo` sink when `BROKKR_MARKER_FIFO` names one |

`NoProcessActions` leaves the process alone, `Fixed` supplies fixed resource values, `Diagnostics::discard()` drops every event, and `PageCachePolicy::Keep` leaves the page cache alone - the shape a long-running host process usually wants. Concurrent runs in one process can be kept apart: each run owns its own scratch directory, so two runs never share a scratch file, and give each run its own `Invocation`, and its counters, its instrumentation stream and its once-only diagnostics stay its own. A `Diagnostics` is only where events go: runs may share one (or clones of one), and each still reports its own warnings, but a sink shared between runs receives their events interleaved, so give each run its own `Diagnostics` to keep its events apart. The diagnostics a run reports only once (a missing-indexdata note, an unrecognised-indexdata warning, a writer degradation) are once per `Invocation`: two runs under one token, concurrent or one after the other, are one invocation, add into the same totals and report each such diagnostic once between them. Two publications under one token are serialized, so each `PUBLISH_START` .. `PUBLISH_END` span on the stream carries its own `publish_sync_*` samples. To collect several runs in one place, give each its own sink that tags what it forwards; a sink's calls carry no run identity, arrive from many threads at once (and from destructors during unwinding), must be prompt, and must not call back into the library under the same invocation. A panic in a sink switches that sink off and the run carries on. The allocator (`*_mallinfo_*`) and page-fault counters observe the whole process at the run's phase boundaries, so a concurrent run's work shows in them. Thread-local scratch peaks (`pipeline_scratch_*_capacity_peak_bytes`) observe what a pool thread retained, from this run or an earlier one on the same pool.

Scratch is not a `Host` policy: the entry points that write scratch (`altw::add_locations_to_ways`, `altw::external::external_join`, `diff::diff`, `diff::derive::derive_changes`, `geocode_index::builder::build_geocode_index`) take a `pbfhogg::scratch::RunScratch` by value (`diff::diff` takes an `Option` and accepts `None` when its options run a single shard, `Fixed(1)` or `verbose`; `None` with more shards is refused; `diff::diff_stats`, which counts the same differences without producing text, takes no scratch at any shard count). `RunScratch::create(parent, &host)` checks that `parent` exists and is writable, makes the run's `.pbfhogg-run-*` directory in it, and removes the directory when the guard drops (a removal that fails is reported through `host.diagnostics` with the path left behind). Create it before opening any input, so a bad scratch location fails first. `scratch::find_stale(dir)` returns a typed `StaleScan` of what crashed runs left in a directory and `scratch::report_stale` renders it as warnings; nothing is ever deleted, and a `StaleKind::OutputBackup` entry can be the only copy of a previous output. The scratch parent and the path to it are trusted: they must stay stable for the run.

The CLI's arena cap is a `mallopt(M_ARENA_MAX, 2)` call, which overrides `GLIBC_TUNABLES=glibc.malloc.arena_max` for the rest of the process. An embedding caller that wants the tunable to stand should not call `cap_malloc_arenas`.

## Compression modes

All PBF-writing commands accept `--compression`. It applies to the blobs a command encodes; passed-through blobs keep their compression, and `repack` re-encodes every blob.

| Value | Description |
|-------|-------------|
| `none` | No compression. Fastest writes, largest files. Ideal for intermediate files or erofs storage. |
| `zlib` | Zlib level 6 (default; `renumber` defaults to `zlib:1`). Standard PBF compression, compatible with all tools. |
| `zlib:LEVEL` | Zlib with explicit level (0-9). Higher = smaller + slower. |
| `zstd` | Zstandard level 3. Better ratio and faster decompression than zlib. |
| `zstd:LEVEL` | Zstandard with explicit level. |

Compression runs on the parallel pool alongside encoding, so the choice affects file size and downstream read speed as well as write time.

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
pbfhogg extract input.osm.pbf --config regions.json --simple
```

The config file defines multiple extract regions, each with a name, output path, and bounding box or polygon. With `--simple` on sorted input, all regions are extracted in one pass over the input. The complete-ways (default) and `--smart` strategies, and unsorted input, extract the regions one after another, each with its own passes.

Use `-d` to override the output directory for all extracts:

```sh
pbfhogg extract input.osm.pbf --config regions.json --simple -d /data/extracts/
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
