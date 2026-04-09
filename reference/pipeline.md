# pbfhogg Pipeline Reference

This document has two parts:

1. **Pipeline inventory** — all read, write, and command pipelines in the codebase, how they compose, and which commands use them.
2. **Author's production pipeline** — the specific deployment that drives pbfhogg's development: planet-scale PBF refresh feeding tile generation and reverse geocoding.

---

## Core Infrastructure Pipelines

### Pipelined Read

**3-stage pipeline** — `src/read/pipeline.rs`, driven by `ElementReader`

| Stage | Thread | Work | Buffer |
|-------|--------|------|--------|
| 1. I/O | Dedicated | Read raw compressed blobs sequentially via `BlobReader` | 16-blob read-ahead channel |
| 2. Decode | Rayon pool (`nproc - 2` threads) | Decompress (thread-local `flate2::Decompress`) + parse `PrimitiveBlock` | `DecompressPool` recycles `Vec<u8>` buffers |
| 3. Reorder | Caller's thread | `ReorderBuffer` restores file order, delivers blocks to consumer | 32-slot decode-ahead |

Entry points:
- `ElementReader::for_each_pipelined()` — element-level callback
- `ElementReader::for_each_block_pipelined()` — owned `PrimitiveBlock` callback
- `ElementReader::into_blocks_pipelined()` — returns `Iterator<Item = Result<PrimitiveBlock>>`

Used by most commands. See Sequential Read below for exceptions.

### Sequential Read

Single-threaded: `BlobReader` → decompress → `PrimitiveBlock` on the calling thread. ~6x slower than pipelined, but avoids cross-thread allocation/free churn.

Used by:
- `diff` / `derive-changes` (via `StreamingBlocks::new_sequential()`) — two files read in lockstep
- `tags-count` — avoids 25+ GB heap retention at planet scale

### Blob Filtering (pre-decode)

Skips entire blobs before decompression using metadata embedded in `BlobHeader`:

| Filter | Source | Effect |
|--------|--------|--------|
| Element type | `BlobIndex::ElemKind` (indexdata) | Skip blobs with wrong type (~85% reduction for single-type queries) |
| Tag key/prefix | Tagdata (BlobHeader field 4) | Skip blobs without required tag keys |
| Spatial bbox | Coordinate bounds in indexdata | Skip blobs outside bounding box (nodes only) |

Applied via `ElementReader::with_blob_filter()`. Used by `cat --type`, `tags-filter`, `extract`.

### Pipelined Write

**Parallel compression with sequential output** — `src/write/writer.rs`

| Stage | Thread | Work |
|-------|--------|------|
| 1. Frame + compress | Rayon pool | Per-thread `FrameScratch` (reusable buffers + lazy-init compressors). Zlib/zstd/none. |
| 2. Reorder + write | Dedicated writer thread | `ReorderBuffer` (32-slot write-ahead), writes to `FileWriter` |

Output modes:
- `PbfWriter::to_path()` — buffered I/O
- `PbfWriter::to_path_direct()` — O_DIRECT (Linux, `linux-direct-io` feature)
- `PbfWriter::to_path_uring()` — io_uring with registered buffers (Linux, `linux-io-uring` feature)

Special: raw passthrough for unmodified blobs via `write_raw()` / `write_raw_chunks()` — zero decompression/recompression, uses `copy_file_range` on Linux.

### io_uring Writer

`src/write/uring_writer.rs` — replaces the buffered writer thread when `--io-uring` is set.

64 × 256 KB page-aligned registered buffers. Accumulates data, submits `WriteFixed` SQEs when a buffer fills, reaps CQEs to recycle buffers. Supports `CopyRange` for passthrough blobs.

Used by `sort`, `cat --dedupe`, `apply-changes`.

### Block Builder

`src/write/block_builder.rs` — accumulates elements into PBF blocks.

- Max 8000 entities per block, one element type per block
- `StringTable` with `FxHashMap<Rc<str>, u32>` dedup
- Reusable wire scratch buffers (`wire.rs` encoding primitives)
- Output: `OwnedBlock = (Vec<u8>, BlobIndex, Option<Vec<u8>>)` — serialized data + index + optional tagdata

Used by all commands that produce PBF output.

### Node-Only Wire Scanner

`src/commands/node_scanner.rs` — parses `DenseNodes` directly from decompressed wire format, bypassing `PrimitiveBlock` construction. Zero per-block heap allocation. Extracts `(id, lat, lon)` tuples.

