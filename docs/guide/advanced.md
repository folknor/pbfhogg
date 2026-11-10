# Advanced Topics

## Indexdata

pbfhogg embeds additional metadata in BlobHeader fields: element type, ID range, spatial bounding box, and tag key sets. Standard PBF readers silently skip these fields per protobuf wire format rules.

This metadata enables commands to skip decompression of irrelevant blobs entirely. For example, `apply-changes` classifies blobs in O(1) without decompressing them and passes the blobs a diff does not touch through as raw bytes. `tags-filter` skips blobs that provably lack required tag keys.

Generate an indexed PBF:

```sh
pbfhogg cat input.osm.pbf -o indexed.osm.pbf
```

Without a `--type` flag, `cat` adds indexdata via decompress+scan without re-compressing blobs. Memory usage is minimal and the size overhead is small; see [Performance](./performance.md) for measurements.

Which commands need indexdata, and what each does without it, is in [Indexdata](./indexdata.md#which-commands-require-indexdata).

## O_DIRECT for planet-scale I/O

Planet-scale operations read and write 80 GB+, polluting the entire page cache and evicting useful data from co-resident processes. The `linux-direct-io` feature enables `--direct-io` to bypass the page cache on supported paths. The [I/O capability matrix](../cli/commands.md#io-capability-matrix) lists which reads and writes use it; several bulk read paths remain buffered.

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --direct-io
```

O_DIRECT requires a real filesystem (not tmpfs). The intended benefit is cache hygiene at planet scale, not wall time; whether it is faster or slower than buffered I/O depends on the command and the host, so measure on yours.

## io_uring writes

The `linux-io-uring` feature replaces the synchronous writer thread with io_uring `WriteFixed` and pre-registered page-aligned buffers. Requires Linux 5.1+ and sufficient `RLIMIT_MEMLOCK` (16 MB for the default 64-buffer pool).

```sh
pbfhogg apply-changes base.osm.pbf changes.osc.gz -o output.osm.pbf --io-uring
```

Whether it beats buffered writes depends on output size and host: when the page cache absorbs the whole output, buffered writes keep up. See [Performance](./performance.md).

## Embedding pbfhogg

The library never mutates process state, reads the environment or writes to stderr on its own (ADR-0016). Every command entry point takes a `&pbfhogg::host::Host`, which has no default: the caller states each policy the CLI used to decide for itself. `Host`'s public fields, with the CLI's choice for each:

| Field | What it decides | The CLI's choice |
|-------|-----------------|------------------|
| `process: Arc<dyn ProcessActions>` | Process-wide actions, invoked at the exact point a command needs them: `fd_allowance(wanted)` before a run that holds many descriptors, answering with an `FdReservation` of how many it may open on top of those already open, which the command holds while those descriptors are open and drops after. A snapshot of the open descriptors (`FdReservation::unreserved`, what `NoProcessActions` answers) lets two concurrent calls see the same free descriptors; a caller running descriptor-heavy calls concurrently shares one `host::FdPool` across their hosts, whose grants come out of one caller-sized budget and return to it when each reservation drops, so the calls' grants never sum past it whatever their timing. The budget covers only the asked-for descriptors (`cat`'s pins and `cat --dedupe`'s pass-2 sources, a multi-region `extract`'s per-region writers, external `add-locations-to-ways`' stage-1 shard files); everything else every concurrent call opens stays the caller's to fit under the soft limit less the budget, a multi-region extract's per-output staging descriptors included, which are opened when the caller builds its `OutputSet`. The library never raises the limit itself: `cat` holds a descriptor per distinct input for its whole run, and `extract_multi` on the single-pass route (simple strategy, nothing cleaned) a writer per region, and each refuses up front, naming the remedy, when the answer is short, so an embedding program that cats many files or extracts many regions raises its own soft limit or grants more | `fd_allowance` raises the soft `RLIMIT_NOFILE` to the hard cap and subtracts the descriptors already open (`process::raise_nofile_to_hard_cap`) |
| `resources: Arc<dyn Resources>` | Thread counts and memory budgets, probed where each command sizes them | `MachineProbe` (`available_parallelism`, `MemAvailable` from `/proc/meminfo`, on Linux capped by the visible cgroup v2 `memory.max - memory.current` headroom) |
| `diagnostics: Diagnostics` | Where progress lines, warnings, notes and summaries go, one event per line | A sink that prints each event as one stderr line, with `Warning:` before warnings and `Note:` before notes |
| `page_cache: PageCachePolicy` | Whether host-bound buffered blob and element readers advise dropping input pages behind the read head; raw-frame walks and positional reads retain them (see below) | `Evict` |
| `executor: Executor` | Which rayon pool runs the library's shared rayon tasks: writer framing and the parallel-iterator work in repack, degrade, simple extract, the geocode index builder and merge-changes, and the batch workers of merge-changes and the OSC tags-filter (with `Parallelism::Fixed(n)`, those two use a dedicated pool of `n` threads instead). It does not confine all library computation: dedicated pipelines (the pipelined reader's decode pool, the ordered and classify worker pools, per-stage threads) create their own workers for the call or the returned session, sized from explicit options, `resources` or fixed constants. The rayon pools the library builds itself (the decode pool, and the `Fixed(n)` pools of merge-changes and the OSC tags-filter) are joined before the call ends, unwinding included - for `into_blocks_pipelined`, before the iterator's drop returns - so none of their threads, nor their thread-local destructors, are still running afterwards; a sink callback that runs on one of their workers must not leave work there that waits for the call to end (see `debug::InstrumentSink`). The other dedicated workers are `std` threads whose work ends with the call, though a scoped thread's thread-local destructors may still be running when it returns. `resources` is sizing input, not an aggregate concurrency limit | `Ambient` |
| `invocation: Invocation` | The run itself: its cancellation token and its counters. `Invocation::cancel` (from any thread) makes the run stop at its next checkpoint - a blob read or dispatched, a pool result received, a stage boundary, an output write, the publication decision - remove its staging and scratch, publish nothing and return `ErrorKind::Cancelled` (`Error::is_cancelled`); it also refuses every output bound to the invocation and not yet decided, and returns a `CancelOutcome` saying whether it came before a decision (`BeforeDecision`), after one (`AfterDecision`: that publication completes) or with none in sight. Every publication decision of the invocation is a row of `Invocation::for_each_publication`. Every pass of the run records its engine counters (`pipeline_*`, `ordered_pool_*`, `classify_unordered_*`, `writer_*`, `fadvise_failures`, `fadvise_input_evictions`, every input page-cache eviction it asked for, which stays 0 under `PageCachePolicy::Keep`, and `fadvise_scratch_evictions`, every eviction of its own scratch files it asked for, under either policy, and `fadvise_willneed_wait_ns`, the time spent in its read-ahead advice calls) into it, readable with `Invocation::for_each_counter`. Phase markers and counter samples go to the invocation's `debug::InstrumentSink`, if it was built with `Invocation::with_sink`; `Invocation::new()` emits nothing. Several invocations sharing one sidecar FIFO each own their own `debug::CallSink`, from `MarkerFifo::call(CallId::new("...")?)?` (an id is issued once per FIFO) handed to `Invocation::with_call_sink(sink, tap)`, which tags their lines with the call id on the FIFO's one descriptor and clock (`MarkerFifo::now_us` reads that clock for a call's start and end); the optional `tap` sees every ordinary record after the sink. `Invocation::seal()` ends an invocation without waiting (ADR-0020): it returns `SealOutcome::Sealed` with the final snapshot of its counters once no call is in progress and nothing issued to the caller (a block iterator, a writer, a bound output, a kept block) is alive, `Outstanding` with that census while something is, or `Refused` with the reason (a call in progress, as from inside a callback, among them). A seal of an invocation with a call sink ends its stream: every snapshot counter as a sealing emission, then the terminal record, accounted in `Invocation::stream_account()`. A sealed invocation admits nothing more: every entry under it returns `ErrorKind::Sealed`, and `cancel` returns `CancelOutcome::Ended`. Tripping is permanent and the counters only grow: give each independent run a fresh `Invocation::new()`. The `hotpath` and `hotpath-alloc` profiling features are outside this: their state is process-wide (and `hotpath-alloc` installs nothing itself; the CLI sets the counting global allocator), so their reports mean one run per process | A fresh token per process, tripped by the first SIGINT or SIGTERM (exits 130 or 143, ADR-0019), with a `debug::MarkerFifo` sink when `BROKKR_MARKER_FIFO` names one, on whose process stream the CLI also marks its command interval itself (`CLI_COMMAND_START` / `CLI_COMMAND_END`, see the CLI reference's "Sidecar marker FIFO") |

`NoProcessActions` leaves the process alone, `Fixed` supplies fixed resource values, `Diagnostics::discard()` drops every event, and `PageCachePolicy::Keep` leaves the page cache alone - the shape a long-running host process usually wants. Concurrent runs in one process can be kept apart: each run owns its own scratch directory, so two runs never share a scratch file, and give each run its own `Invocation`, and its counters, its instrumentation stream and its once-only diagnostics stay its own. A `Diagnostics` is only where events go: runs may share one (or clones of one), and each still reports its own warnings, but a sink shared between runs receives their events interleaved, so give each run its own `Diagnostics` to keep its events apart. The diagnostics a run reports only once (a missing-indexdata note, an unrecognised-indexdata warning, a writer degradation) are once per `Invocation`: two runs under one token, concurrent or one after the other, are one invocation, add into the same totals and report each such diagnostic once between them. Two publications under one token run one at a time, decision through completion, so their ordinals are their execution order and each `PUBLISH_START` .. `PUBLISH_END` span on the stream carries its own `publish_sync_*` samples. To collect several runs in one place, give each its own sink that tags what it forwards; a sink's calls carry no run identity, arrive from many threads at once (and from destructors during unwinding), must be prompt, and must not call back into the library under the same invocation. A panic in a sink switches that sink off and the run carries on. The allocator (`*_mallinfo_*`) and page-fault counters observe the whole process at the run's phase boundaries, so a concurrent run's work shows in them. Thread-local scratch peaks (`pipeline_scratch_st_capacity_peak_bytes`, `pipeline_scratch_gr_capacity_peak_bytes`) are the largest capacity any one decode worker of the invocation's reads retained, and `pipeline_scratch_pool_capacity_peak_bytes` is the largest total one read's decode workers retained (allocated capacity, not resident memory; reads that overlap are not added together); each read has its own decode pool, joined before the read ends, so they never carry over from another run.

`page_cache` controls input eviction by reader, not by command. On Linux,
`BlobReader::open_with` and the command blob/element readers built on it
honour `Host::page_cache`: `Evict` advises dropping the consumed prefix after
copying a payload out; `Keep` issues no input eviction advice. Header-only
advances and `skip_frame_payload` do not themselves request eviction; a later
payload read can advise away that earlier prefix. This
also covers sequential and pipelined element iteration. Public readers opened
without a host keep pages unless eviction is explicitly enabled. Advice is
best effort, not a guarantee that pages leave memory.

The other input walkers deliberately retain cached pages under either policy:

| Walker | Users and reason to retain pages |
|--------|---------------------------------|
| Raw sequential `Frames<FileReader>` walks, including `map_frames_ordered` | `cat` passthrough and type filtering, cat reindex, degrade, `check --index`, and getid's sequential raw-frame path. Keep buffered readahead and avoid adding eviction calls to these copy/scan paths without evidence that the trade pays. |
| Input-front probes | Header and indexdata probes read bytes the following pass can reuse. Evicting after a probe would discard that reuse. |
| `HeaderWalker`, including its inline `pread_data` reads | Header/schedule scans for getid, getparents, inspect, checks, extract, tags-filter, sort/merge, apply-changes, renumber, add-locations-to-ways and geocode index building. Headers and selected bodies may be reused by later passes; a schedule producer advancing past a blob does not mean its consumers have read it. |
| Positional body/frame reads through `InputFile`, shared file descriptors and `ReadBuf`, including classify pools and range-copy/splice routes | Scheduled consumers in the commands above, including cat's cleaning path, can read out of order, overlap, or revisit input in later passes. They have no single consumed prefix that is safe to evict on behalf of all consumers. |

A command can combine these walkers with evicting blob/element passes, so its
`fadvise_input_evictions` count describes only advice actually requested by
those passes. In particular, raw cat and cat reindex request none even under
`Evict`. `Keep` prevents input eviction advice throughout; it does not prevent
scratch eviction. `O_DIRECT` readers bypass the page cache rather than issuing
input eviction advice, subject to the filesystem caveats above. No read path
promises to evict every page a command brought into cache.

Scratch is not a `Host` policy: the entry points that write scratch (`altw::add_locations_to_ways`, `altw::external::external_join`, `diff::derive::derive_changes`, `geocode_index::builder::build_geocode_index`) take a `pbfhogg::scratch::RunScratch` by value. `diff::diff` and `diff::diff_stats` write none and take none, at any worker count. `merge_changes::merge_changes` takes it in its `MergeMode::Stream` as an `Option`: it is required exactly when `merge_changes::stream_writes_scratch(input_count, parallelism)` is true (several inputs, any parallelism but `Fixed(1)`), and `MergeMode::Simplify` writes none and has no place for one. `RunScratch::create(parent, &host)` checks that `parent` exists and is writable, makes the run's `.pbfhogg-run-*` directory in it, and removes the directory when the guard drops (a removal that fails is reported through `host.diagnostics` with the path left behind). Create it before opening any input, so a bad scratch location fails first. `scratch::find_stale(dir)` returns a typed `StaleScan` of what crashed runs left in a directory and `scratch::report_stale` renders it as warnings; nothing is ever deleted, and a `StaleKind::OutputBackup` entry can be the only copy of a previous output. The scratch parent and the path to it are trusted: they must stay stable for the run.

Neither the library nor the CLI changes allocator settings, and the library raises no allocator event: glibc's settings are process-global, so a program that wants one sets it once at start. `process::cap_malloc_arenas` (a `mallopt(M_ARENA_MAX, 2)` call, which overrides `GLIBC_TUNABLES=glibc.malloc.arena_max` for the rest of the process and cannot be raised again) and `process::set_mmap_threshold` (which overrides `glibc.malloc.mmap_threshold` and switches off glibc's sliding threshold) are there for a program that does. Two arenas make every worker's larger allocations queue on two locks: at planet, the re-encoding commands took 10 to 82 % more wall with them, and apply-changes twenty times as long, so cap arenas only for a workload measured to need it. glibc's sliding mmap threshold keeps freed blob-scale buffers resident in the arenas, which at planet raised multi-phase commands' peak memory by 4 to 41 %; a fixed threshold buys some of that back but costs wall where buffers are allocated per blob, and no value measured was better on every command ([reference/performance.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance.md), "glibc's sliding mmap threshold across commands").

## Compression modes

The values `--compression` accepts, and which blobs it reaches, are in the CLI reference's [Common flags](../cli/commands.md#common-flags). `none` gives the fastest writes and the largest files, `zlib` the format's standard compression, and `zstd` a better ratio and faster decompression; a higher level is smaller and slower. Compression runs on the parallel pool alongside encoding, so the choice affects file size and downstream read speed as well as write time.

Zlib uses `zlib-rs` (pure Rust). No C compiler needed.

## Add-locations-to-ways index types

`add-locations-to-ways` embeds node coordinates in ways. It supports two index strategies, chosen between at runtime by `auto`, the default, or named with `--index-type`:

```sh
pbfhogg add-locations-to-ways input.osm.pbf -o output.osm.pbf --index-type external
```

Which to choose:

- **auto** (the default) to let the run decide: unsorted or unindexed input goes to sparse, and sorted, indexed input to external when the estimated store exceeds a fixed share of the page-cache budget. Where that budget cannot be read from the host (every non-Linux host, some containers) or available memory is under about 3 GiB, it stays on sparse. The choice is reported on stderr. Its estimate counts every node, so a sorted file of mostly standalone nodes can be sent to external needlessly; name sparse there.
- **sparse** when the referenced nodes' working set (about 8 bytes per referenced node) fits the page cache. It works on any PBF, sorted or not. With `--inject-prepass` its first pass also remembers which referenced nodes are shared, so its ID-set memory can approach double what it is without that flag.
- **external** when it does not: bounded memory at the cost of heavy scratch-disk use. It needs sorted input with indexdata.

The flag's exact contract is in the CLI reference's [add-locations-to-ways](../cli/commands.md#add-locations-to-ways) section; how each backend works, and the full routing rule, in [pipeline.md](https://github.com/folknor/pbfhogg/blob/main/reference/pipeline.md#add-locations-to-ways), and measured resource use in [performance.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance.md).

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

Build time and memory use depend on the input and host. See [reference/performance.md](https://github.com/folknor/pbfhogg/blob/main/reference/performance.md) for measurements.

The index can be queried from Rust using the `geocode-reader` feature:

```toml
[dependencies]
pbfhogg = { version = "0.6", default-features = false, features = ["geocode-reader"] }
```

`Reader::query` and `Reader::candidates` return a `Result`: a non-finite or
out-of-range latitude or longitude is a typed `QueryError` rather than a
silently wrong cell lookup.

`Reader::query` answers each class (address, street, interpolation) with the
nearest record closer than the index's fine search radius, and for a class
with none there the nearest closer than its coarse radius, or nothing.
`Reader::candidates` lists every record closer than the fine radius, and for
a class with none there every one closer than the coarse radius. Both search
the whole radius, not only the cells next to the query's. Distances are the
reader's flat approximation: latitude and longitude differences in radians,
longitude scaled by the cosine of the query's latitude and taken the short
way around, times the Earth's mean radius.

The on-disk format carries a version number. Readers reject any version but
their own outright with a message naming `build-geocode-index` as the remedy,
so an index built by an earlier release must be rebuilt rather than migrated -
the index is a directory of regenerable derived files, not a document.

Every string an answer carries (names, house numbers, streets, postcodes,
country codes) is the tag value whole: a value holding a NUL byte, which PBF
allows, is not cut at it. A caller passing one to a C API or into XML has to
handle that byte itself.
