# add-locations-to-ways Optimization Plan (Indexed Pipeline First)

## Scope

This plan targets the production path:

1. Input is already indexdata-enriched (`cat` ran first).
2. `add-locations-to-ways` should optimize for that case first.
3. Generic/raw PBF support remains correct, but is secondary for optimization work.

Code references are in `src/commands/add_locations_to_ways.rs` unless noted otherwise.

## P0 — DONE

All six P0 items landed in a single coordinated rewrite of `write_output_passthrough`.
Cross-validated identical to osmium on Denmark.

Key changes: slot-based parallel decode pipeline, passthrough coalescing,
worker scratch reuse, None fallback routing fix, byte-budgeted batching.

## P1 — DONE

All four P1 items landed across commits `1c6f763` (P1.1–P1.3) and `63112f8` (P1.4).
Cross-validated identical to osmium on Denmark. Benchmark: 8724 ms → 6615 ms (−24%).

Key changes: dense lookup single-load decode, three-buffer single-pass way encode,
per-worker DecompressPool, copy-range passthrough (feature-gated: `linux-direct-io`).

## P2 (cleanup and maintainability)

- [ ] Consolidate duplicated transform logic — `process_block` and
  `process_way_block` share most way logic; `process_node_block` repeats node
  filtering. Extract shared helpers that operate on reusable scratch and a mode enum.
- [x] Move raw-frame reader into shared internal utility — `read_raw_frame` now
  lives only in `src/commands/mod.rs`, used by both merge and add-locations.
  The add-locations two-phase `read_blob_header` is a separate function with
  different semantics (header-first read for classify-then-skip).
