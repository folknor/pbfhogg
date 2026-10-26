# pbfhogg Pipeline Reference

This document has two parts:

1. **Pipeline inventory** - all read, write, and command pipelines in the codebase, how they compose, and which commands use them.
2. **Author's production pipeline** - the specific deployment that drives pbfhogg's development: planet-scale PBF refresh feeding tile generation and reverse geocoding.

---

## Core Infrastructure Pipelines

### Pipelined Read

**3-stage pipeline** - `src/read/pipeline.rs`, driven by `ElementReader`

| Stage | Thread | Work | Buffer |
|-------|--------|------|--------|
| 1. I/O | Dedicated | Read raw compressed blobs sequentially via `BlobReader` | 16-blob read-ahead channel |
| 2. Decode | Rayon pool (`nproc - 2` threads) | Decompress (thread-local `flate2::Decompress`) + parse `PrimitiveBlock` | `DecompressPool` recycles `Vec<u8>` buffers |
| 3. Reorder | Caller's thread | `ReorderBuffer` restores file order, delivers blocks to consumer | 32-slot decode-ahead |

Entry points:
- `ElementReader::for_each_pipelined()` - element-level callback
- `ElementReader::for_each_block_pipelined()` - owned `PrimitiveBlock` callback
- `ElementReader::into_blocks_pipelined()` - returns `Iterator<Item = Result<PrimitiveBlock>>`

Decode admission is bounded (commit `a0a2e3b`): at most `decode_ahead`
(default 32) decode tasks may be admitted but not yet delivered from the
reorder buffer, so decoded-block memory is a fixed working set instead of
growing with file size, and consumer backpressure propagates through the
admission gate to the reader thread. Early drop or an error from the block
closure stops the pipeline within ~`decode_ahead` blobs.

Used by most commands. See Sequential Read below for exceptions.

### Sequential Read

Single-threaded: `BlobReader` → decompress → `PrimitiveBlock` on the calling thread. ~6x slower than pipelined, but avoids cross-thread allocation/free churn.

Used by:
- `diff` / `derive-changes` (via `StreamingBlocks::new_sequential()`) - two files read in lockstep

`inspect tags` uses pread workers through `parallel_classify_phase`, with
per-blob tag counts merged by the caller.

### Ordered pread pool

**Workers in any order, results in schedule order, bounded** -
`src/ordered_pool.rs`