Used internally by external join (stage 2), ALTW dense/sparse (pass 1), and merge `--locations-on-ways`.

---

## Command Pipelines

### cat (passthrough)

No `--type` filter: reads raw blob frames, adds indexdata via `reframe_raw_with_index()`, writes raw. Zero decompression.

### cat --type / --clean

Blob filter → pipelined decode → element-level type check → `BlockBuilder` → pipelined write. Batch processing (64 blocks or 32 MB budget).

### cat --dedupe

K-way sorted merge of multiple PBFs. Blob-level passthrough for non-overlapping ranges, decode + dedup for overlaps. All inputs must be sorted.

### sort

Two-pass blob-level permutation sort.
1. Scan all blobs, build index of (element_type, min_id, max_id)
2. Non-overlapping blobs: raw passthrough. Overlapping blobs: decode → binary heap merge → re-encode.

### apply-changes (merge)

Single-pass batch pipeline applying an OSC diff to a sorted PBF. OSC parsed into `CompactDiffOverlay` (arena-packed, `FxHashMap` index). `DiffRanges` enables O(log n) overlap checks.

3 phases per batch:
1. **Parallel classify** (rayon) — indexdata fast-path skips ~92% of blobs at Denmark scale
2. **Sequential assign** — passthrough / false-positive / rewrite decision per blob
3. **Streaming rewrite + output** — rayon tasks own their `PrimitiveBlock`, results reordered for sequential output. Gap creates interleaved at sorted positions.

Optional `--locations-on-ways`: preserves/updates inline way-node coordinates through the merge.

### diff / derive-changes

Two-pointer merge-join over two sorted PBFs via `StreamingBlocks` (sequential readers). Three phases per element type (nodes, ways, relations). Each element compared by content equality (coordinates, tags, refs, members).

- `diff` → text or summary output
- `diff --format osc` (derive-changes) → OSC XML output

### extract

Geographic extraction with three strategies:
- `--simple` — single pass, blob-filter by bbox, may leave dangling refs
- Default (complete-ways) — two passes: pass 1 collects way-referenced node IDs, pass 2 emits all
- `--smart` — three passes: also completes multipolygon/boundary relation members

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

Embeds node coordinates in ways. Three index strategies:

**Dense** (default): file-backed mmap, direct addressing by node ID. Pass 0 builds `IdSetDense` of way-referenced nodes, pass 1 populates index via node-only scanner (lock-free parallel writes), pass 2 enriches ways.

**Sparse**: chunk-indexed sparse array (~540 MB RAM). Batched sorted access converts random I/O to sequential scans.

**External** (4-stage radix partition):
1. Way pass → emit `(node_id, slot_pos)` COO pairs into 256 buckets
2. Node join → merge-join with sorted COO buckets, emit resolved `(slot_pos, lat, lon)`
3. Slot reorder → scatter buffer into final coord_slots file
4. Assembly → pread-from-workers, enrich ways with coordinates

Bounded memory (<2 GB), all sequential I/O, uses temp disk (~4 GB Denmark, ~112 GB Europe, ~300 GB planet).

### renumber

Single-pass sequential: 3 `FxHashMap` (node/way/relation) for old→new ID mapping. Remaps cross-references in way node refs and relation members.

### time-filter

Single-pass grouped snapshot: maintains `PendingGroup` per object, emits latest version with `timestamp <= cutoff`, skips deleted (`visible=false`).

### inspect

Header-only scan (fast path on indexed PBFs — reads blob headers without decompression). Selective decode on demand for `--nodes`, `--blocks`, or element display.

### check

- `--ids`: sequential scan checking ID uniqueness and ordering. Optional `--full` bitmap for duplicate detection.
- `--refs`: referential integrity via `IdSetDense` (roaring bitmap). Optional `--check-relations`.

### build-geocode-index

4-pass build pipeline:
1. Relations (admin boundaries)
2. Referenced node collection (`IdSetDense`)
3. Nodes + ways fused scan (compact rank-indexed coord array, streaming data files)
4. Bucketed S2 cell assignment (256 temp-file buckets per level)

Outputs 19 binary files. Self-contained module in `src/geocode_index/`.

### merge-changes

OSC-only: merges multiple OSC XML files into one. Optional `--simplify` keeps only the last change per object. No PBF I/O.

---

---

## Author's Production Pipeline

