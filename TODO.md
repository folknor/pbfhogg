# pbfhogg TODO.

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

## Memory work

Merge instrumentation (peak RSS, per-phase RSS/timers, blob stats, rewrite ratio)
is complete. Memory optimization research (E1.1–E3.1) is done. Pipeline reference
in `notes/pipeline.md`.

- [x] ~~I/O throughput in bench-merge.~~ Done.

## Performance: parallelism

- [ ] `pipeline.rs:14-18` — `READ_AHEAD=16` / `DECODE_AHEAD=32` are hardcoded.
  `READ_AHEAD` bounds the `sync_channel` between the I/O thread (Stage 1) and
  the rayon decode pool (Stage 2) — the I/O thread blocks when 16 compressed
  blobs are buffered. `DECODE_AHEAD` bounds the channel between the decode pool
  and the main-thread reorder buffer (Stage 3) — decode threads block when 32
  decoded blocks are pending. `DECODE_AHEAD` is 2× `READ_AHEAD` because decode
  results arrive out-of-order and the reorder `VecDeque` needs headroom to
  reconstruct file order without stalling Stage 1.

  Backpressure is automatic via bounded `sync_channel`: if the main thread's
  `block_fn` is slow, the decode channel fills → decode threads block on send →
  raw channel fills → I/O thread blocks on send. No manual tuning needed.

  Memory cost: ~16 × 32KB (compressed) + 32 × 1.4MB (decoded) ≈ **51 MB** peak
  pipeline overhead, independent of file size. The `DecompressPool` recycles
  decode buffers so cumulative alloc is near-zero (vs 10.2 GB naive for Denmark,
  ~1.7 TB for planet).

  Making these configurable would require a pipeline config struct on the public
  `for_each_pipelined` API. Hotpath profiling (Denmark through Japan) shows the
  pipeline is balanced at all tested scales — I/O thread doesn't stall, rayon
  workers are barely loaded, main thread is the bottleneck. **Low priority** —
  configure when someone reports a problem on a memory-constrained system.

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  the landscape. Key options:
  - **paralight** (v0.0.8) — lightweight, targets slice/mut-slice parallelism. Can run on top of
    rayon's thread pool via `RayonThreadPool::new_global` (no extra threads). Has proper
    `try_for_each_init` that inits once per thread (rayon inits once per work item). Only needs
    `&` not `&mut` for the rayon backend. Limitation: no scopes, no graph algorithms, no recursive
    parallelism. Max `u32::MAX` elements.
  - **orx-parallel** — has `using()` API for guaranteed per-thread init. No thread pool yet
    (spawns threads per pipeline), on roadmap. No scopes/graph support.
  - **chili** — low-level, only provides `join`. A rayon fork (`par-iter`) builds par_iter on top
    of it. Uses lazy scheduling (less overhead for fine-grained work).
  - **forte** — experimental, rayon-like API with lazy scheduling. Supports spawn, join, scopes,
    scoped spawns. No par_iter or par_bridge yet.
  - **spindle** — built on rayon, optimised for small tasks. Very early.

  Wild's `thread_local` crate trick is also relevant: wrap per-thread state in
  `thread_local::ThreadLocal` and `.get_or()` inside rayon closures to guarantee one init per
  thread. Simple and works today without switching libraries.

  **Current rayon usage (3 sites, all working well):**

  | Site | Pattern | Pool | Purpose |
  |------|---------|------|---------|
  | `pipeline.rs:85-104` | `ThreadPoolBuilder` + `spawn` | Dedicated | Decode pool (Stage 2) |
  | `writer.rs:289` | `rayon::spawn()` | Global | Parallel compression |
  | `merge.rs` | `par_iter().map_init()` | Global | Batch classify + parallel rewrite |
  | `reader.rs:350` | `into_par_iter().try_fold().try_reduce()` | Global | `par_map_reduce` |

  The pipeline decode pool uses a dedicated `ThreadPoolBuilder` with `available_parallelism() - 2`
  threads (reserving 2 for I/O + consumer) and raw `rayon::spawn` — it doesn't use par_iter at
  all. The writer uses global-pool `rayon::spawn` for parallel compression. `par_map_reduce` batch-
  collects all blobs then uses lock-free `into_par_iter` (replaced an earlier `par_bridge` +
  Mutex approach that had contention at 8+ cores). Merge uses `par_iter().map_init(Vec::new, ...)`
  for per-thread decompression buffer reuse during classify.

  The `thread_local::ThreadLocal` trick could replace merge's `map_init(Vec::new, ...)`, but the
  practical gain is zero — `Vec::new()` is stack-only (no heap allocation until first push), so
  rayon re-initing it under work-stealing costs nothing. Switching to paralight would add a
  dependency for marginal benefit on a path that already works well. **Low priority** — revisit
  only if rayon becomes a proven bottleneck (e.g. if parallel `rewrite_block` exposes contention
  in the global pool).

