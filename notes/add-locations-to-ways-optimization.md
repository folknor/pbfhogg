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
- `RawBlobFrame`, `read_raw_frame`, `Stats`, `merge_stats` — unchanged.

## P1 (next)

1. Dense lookup micro-optimization
- Replace slice+`try_into` path in `DenseMmapIndex::get` with direct unaligned reads from pointer/slice.
- Keep bounds checks; only optimize data extraction.
- Consider packing lat/lon as one `u64` in memory for single-load decode.

2. Single-pass way encode API to remove `refs_buf` + `locations_buf` materialization
- Add a `BlockBuilder` method that accepts an iterator of `(ref_id, (lat, lon))`.
- Encode refs/lat/lon packed fields directly in one pass.
- This is a deeper change but removes two temporary vectors from the hottest transform loop.

3. Reuse decode buffers with pool-backed ownership
- Add an add-locations decode path equivalent to read pipeline's `DecompressPool`/`Bytes::from_owner`.
- Goal: avoid per-blob allocate/free churn while still allowing owned `PrimitiveBlock` handoff.

4. Add copy-range passthrough option where supported
- For high passthrough segments, allow kernel-assisted copy path (similar to merge's direct-io copy strategy) when it is compatible with writer mode.

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
