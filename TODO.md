# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    brokkr check -- --ignored

`sorted_flag_but_unsorted_nodes_panics` in `tests/read_paths.rs` is `#[ignore]` — it
verifies the debug monotonicity assertion fires on unsorted nodes when `Sort.Type_then_ID`
is declared. Requires `debug_assertions` to be enabled in the test profile. Nightly 1.95
(2026-02-25) has a regression where `debug_assertions` is off in test builds.

## Performance

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  alternatives (`paralight`, `orx-parallel`, `chili`, `forte`, `spindle`).
  Revisit only if rayon becomes a proven bottleneck.

- [ ] **Extract sorted pass1 (`37b7c19`): benchmark and clean up.** Parallelizes
  way/relation ID collection for sorted PBFs by batching blocks and using
  `par_iter` with thread-local Vecs. Algorithm is correct but has open issues:
  1. **No benchmark data.** Never measured — no results in brokkr at this commit.
     Two prior attempts regressed 14x and 33-43x respectively. Must run
     `brokkr bench extract` (Denmark + Japan, indexed) before and after to
     validate the optimization actually helps.
  2. **~300 lines of duplication** between `collect_pass1` and `collect_pass1_smart`.
     The sorted path, unsorted fallback, and batch-flush logic are near-identical.
     Extract shared helpers or a generic pass1 driver.
  3. **`Mixed | Empty` handler is a full sequential fallback** that defeats the
     optimization. A single Mixed block flushes both batches and processes all
     element types sequentially. Correct but fragile — rare in practice.
  4. **Vec-per-block allocation in batch helpers.** Each `par_iter` task creates
     new Vecs for local IDs. For 64 way blocks with ~8000 ways each, the
     `local_node_ids` Vec could hold millions of entries per batch.
  5. **`decode_threads(1)` may under-utilize.** Reduces pipeline decode to one
     thread since the consumer does its own parallelism. Sensible tradeoff but
     may leave the I/O thread idle waiting for the single decoder.

- [x] **`merge --locations-on-ways`: parallelize Phase 2.5 blob scans** —
  Passthrough node blob decompression dispatched to rayon pool. At Denmark
  scale (883 blobs) the improvement is negligible (<5ms) since per-batch
  work is already small, but should help at planet scale with larger scan
  sets. Note: the 12,790 "needed from base" nodes that aren't found are
  untagged nodes dropped by ALTW — they don't exist in the base PBF. This
  is inherent to the LocationsOnWays workflow, not a bug.
  `build_from_diff` already correctly excludes deleted ways (they're removed
  from `way_index` by the OSC parser).

- [ ] **Run Germany full profiling suite** (4.5 GB, ~500M elements). Currently only
  merge timing exists — missing read baselines (`tags-count`, `check-refs`),
  decode+write (`cat --type`), and allocations. Run:
  `brokkr profile --dataset germany`

## Consolidation

- [ ] **Investigate shared reader thread for raw-frame streaming** — `merge.rs` spawns a
  dedicated reader thread (bounded mpsc channel, `read_raw_frame` loop, skips OsmHeader).
  `sort.rs` does sequential seeks and `add_locations_to_ways.rs` has its own scan loop.
  Investigate whether extracting `spawn_reader_thread(path, direct_io) -> (JoinHandle,
  Receiver<RawBlobFrame>)` into `mod.rs` would benefit sort and ALTW, or whether their
  access patterns are too different (random seek vs sequential scan).

## Release prep

### crates.io blockers

- [ ] **Publish `protohoggr` first** — currently `path = "../protohoggr"` only. Add `version = "0.2"` alongside the path dep so crates.io resolves it. Publish protohoggr before pbfhogg.
- [ ] **Add `version` to CLI path dep** — `cli/Cargo.toml` needs `version = "0.2"` on the `pbfhogg` dep if we publish pbfhogg-cli too (or skip publishing the CLI crate).
- [x] **Add `readme` field** — added to root `Cargo.toml` (CLI has no README, skipped).
- [x] **Add `rust-version`** — set to `1.85` (edition 2024 minimum) in both Cargo.toml files.
- [x] **`hotpath` dep** — must stay unconditional; `#[hotpath::measure]` attributes are used throughout library code and need the crate present to compile. When the `hotpath` feature is off (default), proc macros expand to nothing — zero runtime cost.

### Public API cleanup

- [x] **Audit wildcard re-exports** — replaced all 6 wildcard `pub use` with explicit named re-exports (42 types). Downgraded 6 internal blob.rs free functions from `pub` to `pub(crate)`.
- [x] **`commands` module visibility** — keeping `#[doc(hidden)] pub`. CLI crate depends on these as a separate package so `pub(crate)` won't work. Feature-gating adds complexity for no compile-time benefit (heavy deps already gated). Standard Rust convention (serde, tokio do the same).
- [ ] **Clarify license** — README mentions MIT but only Apache-2.0 is declared. Pick one story.

### Testing

- [ ] **Planet-scale merge on 32 GB host** — verify `apply-changes` on a full planet file (~80 GB) completes without OOM on the 32 GB dev machine. README claims this should work (adaptive in-flight budget, 600 MB RSS at NA scale). Must validate before release.
- [ ] **`cat --type` OOM on planet (87 GB, 30 GB host)** — OOM-killed both with and without `--direct-io` (~19-22 GB written, 27.8 GB RSS at kill). The pipelined writer's rayon pool and reorder buffer accumulate too many in-flight blocks. Unlike merge, `cat` lacks adaptive byte budgeting. Works on europe (32.4 GB). Fix: add backpressure or memory-bounded batching to the cat filtered path.

### Other

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Add a CHANGELOG.md before first tagged release
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