## Performance: regressions from indexdata/tagdata

Measured at commit `23862d1` with `zlib-ng`, Denmark indexed PBF (487 MB).
Old baselines: read at `90df51f` (461 MB non-indexed), write at `def80d9`.

- [ ] **Parallel read: 0.31s → 0.45s (+45%).** `par_map_reduce` regression.
  Larger blob headers (42-byte indexdata + variable tagdata) are parsed across
  all cores. The per-blob overhead is small but multiplied by rayon parallelism
  it adds up. Investigate whether the wire parser skips unknown BlobHeader fields
  efficiently or does unnecessary work.

- [ ] **Write floor: 6.2s → 7.1s pipelined, 7.8s sync (+15-26%).** The
  decode+encode floor moved up because BlockBuilder now computes tagdata
  (per-block tag key set) and bbox (decimicrodegree min/max) for BlobIndex v2.
  Also reading from the larger indexed PBF adds ~1s decode overhead. Profile
  tagdata collection and bbox tracking to find low-hanging fruit.

- [ ] **Write sync zlib:6: 14.5s → 16.4s (+13%).** Follows from the higher
  floor — compression time itself is unchanged but the encode path is slower.

- [ ] **Write sync zstd:3: 8.1s → 9.9s (+22%).** Same root cause as above.

## Performance: CLI commands vs osmium

All CLI commands now beat osmium except extract --simple.

- [ ] **Extract simple: remaining gap vs osmium.** Simple is 1.47x slower on
  Denmark, 1.70x on Japan. The gap is structural: 2 passes vs osmium's 1.
  The extra file read costs ~1.3s on Denmark, ~5s on Japan.
  - [ ] **Single-pass simple with parallel inline writing.** Stream through
    the pipelined reader, collect + filter matching elements per block, batch
    matched blocks for parallel writing via rayon. Eliminates the second file
    read. Challenge: the collection consumer (which is sequential) and the
    write dispatch (which needs rayon) must coexist in the same pass.

- [ ] **`add-locations-to-ways` Pass 1 (hash index building).** Already faster
  than osmium on Denmark (11.4s vs 12.0s), but Pass 1 (FxHashMap node index
  build) is sequential and the bottleneck at larger scales. Options:
  - Parallel hash map build: partition nodes by ID range across threads, each
    builds a sub-map, then merge. Or use a concurrent map (dashmap/flurry).
  - The Dense mmap index variant avoids this entirely (direct indexing, no
    hash table) but requires `vm.overcommit_memory=1` for planet-scale capacity.

  **Production pipeline — runs every planet refresh cycle:**
  ```
  pbfhogg cat → pbfhogg merge → pbfhogg add-locations-to-ways
                                        │
                                        ├── elivagar → PMTiles → nidhogg (tile serving)
                                        └── nidhogg (PBF ingest → query API)
  ```
  The enriched PBF feeds both consumers. elivagar gets inline coordinates
  via `Way::node_locations()`, eliminating the node store entirely.
  North America benchmark (18.8 GB, plantasjen, commit 8704b11): 605s,
  22.8 GB peak RSS (of 25 GB). Node store is ~12.4 GB at NA, extrapolated
  ~44 GB at planet — the dominant memory consumer. With `add-locations-to-ways`
  in the pipeline, estimated elivagar planet peak RSS drops from ~65-75 GB
  to ~15-20 GB. **High priority** — on the critical path for planet-scale
  feasibility on 64 GB hosts.

