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
- `tags-count` - avoids 25+ GB heap retention at planet scale

### Blob Filtering (pre-decode)

Skips entire blobs before decompression using metadata embedded in `BlobHeader`:

| Filter | Source | Effect |
|--------|--------|--------|
| Element type | `BlobIndex::ElemKind` (indexdata) | Skip blobs with wrong type (~85% reduction for single-type queries) |
| Tag key/prefix | Tagdata (BlobHeader field 4) | Skip blobs without required tag keys |
| Spatial bbox | Coordinate bounds in indexdata | Skip blobs outside bounding box (nodes only) |

Applied via `ElementReader::with_blob_filter()`. Used by `cat --type`, `tags-filter`, `extract`.

### Pipelined Write

**Parallel compression with sequential output** - `src/write/writer.rs`

| Stage | Thread | Work |
|-------|--------|------|
| 1. Frame + compress | Rayon pool | Per-thread `FrameScratch` (reusable buffers + lazy-init compressors). Zlib/zstd/none. |
| 2. Reorder + write | Dedicated writer thread | `ReorderBuffer` (32-slot write-ahead), writes to `FileWriter` |

Output modes:
- `PbfWriter::to_path()` - buffered I/O
- `PbfWriter::to_path_direct()` - O_DIRECT (Linux, `linux-direct-io` feature)
- `PbfWriter::to_path_uring()` - io_uring with registered buffers (Linux, `linux-io-uring` feature)

Special: raw passthrough for unmodified blobs via `write_raw()` / `write_raw_chunks()` - zero decompression/recompression, uses `copy_file_range` on Linux.

### io_uring Writer

`src/write/uring_writer.rs` - replaces the buffered writer thread when `--io-uring` is set.

64 × 256 KB page-aligned registered buffers. Accumulates data, submits `WriteFixed` SQEs when a buffer fills, reaps CQEs to recycle buffers. Supports `CopyRange` for passthrough blobs.

Used by `sort`, `cat --dedupe`, `apply-changes`.

### Block Builder

`src/write/block_builder.rs` - accumulates elements into PBF blocks.

- Max 8000 entities per block, one element type per block
- `StringTable` with `FxHashMap<Rc<str>, u32>` dedup
- Reusable wire scratch buffers (`wire.rs` encoding primitives)
- Output: `OwnedBlock { bytes: Vec<u8>, index: BlobIndex, tagdata: Option<Vec<u8>>, way_members: Option<Vec<u8>> }` - serialized data + index + optional tagdata + optional BlobHeader field-5 way-member payload (`None` from every current producer)

Used by all commands that produce PBF output.

### Node-Only Wire Scanner

`src/scan/node.rs` - parses `DenseNodes` directly from decompressed wire format, bypassing `PrimitiveBlock` construction. Zero per-block heap allocation. Extracts `(id, lat, lon)` tuples.

Used internally by external join (stage 2), ALTW dense/sparse (pass 1), and merge `--locations-on-ways`.

---

## Command Pipelines

### cat (passthrough)

No `--type` filter: reads raw blob frames, adds indexdata via `reframe_raw_with_index()`, writes raw. Zero decompression.

### cat --type / --clean

Blob filter → pread-worker parallel decode + reframe, file order restored by a `ReorderBuffer` → element-level type check / clean → `BlockBuilder` re-encode → write. Migrated off the earlier `into_blocks_pipelined` + batch-budget path (see `src/commands/cat/mod.rs`).

### cat --dedupe

K-way sorted merge of multiple PBFs. Blob-level passthrough for non-overlapping ranges, decode + dedup for overlaps. All inputs must be sorted.

### sort

Two-pass blob-level permutation sort.
1. Scan all blobs, build index of (element_type, min_id, max_id)
2. Non-overlapping blobs: raw passthrough. Overlapping blobs: decode → binary heap merge → re-encode.

### apply-changes (merge)

Descriptor-first streaming pipeline applying an OSC diff to a sorted PBF (`src/commands/apply_changes/`). OSC parsed into `CompactDiffOverlay` (arena-packed, `FxHashMap` index, defined in `src/osc/parse.rs`). `DiffRanges` (`diff_ranges.rs`) enables O(log n) overlap checks.

Stages:
1. **Scanner** (`scanner.rs`) - walks blob headers, emits a `BlobDescriptor` per blob classified as `Passthrough` or `Candidate` using indexdata + `DiffRanges`. Indexdata fast-path skips ~92% of blobs at Denmark scale.
2. **Workers** (`rewrite.rs`, `rewrite_block.rs`) - pull candidate descriptors, perform precise overlap check, emit rewritten `OwnedBlock`s or demote to false-positive passthrough.
3. **Drain / stream output** (`drain.rs`, `stream_output.rs`, `streaming.rs`) - results reordered to file order; consecutive passthroughs coalesced; gap creates interleaved at sorted positions.

Optional `--locations-on-ways` (`node_locations.rs`): preserves/updates inline way-node coordinates through the merge.

### diff / derive-changes

Two-pointer merge-join over two sorted PBFs. Three phases per element type (nodes, ways, relations). Each element compared by content equality (coordinates, tags, refs, members).

- Sequential path (`diff/mod.rs`, `derive.rs`): `StreamingBlocks` merge-join on the calling thread.
- Sharded parallel path (`diff/parallel.rs`, `derive_parallel.rs`): `DiffOptions::num_shards >= 2` partitions the ID space across shards and dispatches per-shard merge-joins in parallel.
- `diff` → text or summary output
- `diff --format osc` (derive-changes) → OSC XML output

