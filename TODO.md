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

## Performance: add-locations-to-ways batch sizing on unsorted input

The passthrough ordering fix (flush decode batch before accumulating passthrough
blobs) ensures correct element ordering in the output (nodes → ways → relations),
but has a potential performance impact on unsorted indexed PBFs. On sorted input,
the flush triggers exactly once (at the way→relation boundary) — zero cost. On
unsorted input with interleaved blob types, every decode→passthrough transition
triggers a batch flush, producing many small batches with worse rayon amortization.

- [ ] **Measure unsorted impact.** Generate an unsorted indexed PBF (e.g.
  shuffle blob order in a Denmark PBF) and benchmark add-locations-to-ways
  vs the sorted variant. If the difference is measurable, consider a smarter
  approach: track interleaved decode/passthrough segments and flush them in
  input order at batch boundaries, preserving large batches while maintaining
  correct output ordering.

## Performance: parallelism (low priority)

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

## Inspect: `--blocks` improvements

Current `--blocks` dumps one line per block — unusable at planet scale (~300K
blocks). The per-type summary (block counts + sizes) is already shown without
`--blocks`; these items fill the gap between that and the raw dump.

- [ ] **Machine-readable output (`--json`).** JSON output for piping to `jq`
  or other tools. Makes the raw per-block listing useful for scripting even at
  scale. Scope: inspect-only initially, could extend to other commands later.
- [ ] **Anomaly highlighting.** Only show blocks that deviate from the norm —
  unusually small (partial batches), unusually large, or mixed-type blocks.
  These are the interesting ones when debugging a PBF. Requires defining
  "anomalous" thresholds (e.g. <50% or >150% of median elements for type).

## Before crates.io publish

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Publish to crates.io

## GitHub

- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

## Code TODOs

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

## Performance review

- [ ] **P2-12: Remove sqpoll code path from io_uring writer.** `uring_writer.rs`
  has `sqpoll` support (`setup_sqpoll(2000)`, `push_sqe_pair` SQ overflow
  handling, `--sqpoll` CLI flags). Measured <1% benefit across all scales
  (Denmark through North America). Syscall overhead is ~0.29% of wall time.
  Removing it deletes ~30 lines and the kernel 5.12+ dependency.

  **North America results confirm sqpoll is pure overhead.** At commit `a6ebbfe`
  (18.8 GB input, 322K blobs, 19.6K rewrites), sqpoll was consistently slower
  than plain uring across all compression modes:

  | Variant | uring (ms) | sqpoll (ms) | delta |
  |---|---|---|---|
  | zlib | 15,157 | 16,349 | +8% slower |
  | none | 11,850 | 12,346 | +4% slower |

  sqpoll burns a kernel thread spinning on the SQ, stealing a CPU core from
  rayon's compression/rewrite pool. The benefit — skipping `io_uring_enter()`
  syscalls (~1-2µs each) — is irrelevant at pbfhogg's IO submission rate
  (hundreds/sec of 256KB writes, not 500K+ small random IOPS). sqpoll's sweet
  spot is NVMe random 4K IO at extreme IOPS where per-syscall cost is a
  meaningful fraction of per-IO latency. Large sequential writes are the
  opposite of that. **Safe to delete without planet-scale verification** — the
  workload characteristics don't change at larger scale, only the duration.

- [ ] **P2-13: Extract pass 1 parallel fold for IdSetDense.** Would give
  ~2-4x speedup on pass 1 (~170s → ~50-85s at planet scale). Attempted and
  reverted (`b67aa96`) — `par_iter` on the consumer side contends with the
  pipeline's dedicated rayon decode pool, causing 14x regression at Denmark
  scale. `IdSetDense::merge()` exists but is `#[allow(dead_code)]`. Needs a
  shared thread pool architecture where pipeline decode and consumer
  parallelism use the same pool.

## Benchmarking

- [ ] Run Germany full profiling suite (4.5 GB, ~500M elements). Currently only
  merge timing exists — missing read baselines (tags-count, check-refs),
  decode+write (cat --type), and allocations. Run:
  `brokkr profile --dataset germany`

## Code quality: duplicated business logic

Audit findings from code quality review. Ranked by effort/impact.

Completed: 5 high-value helpers in `mod.rs` (require_indexdata, flush_local,
BATCH_SIZE, Result alias, TypeFilter), batch collection loop, parallel batch
drain, RawBlobFrame consolidation, ReorderBuffer utility, CLI flag groups.

Full deep-dive with 6 large consolidation opportunities reviewed and rejected
(see `notes/consolidation-review-{1..6}-*.md` for detailed reports). Two small
concrete improvements identified:

- [ ] **Promote `ensure_*_capacity_local` to `mod.rs`.** Merge already has
  `ensure_node_capacity_local`, `ensure_way_capacity_local`,
  `ensure_relation_capacity_local` (3-line helpers wrapping the
  `if !bb.can_add_*() { flush_local(...) }` pattern). Promoting these to
  `mod.rs` replaces the inline capacity check at 28 emission sites across
  cat, getid, tags_filter, extract, and add_locations_to_ways.

- [ ] **Hoist tag/ref/member buffer Vecs into `map_init`.** In cat, getid,
  tags_filter, and extract, the `tags_buf`, `refs_buf`, `members_buf` Vecs
  are created inside the `map_init` closure body (re-allocated per batch).
  Moving them into the init tuple (like add_locations_to_ways already does
  for `refs_buf`/`locations_buf`) gives cross-batch reuse on rayon threads.

## Deep-dive findings (2026-03-03)

- [ ] **P1 performance: passthrough coalescing currently memcpy-copies full frames.**
  `merge` and `add-locations-to-ways` coalescing appends frame bytes into one
  `Vec<u8>` (`extend_from_slice`) before `write_raw_owned`, so "passthrough"
  still copies bytes in userspace when copy-file-range path is unavailable.
  See `src/commands/merge.rs::coalesce_passthrough` and
  `src/commands/add_locations_to_ways.rs::coalesce_passthrough`.
  Consider segmented passthrough buffers (`Vec<Vec<u8>>` + vectored write or
  writer-side chunk API) to eliminate large memcpy overhead on rewrite-light
  workloads.
  Investigation/design note:
  `notes/passthrough-coalescing-memcpy-investigation-2026-03-03.md`

- [ ] **P2 correctness edge case: Null Island ambiguity in dense index sentinel.**
  `DenseMmapIndex` uses `(0,0)` as "unset", so valid node coordinates at exactly
  `0,0` are treated as missing (currently documented as acceptable). If we want
  strict correctness for all coordinates, store a separate occupancy bitmap (1
  bit/node) or reserve an impossible sentinel with explicit valid-bit tracking.
