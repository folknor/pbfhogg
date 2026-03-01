# add-locations-to-ways Optimization Plan (Indexed Pipeline First)

## Scope

This plan targets the production path:

1. Input is already indexdata-enriched (`cat` ran first).
2. `add-locations-to-ways` should optimize for that case first.
3. Generic/raw PBF support remains correct, but is secondary for optimization work.

Code references are in `src/commands/add_locations_to_ways.rs` unless noted otherwise.

## P0 — DONE

All six P0 items landed in a single coordinated rewrite of `write_output_passthrough`.
Cross-validated identical to osmium on Denmark (3.5M nodes, 6.6M ways, 46K relations, 0 missing locations).

### What changed

1. **Slot-based parallel decode pipeline** — the main thread no longer decompresses or
   parses blobs. Raw frames are classified by indexdata (`ElemKind`), accumulated into
   `BatchSlot` (Way/Node/Unknown), and dispatched to rayon workers that decompress +
   parse + transform in parallel. `process_slot_batch` replaces the old
   `process_way_batch` / `process_node_batch` / `process_batch` trio.

2. **Passthrough coalescing** — consecutive passthrough blobs (relations, nodes when
   `keep_untagged_nodes`) accumulate in a `Vec<u8>` buffer via `coalesce_passthrough`,
   flushed as a single `write_raw_owned` before decode batch dispatch. Mirrors merge's
   pattern (`coalesce_passthrough` / `flush_passthrough_buf`).

3. **Worker scratch reuse** — `refs_buf: Vec<i64>` and `locations_buf: Vec<(i32, i32)>`
   persist across blocks via rayon `map_init` instead of being allocated/dropped per block.
   `tags_buf` and `members_buf` stay per-block (borrowed `&str` tied to block lifetime).

4. **None fallback routing fix** — `BatchSlot::Unknown` routes through the generic
   `process_block` handler, which correctly handles all element types. Previously, blobs
   without indexdata were always pushed to `way_batch`, silently dropping any nodes or
   relations they contained.

5. **Byte-budgeted batching** — replaced fixed `BATCH_SIZE=64` with `BATCH_BYTE_BUDGET`
   (128 MB) + `BATCH_MIN_BLOBS` (8) / `BATCH_MAX_BLOBS` (128) guardrails, matching
   merge's approach for planet-scale blob size variability.

### What did NOT change

- `write_output_decode_all` (non-indexed fallback) — still uses `process_batch` with
  the old `BATCH_SIZE` constant on decoded `PrimitiveBlock`s from the read pipeline.
- Pass 1 node index building — already parallel (commit 4055e64).
- `process_way_block`, `process_node_block`, `process_block` internal logic — only
  signatures changed (scratch buffer params).
- `RawBlobFrame`, `Stats`, `merge_stats` — unchanged.
  (`read_raw_frame` gained `file_offset` tracking in P1.4.)

## P1 — DONE

All four P1 items landed across commits `1c6f763` (P1.1–P1.3) and `63112f8` (P1.4).
Cross-validated identical to osmium on Denmark. Benchmark: 8724 ms → 6615 ms (−24%)
for P1.1–P1.3; P1.4 neutral at Denmark scale but eliminates ~69 GB of userspace copies
at planet scale (~92% passthrough).

### What changed

1. **Dense lookup single-load decode** — `DenseMmapIndex::get` reads one `u64` from the
   mmap and splits lat/lon via bit shift, replacing the previous slice + `try_into` path
   with two separate `i32::from_le_bytes` calls.

2. **Three-buffer single-pass way encode** — `encode_way_with_locations` encodes refs,
   lats, and lons in a single zip loop using three reusable scratch buffers
   (`packed_lat_scratch`, `packed_lon_scratch` on `BlockBuilder`, plus existing
   `packed_refs`). Eliminates a second pass over locations.

3. **Per-worker DecompressPool** — `process_slot_batch` creates a `DecompressPool` per
   rayon worker via `map_init`, reusing decompress buffers across blobs within a batch.
   Replaces per-blob `decompress_blob_data_into` with `decompress_blob` + pool.

4. **Copy-range passthrough** (feature-gated: `linux-direct-io`) — two-phase blob read
   (`read_blob_header` → `read_blob_data` or `skip_blob_data`) enables skipping
   passthrough blob data entirely. `CopyRange` coalesces consecutive passthrough blobs
   into contiguous file ranges, flushed as single `copy_file_range` calls. Decode blobs
   between passthrough runs flush the pending range to prevent corrupt coalesced copies.
   `FileReader::skip` / `DirectReader::skip` advance the reader without materializing
   data. Second fd opened for `copy_file_range` (explicit offsets, thread-safe).
   Without the feature, the userspace coalescing path is retained.

### What did NOT change

- `write_output_decode_all` (non-indexed fallback) — unchanged.
- `process_way_block`, `process_node_block`, `process_block` internal logic — unchanged.
- Pass 1 node index building — unchanged.
- Writer infrastructure (`write_raw_copy`, `PipelinePayload::CopyRange`) — reused from merge.

## P2 (cleanup and maintainability)

1. Consolidate duplicated transform logic
- `process_block` and `process_way_block` share most way logic; `process_node_block` repeats node filtering.
- Extract shared helpers that operate on reusable scratch and a mode enum.

2. Move raw-frame reader into shared internal utility
- `read_raw_frame` is duplicated between merge and add-locations.
- Shared utility reduces divergence and bug risk.

## Future Investigations (if concrete steps are insufficient)

1. One-pass external join architecture
- Replace in-memory global node index with disk-backed or partitioned join strategy (node chunks + way replay).
- Higher complexity, but could reduce RAM pressure further for extreme inputs.

2. NUMA-aware dense index sharding
- Partition dense index by ID ranges and pin worker groups per NUMA node.
- Could reduce remote memory traffic at planet scale.

3. SIMD-accelerated decode/lookups
- Explore SIMD varint decode and batched ref lookup for way processing.
- High implementation cost; validate with profiling first.

4. Adaptive mode selection from header/index stats
- At startup, inspect index richness and choose tuned strategy:
  - heavy passthrough mode,
  - balanced mode,
  - decode-heavy mode.

5. Writer-path specialization for transformed way blobs
- Investigate whether way-only transformed output can use larger frame aggregation or alternative write queue tuning to reduce per-blob framing overhead.