_The following describes the specific deployment that drives pbfhogg's development. It documents how the author uses pbfhogg in a planet-scale OSM refresh pipeline feeding tile generation and reverse geocoding. It is not part of the library's public API or general documentation — it records operational context, allocation budgets, and performance measurements specific to this ecosystem._

**Production pipeline** (runs every planet refresh cycle):
```
Bootstrap (once):  pbfhogg cat → pbfhogg add-locations-to-ways → enriched PBF
                   pbfhogg build-geocode-index → reverse geocoding index
Steady state:      pbfhogg apply-changes --locations-on-ways (daily diffs)
                   pbfhogg build-geocode-index (rebuild after merge)
                                      │
                                      ├── elivagar → PMTiles → nidhogg (tile serving)
                                      └── nidhogg (PBF ingest + reverse geocoding)
```

`merge --locations-on-ways` preserves and updates inline way-node coordinates
through OSC diffs, so `add-locations-to-ways` only needs to run once to bootstrap
the initial enriched PBF. Subsequent daily diffs maintain coordinates automatically:
surviving base ways forward raw lat/lon bytes, OSC ways look up node coordinates
from a sparse index built from the diff and base PBF.

**`sort` is not in the pipeline.** Geofabrik and planet PBFs are always `Sort.Type_then_ID`, and every pipeline step preserves sorted order: `cat` copies blobs in input order, `merge` interleaves upserts at sorted positions, `add-locations-to-ways` passes through or decodes without reordering. The `sort` command exists for repairing unsorted PBFs from other tools (osmosis, custom exporters) — a one-time fix, not a recurring step.

The two downstream consumers are:
- **elivagar** (`~/Programs/elivagar`) — vector tile generator. Reads the enriched PBF (with inline way coordinates via `Way::node_locations()`) to produce PMTiles. Pre-processing with `add-locations-to-ways` eliminates elivagar's node store (~44 GB at planet scale), dropping peak RSS from ~65-75 GB to ~15-20 GB.
- **nidhogg** (`~/Programs/nidhogg`) — planet refresh service. Reads the planet PBF for data ingest, then merges daily OSC diffs to keep it current.

## Downstream Consumers

**Elivagar** — tile generation (read-only)
- `Cargo.toml`: `pbfhogg = { path = "../pbfhogg" }` (default features = `commands`)
- Only uses the **read pipeline**; no writes
- Entry: `ElementReader::from_path()` → `.into_blocks_pipelined()` → iterates owned `PrimitiveBlock`s
- Sends way blocks to a worker thread via `SyncSender<PrimitiveBlock>` (bounded queue of 1)
- API surface: `node.id()`, `.decimicro_lat()`, `.decimicro_lon()`, `.tags()`, `way.id()`, `.refs()`, `.node_locations()`, `.tags()`, `rel.id()`, `.tags()`, `.members()`
- `Way::node_locations()` yields `WayNodeLocation` (lat/lon) from enriched PBFs — eliminates the node coordinate store entirely
- Also uses `protohoggr` directly for MVT/PMTiles protobuf encoding (unrelated to PBF I/O)
- File: `~/Programs/elivagar/src/pipeline.rs:340-605`

**Nidhogg** — planet refresh pipeline (read + merge)
- `Cargo.toml`: `pbfhogg = { path = "../pbfhogg" }` (default features = `commands`)
- **Read path**: `ElementReader::from_path()` → `.for_each_pipelined(|element| ...)` — two-pass ingest
  - File: `~/Programs/nidhogg/src/ingest/mod.rs:72-324`
- **Merge path**: delegates entirely to `pbfhogg::merge::merge(base, osc, output, &MergeOptions { .. })`
  - File: `~/Programs/nidhogg/src/merge.rs:6`
  - Currently: zlib compression, no direct_io/io_uring, no locations_on_ways
  - TODO: enable `locations_on_ways: true` once the enriched PBF is bootstrapped
- **No direct BlockBuilder/PbfWriter usage** — nidhogg never constructs PBF blocks itself
- Also reads PBF headers via `BlobReader::from_path()` → `.to_headerblock()` for replication state
  - File: `~/Programs/nidhogg/src/update.rs:95-114`

## Cargo Features in Play