## Performance: Linux kernel features for planet-scale I/O

io_uring implementation: `src/write/uring_writer.rs`.

Target deployment: nidhogg weekly planet merge on Linux 6.18, planet PBF on erofs.
Nidhogg will use erofs (atomic swap of entire planet data at runtime), so
`Compression::None` PBFs on erofs is the baseline assumption for the optimized path.

- [ ] **Large folios for mmap reads.** On 6.14+, file-backed mmap gets transparent
  2MB huge pages automatically. Low priority — mmap is not the production hot path
  and is already the slowest read mode. Only relevant at planet scale (80GB, 20M
  TLB entries). If implemented, should be opt-in to avoid regressing small files.

## Before crates.io publish

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [x] Fix crate-level doc example: `"0.1"` → `"0.2"`
- [x] Doc comments on `writer.rs` public API — already complete (PbfWriter, Compression, all methods)
- [x] Doc comments on `block_builder.rs` public API — already complete (BlockBuilder, Metadata, MemberData, HeaderBuilder)
- [x] Crate-level write workflow docs in `lib.rs` (sync + pipelined examples)
- [x] ~~Tighten module visibility~~ — `commands` and command re-exports are
  `#[doc(hidden)]`, `file_reader`/`file_writer` are `pub(crate)`
- [x] Fix `error.rs:30` doc: "when reading PBF files" → "when reading or writing PBF files"
- [ ] Publish to crates.io

## GitHub

- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

## Code TODOs

- [x] ~~OSC parsing lat/lon fallback~~ — Not a real issue. Delete nodes
  early-return before lat/lon parsing. Create/modify nodes always carry
  lat/lon per the OSM API spec. Verified across Denmark + Germany diffs
  (0 nodes without lat/lon). Comment added to `src/osc.rs`.

- [ ] **Merge function complexity hotspot:** The merge flow is very large and
  highly stateful. Ref: `src/commands/merge.rs:943`. Good candidate for
  maintainability hardening (fewer latent bugs during future changes).

- [ ] `src/indexed.rs:42` — `relation_ids` field in `IdRanges` is populated but
  unused. `IndexedReader` only has `read_ways_and_deps` (2-pass: filter ways →
  fetch dependent nodes) and `for_each_node`. A `read_relations_and_deps` would
  need 3+ passes: pass 1 filter relations → collect member way/node/relation IDs;
  pass 2 fetch member ways → collect their node refs; pass 3 fetch all dependent
  nodes. Recursive relation members (relations containing relations) add another
  pass or fixpoint loop. The `relations_available()` method is already written
  but commented out (line 80-89). The field and method are zero-cost as-is —
  park until a concrete consumer exists (e.g. extract --smart, or a library user
  doing relation-based filtering).

## Performance review: remaining items

Performance review (boxes 1-8) is complete — 20/24 items implemented.
Full review was in `notes/perf-review/` (deleted). Cross-reference synthesis
had the unified priority list. The 4 open tracked items and minor uncaptured
items are preserved below.

### Open tracked items

- [ ] **P2-12: Remove sqpoll code path from io_uring writer.** `uring_writer.rs`
  has `sqpoll` support (`setup_sqpoll(2000)`, `push_sqe_pair` SQ overflow
  handling, `--sqpoll` CLI flags). Measured <1% benefit across all scales
  (Denmark through North America). Syscall overhead is ~0.29% of wall time.
  Removing it deletes ~30 lines and the kernel 5.12+ dependency. **Deferred
  until planet-scale verification** — if sqpoll shows no gain at 75 GB, delete.

