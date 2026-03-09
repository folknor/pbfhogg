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
  1. ~~**No benchmark data.**~~ Benchmarked (commit `1b10bfd`): Denmark simple
     2259ms (-14% from 2625ms baseline), Japan simple 11,643ms (-8% from
     12,619ms). Sorted pass1 optimization validated — single-pass eliminates
     second file read. Full results in `notes/performance.md`.
  2. ~~**~300 lines of duplication** between `collect_pass1` and `collect_pass1_smart`.~~
     Refactored into `collect_pass1_generic<H: RelationHandler>` with
     `CompleteRelationHandler` (no-op) and `SmartRelationHandler` (collects
     extra way/node IDs). Net -144 lines. Verified via `brokkr verify extract`.
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

- [x] **Run Germany full profiling suite** (4.7 GB, ~496M elements, commit `1b10bfd`).
  Timing: inspect-tags 23.9s, check-refs 74.1s, merge zlib 6.2s, merge none 4.4s.
  Allocations: merge 293 MB net (17+ GB cumulative churn through rewrite pipeline).
  check-refs is single-threaded consumer bound (74s wall, 73s on one core).
  cat --type (zlib): 61.8s, 10.9 GB RSS, 240 GB cumulative alloc (175 MB net).
  Full results in `notes/performance.md`.

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

### Pass 0: referenced-nodes-only index (implemented)

**Implemented** (uncommitted, post `3677069`): `collect_way_referenced_node_ids`
scans way blobs to build an `IdSetDense` bitset (~1.6 GB for planet's ~2B
unique way node refs). `build_node_index_dense` then only inserts nodes
present in the bitset. Reduces touched mmap pages from ~80 GB to ~16 GB at
planet scale.

**Result on plantasjen (30 GB RAM + 8 GB swap):** No improvement at Europe
scale — 2631s vs 2565s baseline (+2.6%, noise). The reduced 16 GB mmap
working set + 33 GB input file page cache still exceeds 30 GB physical
memory, so swap thrashing dominates regardless. The optimization should help
on 64 GB hosts where 16 GB fits in RAM, and is strictly better than before
(fewer pages touched = less swap pressure when memory is merely tight rather
than catastrophically insufficient).

**Denmark (465 MB):** 6.4s, fits entirely in RAM, no measurable difference.

### Sparse index: Planetiler-inspired chunk-indexed array (implemented)

**Implemented** (uncommitted, post pass 0): `--index-type sparse` uses a
`SparseArrayIndex` — chunk-indexed (chunk size 256) sparse array. RAM:
`offsets` Vec<u64> + `start_pad` Vec<u8> (~540 MB at planet). On-disk:
compact packed (lat, lon) values file via read-only mmap (~16 GB for planet).
Way lookups are batched and sorted by file offset, converting random I/O
into sequential scans via `FxHashMap` pre-resolution.

**Denmark (465 MB):** dense 8.4s vs sparse 15.5s (+85%). Overhead is pure CPU
(sorting, hashing) — no I/O pressure at this scale.

**Planet-scale validation pending.** The sparse index should eliminate the page
fault thrashing that makes dense take 96 minutes on plantasjen (30 GB RAM),
since it uses ~540 MB RAM + sequential I/O instead of 16 GB random mmap access.

### Remaining approaches (if sparse index isn't sufficient at planet scale)

1. **On-disk sorted store** — Sort all nodes by ID into a temporary file on
   nvme, then merge-join with way node references (also sorted by referenced
   node ID). Memory = just I/O buffers. Slowest approach but constant memory
   regardless of planet size.

2. **Partitioned/chunked index** — Split the node ID range into chunks (e.g.
   1M IDs each). Process the file in rounds, one chunk at a time — each round
   only needs the chunk's memory. Trades many passes for arbitrarily low
   memory. Similar to osmium's `flex_mem` strategy.

### Quick wins to test first

- [ ] **Planet-scale sparse index validation** — run `add-locations-to-ways
  --index-type sparse` on planet (87 GB) on plantasjen (30 GB RAM + 8 GB swap).
  Expected: eliminates page fault thrashing, completes in reasonable time.
- [ ] **Test dense on 64 GB host** — the pass-0 optimization should eliminate swap
  pressure entirely when 16 GB mmap + input page cache fits in physical memory.

### Measured baselines (commit `69a127f`, plantasjen, 30 GB RAM + 8 GB swap)

| Dataset | Size | Elements | Time | Notes |
|---------|------|----------|------|-------|
| Europe | 33.6 GB | 4.2B (3.7B nodes, 454M ways, 8.2M rels) | 2565s (43m) | buffered, commit `69a127f` (no pass 0) |
| Europe | 33.6 GB | 4.2B | 2611s (43m) | `--direct-io` (+2%, no benefit), commit `69a127f` |
| Europe | 33.6 GB | 4.2B | 2631s (44m) | buffered, post `3677069` (with pass 0), +2.6% noise |
| Planet | 87.7 GB | 11.6B (10.4B nodes, 1.17B ways, 14.1M rels) | 5773s (96m) | buffered, memory-latency-bound, commit `69a127f` |

## Release prep

### crates.io blockers

- [ ] **Publish `protohoggr` first** — currently `path = "../protohoggr"` only. Add `version = "0.2"` alongside the path dep so crates.io resolves it. Publish protohoggr before pbfhogg.
- [ ] **Add `version` to CLI path dep** — `cli/Cargo.toml` needs `version = "0.2"` on the `pbfhogg` dep if we publish pbfhogg-cli too (or skip publishing the CLI crate).
- [ ] **Clarify license** — README mentions MIT but only Apache-2.0 is declared. Pick one story.

### Testing

See `notes/test-plan.md` for the full pre-release test matrix (feature permutations,
I/O modes, CLI commands) and `notes/performance.md` for consolidated baselines.

### Cross-validation known diffs

Three `brokkr verify` commands show known differences vs osmium. These are semantic
disagreements, not bugs — but should be investigated and either fixed or documented
before release.

- [ ] **Planet-scale merge on 32 GB host** — verify `apply-changes` on a full planet file (~80 GB) completes without OOM on the 32 GB dev machine. README claims this should work (adaptive in-flight budget, 600 MB RSS at NA scale). Must validate before release.
- [ ] **`cat --type` OOM on planet (87 GB, 30 GB host)** — Two fixes landed:
  1. Batch-side (commit `abe2782`): `DECODE_BATCH_BYTE_BUDGET = 32 MiB` caps
     decompressed bytes per batch via `for_each_primitive_block_batch_budgeted`.
  2. Writer-side: compression moved into the `par_iter` parallel phase, then
     `write_raw_owned` feeds the writer thread's bounded `sync_channel(32)`.
     Eliminates the unbounded `rayon::spawn` queue that was the main OOM cause.
  Europe (33.6 GB) completes in 121s, 224/8200 batches byte-limited.
  **Planet validation still pending.** Strip `eprintln!` instrumentation
  in `cat_filtered` after planet run.

### Other

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Add a CHANGELOG.md before first tagged release
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