Both consumers use default features. In practice:
- `commands` feature: **enabled** (brings in `roaring`, `serde_json`, `s2` — needed for merge + geocode builder)
- `geocode-reader` feature: **implied by `commands`**. nidhogg can alternatively depend on just `geocode-reader` for the reverse geocoding reader without pulling in `roaring`/`serde_json`.
- `linux-direct-io`: **disabled** (nidhogg passes `false`)
- `linux-io-uring`: **disabled** (nidhogg passes `false`)
- Zlib backend: **zlib-rs** (hardcoded, pure Rust, via flate2)
- Zstd: available but **unused** — nidhogg hardcodes `Compression::Zlib(6)`

## Pipeline 1: Pipelined Read (both consumers)

All source PBFs are zlib-compressed (Geofabrik/AWS). Every read decompresses.

**3-stage pipeline** — `src/pipeline.rs`
1. **I/O thread**: reads raw compressed blobs (~32KB each) sequentially, sends via channel (READ_AHEAD=16 slots)
2. **Rayon decode pool**: parallel decompress + parse. Thread count = `available_parallelism() - 2`
   - `decompress_blob()` — thread-local `flate2::Decompress` with `reset(true)`, reused per thread
   - `DecompressPool` — returns `Vec<u8>` to pool on drop instead of freeing
   - `PooledBuffer` wrapper — custom Drop via `Bytes::from_owner`
3. **Reorder buffer**: `VecDeque<Option<PrimitiveBlock>>` restores file order

**PrimitiveBlock ownership** — `src/read/block.rs`:
- Owns `Bytes` buffer (decompressed ~1.4 MB) + `WireBlock<'static>` (self-referential, unsafe transmute)
- `WireBlock::parse()` — builds `WireStringTable` and `group_ranges` as inline (offset, count) into the decompressed buffer — zero separate allocation
- `to_primitiveblock_inline()` with pool recycling: reuses `PrimitiveBlock` across blobs (string table Vec, group Vecs) via `clear_and_reuse()`

**Element iteration** — zero-copy, zero-alloc:
- `WireGroup` lazy scanner — scans protobuf on-the-fly
- Tag/ref iterators use `PackedSint64Iter`/`PackedUint32Iter` from protohoggr — decode varints from raw bytes, no Vec

**Key allocation sites per blob (read path):**
- Decompression buffer: ~1.4 MB (pooled, reused)
- `WireStringTable`: inline (offset, count) into decompressed buffer — zero separate allocation
- `WireBlock` group_ranges: inline (offset, count) into decompressed buffer — zero separate allocation
- Element wrappers: stack-allocated (~24 bytes each)

## Pipeline 2: Merge (nidhogg only)

**Entry**: `src/commands/merge.rs` — `merge(base, osc, output, &MergeOptions, &HeaderOverrides)`

**OSC diff parsing**: `src/osc.rs` — `CompactDiffOverlay` with arena-packed binary layouts (`Vec<u8>` per type), `FxHashMap<i64, u32>` index (byte offsets into arenas), `StringInterner` for tag keys/roles, `HashSet<i64>` for deletes. 40-60% less memory than per-element HashMap. Typical Denmark diff: ~300KB compressed, ~50K entries.

**DiffRanges**: `src/commands/merge.rs:231` — pre-sorted `Vec<i64>` per element type (separate vecs for all-IDs and upsert-only-IDs) for O(log n) overlap checks via `partition_point`. Wrapped in `Arc` for sharing across rayon tasks.

**Reader thread**: dedicated `std::thread::spawn` with `sync_channel::<RawBlobFrame>(128)` read-ahead. Decouples I/O from processing — while the main thread runs classify/rewrite/output on the current batch, the reader pre-fills the next.

**Byte-budgeted batch processing** (`BATCH_BYTE_BUDGET=128MB`, `BATCH_MIN_BLOBS=8`, `BATCH_MAX_BLOBS=128`). `estimate_blob_cost()` returns raw frame size for passthrough blobs, `raw * 21` for potential rewrites (raw + ~16x decompressed + ~5x rewrite estimate). Batches fill via `try_recv` until the byte budget or max blob count is reached.

**3-phase pipeline per batch:**

- **Phase 1 — Parallel classify** (rayon `par_iter`):
  - `classify_only()` — `merge.rs:935`
  - Fast path (index hit): blob has indexdata → `DiffRanges::range_overlaps()` → false = `Passthrough`. **Zero decompression.**
  - Medium path (scan): decompress into reusable `Vec<u8>`, `scan_block_ids()` for min/max ID
  - Slow path (precise): full `PrimitiveBlock` parse, `block_overlaps_diff()` checks each element ID against diff

