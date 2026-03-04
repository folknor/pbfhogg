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

- [x] **Measure unsorted impact.** Measured at commit `34672c9` using
  `examples/shuffle_blobs.rs` (deterministic Fisher-Yates, seed 42) on
  Denmark indexed PBF (464 MB, 7396 blobs).

  **Default mode** (untagged nodes dropped): no measurable impact.
  Only 6 relation blobs are passthrough out of 7396 total, so shuffling
  creates at most 6 extra batch flushes — negligible.

  | Input | min (ms) | median (ms) | passthrough | decoded |
  |---|---|---|---|---|
  | Sorted | 5991 | 6475 | 6 | 7390 |
  | Shuffled | 5313 | 5435 | 6 | 7390 |

  **`--keep-untagged-nodes` mode**: **2.25x regression** (median 5.8s → 13.1s).
  Node blobs (6562) become passthrough, creating ~6500 decode↔passthrough
  transitions that each force a batch flush. The decode batch (828 way blobs)
  is fragmented into thousands of tiny batches with terrible rayon amortization.

  | Input | min (ms) | median (ms) | passthrough | decoded |
  |---|---|---|---|---|
  | Sorted | 5650 | 5807 | 6568 | 828 |
  | Shuffled | 7702 | 13053 | 6568 | 828 |

  **Conclusion**: the regression only affects `--keep-untagged-nodes` on
  unsorted input. Production use (`add-locations-to-ways` in the planet
  refresh pipeline) always uses sorted PBFs and drops untagged nodes, so
  this is not a practical concern. If unsorted+keep-nodes becomes a real
  use case, the fix is to buffer interleaved decode/passthrough segments
  and flush them in input order at batch boundaries.

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

## Performance: regressions from indexdata/tagdata (partially resolved)

Measured at commit `23862d1` with `zlib-ng`, Denmark indexed PBF (487 MB).
Old baselines: read at `90df51f` (461 MB non-indexed), write at `def80d9`.

Fixes applied at commit `3bc928b`:
1. Gate indexdata parsing in BlobReader (default on for API compat, disabled in
   `par_map_reduce` and unfiltered pipeline)
2. Single-pass tag key tracking (removed double iteration in add_way/add_relation)
3. Zero-alloc tagdata serialization (sort string table indices, no Box<[u8]>)

A/B results (compression=none, `brokkr results --compare f419ba1 3bc928b`):

| Dataset | Variant | Before | After | Change |
|---|---|---|---|---|
| Japan 2.4 GB | sync-none | 40,682 ms | 38,522 ms | **-5.3%** |
| Japan 2.4 GB | pipelined-none | 35,485 ms | 34,789 ms | **-2.0%** |
| Germany 4.5 GB | sync-none | 83,086 ms | 81,281 ms | **-2.2%** |
| Germany 4.5 GB | pipelined-none | 73,338 ms | 71,696 ms | **-2.2%** |
| Japan 2.4 GB | parallel read | 2,105 ms | 2,098 ms | **-0.3%** |

- [x] **Parallel read: 0.31s → 0.45s (+45%).** Fixed by gating indexdata parsing
  in `par_map_reduce` (commit `3bc928b`). Measured -0.3% on Japan — minimal
  remaining regression. Original Denmark measurement was on smaller non-indexed PBF.

- [x] **Write floor: 6.2s → 7.1s pipelined, 7.8s sync (+15-26%).** Reduced by
  single-pass tag key tracking and zero-alloc tagdata serialization (commit
  `3bc928b`). Write sync-none improved -5.3% on Japan, -2.2% on Germany.
  Remaining floor difference is inherent to computing bbox/tagdata for BlobIndex v2.

- [x] **Write sync zlib:6: 14.5s → 16.4s (+13%).** Addressed by write floor fixes.

- [x] **Write sync zstd:3: 8.1s → 9.9s (+22%).** Addressed by write floor fixes.

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

- [ ] **Benchmark OSM ID ordering overhead.** `osm_id_cmp` / `osm_id_key` replaced
  plain `i64::cmp` in sort, merge, diff, and derive-changes. For positive-only IDs
  (all production PBFs), the extra tuple construction should be optimized away, but
  verify with A/B benchmarks on Denmark and Japan:
  `brokkr bench read`, `brokkr bench merge`, `brokkr bench commands sort`

## Design guidance we're already following
  - libosmium#250 — Comparison without timestamp. Only matters if we ever add version+timestamp ordering. Currently N/A since merge is ID-based.
  - libosmium#325 — Selective node caching = multi-pass. Already the pattern in add-locations-to-ways.
  - libosmium#326 — TagsFilter OR semantics. osmium uses OR, rejected AND PR. pbfhogg's tags-filter already uses OR.
  - libosmium#309 — Geometry recursion diagnostics. pbfhogg doesn't do geometry assembly.

## Upstream issues with action items (2026-03-03)