### extract

Geographic extraction with three strategies:
- `--simple` - single pass, blob-filter by bbox, may leave dangling refs
- Default (complete-ways) - two passes: pass 1 collects way-referenced node IDs, pass 2 emits all
- `--smart` - three passes: also completes multipolygon/boundary relation members

Multi-extract via `--config` JSON: single pass producing multiple output files.

### tags-filter

Tag expression matching with dependency expansion:
1. Build `BlobFilter` from union of tag keys
2. Blob filter → pipelined decode → element matching
3. Default: matched relations pull in member ways/nodes transitively (multi-pass)
4. With `-R`: single pass, direct matches only

Also supports `--input-kind osc` for filtering OSC change files.

### getid / getparents

- `getid`: indexed seek (via `IndexedReader`) or full scan. Optional `--add-referenced` (two-pass) and `--invert` (removeid).
- `getparents`: full scan, reverse-lookup of ways/relations referencing given IDs.

### add-locations-to-ways

Embeds node coordinates in ways. Index strategies (`--index-type`, default
`sparse`; `dense` was removed 2026-04 - sparse is faster at every measured
scale):

**Sparse** (default): rank-indexed flat file. Builds an `IdSet` of
way-referenced nodes; workers pwrite packed `(lat, lon)` at byte offset
`rank << 3` into a values file (`referenced_count * 8` bytes, ~29 GB at
europe), read back via mmap with batched sorted access. Small-to-europe
scale; likely thrashes at planet.

**External** (`altw/external/`, 4-stage bounded-memory join; requires
sorted + indexed input):
1. Way pass → emit `(node_id, slot_pos)` records into 256 node-id buckets
2. Node join → per bucket, sort by node id, merge-join against the node
   stream (pread workers), emit resolved coords into slot buckets
3. Slot reorder → per bucket, sort by slot_pos, emit blob-ordered
   delta-varint `coord_payloads`
4. Assembly → pread workers reframe way blobs, splicing coord payloads

~8.7 GB RAM, ~224 GB temp disk at planet; the only mode that survives
planet on a 30 GB-class host.

**Auto**: external if sorted + indexed, sparse otherwise.

### renumber

Multi-stage parallel rewrite using rank-indexed `IdSet` bitsets (not hashmaps) for old→new ID mapping (`src/commands/renumber/`):
1. **Pass 1** (`pass1.rs`) - parallel wire-format node rewriter across work-stealing workers; each shard populates an `IdSet` of seen node IDs.
2. **Stage 2** (`stage2.rs`, `wire_rewrite.rs`) - parallel way assembly; builds a separate `IdSet` for ways and remaps node refs via rank-indexed lookup.
3. **Relations** (`relations.rs`) - sequential collect of relation IDs, then parallel rewrite remapping member refs across all three kinds.

### time-filter

Single-pass grouped snapshot: maintains `PendingGroup` per object, emits latest version with `timestamp <= cutoff`, skips deleted (`visible=false`).

### inspect

Header-only scan (fast path on indexed PBFs - reads blob headers without decompression). Selective decode on demand for `--nodes`, `--blocks`, or element display.

### check

- `--ids`: sequential scan checking ID uniqueness and ordering. Optional `--full` bitmap for duplicate detection.
- `--refs`: referential integrity via `IdSet` (custom chunked bitset, `src/idset.rs`). Optional `--check-relations`.

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

---

## Author's Production Pipeline

_The following describes the specific deployment that drives pbfhogg's development. It documents how the author uses pbfhogg in a planet-scale OSM refresh pipeline feeding tile generation and reverse geocoding. It is not part of the library's public API or general documentation - it records operational context, allocation budgets, and performance measurements specific to this ecosystem._

**Production pipeline** (ratified 2026-07-10; steady-state shape is
decision 1 / option (a) of `notes/injected-prepass.md` section 7):

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
- Enrichment injection is flag-gated (working name `--inject-prepass`);
  default altw output stays byte-identical, brokkr passes the flag when
  enriching, and sparse implements both fields too so the backend-parity
  canary keeps covering them (decision 2 of the same note, ratified
  alongside).
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
  - Currently: zlib compression, no direct_io/io_uring, no locations_on_ways
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
- Zlib backend: **zlib-rs** (hardcoded, pure Rust, via flate2)
- Zstd: available but **unused** - nidhogg hardcodes `Compression::Zlib(6)`

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
- Dense node vectors pre-allocated to 8000 (MAX_ENTITIES_PER_BLOCK)
- String table: `FxHashMap<Rc<str>, u32>` + `Vec<Rc<str>>` - one `Rc<str>` alloc per unique string, shared between map key and vec entry
- Wire scratch buffers: `group_buf`, `elem_scratch`, `packed_scratch`, `info_scratch` - grow once, reused
- `encode_buf` - serialized output, moved via `std::mem::take` (zero copy)
- `take_owned()` - returns `(Vec<u8>, BlobIndex, Option<Vec<u8>>)`

**PbfWriter** - `src/write/writer.rs`:
- `FrameScratch` - `blob_buf`, `header_buf`, `compress_buf`, lazy-init `Compress`/`zstd::Compressor`
- Thread-local `PIPELINE_SCRATCH` - per-rayon-thread, reused across blobs
- `frame_blob_into()` - allocates **one Vec per blob** for final framed output (exact capacity)
- `compress_zlib()` - `flate2::Compress` with `reset()`, reuses `compress_buf`
- Pipelined writer thread - VecDeque reorder buffer (WRITE_AHEAD=32)

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