- **Phase 2 — Sequential assign** (main thread):
  - Assigns each blob to `BatchSlot::Passthrough | FalsePositive | Rewrite`
  - For rewrites: binary search (`partition_point`) into sorted upsert IDs computes `upsert_range: (usize, usize)` — range indices into the DiffRanges upsert vec (no per-job Vec copy)
  - Builds `RewriteJob { block: PrimitiveBlock, kind: ElemKind, upsert_range: (usize, usize) }`

- **Phase 3+4 — Streaming rewrite + output** (`rayon::spawn` + bounded `sync_channel`):
  - Each rewrite job is dispatched via `rayon::spawn`, owning its `RewriteJob` (including `PrimitiveBlock`). Channel bounded to `rayon::current_num_threads().min(rewrite_count)`.
  - `rewrite_block_parallel()` — `merge.rs:760` — allocates a local `BlockBuilder` per task, pre-seeds string table from base block, iterates elements, skips deleted, applies modifications, interleaves creates at sorted positions via `&upserts[range.0..range.1]`. Returns `RewriteOutput { blocks: Vec<OwnedBlock>, stats }`.
  - PrimitiveBlock freed as soon as each task completes (not held until all finish).
  - Main thread processes slots in file order. Out-of-order rewrite results buffered in `received: Vec<Option<RewriteOutput>>`, consumed when their slot is reached.
  - Passthrough: `coalesce_passthrough()` — accumulates consecutive raw frames in `passthrough_buf`, flushed as single `write_raw_owned()` (move semantics). On `linux-direct-io` with `copy_file_range`, passthrough uses kernel-space copy instead.
  - Rewrite: flush passthrough buf, then write each `OwnedBlock` via `write_primitive_block_owned()` (move, no copy)
  - Gap creates between blobs: `emit_gap_creates()` via `BlockBuilder`

**Passthrough ratios** (measured):
- Denmark (465 MB, ~300K changes): ~92% passthrough, ~8% rewrite
- Germany (4.5 GB, ~146K changes): ~82% passthrough, ~18% rewrite
- Planet (87 GB, daily diff): ~8% passthrough, ~92% rewrite (most blobs touched)

## Pipeline 3: Cat (pbfhogg CLI, used to generate indexed PBFs)

**Entry**: `src/commands/cat.rs` — `cat(files, output, type_filter, &CleanAttrs, compression, direct_io, force, &HeaderOverrides)`

**No type filter** (passthrough) — reads raw blob frames, adds indexdata via `reframe_raw_with_index()`, writes raw. **No decompress/compress.**

**With type filter** — full decode → `BlockBuilder` → re-encode → compress. Same allocation pattern as merge rewrite path. `CleanAttrs` optionally strips metadata attributes (version, timestamp, changeset, uid, user).

**Dedupe mode** (`--dedupe`) — K-way sorted merge of multiple PBFs with blob-level passthrough and exact-duplicate deduplication. All inputs must be sorted.

## Pipeline 4: Add-locations-to-ways (enrichment step)

**Entry**: `src/commands/add_locations_to_ways.rs` — `add_locations_to_ways(input, output, keep_untagged_nodes, compression, direct_io, force, &HeaderOverrides, index_type)`

Three index strategies that embeds node coordinates directly into way elements:

- **`Dense`** (`DenseMmapIndex`): file-backed anonymous mmap, 8 bytes/slot, direct addressing by node ID. 128 GB virtual address space, ~16 GB touched at planet (after pass 0 filtering). Fastest when working set fits in RAM.
- **`Sparse`** (`SparseArrayIndex`): Planetiler-inspired chunk-indexed sparse array (chunk size 256). RAM: `offsets` Vec<u64> + `start_pad` Vec<u8> (~540 MB at planet). On-disk: compact packed (lat, lon) values file via read-only mmap (~16 GB for planet). Way lookups use batched sorted access — collect all node refs from a batch, sort by file offset, sequential scan into `FxHashMap`, then process blocks with pre-resolved coordinates. Memory-bounded for planet on low-RAM hosts.
- **`External`** (`external_join`): double radix permutation via temp disk. Bounded memory (~1.4 GB stages 1-3, ~2.1 GB stage 4 at Europe). See Pipeline 4b below.

### Dense/Sparse path (2-pass + pass 0)