Every pool that hands results to its caller in schedule order runs on one
engine, `ordered_pool::run_ordered`: classify's ordered phase
(`parallel_classify_phase_ordered`: cat `--clean`, geocode pass 2, repack,
degrade, getparents, time-filter's snapshot path, tags-filter pass 2,
multi-extract's region writer), renumber's rewrite stages, extract's
`pread_execute`, multi-extract's node pass, and altw's sparse passthrough,
external stage 1 pass A and external stage 4. Workers claim tasks from an
atomic counter; a task may only be worked once it is inside a window of
the results the consumer has finished with, so at most a window of tasks is
admitted and not yet consumed however slow one blob is (the bound counts
tasks, not bytes). The default window is `ORDERED_POOL_WINDOW_PER_WORKER` per
worker with a floor of `ORDERED_POOL_WINDOW_MIN`. Results reach the consumer
through a channel no deeper than `POOL_RESULT_CHANNEL_DEPTH`, the depth the
hand-built pools used: a consumer-bound run blocks its workers in `send`,
as those pools did, and the window binds only behind a slow head, when the
consumer drains the channel into its reorder buffer while it waits. Worker
failures surface in schedule order (the lowest failing task's error, or the
consumer's own at its position), cancellation is checked on the consumer,
and any consumer exit closes the gate and drops the result receiver before
the scope joins. Work already admitted is not interrupted, but a failing run
calls the consumer's `abandon` first, which altw stage 4 uses to abort its
router so workers parked in its readiness wait are released before the
join. Every run records its gate, send and receive waits into one
process-wide set of stall counters (`ordered_pool_*_wait_ns`).

Where only some schedule positions are decoded and the rest are passthrough
frames the consumer copies itself (extract, multi-extract's node pass, altw
passthrough and stage 4), the pool runs over the decode items alone and the
consumer emits the passthrough positions between them (`Interleaved`), so
passthrough takes no worker, channel slot or window slot, and output and
failure order are the full schedule's. `commands::blob_merge` (sort,
`cat --dedupe`) streams each overlap run's blocks before the run finishes,
so it is not one result per task; it runs on the engine's streamed entry
point, `run_ordered_streams`. There each task has its own channel (an
overlap run's `OVERLAP_RUN_CHANNEL_DEPTH` blocks), and the consumer
dispatches: it issues a task by making its channel and queueing the sender
on a FIFO job queue, at most a window of tasks ahead of the one it is
reading, so no gate is involved. Its result send and receive waits go to
the same `ordered_pool_*_wait_ns` counters, and its workers' wait for a job
to `ordered_pool_job_lock_wait_ns` and `ordered_pool_job_recv_wait_ns`. The
read pipeline above keeps its own permit gate.

### Blob Filtering (pre-decode)

Skips entire blobs before decompression using metadata embedded in `BlobHeader`:

| Filter | Source | Effect |
|--------|--------|--------|
| Element type | `BlobIndex::kind` (`ElementKind`) (indexdata) | Skip blobs with wrong type (~85% reduction for single-type queries) |
| Tag key/prefix | Tagdata (BlobHeader field 4) | Skip blobs without required tag keys |
| Spatial bbox | Coordinate bounds in indexdata | Skip blobs outside bounding box (nodes only) |

**Every filter here believes the claim it filters on**; what a false one
costs per command is in [indexdata-trust.md](indexdata-trust.md). One
mechanism detail belongs here: `repack` and `degrade` schedule an indexed
blob into the one per-kind phase its indexdata names, and that phase
re-encodes only elements of its kind. Checking the kind per blob is not
possible in the worker, because the pool's classify closure receives only the
decoded block and cannot tell an indexed blob from an unindexed one that the
schedule builder replicated into all three kinds (a legitimate no-match);
checking it per element is a cost on a measured path. `cat --clean` has no
per-kind phases: it skips a blob whose indexdata names an unselected kind and
keeps every decoded element of a selected kind, whatever its blob claims.

Applied via `ElementReader::with_blob_filter()`. Used by `cat --type`, `tags-filter`, `extract`.
Only the pipelined iteration methods honour it; the sequential `for_each` and
`par_map_reduce` decode every blob and ignore a configured filter.

### Pipelined Write

**Parallel compression with sequential output** - `src/write/writer.rs`

| Stage | Thread | Work |
|-------|--------|------|
| 1. Frame + compress | Rayon pool | Per-thread `FrameScratch` (reusable buffers + lazy-init compressors). Zlib/zstd/none. |
| 2. Reorder + write | Dedicated writer thread | `ReorderBuffer` (32-slot write-ahead), writes to `FileWriter` |

Every pipelined constructor takes a `host::Executor` naming the rayon pool that runs stage 1 (`Ambient` or a caller-owned `Pool`); a writer driven from a worker of that pool waits for permits cooperatively instead of blocking.

Output modes:
- `PbfWriter::to_path()` - buffered I/O
- `PbfWriter::to_path_direct()` - O_DIRECT (Linux, `linux-direct-io` feature)
- `PbfWriter::to_path_uring()` - io_uring with registered buffers (Linux, `linux-io-uring` feature)

Each syncs its output when `flush()` completes. `PbfWriter::to_path_sync()`
is the synchronous counterpart: no pipeline and no executor, compression on
the calling thread into a 256 KiB buffer it owns, and the same sync on
`flush()`. `PbfWriter::new()` wraps a caller-owned `Write` and only calls its
`flush`.

Command-layer writers are different: they write an `Output`'s staging file
(`Output::pbf_writer` on any of the backends above, and multi-extract's
synchronous per-region writers over their staged files), finish the file at
`flush()` without syncing it, and leave the `fsync` to the output's
publication, which runs after the command's phase markers have ended
(ADR-0017).

Special: raw passthrough for unmodified blobs via `write_raw()` / `write_raw_chunks()` - zero decompression/recompression, uses `copy_file_range` on Linux.

### io_uring Writer

`src/write/uring_writer.rs` - replaces the buffered writer thread when `--io-uring` is set.

64 × 256 KB page-aligned registered buffers. Accumulates data, submits `WriteFixed` SQEs when a buffer fills, reaps CQEs to recycle buffers. Supports `CopyRange` for passthrough blobs.

Used by `sort`, `cat --dedupe`, `apply-changes`.

### Block Builder

`src/write/block_builder.rs` - accumulates elements into PBF blocks.

- Default element cap from `block_builder::DEFAULT_ELEMENT_CAP`, one element type per block
- `StringTable` with `FxHashMap<Rc<str>, u32>` dedup
- Reusable wire scratch buffers (`wire.rs` encoding primitives)
- Output: `OwnedBlock { bytes: Vec<u8>, index: BlobIndex, tagdata: Option<Vec<u8>>, way_members: Option<Vec<u8>> }` - serialized data + index + optional tagdata + optional BlobHeader field-5 way-member payload (`None` from every current producer)

Used by all commands that produce PBF output.

### Node-Only Wire Scanner

`src/scan/node.rs` - parses `DenseNodes` directly from decompressed wire format, bypassing `PrimitiveBlock` construction. Zero per-block heap allocation. Extracts `(id, lat, lon)` tuples.

Used internally by external join (stage 2) and ALTW sparse (pass 1).
Apply-changes uses its separate checked coordinate extractor in
`src/commands/apply_changes/node_coords.rs`.

---

## Command Pipelines

### cat (passthrough)

No `--type` filter: decompresses each blob to derive ID and tag metadata,
then reframes the original compressed payload via `reframe_raw_with_index()`.
The payload is not recompressed.

### cat --type

Sequential frame walk: an indexed blob of a selected kind is copied raw, one
of another kind is skipped, and a blob without indexdata is decoded, filtered
by element kind and re-encoded.

### cat --clean

One pass per input: a header walk schedules every data blob except those
whose indexdata names an unselected kind; pread workers decode each scheduled
blob once, keep the selected elements by actual kind, clean and re-encode them
through `BlockBuilder` and frame the output; the ordered pool
(`parallel_classify_phase_ordered`) merges in schedule order, so output order
is input order, with at most its admission window of results alive. Sidecar
phases: one `CAT_FILTERED_*` pair per input, with `CAT_NODES_*`,
`CAT_WAYS_*` and `CAT_RELATIONS_*` nested inside as write-side kind
transitions, not barriers (see `reference/performance.md` before comparing
them with the per-kind shape). Migrated
off the earlier `into_blocks_pipelined` + batch-budget path, then off one
phase per kind (see `src/commands/cat/mod.rs`).

### cat --dedupe

K-way sorted merge of multiple PBFs. Blob-level passthrough for non-overlapping ranges, decode + dedup for overlaps. All inputs must be sorted. The same engine as `sort` (below, `src/commands/blob_merge.rs`), over several inputs, dropping an element equal in (type, id, version) to the one just emitted; an absent version is 0.
Sidecar phases: `MERGEPBF_*` spans both passes (from after the input pins
and indexdata probes to the writer's flush, before publication), with
`MERGEPBF_PASS1_*` (index walk and plan) and `MERGEPBF_PASS2_*` inside it,
and `MERGEPBF_WRITE_LOOP_*` (the ordered write with the overlap sweeps
beside it, the pool's join included) and `MERGEPBF_FLUSH_*` (the writer
draining) inside pass 2, as `sort`'s `SORT_*` phases nest.

### sort

Two-pass blob-level permutation sort, through the engine `cat --dedupe`
shares (`src/commands/blob_merge.rs`).
1. Walk all blob headers (`HeaderWalker`, pread only), build index of
   (element_type, min_id, max_id). For
   inputs whose header does not declare `Sort.Type_then_ID` (including
   indexed inputs - blob indexdata alone does not prove intra-blob
   order), pass 1 decodes every blob payload and checks intra-blob ID
   monotonicity along the way, flagging any internally out-of-order
   blob for the rewrite path even when its range does not overlap its
   neighbours. Declared-sorted inputs keep the header-only fast path
   (index built from indexdata, no payload decode) and trust the
   header's claim.
2. Non-overlapping, intra-sorted blobs: raw passthrough, coalesced into
   one `copy_file_range` per contiguous stretch of an input where the
   output can splice. Overlapping or intra-unsorted blobs: decode → binary
   heap merge → re-encode, one sweep per run: an overlap component (a
   kind-bounded group of blobs whose ranges chain together), or one
   internally unsorted blob that overlaps nothing. Runs are
   swept on worker threads and stream their blocks to the write loop in
   run order through a bounded channel per run, with an admission window
   of `OVERLAP_RUN_WINDOW_PER_WORKER` runs per worker, so the re-encoded
   output buffered is bounded whatever the input. A sweep's heap is not:
   it holds whatever overlaps the current position, which one blob
   spanning a kind's whole ID range makes the whole kind; bounding that
   would take an external merge.

### export

Single sequential `ElementReader::for_each` pass, no parallelism. Rejects
up front if way export is requested and the header lacks
`LocationsOnWays`. Per element: tag filter → (for ways) geometry
assembly and area/winding classification → property serialization →
streamed to a `FeatureWriter` (`geojsonseq`: one Feature per line;
`geojson`: buffered into one FeatureCollection). Untagged nodes/ways and
ways with invalid geometry are skipped and counted, not errored.

### apply-changes (merge)

Descriptor-first streaming pipeline applying an OSC diff to a sorted PBF (`src/commands/apply_changes/`). OSC parsed into `CompactDiffOverlay` (arena-packed, `FxHashMap` index, defined in `src/osc/parse.rs`). `DiffRanges` (`diff_ranges.rs`) enables O(log n) overlap checks.

Stages:
1. **Scanner** (`scanner.rs`) - walks blob headers, emits a `BlobDescriptor` per blob classified as `Passthrough` or `Candidate` using indexdata + `DiffRanges`. Indexdata fast-path skips ~92% of blobs at Denmark scale.
2. **Workers** (`rewrite.rs`, `rewrite_block.rs`) - pull candidate descriptors, perform precise overlap check, emit rewritten `OwnedBlock`s or demote to false-positive passthrough.
3. **Drain / stream output** (`drain.rs`, `stream_output.rs`, `streaming.rs`) - results reordered to file order; consecutive passthroughs coalesced; gap creates interleaved at sorted positions.

Optional `--locations-on-ways` (`node_locations.rs`): preserves/updates inline way-node coordinates through the merge.

### diff / derive-changes

Two-pointer merge-join over two sorted PBFs. Three phases per element type (nodes, ways, relations). Each element compared by content equality (coordinates, tags, refs, members).

- Indexed inputs (`diff/shard.rs`, `diff/sharded.rs`, `osc/merge_join.rs`): a header walk of both files, then the block-pair merge over ID-range shards, one per job (one at `-j 1`, run on the calling thread; one for inputs holding negative IDs). Multi-shard phases run each shard on its own thread and concatenate the shards' scratch output in order. One engine and one sink per command (`DiffSink`, `ChangeSink`) at every shard count.
- Unindexed inputs (`diff/mod.rs`, `derive.rs`): `StreamingBlocks` element-level merge-join on the calling thread, through the same sinks.
- `diff` → text or summary output
- `diff --format osc` (derive-changes) → OSC XML output

### extract

Geographic extraction with three strategies:
- `--simple` - single pass, blob-filter by bbox, may leave dangling refs
- Default (complete-ways) - two passes: pass 1 collects way-referenced node IDs, pass 2 emits all
- `--smart` - three passes: also completes multipolygon/boundary relation members

Multi-extract via `--config` JSON: single pass producing multiple output files.
Node classification against N regions prunes candidates with a CSR grid
(`RegionGrid`, `src/read/region_grid.rs`; 3600x1800 cells of 0.1 degree over
the region bboxes) once `N >= 16` and the grid fits a 256 MiB coverage
budget; below either threshold it falls back to the linear per-region scan.
Output is identical either way - the grid only narrows which regions get
tested per node.

### tags-filter

Tag expression matching with dependency expansion:
1. Build `BlobFilter` from union of tag keys
2. Blob filter → pipelined decode → element matching
3. Default: matched relations pull in member ways/nodes transitively (multi-pass)
4. With `-R`: single pass, direct matches only

Also supports `--input-kind osc` for filtering OSC change files.

### getid / getparents

Both commands' include mode (the default query path, not `--invert`)
dispatch between two scan arms via the shared `dispatch_scan_arm`
(ADR-0006): a pread `HeaderWalker` walk (single-threaded, one pread per
blob header, skips blob bodies it doesn't need) or a full pipelined scan.
Non-indexed input is pinned to the walker arm. Indexed input estimates
the OSMData blob count and picks the walker below
`FULL_SCAN_ARM_MIN_BLOBS` (150,000 blobs), full scan at or above it -
the walker wins on low-blob-count planet-shaped encodings, the scan
wins once per-blob pread overhead dominates on high-blob-count
encodings (see `reference/blob-density.md`). `getid --invert`
(removeid) is pinned to the walker arm; its streaming full-scan
counterpart doesn't exist yet.

- `getid`: optional `--add-referenced` (two-pass, pulls in referenced nodes of matching ways).
- `getparents`: reverse-lookup of ways/relations referencing given IDs.

### add-locations-to-ways

Embeds node coordinates in ways. Index strategies (`--index-type`, default
`sparse`; `dense` was removed. See [performance.md](performance.md) for
measured comparisons):

**Sparse** (default): rank-indexed flat mmap. Builds an `IdSet` of
way-referenced nodes; pass 1 workers store packed `(lat, lon)` at byte
offset `rank_if_set(node_id) << 3` via relaxed `AtomicU64::store`
directly into a pre-allocated, `MmapMut`-mapped values file
(`referenced_count * 8` bytes) - no chunk format, no
serial writer bottleneck. Pass 2's `get(node_id)` is
`rank_if_set(node_id)` plus an `AtomicU64::load` at the same offset;
once pass 1 finishes, the store is advised `MADV_RANDOM` so pass 2's
genuinely-random per-ref lookups stop paying kernel readahead
speculation. The effect on disk reads and wall time depends on the dataset
and host; see [performance.md](performance.md) for measured results. Its
working-set limit depends on available page cache.

**External** (`altw/external/`, 4-stage bounded-memory join; requires
sorted + indexed input):
1. Way pass → emit `(node_id, slot_pos)` records into 256 node-id buckets
2. Node join → per bucket, sort by node id, merge-join against the node
   stream (pread workers), emit resolved coords into slot buckets
3. Slot reorder → per bucket, sort by slot_pos, emit blob-ordered
   delta-varint `coord_payloads`
4. Assembly → pread workers reframe way blobs, splicing coord payloads

Bounded memory with high scratch-disk demand. See
[performance.md](performance.md) for measured resource use. Requires sorted
input with indexdata.

Free space is checked once, between the way pass and the node join, on
Linux: the way pass yields the exact ref count, and the join then refuses
unless the scratch filesystem holds 23 bytes per ref (12-byte resolved
records plus at most 11 payload bytes per ref) plus per-file slack, on
top of the way pass's own ID shards already on disk. Indexdata counts
elements, not refs, so no earlier figure is exact without an extra
decode of every way blob; a full scratch filesystem during the way pass
itself still fails as an ordinary write error within that pass. The
final output's space is not included.

**Auto**: sparse is selected unless the input is sorted and indexed and the
estimated sparse store (the per-blob indexed node count times 8 bytes) exceeds
80 % of the page-cache budget. The budget is the host's available memory
minus 3 GiB of anonymous-memory headroom. Available memory comes from the
caller's `host::Resources`: `Fixed` resources supply it directly on any
platform, and the CLI's machine probe reads `MemAvailable` and, on Linux,
caps it by the smallest `memory.max - memory.current` headroom of the
process's cgroup v2 group and every ancestor visible through this mount
namespace. Charged page cache counts against that headroom, so the cap is
conservative. Cgroup v1 limits, `memory.high` and ancestors hidden above a
delegated mount are not observed; a cgroup v1-only host uses `MemAvailable`
alone.

The budget is unknown when `/proc/meminfo` or `/proc/self/cgroup` cannot be
read (every non-Linux target), when the process is in a unified hierarchy
but no `cgroup2` mount covering its group is visible, or when a limited
group's `memory.max` or `memory.current` cannot be read or parsed. The store
estimate's header walk refuses a partially indexed input (some OsmData blobs
indexed, others not) with the missing-indexdata error, so a sorted input
whose first data blob is indexed always gets an estimate. If the budget is
unknown, sorted and indexed inputs fall back to external, and the auto-select
note says which estimate was missing. Unsorted or unindexed inputs always
select sparse, since external cannot run on them. Whether the input is
indexed is read from the first data blob's indexdata, the same hint the
backends' own indexdata check reads, so a header-only file counts as indexed
and frames before the first data blob are skipped. An unreadable or malformed
frame header up to there is an input error, not a reason to pick sparse; the
data blob payloads are not read by the selection. This runtime rule follows the measured
store-versus-cache behavior documented in [performance.md](performance.md).

Sparse, not auto, is the default, and the docs recommend neither over the
other. The 80 % threshold is bracketed by two measured inputs on a single
host rather than fitted, and the unknown-estimate fallback sends sorted,
indexed input to external and its scratch-disk demand. Auto becoming the
default needs that calibration first.

### renumber

Multi-stage parallel rewrite using rank-indexed `IdSet` bitsets (not hashmaps) for old→new ID mapping (`src/commands/renumber/`):
1. **Pass 1** (`pass1.rs`) - parallel wire-format node rewriter across work-stealing workers; each shard populates an `IdSet` of seen node IDs.
2. **Stage 2** (`stage2.rs`, `wire_rewrite.rs`) - parallel way assembly; builds a separate `IdSet` for ways and remaps node refs via rank-indexed lookup.
3. **Relations** (`relations.rs`) - sequential collect of relation IDs, then parallel rewrite remapping member refs across all three kinds.

### time-filter

Two paths, picked from the header's `HistoricalInformation` (`src/commands/time_filter/mod.rs`):
- **History input**: single-pass grouped snapshot on the pipelined reader that read the header: maintains `PendingGroup` per object, emits latest version with `timestamp <= cutoff`, skips deleted (`visible=false`).
- **Snapshot input**: each object appears once, so blocks are independent: a classify schedule walk feeds `parallel_classify_phase_ordered`, each worker filters and frames one blob, and the ordered pool delivers them in file order within its window.

The input is pinned before the header read (see "One input, one file, across passes").

### inspect

Header-only scan (fast path on indexed PBFs - reads blob headers without decompression). Selective decode on demand for `--nodes`, `--blocks`, or element display.

Pinning (see "One input, one file, across passes") is decided per library entry: the scan pins when its index-only walk may run, since a data blob without indexdata sends it back to the start for a full decode; a decode-only scan (`--extended`, `--locations`, `--distrust-metadata`) opens once and takes no pin; `node_stats` pins (indexdata probe, then schedule walk); the `--get` header route (`inspect::inspect_header`) opens once and takes `file.size` from that open's descriptor. The CLI pins once itself when `--indexed` or `--nodes` joins the scanning report, and hands the pin to every call.

### check

- `--ids`: sequential scan checking ID uniqueness and ordering. Optional `--full` bitmap for duplicate detection.
- `--refs`: referential integrity via `IdSet` (custom chunked bitset, `src/idset.rs`). Optional `--check-relations`.

Pinning (see "One input, one file, across passes"): `verify_ids` pins (header and indexdata probe, then schedule walk) and takes its `--full` allocation size from the pin. `check_index` reads front to back through one open and `check_refs` opens once (its schedule walk, sizing its allocation from an fstat of that descriptor), so neither pins of its own; `check_index` alone still reads a FIFO, while `check_refs` reads by position and cannot. All three take an `InputSource`. The CLI pins once itself when two or more checks run (a bare `check` is `--ids` then `--refs`), and hands the pin to every check.

### build-geocode-index

4-pass build pipeline:
1. Relations (admin boundaries)
2. Referenced node collection (`IdSet`)
3. Nodes + ways fused scan (compact rank-indexed coord array, streaming data files)
4. Bucketed S2 cell assignment (256 temp-file buckets per level)

Outputs 19 binary files. Self-contained module in `src/geocode_index/`.

### merge-changes

OSC-only: merges multiple OSC XML files into one. Optional `--simplify` keeps only the last change per object. No PBF I/O.

---

## Author's Production Pipeline

_The following describes the specific deployment that drives pbfhogg's development. It documents how the author uses pbfhogg in a planet-scale OSM refresh pipeline feeding tile generation and reverse geocoding. It is not part of the library's public API or general documentation - it records operational context, allocation budgets, and performance measurements specific to this ecosystem._

**Production pipeline** (ratified 2026-07-10):

```
Bootstrap (once):  pbfhogg cat → indexed planet PBF
Daily refresh:     pbfhogg apply-changes                      (plain merge)
                   pbfhogg add-locations-to-ways              (external, re-enrichment)
                   pbfhogg build-geocode-index                (rebuild)
                                      │
                                      ├── elivagar → PMTiles → nidhogg (tile serving)
                                      └── nidhogg (PBF ingest + reverse geocoding)
```

**altw runs in the daily loop, not once at bootstrap.** The original
design ran `add-locations-to-ways` once and maintained inline way-node
coordinates through daily diffs via `apply-changes --locations-on-ways`.
That architecture dates from when altw meant the dense path at ~96 min
planet; post-A1 external is ~9 min (546.0 s, UUID `7fd04130`, commit
`16e3694`, plantasjen), and the daily loop already carries a post-merge
rebuild of the same magnitude (`build-geocode-index`, ~7 min planet).
Re-running altw after each merge was ratified 2026-07-10 because it keeps
every enrichment fresh each cycle:

- `LocationsOnWays` inline coordinates, and
- the injected-prepass fields (`pbfhogg.WayMembers-v1` BlobHeader field 5,
  `pbfhogg.SharedNodePins-v1` Way field 20 - landed 2026-07-11, see
  `decisions/0007-injected-prepass-wire-extensions.md`), which cannot be maintained
  incrementally through `apply-changes`: stale field 5 is a
  false-negative membership risk (wrong tiles downstream), and field 20
  needs exact global ref counts with decrement.

Consequences:

- The production merge drops `--locations-on-ways`. The feature stays in
  the library and CLI for users who do not re-enrich; it is just no
  longer load-bearing for this pipeline.
- Enrichment injection is flag-gated (`add-locations-to-ways
  --inject-prepass`); default altw output stays byte-identical, brokkr
  passes the flag when enriching, and sparse implements both fields too
  so the backend-parity canary keeps covering them (decision 2 of the
  same note, ratified alongside).
- Daily write volume roughly doubles at planet: apply-changes rewrites
  ~92% of blobs (~90 GB output) and altw writes the full enriched file
  again. Wall stays comfortable (~17-18 min for the whole loop); NVMe
  endurance is the number to watch if the host changes.

**`sort` is not in the pipeline.** Geofabrik and planet PBFs are always `Sort.Type_then_ID`, and every pipeline step preserves sorted order: `cat` copies blobs in input order, `merge` interleaves upserts at sorted positions, `add-locations-to-ways` passes through or decodes without reordering. The `sort` command exists for repairing unsorted PBFs from other tools (osmosis, custom exporters) - a one-time fix, not a recurring step.

The two downstream consumers are:
- **elivagar** (`~/Programs/elivagar`) - vector tile generator. Reads the enriched PBF (with inline way coordinates via `Way::node_locations()`) to produce PMTiles. Pre-processing with `add-locations-to-ways` eliminates elivagar's node store (~44 GB at planet scale), dropping peak RSS from ~65-75 GB to ~15-20 GB.
- **nidhogg** (`~/Programs/nidhogg`) - planet refresh service. Reads the planet PBF for data ingest, then merges daily OSC diffs to keep it current.

## Downstream Consumers

**Elivagar** - tile generation (read-only)
- `Cargo.toml`: `pbfhogg = { path = "../pbfhogg" }` (default features = `commands`)
- Only uses the **read pipeline**; no writes
- Locations-on-ways path (production shape): elivagar's own bounded read
  loop over `BlobReader` + `Blob::to_primitiveblock()` - Option 3 of the
  decode-backpressure decision (`reference/performance-history.md`
  "Pipelined-reader decode-admission bound"). The raw-Geofabrik path stays
  on the legacy pipelined reader.
- With injected-prepass landed 2026-07-11, that loop sets `parse_waymembers`
  on `BlobReader` when the header declares `pbfhogg.WayMembers-v1` and
  consumes `Blob::way_members()` / `Way::shared_node_pins()` (their
  Bricks 4/5).
- API surface: `node.id()`, `.decimicro_lat()`, `.decimicro_lon()`, `.tags()`, `way.id()`, `.refs()`, `.node_locations()`, `.tags()`, `rel.id()`, `.tags()`, `.members()`
- `Way::node_locations()` yields `WayNodeLocation` (lat/lon) from enriched PBFs - eliminates the node coordinate store entirely (~44 GB at planet; peak RSS ~65-75 GB → ~15-20 GB)
- Also uses `protohoggr` directly for MVT/PMTiles protobuf encoding (unrelated to PBF I/O)

**Nidhogg** - planet refresh pipeline (read + merge)
- `Cargo.toml`: `pbfhogg = { path = "../pbfhogg" }` (default features = `commands`)
- **Read path**: `ElementReader::from_path()` → `.for_each_pipelined(|element| ...)` - two-pass ingest
  - File: `~/Programs/nidhogg/src/ingest/mod.rs:72-324`
- **Merge path**: delegates entirely to `pbfhogg::apply_changes::merge(base, osc, output, &MergeOptions { .. })`
  - File: `~/Programs/nidhogg/src/merge.rs:6`
  - Currently: zlib compression, the default `WriteOptions` (no direct I/O or io_uring), no locations_on_ways
  - `locations_on_ways` stays `false` by design: the ratified steady
    state re-runs `add-locations-to-ways` after each merge instead of
    maintaining coordinates through the diff (the earlier TODO to enable
    it is retired)
- **No direct BlockBuilder/PbfWriter usage** - nidhogg never constructs PBF blocks itself
- Also reads PBF headers via `BlobReader::from_path()` → `.to_headerblock()` for replication state
  - File: `~/Programs/nidhogg/src/update.rs:95-114`

## Cargo Features in Play

Both consumers use default features. In practice:
- `commands` feature: **enabled** (brings in `roaring`, `serde_json`, `s2` - needed for merge + geocode builder)
- `geocode-reader` feature: **implied by `commands`**. nidhogg can alternatively depend on just `geocode-reader` for the reverse geocoding reader without pulling in `roaring`/`serde_json`.
- `linux-direct-io`: **disabled** (nidhogg passes `false`)
- `linux-io-uring`: **disabled** (nidhogg passes `false`)
- Neither feature gates passthrough splicing (`copy_file_range`), the
  sequential-readahead hint or altw scratch eviction: those run in every
  Linux build, so the consumers get them with default features.
- Zlib backend: **zlib-rs** (hardcoded, pure Rust, via flate2)
- Zstd: available but **unused** - nidhogg hardcodes `Compression::Zlib(6)`

## One input, one file, across passes

A command that reads its input in several passes opens it once per pass or
access pattern, and keeps those opens separate: `posix_fadvise` advice and
readahead state belong to an open file description, so the header walker's
`FADV_RANDOM` and a buffered pass's `FADV_SEQUENTIAL` stay on their own
descriptions (a description shared today, such as the classify schedule's
walker descriptor handed to its worker pool, stays shared). Opening the path
each time would resolve it each time, so a file renamed over the input
mid-run would be read partly from each file.

`read::input::PinnedInput` is the guard. A command pins each input once, at
its start, and every later open of it is an `InputSource::Pinned` open:

- On Linux, when `/proc/self/fd` can reopen the pin (checked with a real
  open when the pin is taken), each open is a new description of the pinned
  file itself. A replaced or unlinked input does not affect the run.
- Otherwise each open goes through the path, non-blocking, and an open that
  finds a different file there (device and inode) fails the run with
  `ErrorKind::InputReplaced` instead of mixing files. A device put in the
  input's place still sees that open before it is refused.

What a pin does not catch: a change behind the same file (an in-place
rewrite or truncation), and a replacement before the pin is taken. Only a
regular file can be pinned; a block device (its `st_size` is zero, which the
header walker would read as empty), FIFO, socket or character device is
refused when the pin is taken. On Unix each pin holds one descriptor for the run. Off Unix,
where `std` reports no device and inode, a pin holds only the path and
nothing is verified. One-shot opens
(`InputSource::Path`), and scratch files, resolve the path as before.

A library entry point may take its input as an `InputSource` and turn it
into a pin with `InputSource::pin`: a path is pinned there, a caller's pin is
shared as it is (keeping the host it was taken under). That lets a caller
that does other work on the run's inputs first pin them all up front; the
getid and getparents CLIs pin their input, then their `-I` PBF, before
reading any ID file. `has_indexdata` opens once and does not pin: handed a
pin, it probes through it.

`cat`, which takes any number of inputs, pins them all, in argument order,
before it opens any (`read::input::pin_sources`, which uses a caller's pin
as it is, as `diff` does below). An argument spelled exactly
as an earlier one shares that argument's pin, so `cat a a` reads one file
twice rather than two files if `a` is replaced while the pins are taken;
other spellings of one file are pinned separately. Each argument's opens
stay its own. Since its input count is unbounded, `cat` first asks the host
for the descriptors it will hold (`ProcessActions::fd_allowance`) and
refuses the run up front when allowed fewer: plain `cat` holds its pins,
one per distinct input, for the whole run; `cat --dedupe` asks for one per
argument, because its second pass holds a source per argument, opened from
the pins, each pin dropped as its sources open. Both ask for a fixed
allowance on top, for the writer and one transient open.

`diff` pins its two inputs the same way, `old` first, once its options are
accepted and before either is read, so `diff a a` compares one pinned file
with itself. Its entry points take each input as an `InputSource`, and
`read::input::pin_sources` keeps a caller's pin apart from the spelling
rule: a pin is used as it is, only paths share a pin with an earlier path
spelled the same, and a path is never folded into a caller's pin of the
same path (the two may name different files on purpose). With a fixed two
inputs, `diff` asks the host for no descriptors. A run refused at a later
open (`InputReplaced`) may already have written text to the caller's
writer; the CLI publishes a named output only after the run succeeds.
`derive_changes` (`diff --format osc`) pins its pair through the same
rule, after its output-name refusal; it owns its `Output` and writes it
only after every input open, so a refused run publishes nothing.

A command whose route is known before it opens anything pins only the
routes that reopen. `degrade` chooses its route from the flags alone: the
decode path pins the input and derives its three opens (the indexdata
probe, the header reader, the schedule walk) from the pin, while the
header-only passthrough reads the header and every data frame through one
open and takes no pin of its own, so it reads a FIFO. A caller's pin is
honoured on both routes. A command that learns from the input itself
whether it will reopen has to pin before that read instead: `time-filter`
picks its path from the header's `HistoricalInformation`, and only the
snapshot path opens the input again (its schedule walk), so it pins before
the header read and refuses a FIFO on the history path too, which reads
once. `inspect`'s scan is the same case: whether its index-only walk falls
back to a full decode is learned from the walk, so it pins before the walk,
while a scan the options make decode-only reads once and takes no pin.
Where a front end composes several entry points on one input, it pins once
and hands the pin to each (the `inspect` CLI does, when `--indexed` or
`--nodes` joins the scan; the `check` CLI does, when more than one check
runs).

A pass whose reads a later pass repeats can be folded into that pass
instead of pinned. `digest` checks the blob envelope (unknown blob types,
each blob's indexdata claims, the partial-index refusal) in its one
decoding pass rather than in a header walk ahead of it, through
`ElementReader::for_each_fused_blob`, whose transform sees each blob's
kind, indexdata and provenance as well as its block. It takes no pin of
its own, reads a FIFO, and opens a caller's pin as it is.

## Read-path internals (both consumers)

All source PBFs are zlib-compressed (Geofabrik/AWS). Every read decompresses.

**3-stage pipeline** - `src/read/pipeline.rs`
1. **I/O thread**: reads raw compressed blobs (~32KB each) sequentially, sends via channel (READ_AHEAD=16 slots)
2. **Rayon decode pool**: parallel decompress + parse. Thread count = `available_parallelism() - 2`
   - `decompress_blob()` - thread-local `flate2::Decompress` with `reset(true)`, reused per thread
   - `DecompressPool` - returns `Vec<u8>` to pool on drop instead of freeing
   - `PooledBuffer` wrapper - custom Drop via `Bytes::from_owner`
3. **Reorder buffer**: `VecDeque<Option<PrimitiveBlock>>` restores file order

**PrimitiveBlock ownership** - `src/read/block.rs`:
- Owns `Bytes` buffer (decompressed ~1.4 MB) + `WireBlock<'static>` (self-referential, unsafe transmute)
- `WireBlock::parse()` - builds `WireStringTable` and `group_ranges` as inline (offset, count) into the decompressed buffer - zero separate allocation
- `to_primitiveblock_inline()` with pool recycling: reuses `PrimitiveBlock` across blobs (string table Vec, group Vecs) via `clear_and_reuse()`

**Element iteration** - zero-copy, zero-alloc:
- `WireGroup` lazy scanner - scans protobuf on-the-fly
- Tag/ref iterators use `PackedSint64Iter`/`PackedUint32Iter` from protohoggr - decode varints from raw bytes, no Vec

**Key allocation sites per blob (read path):**
- Decompression buffer: ~1.4 MB (pooled, reused)
- `WireStringTable`: inline (offset, count) into decompressed buffer - zero separate allocation
- `WireBlock` group_ranges: inline (offset, count) into decompressed buffer - zero separate allocation
- Element wrappers: stack-allocated (~24 bytes each)

## Merge passthrough ratios (measured)

The apply-changes descriptor pipeline (see Command Pipelines above) is
passthrough-dominated on small extracts and rewrite-dominated at planet:

- Denmark (465 MB, ~300K changes): ~92% passthrough, ~8% rewrite
- Germany (4.5 GB, ~146K changes): ~82% passthrough, ~18% rewrite
- Planet (~90 GB, daily diff): ~8% passthrough, ~92% rewrite (most blobs touched)

The planet ratio is why the daily loop's write volume is effectively one
full file per step: passthrough saves little there.

## Write Side (shared by merge rewrite + cat filtered + add-locations-to-ways)

**BlockBuilder** - `src/write/block_builder.rs`:
- Dense node vectors pre-allocated up to the requested cap and preallocation budget
- String table: `FxHashMap<Rc<str>, u32>` + `Vec<Rc<str>>` - one `Rc<str>` alloc per unique string, shared between map key and vec entry
- Wire scratch buffers: `group_buf`, `elem_scratch`, `packed_scratch`, `info_scratch` - grow once, reused
- `encode_buf` - serialized output, moved via `std::mem::take` (zero copy)
- `take_owned()` - returns `(Vec<u8>, BlobIndex, Option<Vec<u8>>)`

**PbfWriter** - `src/write/writer.rs`:
- `FrameScratch` - `blob_buf`, `header_buf`, `compress_buf`, lazy-init `Compress`/`zstd::Compressor`
- Thread-local `PIPELINE_SCRATCH` - per-rayon-thread, reused across blobs
- `frame_in_scratch()` - the one framing implementation (size guards, header encoding, metrics); frames into the scratch in place
- `frame_blob_into()` - pipelined hand-off: moves the framed header and body out of the scratch, leaving two replacement buffers at the warmed capacity (capped by the retention limit); no copy
- `write_framed_blob()` - sync hand-off: writes the parts straight from the scratch, no per-blob allocation after warmup
- `compress_zlib()` - `flate2::Compress` with `reset()`, reuses `compress_buf`
- Pipelined writer thread - VecDeque reorder buffer, `WRITE_AHEAD` =
  `PIPELINE_DISPATCH_PERMITS` (64). The channel is deliberately not the
  memory bound - the permit pool is - and sizing it below the permit count
  lets a rayon framer block in `send` while holding the earlier-seq task the
  writer thread is waiting for. A compile-time assertion pins the relation.

## Error propagation and provenance

Every fallible library function returns `pbfhogg::Result`. An error is an
`ErrorKind` (`Io`, `PlatformUnsupported`, `InvalidArgument`, `InvalidData`,
`Internal`, `External`, plus the PBF-structure kinds), optional context text,
an optional `Location` (file, blob ordinal, byte offset) and, where the cause
has a type of its own, that cause as `source()`. `Error` is `Clone`, so a
first-failure slot or a cross-thread channel carries the error itself.

**Rule: a typed error keeps its kind and its cause for as long as it travels.**

- Propagate with `?`. Add words with `Error::context`, which never changes the
  kind. Reclassify only at a deliberate boundary (a config bbox that fails a
  range check becomes `InvalidData` for that config, with the leaf error as the
  cause), never as a side effect of passing an error along.
- Text is produced in two places only: the final rendering (`Error::report`,
  and the CLI's print of it) and the message of a caught panic
  (`panic_error`). Nothing in between turns an error into a `String`, an
  `io::Error::other`, a `Box<dyn Error>` or an `Error` built from
  `format!("... {e}")`. What enforces this is chiefly the type system:
  `Error` has no `From<String>` or `From<&str>`, so a string cannot become an
  error by `?` or `.into()`, and every constructor that takes text says which
  kind it is. The `brokkr.toml` textlint rules `no-io-error-other`,
  `no-io-error-new-other`, `no-io-error-restringing`, `no-map-err-to-string`,
  `no-string-error-result`, `no-box-dyn-error`, `no-error-kind-construction`
  and `no-error-interpolated-into-message` are guardrails over `src/` against
  the common flattening shapes (the interpolation rule sees single-line
  constructors only); they cannot catch every flattening. `src/error.rs` is the
  one file exempt, because it owns the constructors and the `io::Error`
  carrier.
- Decisions that depend on an OS error (both `copy_file_range` loops falling
  back to userspace on the errnos `copy_range::kernel_copy_refused` names, the
  shared `EINTR` retry in `copy_range::retry_eintr`, `sync_file` tolerating an
  unsupported fsync, classifying a failed `O_DIRECT` open) look at the original
  `io::Error` before it is converted. Once it is a `pbfhogg::Error`, the kind
  is the answer.
- An error returned through a standard `Write`/`Read`/`Seek` implementation is
  an `io::Error`; a `pbfhogg::Error` crossing that boundary rides inside it as
  a carrier, and `Error::from(io::Error)` and `report()` see through the carrier
  to the original.

**Provenance.** The code that walks the file owns it. `HeaderWalker` hands out
the blob's true ordinal and frame start (`BlobHeaderMeta`); a pool that decodes
what the walker scheduled keeps those two numbers in its work item, captures
the input path once per worker, and stamps a failure with
`Error::locate(Location::new(Some(path), Some(blob_index), Some(frame_start)))`.
`locate` applies only when the error has no location yet and applies as a unit,
so the innermost, most precise site wins. A pipeline sequence number is never a
location: it counts a filtered subset, not the file. Errors that came out of a
caller-supplied callback are never located by the library. Pools whose work
items are `(seq, offset, size)` schedules (the pread classify pools) carry only
the frame offset or nothing; they do not invent a blob ordinal.

**Exit codes.** The CLI picks exit `3` by walking the error chain for
`ErrorKind::PlatformUnsupported`, through context, `io::Error` carriers and
`External` wrappers, not by matching text. A command that runs several stages
on several threads must therefore hand the typed error across the join, not a
string.

## Compression Summary

| Path | Decompress | Compress | Backend |
|---|---|---|---|
| Read (all ingest) | every blob | - | zlib-rs via flate2 |
| Merge passthrough | none (~92% DK) | none | - |
| Merge rewrite | yes | zlib:6 | zlib-rs via flate2 |
| Cat passthrough | none | none | - |
| Cat filtered | every blob | zlib (default) | zlib-rs via flate2 |
| add-locations-to-ways passthrough | none (~89% keep-untagged) | none | - |
| add-locations-to-ways decode | yes (way blobs + filtered nodes) | zlib (default) | zlib-rs via flate2 |

## Benchmark context (daily loop at planet, plantasjen)

The measurement record is `reference/performance.md` (current baselines
and per-command breakdowns), `reference/performance-history.md` (arcs
and retrospectives), and `.brokkr/results.db`. Headline planet figures
for the ratified daily loop:

| Step | Wall | Pin |
|---|---:|---|
| `apply-changes` (zstd:1, cross-disk, parallel pwrite) | 80.9 s | performance.md apply-changes table |
| `add-locations-to-ways --index-type external` | 546.0 s | UUID `7fd04130`, commit `16e3694` |
| `build-geocode-index` | 432.9 s | 2026-04-18 arc, performance.md |

Total ~17-18 min per daily refresh on a 30 GB-class host. The
injected-prepass flag (landed 2026-07-11) must stay inside the standing
**external <= 3% regression bound** (~16 s against the 546.0 s
baseline); the flag-on planet verdict awaits the brokkr
`--inject-prepass` passthrough and an explicit green-light, and the
enriched run becomes its own brokkr variant with its own recorded
price.