- [ ] **Use spare latitude bit in DenseMmapIndex for tagged/untagged node flag.**
  [libosmium#395](https://github.com/osmcode/libosmium/issues/395): latitude only needs
  31 of 32 bits (range ±90° vs longitude ±180°), freeing one bit per slot. In
  `DenseMmapIndex` (8 bytes/slot = lat+lon) this bit could store whether a node is
  tagged, eliminating the need for the separate `--keep-untagged-nodes` mode in
  `add-locations-to-ways`. Currently the tagged/untagged decision happens at a different
  stage — this would fold it into the index itself.

- [ ] **tags-filter should preserve delete actions from OSC input.**
  [osmium-tool#298](https://github.com/osmcode/osmium-tool/issues/298): new `tags-filter-osc`
  subcommand — filters creates/modifies by tag expressions, forwards all deletes
  unconditionally. ~170 lines, no base PBF needed. Design and implementation plan
  in `notes/tags-filter-osc-support.md`.

- [ ] **Map version=-1 and changeset=-1 to None during PBF parsing.**
  [libosmium#247](https://github.com/osmcode/libosmium/issues/247): Osmosis writes -1
  for version and changeset when metadata is absent (protobuf default value). pbfhogg
  currently stores these as literal `Some(-1)` instead of `None`. Round-tripping an
  Osmosis-generated PBF preserves -1 as a real version number, which is semantically
  wrong. Fix: in `WireInfo` / `WireDenseInfo` parsing, map -1 to None for version and
  changeset fields.

- [ ] **Check `LocationsOnWays` header before add-locations-to-ways.**
  [libosmium#199](https://github.com/osmcode/libosmium/issues/199): if the input PBF
  already has inline way-node coordinates (indicated by `LocationsOnWays` in
  `optional_features`), `add-locations-to-ways` should warn or skip rather than
  building a full node location index for redundant work.

- [ ] **Set history header flags when writing PBF from history-carrying input.**
  [libosmium#366](https://github.com/osmcode/libosmium/issues/366): when output may
  contain deleted elements (`visible=false`), the PBF header should include
  `HistoricalInformation` as a required feature and set
  `has_multiple_object_versions=true`. Without these, consuming tools may ignore the
  `visible` field. Relevant for any future command that writes PBF from OSC or history
  data.

- [ ] **Report all duplicate IDs in validation, not just the first.**
  [libosmium#349](https://github.com/osmcode/libosmium/issues/349): libosmium's
  `CheckOrder` handler throws on the first duplicate ID. For data quality auditing,
  reporting all duplicates is more useful. If pbfhogg's `check-refs` or sort commands
  detect duplicates, they should collect and report all of them rather than stopping at
  the first.

- [ ] **tags-filter: resolve matched relation members.**
  [osmium-tool#215](https://github.com/osmcode/osmium-tool/issues/215): when a relation
  matches (without `-R`), pbfhogg does not pull in its member ways, nodes, or
  sub-relations. Worse than osmium's original bug (which only missed sub-relations).

- [ ] **extract: antimeridian polygon handling.**
  [osmium-tool#220](https://github.com/osmcode/osmium-tool/issues/220),
  [#226](https://github.com/osmcode/osmium-tool/issues/226): no polygon splitting for
  regions crossing the 180th meridian. Extracts of Alaska, Russia, Fiji etc. will
  produce incorrect results. osmium fixed this with antimeridian polygon splitting in
  PR #229.

- [ ] **Warn when LocationsOnWays data is lost through cat/sort/merge.**
  [osmium-tool#240](https://github.com/osmcode/osmium-tool/issues/240):
  `build_output_header` doesn't propagate optional features from the input header. If a
  PBF with embedded way-node coordinates is cat'd or sorted, the locations are silently
  lost. Same open bug in osmium.

- [ ] **add-locations-to-ways: preserve untagged relation-member nodes.**
  [osmium-tool#239](https://github.com/osmcode/osmium-tool/issues/239): untagged nodes
  that are relation members are dropped along with all other untagged nodes. Fixing
  requires an extra pass over relations to collect member node IDs. Same limitation in
  osmium. Low priority.

## Missing commands (osmium-tool parity)

- [ ] **`merge-changes`** — merge multiple OSC files, optionally simplifying
  (keep only latest version per object). Relevant upstream:
  [osmium-tool#262](https://github.com/osmcode/osmium-tool/issues/262) (duplicate IDs
  from broken input),
  [#282](https://github.com/osmcode/osmium-tool/issues/282) (same-version delete
  ambiguity with overlapping extracts),
  [osmosis#150](https://github.com/openstreetmap/osmosis/issues/150) (duplicate
  same-version updates abort simplify),
  [osmosis#72](https://github.com/openstreetmap/osmosis/issues/72) (simplification
  must not merge distinct action types with same ID).

- [ ] **`time-filter`** — filter history PBF to a snapshot at a given timestamp.
  Relevant upstream:
  [osmium-tool#285](https://github.com/osmcode/osmium-tool/issues/285) (output header
  timestamp must reflect the filter cutoff, not the original file).