- [ ] **P2-13: Extract pass 1 parallel fold for IdSetDense.** Would give
  ~2-4x speedup on pass 1 (~170s → ~50-85s at planet scale). Attempted and
  reverted (`b67aa96`) — `par_iter` on the consumer side contends with the
  pipeline's dedicated rayon decode pool, causing 14x regression at Denmark
  scale. `IdSetDense::merge()` exists but is `#[allow(dead_code)]`. Needs a
  shared thread pool architecture where pipeline decode and consumer
  parallelism use the same pool.

- [ ] **P3-20: SIMD varint decode/encode in protohoggr.** ~25s read-side +
  ~175s write-side savings at planet scale (attacking the irreducible
  varint floor). Requires a batch-decode API change in `protohoggr` —
  current `Cursor::read_varint()` is byte-at-a-time. High effort,
  speculative. Only worth pursuing after compression is no longer the
  dominant cost (i.e. `Compression::None` production path).

- [ ] **P3-22: Streaming merge-join for derive_changes / diff.** Both
  commands load entire PBFs into memory (`owned_elements.rs`), OOMing at
  planet scale (~680 GB per file). A streaming merge-join on two sorted
  iterators would fix this. High effort, no current use case — neither
  command is used at planet scale in nidhogg.

### Minor uncaptured items

- [x] ~~Unused public decompress functions in `blob.rs`.~~ Deleted 5 functions
  with zero callers.

- [x] ~~BlockBuilder StringTable double String allocation.~~ Fixed: `Rc<str>`
  shared between HashMap key and Vec entry — one heap alloc per unique string.

- [x] ~~Document fd registration stall bound in `uring_writer.rs`.~~ Done.

## Benchmarking

- [x] ~~Track peak RSS during reads and merges at scale.~~ `peak_rss_mb` column in results DB, `VmHWM` captured after merge.
- [ ] Run Germany full profiling suite (4.5 GB, ~500M elements). Currently only
  merge timing exists — missing read baselines (tags-count, check-refs),
  decode+write (cat --type), and allocations. Run:
  `brokkr profile --dataset germany`

## Indexdata awareness: warnings and fast paths

Commands that use `BlobFilter` silently degrade to decompressing all blobs
when the input PBF lacks indexdata. `merge`, `sort`, and `add-locations-to-ways`
already warn + require `--force` (commit d3fba45). The remaining commands fall
into three tiers.

### `is-indexed` CLI command

- [x] ~~**Add `pbfhogg is-indexed <file>` command.**~~ Done. Exit code 0 if
  indexed, 1 if not. Uses `has_indexdata()` from `src/commands/mod.rs`.

### Tier 1: New fast path needed

- [x] ~~**`fileinfo --extended`: header-only fast path.**~~ Done. Reads
  `BlobIndex::count` by `kind` from blob headers — no decompression. Falls
  back to full decode for non-indexed files. Also reports `is_indexed` status.

### Tier 2: Warning when indexdata is missing (BlobFilter already degrades gracefully)

These commands already set `BlobFilter` correctly. Without indexdata, all
blobs pass through and element-level filtering happens after decompression.
No code path change needed — just a warning + `--force`, same pattern as
merge/sort/add-locations-to-ways.

- [x] ~~**`extract` (complete/smart):**~~ Done. Warns + requires `--force`.
- [x] ~~**`tags_filter`:**~~ Done. Warns + requires `--force`.
- [x] ~~**`getid` (include mode):**~~ Done. Warns + requires `--force`.
- [x] ~~**`cat --type`:**~~ Done. Warns + requires `--force`.
- [x] ~~**`tags_count --type`:**~~ Done. Warns + requires `--force`.
- [x] ~~**`node_stats`:**~~ Done. Warns + requires `--force` (~15% speedup,
  skips way/relation blobs). Used frequently from elivagar.

### Tier 3: No benefit (skip)

- `cat` (no filter) — already zero-decode passthrough
- `derive_changes` — reads all types unconditionally
- `diff` (no `--type`) — reads all types unconditionally
- `check_refs` — consumer-bound, relation blobs are ~1-2%, <1% impact
- ~~`node_stats`~~ — moved to Tier 2, warns + requires `--force`

## Nice to have

- [ ] Consider adding `serde` feature for element serialization
