# pbfhogg Pipeline Reference

Rust library for reading, writing, and merging OpenStreetMap PBF files. The full planet PBF is ~80 GB compressed; a weekly refresh reads the entire file, applies 5-30 daily diffs (~15 MB each), and writes a new PBF. At this scale, per-blob allocations that are invisible on small extracts (Denmark 465 MB, ~7400 blobs) become dominant — the planet has ~600K blobs, so a 64 KB throwaway allocation per blob means 38 GB of allocator churn per run.

The two downstream consumers are:
- **elivagar** (`~/Programs/elivagar`) — vector tile generator. Reads the planet PBF to produce PMTiles for map rendering.
- **nidhogg** (`~/Programs/nidhogg`) — planet refresh service. Reads the planet PBF for data ingest, then merges daily OSC diffs to keep it current.

## Downstream Consumers

**Elivagar** — tile generation (read-only)
- `Cargo.toml`: `pbfhogg = { path = "../pbfhogg" }` (default features = `commands`)
- Only uses the **read pipeline**; no writes
- Entry: `ElementReader::from_path()` → `.into_blocks_pipelined()` → iterates owned `PrimitiveBlock`s
- Sends way blocks to a worker thread via `SyncSender<PrimitiveBlock>` (bounded queue of 1)
- API surface: `node.id()`, `.decimicro_lat()`, `.decimicro_lon()`, `.tags()`, `way.id()`, `.refs()`, `.tags()`, `rel.id()`, `.tags()`, `.members()`
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

**3-stage pipeline** — `src/pipeline.rs:40-234`
1. **I/O thread** (`src/pipeline.rs:62-115`): reads raw compressed blobs (~32KB each) sequentially, sends via channel (READ_AHEAD=16 slots)
2. **Rayon decode pool** (`src/pipeline.rs:117-185`): parallel decompress + parse. Thread count = `available_parallelism() - 2`
   - `Blob::to_primitiveblock_pooled()` — `src/read/blob.rs:450-455`
   - `decompress_blob()` — `src/read/blob.rs:1114-1153` — thread-local `flate2::Decompress` with `reset(true)`, reused per thread
   - `DecompressPool` — `src/read/blob.rs:23-63` — returns `Vec<u8>` to pool on drop instead of freeing
   - `PooledBuffer` wrapper — `src/read/blob.rs:65-106` — custom Drop via `Bytes::from_owner`
3. **Reorder buffer** (`src/pipeline.rs:187-234`): `VecDeque<Option<PrimitiveBlock>>` restores file order

**PrimitiveBlock ownership** — `src/read/block.rs:340-374`:
- Owns `Bytes` buffer (decompressed ~1.4 MB) + `WireBlock<'static>` (self-referential, unsafe transmute at line 369-371)
- `WireBlock::parse()` — `src/read/wire.rs:90-160` — builds `WireStringTable` as `Vec<(u32,u32)>` (8 bytes/entry) and `group_ranges` as `Box<[(u32,u32)]>`

**Element iteration** — zero-copy, zero-alloc:
- `WireGroup` lazy scanner — `src/read/wire.rs:162-206` — scans protobuf on-the-fly
- Tag/ref iterators use `PackedSint64Iter`/`PackedUint32Iter` from protohoggr — decode varints from raw bytes, no Vec

**Key allocation sites per blob (read path):**
- Decompression buffer: ~1.4 MB (pooled, reused)
- `WireStringTable`: `Vec<(u32,u32)>` — 8 bytes × string count per block
- `WireBlock` group_ranges: `Box<[(u32,u32)]>` — 8 bytes × group count
- Element wrappers: stack-allocated (~24 bytes each)

## Pipeline 2: Merge (nidhogg only)

**Entry**: `src/commands/merge.rs:943` — `merge(base, osc, output, compression, direct_io, io_uring, sqpoll)`

**OSC diff parsing**: `src/osc.rs:51-58` — `DiffOverlay` with `HashMap<i64, OscNode/Way/Relation>` + `HashSet<i64>` for deletes. Typical Denmark diff: ~300KB compressed, ~50K entries.

**DiffRanges**: `src/commands/merge.rs:135-231` — pre-sorted `Vec<i64>` per element type for O(log n) overlap checks via `partition_point`

**4-phase batch processing** (BATCH_SIZE=64 blobs):

- **Phase 1 — Parallel classify** (`merge.rs:1091-1098`, rayon):
  - `classify_only()` — `merge.rs:845-884`
  - Fast path (index hit): blob has indexdata → `DiffRanges::range_overlaps()` → false = `Passthrough`. **Zero decompression.**
  - Medium path (scan): decompress into reusable `Vec<u8>`, `scan_block_ids()` for min/max ID. **~64KB alloc, amortized 0.**
  - Slow path (precise): full `PrimitiveBlock` parse, `block_overlaps_diff()` checks each element ID against diff HashMaps

- **Phase 2 — Sequential assign** (`merge.rs:1100-1129`):
  - Assigns each blob to `BatchSlot::Passthrough | FalsePositive | Rewrite`
  - For rewrites: binary search (`partition_point`) into sorted upsert IDs for inline assignment