**Pass 0 — Way-referenced node IDs (both index types):**
- Scans way blobs to build `IdSetDense` bitset (~1.6 GB for planet's ~2B unique way-node refs)
- Dense: filters which mmap slots to populate. Sparse: determines which nodes to store.

**Pass 1 — Node index building:**
- Node-only wire scanner (`extract_node_tuples`) — bypasses PrimitiveBlock, zero per-block heap alloc
- **Dense path**: `par_iter` over `NodeTuple` batches. `SharedDenseWriter` holds raw `*mut u8` into the mmap (`Send + Sync`). Each rayon task writes to disjoint 8-byte slots (`base + node_id * 8`). No merge step — writes are lock-free.
- **Sparse path**: sequential insertion via node-only scanner into chunk-indexed temp file.

**Pass 2 — Output with locations on ways:**
- **Indexed PBF (fast path)**: reads raw blob frames, classifies by `BlobIndex.kind` from BlobHeader indexdata
  - Node blobs + `keep_untagged=true` → `write_raw_owned` (passthrough, zero decode)
  - Node blobs + `keep_untagged=false` → decompress → filter untagged → re-encode
  - Relation blobs → `write_raw_owned` (always passthrough)
  - Way blobs → decompress → coordinate lookup → `add_way_with_locations` → re-encode
  - **Dense**: direct `NodeIndex::get(id)` per way node ref
  - **Sparse**: `resolve_batch_locations` collects all way node refs from the batch, sorts by mmap offset, sequential scan builds `FxHashMap<i64, (i32, i32)>`, then `LocationLookup::Resolved` provides O(1) HashMap lookups during block processing
  - Batch processing via `par_iter().map_init(BlockBuilder::new, ...)` for way and node batches
- **Non-indexed PBF (fallback)**: full decode-all path, same as above but every blob is decoded

**Passthrough ratios** (Denmark, indexed PBF):
- Default (drop untagged): 6 passthrough / 7390 decoded (only relation blobs passthrough)
- `--keep-untagged-nodes`: 6568 passthrough / 828 decoded (~89% passthrough)

## Pipeline 4b: External Join (add-locations-to-ways --index-type external)

**Entry**: `src/commands/external_join.rs`

4-stage sequential I/O pipeline with bounded memory. No mmap, no random access.
Requires sorted PBF with indexdata. Uses temp disk (~4 GB Denmark, ~112 GB Europe).

**Stage 1 — Way pass** (sequential BlobReader + DecompressPool):
- Reads way blobs via indexdata filter, iterates way refs
- Emits `(node_id, slot_pos)` COO pairs into 256 node buckets (radix by high byte of node_id)
- Anon: 70 MB at Europe scale

**Stage 2 — Node join** (P2b-v2, pread-from-workers):
- IO thread reads only blob headers (~50 bytes), filters to node blobs via indexdata
- Sends lightweight descriptors `(seq, data_offset, data_size)` to worker threads
- Workers `pread` blob data from shared `Arc<File>`, decompress via `decompress_blob_raw()`,
  extract `NodeTuple`s — all alloc/free thread-local, zero cross-thread ownership
- Workers call `fadvise(DONTNEED)` after each pread (worker-side eviction)
- Consumer reorders via `ReorderBuffer`, merge-joins against one sorted COO bucket at a time
- Emits `(slot_pos, lat, lon)` resolved entries into 256 slot buckets
- Anon: 1.4 GB at Europe scale (bucket sort data, irreducible)
- Europe: 216s (was 301s sequential, -28%)

**Stage 3 — Slot reorder** (scatter buffer):
- Per slot bucket: load resolved entries, scatter by slot_pos into zeroed buffer, write_all
- Eliminates sort + reduces syscalls (15x speedup over sorted pwrite)

**Stage 4 — Assembly** (P2c, pread-from-workers):
- Header-only pre-scan builds blob schedule with pre-computed slot_starts
  (from ref count sidecar written in stage 1)
- Dedicated worker threads pread + decompress + PrimitiveBlock + assemble
- PrimitiveBlock lifecycle entirely on worker thread (no cross-thread retention)
- P1b: skips node blobs without tagged/member nodes via header indexdata + tagdata
- Consumer reorders + drains OwnedBlocks to pipelined PbfWriter
- Anon: 7.3 GB at Europe scale (parallel in-flight PrimitiveBlocks + OwnedBlocks)

**Measured results:**
- Denmark (465 MB): 12.3s, ~4 GB temp disk
- Europe (32.4 GB, commit `6b09796`): **577s (9.6 min)**, ~112 GB temp disk. Dense ALTW: 2,565s. **4.5x faster.**

**Planet (87 GB, sidecar `98e71e2b`): 1,462s (24.4 min). Dense: 5,773s (96 min). 3.9x faster.**

| Resource | Europe (measured) | Planet (measured) |
|----------|------------------|------------------|
| Peak anon RSS | 7.3 GB (stage 4) | **16.7 GB** (stage 4) |
| Temp disk | ~112 GB | ~300 GB |
| Wall time | 577s (9.6 min) | **1,462s (24.4 min)** |

Memory is bounded by design: pread-from-workers with thread-local buffers
(stages 2/4), sequential reader (stage 1), one bucket at a time (stages 2/3).
Peak anon is stage 4's parallel PrimitiveBlock construction (7.3 GB Europe,
16.7 GB planet). Validated on 30 GB host.

