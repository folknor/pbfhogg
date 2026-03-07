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

## ALTW memory optimization

### Current implementation

`DenseMmapIndex` is a direct-mapped array: `index[node_id] = (lat, lon)`.
Node ID is the array index, so lookup is O(1) — but the array must span the
full node ID range, not just the number of nodes. OSM node IDs go up to ~13B,
so the index is 16B slots × 8 bytes = 128 GB virtual address space. Only
touched pages consume physical memory (~80 GB for planet's 10.4B nodes).
Gaps between IDs waste address space but not much physical memory.

This is the standard approach — osmium uses the same strategy (`dense_mmap_array`).
It's the fastest possible access pattern when it fits in RAM.

### The problem

Planet ALTW takes 96m on plantasjen (30 GB RAM, 8 GB swap) — CPU mostly idle,
bottlenecked on page faults in the dense mmap index. The kernel constantly
evicts and re-faults pages across the 80 GB working set. `vmstat` would show
heavy `si`/`so` (swap in/out) activity. Production host (64 GB) should be
fine but still tight — the working set barely fits.

Planet stats: 10.4B nodes read, 285M written (97% dropped as untagged), 1.17B
ways processed, 14.1M relations, 452 passthrough blobs, 50K decoded, 0 missing
locations. Output 88.4 GB (+0.7% from embedded way-node coordinates).

### Alternative approaches (in priority order)

1. **Two-pass with referenced-nodes-only index** — Pass 1: scan all ways to
   collect the set of referenced node IDs (~2B unique IDs for planet). Pass 2:
   stream through nodes, only indexing those in the referenced set. Memory
   drops from ~80 GB touched to ~16 GB (2B × 8 bytes). The extra pass is
   sequential I/O. Could use a sorted Vec or compact hash map for the
   referenced ID set (~16 GB for 2B i64s).

2. **On-disk sorted store** — Sort all nodes by ID into a temporary file on
   nvme, then merge-join with way node references (also sorted by referenced
   node ID). Memory = just I/O buffers. Slowest approach but constant memory
   regardless of planet size.

3. **Partitioned/chunked index** — Split the node ID range into chunks (e.g.
   1M IDs each). Process the file in rounds, one chunk at a time — each round
   only needs the chunk's memory. Trades many passes for arbitrarily low
   memory. Similar to osmium's `flex_mem` strategy.

All three approaches shift the bottleneck from random mmap faults to sequential
I/O, which is where `--direct-io`, io_uring, and erofs provide real benefit.
The current random 8-byte reads across 128 GB of mmap is the worst possible
access pattern for all three of those tools — `--direct-io` actually adds 2%
overhead for ALTW because sequential readahead from page cache is faster.

### Quick wins to test first

- [ ] **Larger nvme swap** (64-128 GB) on europe dataset — measure how much
  swap sizing alone improves the 30 GB host story before writing new code.
  Current 8 GB swap + 30 GB RAM = 38 GB addressable, well below the ~30 GB
  touched by europe's 3.7B nodes.

### Measured baselines (commit `69a127f`, plantasjen, 30 GB RAM + 8 GB swap)

| Dataset | Size | Elements | Time | Notes |
|---------|------|----------|------|-------|
| Europe | 33.6 GB | 4.2B (3.7B nodes, 454M ways, 8.2M rels) | 2565s (43m) | buffered |
| Europe | 33.6 GB | 4.2B | 2611s (43m) | `--direct-io` (+2%, no benefit) |
| Planet | 87.7 GB | 11.6B (10.4B nodes, 1.17B ways, 14.1M rels) | 5773s (96m) | buffered, memory-latency-bound |

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

See `notes/test-plan.md` for the full pre-release test matrix (feature permutations,
I/O modes, CLI commands) and `notes/performance.md` for consolidated baselines.

- [ ] **Planet-scale merge on 32 GB host** — verify `apply-changes` on a full planet file (~80 GB) completes without OOM on the 32 GB dev machine. README claims this should work (adaptive in-flight budget, 600 MB RSS at NA scale). Must validate before release.
- [ ] **`cat --type` OOM on planet (87 GB, 30 GB host)** — OOM-killed both with and without `--direct-io` (~19-22 GB written, 27.8 GB RSS at kill). The pipelined writer's rayon pool and reorder buffer accumulate too many in-flight blocks. Unlike merge, `cat` lacks adaptive byte budgeting. Works on europe (32.4 GB). Fix: add backpressure or memory-bounded batching to the cat filtered path.

### Other

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Add a CHANGELOG.md before first tagged release
- [x] **All 4 feature permutations pass clippy + tests** (commit `a52ac80`): default,
  `linux-direct-io`+`linux-io-uring`, `--no-default-features`, and
  `--no-default-features`+linux features. Fixed 6 latent clippy errors in
  `direct_reader.rs`, `uring_writer.rs`, `diff.rs`, `blob.rs` plus 2 compile
  errors in `uring_writer.rs` (missing `VecDeque` import, unclosed delimiter).
- [x] **io_uring runtime validated** (commit `eb60cb5`): merge bench with `--uring`
  works correctly after compile fixes. Requires `RLIMIT_MEMLOCK >= 16 MB`.
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