- **Phase 3 — Parallel rewrite** (`merge.rs:1131-1147`, rayon):
  - `rewrite_block_parallel()` — `merge.rs:654-788`
  - Per-rayon-thread `BlockBuilder` via `map_init(BlockBuilder::new, ...)`
  - Pre-seeds string table from base block (`merge.rs:665`)
  - Iterates elements, skips deleted, applies modifications, emits gap creates
  - Returns `RewriteOutput { blocks: Vec<OwnedBlock>, stats }` — where `OwnedBlock = (Vec<u8>, BlobIndex, Option<Vec<u8>>)` (`block_builder.rs:16`)

- **Phase 4 — Sequential output** (`merge.rs:1157-1271`):
  - Passthrough: `coalesce_passthrough()` (`merge.rs:1387-1406`) — coalesces consecutive raw frames via `extend_from_slice`, flushed as single `write_raw_owned()` (move semantics)
  - Rewrite: `flush_passthrough_buf()` then write each `OwnedBlock` via `write_primitive_block_owned()` (move, no copy)
  - Gap creates between blobs: `emit_gap_creates()` (`merge.rs:1360-1382`) via `BlockBuilder`

**Passthrough ratios** (measured):
- Denmark (465 MB, ~300K changes): ~92% passthrough, ~8% rewrite
- Germany (4.5 GB, ~146K changes): ~82% passthrough, ~18% rewrite
- Planet (80 GB, daily diff): ~8% passthrough, ~92% rewrite (most blobs touched)

## Pipeline 3: Cat (pbfhogg CLI, used to generate indexed PBFs)

**No type filter** (passthrough) — `src/commands/cat.rs:109`: reads raw blob frames, adds indexdata via `reframe_raw_with_index()`, writes raw. **No decompress/compress.**

**With type filter** — `src/commands/cat.rs:276`: full decode → `BlockBuilder` → re-encode → compress. Same allocation pattern as merge rewrite path.

## Write Side (shared by merge rewrite + cat filtered)

**BlockBuilder** — `src/write/block_builder.rs:229-285`:
- Dense node vectors pre-allocated to 8000 (MAX_ENTITIES_PER_BLOCK)
- String table: `FxHashMap<String, u32>` — allocs only on first occurrence per block
- Wire scratch buffers: `group_buf`, `elem_scratch`, `packed_scratch`, `info_scratch` — grow once, reused
- `encode_buf` — serialized output, moved via `std::mem::take` (zero copy)
- `take_owned()` — `block_builder.rs:950-957` — returns `(Vec<u8>, BlobIndex, Option<Vec<u8>>)`

**PbfWriter** — `src/write/writer.rs`:
- `FrameScratch` — `writer.rs:97-111` — `blob_buf`, `header_buf`, `compress_buf`, lazy-init `Compress`/`zstd::Compressor`
- Thread-local `PIPELINE_SCRATCH` — `writer.rs:113-122` — per-rayon-thread, reused across blobs
- `frame_blob_into()` — `writer.rs:733-757` — allocates **one Vec per blob** for final framed output (exact capacity)
- `compress_zlib()` — `writer.rs:808-833` — `flate2::Compress` with `reset()`, reuses `compress_buf`
- Pipelined writer thread — `writer.rs:616-660` — VecDeque reorder buffer (WRITE_AHEAD=32)

## Compression Summary

| Path | Decompress | Compress | Backend |
|---|---|---|---|
| Read (all ingest) | every blob | — | zlib-rs via flate2 |
| Merge passthrough | none (~92% DK) | none | — |
| Merge rewrite | yes | zlib:6 | zlib-rs via flate2 |
| Cat passthrough | none | none | — |
| Cat filtered | every blob | zlib (default) | zlib-rs via flate2 |

## Benchmark Context (commit `d180d62`, zlib-rs, no libdeflater)

**Denmark (465 MB) read:**
- sequential: 2864 ms, parallel: 463 ms, pipelined: 1455 ms

**Denmark write:**
- sync-zlib:6: 16,593 ms, pipelined-zlib:6: 7,278 ms
- sync-zstd:3: 9,936 ms (40% faster than zlib, but unused by consumers)

**Merge (Denmark, indexdata+zlib):** ~3.3s (92% passthrough)
**Merge (Germany, indexdata+zlib):** ~35s (18% rewrite, parallel rewrite_block)
**Merge (North America 18.8GB, buffered+zlib):** ~43s

**Known allocation costs** (from hotpath profiling):
- `DecompressPool` eliminates ~1.4 MB/blob decompression buffer churn
- `Compress::reset()` eliminates ~312 KB/blob compressor state churn
- `BlockBuilder` pre-allocates 8000-element dense vectors (~400KB, reused)
- `frame_blob_into` allocates one fresh Vec per blob (~32-64KB, not reused)
- `reframe_raw_with_index` allocates one fresh Vec per passthrough blob needing reindex
