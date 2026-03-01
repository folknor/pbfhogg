# pbfhogg Pipeline Reference

Rust library for reading, writing, and merging OpenStreetMap PBF files. The full planet PBF is ~80 GB compressed; a weekly refresh reads the entire file, applies 5-30 daily diffs (~15 MB each), and writes a new PBF. At this scale, per-blob allocations that are invisible on small extracts (Denmark 465 MB, ~7400 blobs) become dominant — the planet has ~600K blobs, so a 64 KB throwaway allocation per blob means 38 GB of allocator churn per run.

**Production pipeline** (runs every planet refresh cycle):
```
pbfhogg cat → pbfhogg merge → pbfhogg add-locations-to-ways
                                      │
                                      ├── elivagar → PMTiles → nidhogg (tile serving)
                                      └── nidhogg (PBF ingest → query API)
```

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
- **Merge path**: delegates entirely to `pbfhogg::merge::merge(base, osc, output, Compression::Zlib(6), false, false, false)`
  - File: `~/Programs/nidhogg/src/merge.rs:6`
  - direct_io=false, io_uring=false, sqpoll=false
- **No direct BlockBuilder/PbfWriter usage** — nidhogg never constructs PBF blocks itself
- Also reads PBF headers via `BlobReader::from_path()` → `.to_headerblock()` for replication state
  - File: `~/Programs/nidhogg/src/update.rs:95-114`

## Cargo Features in Play

Both consumers use default features. In practice:
- `commands` feature: **enabled** (brings in `roaring`, `serde_json` — needed for merge)
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
- `WireBlock::parse()` — builds `WireStringTable` as `Vec<(u32,u32)>` (8 bytes/entry) and `group_ranges` as `Box<[(u32,u32)]>`

**Element iteration** — zero-copy, zero-alloc:
- `WireGroup` lazy scanner — scans protobuf on-the-fly
- Tag/ref iterators use `PackedSint64Iter`/`PackedUint32Iter` from protohoggr — decode varints from raw bytes, no Vec

**Key allocation sites per blob (read path):**
- Decompression buffer: ~1.4 MB (pooled, reused)
- `WireStringTable`: `Vec<(u32,u32)>` — 8 bytes x string count per block
- `WireBlock` group_ranges: `Box<[(u32,u32)]>` — 8 bytes x group count
- Element wrappers: stack-allocated (~24 bytes each)

## Pipeline 2: Merge (nidhogg only)

**Entry**: `src/commands/merge.rs:1032` — `merge(base, osc, output, compression, direct_io, io_uring, sqpoll)`

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
- Planet (80 GB, daily diff): ~8% passthrough, ~92% rewrite (most blobs touched)

## Pipeline 3: Cat (pbfhogg CLI, used to generate indexed PBFs)

**No type filter** (passthrough) — `src/commands/cat.rs`: reads raw blob frames, adds indexdata via `reframe_raw_with_index()`, writes raw. **No decompress/compress.**

**With type filter** — `src/commands/cat.rs`: full decode → `BlockBuilder` → re-encode → compress. Same allocation pattern as merge rewrite path.

## Pipeline 4: Add-locations-to-ways (enrichment step)

**Entry**: `src/commands/add_locations_to_ways.rs` — `add_locations_to_ways(input, output, keep_untagged_nodes, compression, direct_io, index_type)`

Two-pass algorithm that embeds node coordinates directly into way elements.

**Pass 1 — Parallel node index building:**
- Pipelined read with `BlobFilter::only_nodes()` — skips way/relation blobs entirely
- Batch-and-dispatch: collects `INDEX_BATCH_SIZE=64` blocks, then `par_iter` on rayon global pool
- Uses `elements_skip_metadata()` — skips metadata parsing (only needs id/lat/lon)
- **Hash variant**: `par_iter().fold().reduce()` builds per-thread partial `FxHashMap<i64, (i32, i32)>`, merged pairwise, extended into master map
- **Dense variant**: `SharedDenseWriter` holds raw `*mut u8` into anonymous mmap (`Send + Sync`). Each rayon task writes to disjoint 8-byte slots (`base + node_id * 8`). No merge step — writes are lock-free. Planet-scale: 128 GB virtual, ~68 GB physical (requires `vm.overcommit_memory=1`)

**Pass 2 — Output with locations on ways:**
- **Indexed PBF (fast path)**: reads raw blob frames, classifies by `BlobIndex.kind` from BlobHeader indexdata
  - Node blobs + `keep_untagged=true` → `write_raw_owned` (passthrough, zero decode)
  - Node blobs + `keep_untagged=false` → decompress → filter untagged → re-encode
  - Relation blobs → `write_raw_owned` (always passthrough)
  - Way blobs → decompress → coordinate lookup → `add_way_with_locations` → re-encode
  - Batch processing via `par_iter().map_init(BlockBuilder::new, ...)` for way and node batches
- **Non-indexed PBF (fallback)**: full decode-all path, same as above but every blob is decoded

**Passthrough ratios** (Denmark, indexed PBF):
- Default (drop untagged): 6 passthrough / 7390 decoded (only relation blobs passthrough)
- `--keep-untagged-nodes`: 6568 passthrough / 828 decoded (~89% passthrough)

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