Temp disk is the main constraint at planet scale. The 256 node buckets hold all
`(node_id, slot_pos)` COO pairs (16 bytes each, ~13B pairs = ~208 GB), plus 256
slot buckets with resolved `(slot_pos, lat, lon)` entries (16 bytes each), plus
the final coord_slots file (8 bytes per ref). Total ~300 GB scratch. Cleaned up
on completion (or crash — `ScratchDir` has `Drop` impl).

## Write Side (shared by merge rewrite + cat filtered + add-locations-to-ways)

**BlockBuilder** — `src/write/block_builder.rs`:
- Dense node vectors pre-allocated to 8000 (MAX_ENTITIES_PER_BLOCK)
- String table: `FxHashMap<Rc<str>, u32>` + `Vec<Rc<str>>` — one `Rc<str>` alloc per unique string, shared between map key and vec entry
- Wire scratch buffers: `group_buf`, `elem_scratch`, `packed_scratch`, `info_scratch` — grow once, reused
- `encode_buf` — serialized output, moved via `std::mem::take` (zero copy)
- `take_owned()` — returns `(Vec<u8>, BlobIndex, Option<Vec<u8>>)`

**PbfWriter** — `src/write/writer.rs`:
- `FrameScratch` — `blob_buf`, `header_buf`, `compress_buf`, lazy-init `Compress`/`zstd::Compressor`
- Thread-local `PIPELINE_SCRATCH` — per-rayon-thread, reused across blobs
- `frame_blob_into()` — allocates **one Vec per blob** for final framed output (exact capacity)
- `compress_zlib()` — `flate2::Compress` with `reset()`, reuses `compress_buf`
- Pipelined writer thread — VecDeque reorder buffer (WRITE_AHEAD=32)

## Compression Summary

| Path | Decompress | Compress | Backend |
|---|---|---|---|
| Read (all ingest) | every blob | — | zlib-rs via flate2 |
| Merge passthrough | none (~92% DK) | none | — |
| Merge rewrite | yes | zlib:6 | zlib-rs via flate2 |
| Cat passthrough | none | none | — |
| Cat filtered | every blob | zlib (default) | zlib-rs via flate2 |
| add-locations-to-ways passthrough | none (~89% keep-untagged) | none | — |
| add-locations-to-ways decode | yes (way blobs + filtered nodes) | zlib (default) | zlib-rs via flate2 |

## Benchmark Context (commit `a6ebbfe`)

**Merge (buffered I/O):**
- Denmark (465 MB): zlib 363ms, none 250ms
- Germany (4.5 GB): zlib 5.3s, none 3.4s
- North America (18.8 GB): zlib 17.3s, none 14.9s

**Merge (io_uring, North America):** zlib 15.2s, none 11.9s (-20% vs buffered)

RSS under 600 MB at North America scale (18.8 GB input, 30 GB host).

**Merge `--locations-on-ways` (commit `e7bbfa2`):**
- Denmark (501 MB with LocationsOnWays): pbfhogg 3.9s vs osmium 8.3s (2.1x faster)
- vs separate merge + ALTW pipeline: pbfhogg 2.7s + 6.5s = 9.2s → 3.9s (2.4x faster)
- Overhead vs plain merge (flag off): +170ms at Denmark scale (475ms vs 307ms to /dev/null)
- 883 passthrough node blobs decompressed for coordinate extraction (of 5559 total)
