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

## Correctness: same-version delete semantics (libosmium#403)

**Context:** [libosmium#403](https://github.com/osmcode/libosmium/issues/403) documents a
behavior change in libosmium 2.22 that broke Geofabrik's diff-apply workflow. The root cause
is commit [cff8ff42](https://github.com/osmcode/libosmium/commit/cff8ff428287aaf501ddb0928a479d014dd5fdd9)
which changed `object_order_type_id_reverse_version` to order visible objects before deleted
ones when type+id+version+timestamp all match. This was done to fix [osmium-tool#282](https://github.com/osmcode/osmium-tool/issues/282)
(merging extract diffs was order-dependent), but it broke the more common case of applying
diffs where a delete has the same version as the existing object.

**The core issue is undefined behavior in the OSM data model.** As woodpeck (Geofabrik)
notes: "applying a `<delete>` operation with the same version number as an already-present
object does not have a well-defined outcome." There is no spec governing what happens when
a delete has the same version as the existing object — every tool resolves this by convention,
not by contract. Geofabrik's entire planet-scale pipeline relies on this undefined behavior
(delete-wins-on-same-version) because it worked for years across libosmium, osmosis, and
osmconvert. The osmium developers themselves recognized the ambiguity: osmium-tool already
has `--increment-version` on `derive-changes` as an escape hatch. Geofabrik chose not to use
it because the old convention held everywhere — until libosmium 2.22 changed the convention
to fix a different edge case.

**The tension — two conflicting conventions for the same undefined edge:**
- **Applying diffs** (Geofabrik workflow): delete should win on same version → old behavior
- **Merging extract diffs** ([osmium-tool#282](https://github.com/osmcode/osmium-tool/issues/282)):
  when overlapping extracts produce a delete in one and modify in the other for the same
  version, the modify should win → new behavior

The libosmium maintainer (joto) concluded the old behavior is correct for the primary use
case and created a PR to revert. The merge-extract fix was incomplete anyway — lonvia showed
that overlapping extracts still produce spurious deletions regardless of sort order (an object
moves out of extract A into extract B: A's diff has a delete, B's diff has nothing, merged
result deletes the object even though it still exists).

**pbfhogg status — not affected by design:**
- `merge` uses ID-based lookup into a `CompactDiffOverlay` with separate
  `deleted_nodes/ways/relations` HashSets, not version-based sort comparison. A delete in the
  OSC always wins over the base element regardless of version — there is no sort comparator
  tiebreaker involved. This means pbfhogg has **defined** behavior where the OSM data model
  has undefined behavior: the diff is authoritative, period. See `merge.rs:409-434`.
- `derive-changes` emits deletes with the **same version** as the old element (matches
  osmium's default, no `--increment-version` equivalent). See `derive_changes.rs:344-427`.
- `diff` compares elements by content, not version ordering. Not affected.

- [ ] **Add `--increment-version` flag for `derive-changes`.** When set, bump the version
  of deleted elements by 1 in the output OSC. This sidesteps the undefined-behavior edge
  entirely — the delete has a strictly higher version, so every tool agrees it wins. osmium-
  tool has this flag. Not needed for pbfhogg's own pipeline (our merge doesn't consult
  versions), but necessary for interop when other tools consume pbfhogg-generated diffs.
  Given that libosmium's convention on the undefined edge has now changed *and been reverted*
  within a single release cycle, producing unambiguous diffs is the only robust strategy.

## Upstream issue scan (2026-03-03)

Surveyed open/recent issues across libosmium, osmium-tool, osmosis, and OSM-binary.
Items below are relevant to pbfhogg — either as bugs we might share, features worth
noting, or design decisions that inform our approach.

### libosmium

- **#405: Signed char sign-extension rejects BlobHeaders > 127 bytes.** (open, 2026-03-03)
  `get_size_in_network_byte_order()` uses `const char*` + `static_cast<uint32_t>()`,
  which sign-extends bytes ≥ 0x80 on platforms where `char` is signed. Any PBF with
  a BlobHeader > 127 bytes is rejected. **pbfhogg is not affected** — Rust's `u8` is
  always unsigned, and our blob header size parsing in `blob.rs` reads `u32` via
  `read_u32::<BigEndian>()`. However, this confirms that PBFs with large BlobHeaders
  (e.g. from indexdata) are a real interop hazard — libosmium can't read them until
  this is fixed. Filed by us.

- **#389: Non-packed encoding of packed repeated fields.** (closed via #400, 2026-01-06)
  Protobuf-net (C#) emits single-element packed fields as non-packed. libosmium's
  hand-rolled parser assumes packed encoding and silently drops the data (tags lost).
  joto's fix handles the single-value non-packed case but not the general multi-value
  case. **pbfhogg**: our wire parser (`PackedIter` in protohoggr) also assumes packed
  encoding for repeated fields. We should verify our behavior on non-packed input.
  - [ ] **Test pbfhogg reader with non-packed single-value repeated fields.** Create
    a minimal PBF where a packed repeated field (e.g. `keys`, `vals`) is encoded as
    individual non-packed field entries. Verify whether our parser silently drops them
    (like libosmium pre-fix) or errors. If it drops, decide whether to fix — the spec
    says packed is the canonical encoding but decoders "must" accept both.

- **#402: Write to buffer instead of file descriptor.** (open, joto, 2026-02-10)
  libosmium can only write to fd, not in-memory buffer. **pbfhogg already supports
  this** — `PbfWriter::new(writer)` accepts any `Write` impl including `Vec<u8>`.
  No action needed.

- **#393: Generates invalid multipolygon.** (open, joto, 2025-08-24)
  Broken relation input produces invalid multipolygon geometry in libosmium's area
  assembler. Not relevant to pbfhogg — we don't do geometry assembly.

- **#395: Use extra bit in Location.** (closed, joto, 2026-01-18)
  Latitude only needs 31 of 32 bits, freeing one bit for metadata (e.g. tagged/untagged
  node flag). Interesting for `add-locations-to-ways` DenseMmapIndex — we currently use
  8 bytes per slot (lat+lon). A tagged-node bit could eliminate the separate
  `--keep-untagged-nodes` pass, but would require changing the index format.

- **#151: Optimizing the node location store.** (open, joto, 2016)
  `sparse_mmap_array` is slow due to binary search over cache-hostile data. Ideas:
  separate ID/location arrays, compact lookup table, linear scan near target, mini-cache
  for locality. **pbfhogg's `DenseMmapIndex`** uses direct indexing (O(1) lookup, 8
  bytes/slot) which avoids all of these problems — no binary search at all. Our approach
  trades virtual memory (128 GB mmap) for zero lookup overhead. Confirms our design choice.

### osmium-tool

- **#303: Merge ID comparison mismatch with sort.** (closed PR, 2025-12-20)
  `osmium merge` used plain numeric ID comparison while `osmium sort` uses a comparator
  that orders 0 first, then negative IDs, then positive IDs by absolute value. Merging
  files with negative IDs failed. **pbfhogg**: our merge uses `i64` comparison directly.
  Negative IDs are uncommon in production PBFs (used by JOSM for uncommitted data) but
  if we ever need to handle them, we should check our sort/merge ordering.

- **#258: Roaring bitmap for extract memory.** (open, 2022-12-14)
  User suggests roaring bitmaps to reduce extract memory usage. osmium uses
  `id_set<IdType>` backed by a sorted vector — O(n) memory, O(log n) lookup.
  **pbfhogg already uses roaring** (`roaring::RoaringBitmap`) for extract, but our
  primary ID set is `IdSetDense` (chunked sparse bitset, O(1) set/get). For the
  extract use case, `IdSetDense` is better than roaring for dense ID ranges (which
  OSM node IDs are). The osmium issue confirms external demand for this optimization.

- **#234: Extract and check-refs use too much RAM with high node IDs.** (open, 2021-11-08)
  Custom PBF with numerically high node IDs causes osmium's dense node location store
  to allocate enormous arrays. **pbfhogg**: `DenseMmapIndex` uses anonymous mmap with
  128 GB virtual address space — high IDs only increase virtual size, not RSS (pages
  are demand-faulted). `IdSetDense` uses 512-element chunks, so sparse high IDs waste
  at most one 64-byte chunk per populated region. We handle this case well by design.

- **#240: Warn when locations on ways would be lost.** (open, joto, 2022-01-30)
  If input PBF has locations on ways but output format doesn't preserve them, the data
  is silently lost. **pbfhogg**: our writer always preserves way node locations if
  present in the input `BlockBuilder` data. However, format conversions (if we ever
  support XML/OPL output) would need this warning. Low priority — PBF-only for now.

- **#205: fileinfo show PBF blob compression.** (open, 2021-01-06)
  `osmium fileinfo` reports "Compression: none" for PBFs even when blobs use zlib.
  **pbfhogg `inspect`** already reports per-blob compression type. No action needed.

- **#298: tags-filter with OSC — keep delete actions.** (open, 2025-10-05)
  When using tags-filter on OSC files, delete actions are dropped because they have no
  tags. User wants an option to pass through deletes. Interesting edge case — if pbfhogg
  ever supports OSC tags-filter, we'd face the same issue. Low priority.

- **#93: Diff output order with mismatched metadata.** (open, 2018-03-14)
  When input files have different metadata attributes (one has timestamps, other doesn't),
  diff output order is wrong because the version comparator uses timestamps as tiebreaker.
  With missing timestamps, the tiebreaker is `Timestamp(0)` which changes the ordering.
  Same family of issues as libosmium#403 — undefined behavior in version comparison when
  metadata is incomplete. **pbfhogg `diff`** compares by content equality, not version
  ordering, so we're not affected.

### osmosis

- **#150: Duplicate node update crashes with --simplify-change.** (open, 2024-08-06)
  Daily planet diff contained node ID `10767916505` appearing twice with same version 21.
  Osmosis's `SortedHistoryChangePipeValidator` rejects this as unsorted. This is a real-
  world example of same-version duplicates in production diffs — same family as
  libosmium#403. **pbfhogg merge** handles this correctly via ID-based overlay (last
  write wins within the OSC, duplicates are naturally deduplicated).

### OSM-binary (PBF spec)

- **#80: Unable to distinguish valid stream end from broken stream.** (open, 2024-04-19)
  Java PBF reader throws `EOFException` both at normal EOF and mid-stream truncation —
  callers can't distinguish complete from partial reads. **pbfhogg**: our `BlobReader`
  returns `None` at EOF and `Err` on truncation (incomplete blob header or data). We
  handle this correctly.

## Deep-dive findings (2026-03-03)

- [x] **P1 performance: passthrough coalescing currently memcpy-copies full frames.**
  Fixed: coalescers now collect `Vec<Vec<u8>>` chunks instead of concatenating
  into a single `Vec<u8>`. New `PipelinePayload::ByteChunks` variant and
  `write_raw_chunks` API let the writer thread drain chunks sequentially.
  Verified identical output via `brokkr verify merge` and
  `brokkr verify add-locations-to-ways`.

- [ ] **P2 correctness edge case: Null Island ambiguity in dense index sentinel.**
  `DenseMmapIndex` uses `(0,0)` as "unset", so valid node coordinates at exactly
  `0,0` are treated as missing (currently documented as acceptable). If we want
  strict correctness for all coordinates, store a separate occupancy bitmap (1
  bit/node) or reserve an impossible sentinel with explicit valid-bit tracking.
