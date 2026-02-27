# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    scripts/test.sh -- --ignored

`sorted_flag_but_unsorted_nodes_panics` in `tests/read_paths.rs` is `#[ignore]` — it
verifies the debug monotonicity assertion fires on unsorted nodes when `Sort.Type_then_ID`
is declared. Requires `debug_assertions` to be enabled in the test profile. Nightly 1.95
(2026-02-25) has a regression where `debug_assertions` is off in test builds.

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
  | `merge.rs:1045` | `par_iter().map_init()` | Global | Batch classify |
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

## Performance: Linux kernel features for planet-scale I/O

Research notes: `notes/linux-async-io.md`.

Target deployment: nidhogg weekly planet merge on Linux 6.18, planet PBF on erofs.
Nidhogg will use erofs (atomic swap of entire planet data at runtime), so
`Compression::None` PBFs on erofs is the baseline assumption for the optimized path.
The library also needs to work well for the broader OSM ecosystem (standard
zlib-compressed PBFs, any filesystem, any Linux 5.x+), so there are two tiers.

### Tier 1: Generic path (any Linux, zlib PBFs, any filesystem)

Most users won't use io_uring or erofs. The generic buffered I/O path needs to
be excellent on its own. Read throughput is already strong (0.31s parallel, 1.3s
pipelined on Denmark). The gaps are in CLI commands and the buffered merge path.

- [ ] **CLI command performance vs osmium.** Current numbers on Denmark 465 MB
  (commit `3944a3f`, plantasjen, solo runs via `verify/*.sh`):

  | Command | pbfhogg | osmium | ratio |
  |---------|---------|--------|-------|
  | extract --simple | 4.1s | 1.7s | 2.4x |
  | extract (complete-ways) | 8.6s | 2.8s | 3.1x |
  | extract --smart | 11.2s | 3.5s | 3.2x |
  | add-locations-to-ways | 23.4s → 16.2s | 13.1s | 1.24x |
  | tags-filter highway=primary -R | 2.8s | 1.4s | 2.0x |
  | tags-filter amenity=restaurant -R | 2.8s | 1.2s | 2.3x |
  | tags-filter w/highway=primary -R | 2.8s | 0.6s | 4.7x |
  | getid (9 elements) | 1.8s | 0.8s | 2.1x |
  | removeid (59M elements) | 11.1s | — | — |

  All commands already use pipelined reader + pipelined writer (verified Feb 27).
  Buffer reuse (clear+extend pattern) applied to all commands — shaves 5-31%
  but doesn't close the fundamental gap. The remaining ~2x gap across commands
  is architectural: pbfhogg parallelizes decompression only, while osmium
  parallelizes element processing across cores. Evidence: osmium wall/user
  ratios show ~7x parallelism (e.g. tags-filter: 1.7s/12.5s) vs pbfhogg's
  ~2x (3.3s/6.3s). The main thread is the bottleneck.

  Done:

  - [x] **getid/removeid buffer reuse.** `write_element_reuse()` with hoisted
    buffers per block. Removeid 11.1s → 10.5s (-5%).

  - [x] **tags-filter buffer reuse.** clear+extend in all 3 loops.
    amenity=restaurant 2.8s → 2.5s (-10%), w/highway=primary 2.8s → 2.5s (-10%).

  - [x] **extract buffer reuse.** clear+extend in all 4 write helpers, buffers
    declared per-block in all 3 strategies. Denmark within variance (bbox subset).

  - [x] **add-locations-to-ways FxHashMap.** Switched `std::HashMap` → `FxHashMap`
    for node location index. 23.4s → 16.2s (-31%), ratio 1.8x → 1.24x.

  Remaining:

  - [ ] **Blob-type skipping.** Skip decompressing blobs whose element type is
    irrelevant to the command. For `tags-filter w/highway=primary`, ~85% of
    blobs are nodes and can be skipped entirely. Explains the 4.7x gap on
    way-only filters.

    **Infrastructure (DONE):**
    - `Blob::index() -> Option<BlobIndex>` — expose indexdata from header
    - `BlobFilter { want_nodes, want_ways, want_relations }` — public struct
    - Pipeline Stage 2 in `pipeline.rs` — checks `blob.index()` vs filter,
      returns `None` to skip decompression (~5ns check vs ~1-2ms zlib saved)
    - `ElementReader::with_blob_filter()` — builder method
    - `BlobFilter` exported from `lib.rs`
    - Files without indexdata degrade gracefully (all blobs pass through)

    **Per-command status:**

    | Command | Status | Filter | Notes |
    |---------|--------|--------|-------|
    | `cat --type` | DONE | skip non-matching types | `BlobFilter` from `--type` arg |
    | `tags-filter -R` (single-pass) | DONE | union of expression type filters | `blob_filter_from_expressions()` helper |
    | `tags-filter` (two-pass) | DONE | Pass 1: expr union, Pass 2: nodes+matched types | Nodes always included in Pass 2 for way deps |
    | `tags-count --type-filter` | DONE | skip non-matching types | Only when single type specified |
    | `add-locs-to-ways` Pass 1 | DONE | `only_nodes()` | Pass 2 needs all types |
    | `node-stats` | DONE | `only_nodes()` | Only processes nodes |
    | `getid` | TODO | skip types not in ID set | Analyze `IdSet::{node,way,relation}_ids` emptiness |
    | `getid --add-referenced` | TODO | Pass 1: skip by ID types, Pass 2: needs nodes+ways | Similar to tags-filter two-pass |
    | `removeid` | TODO | skip types not in ID set | Same as getid |
    | `diff --type` | TODO | skip non-matching types | Has `TypeFilter`, loads all into memory |
    | `derive-changes` | TODO | skip non-matching types | Has `type_filter`, loads all into memory |
    | `check-refs` | LIMITED | skip relations if `--check-relations=false` | Needs all 3 types for full check |
    | `extract` | LIMITED | skip relations in simple mode only | complete-ways/smart need all types |
    | `fileinfo` | N/A | no benefit | Counts blobs, needs all |
    | `sort` | N/A | already uses BlobIndex | Blob-level permutation sort |
    | `merge` | N/A | already uses BlobIndex | Specialized blob-level filtering |
    | `cat` (no filter) | N/A | raw passthrough, zero decode | Already optimal |

    **Sorted PBF early exit:** In sorted PBFs (`Sort.Type_then_ID`), all
    node blobs precede way blobs precede relation blobs. Commands that only
    need one type can break the pipeline iterator once the first unwanted
    blob type appears. Already naturally supported by `into_blocks_pipelined`
    (dropping the iterator shuts down the pipeline). With indexdata, the
    transition is detected without decompressing the boundary blob.

  - [ ] **Parallel element processing.** Process decoded blocks in parallel
    with ordered output, not just parallel decompression. Would close the
    ~2x wall-time gap by using ~7 cores instead of ~2.

    **The problem:** The pipelined reader parallelizes decompression (Stage 2)
    but delivers blocks to a single consumer thread (Stage 3) which does all
    element processing sequentially. Evidence from wall/user ratios: osmium
    achieves ~7x parallelism (tags-filter: 1.7s wall / 12.5s user) vs
    pbfhogg's ~2x (3.3s wall / 6.3s user).

    **Existing precedent — merge Phase 3→4:** The merge command already does
    parallel element processing with ordered output:
    ```
    rewrite_jobs.par_iter().map_init(BlockBuilder::new, |bb, job| {
        rewrite_block_parallel(block, &diff, bb, creates, kind)
    }).collect()  // → Vec<RewriteOutput { blocks: Vec<Vec<u8>>, stats }>
    ```
    Then writes results sequentially in Phase 4. Each rayon thread owns its
    own `BlockBuilder` (it's `Send`). Output is serialized `Vec<u8>` block
    bytes, not borrowed data — so it can cross thread boundaries.

    **Architecture for filter/write commands:**
    ```
    Stage 1: I/O + decode (existing pipeline, unchanged)
        → delivers Vec<(seq, PrimitiveBlock)> in batches

    Stage 2: Parallel element processing (NEW)
        batch.par_iter().map_init(BlockBuilder::new, |bb, (seq, block)| {
            for element in block.elements() {
                if filter(&element) { bb.add_*(...) }
            }
            (seq, bb.drain_blocks())  // Vec<Vec<u8>>
        }).collect()

    Stage 3: Reorder + sequential write
        Reorder buffer restores block order.
        writer.write_primitive_block(&block_bytes) for each.
        Writer's internal rayon pool parallelizes compression.
    ```

    **Which commands parallelize:**

    | Command | Cross-block state? | Parallelizable? |
    |---------|-------------------|-----------------|
    | `cat --type` | None | Yes, per block |
    | `tags-filter -R` (single-pass) | None | Yes, per block |
    | `tags-filter` (two-pass) | Pass 1 collects IDs | Each pass separately |
    | `getid` / `removeid` | None (IdSet is read-only) | Yes, per block |
    | `add-locs` pass 2 | None (index is read-only) | Yes, per block |
    | `extract` simple | matched_node_ids grows | Per-phase (nodes→ways→rels) |
    | `extract` complete/smart | Multi-pass state | Each pass separately |
    | `tags-count` | Accumulates HashMap | Par + merge-reduce |
    | `sort` | Global ordering | No (already specialized) |
    | `merge` | Already parallel | Already done |

    **Key details:**
    - `BlockBuilder` is `Send` (all `Vec`/`FxHashMap`/primitives). Each rayon
      thread owns one via `map_init(BlockBuilder::new, ...)`.
    - `BlockBuilder` produces one-type-per-block. Each task may produce
      multiple output blocks from a single input block. Use
      `Vec<Vec<u8>>` to collect (same as merge's `RewriteOutput.blocks`).
    - Need `BlockBuilder::drain_blocks() -> Vec<Vec<u8>>` or similar to
      extract all pending blocks as owned bytes.
    - `PbfWriter` takes `&mut self` — must submit blocks sequentially.
      The writer's internal reorder buffer handles compression parallelism.
    - Stats are per-thread counters, summed after `collect()`.
    - Batching (e.g. 64 blocks per rayon dispatch, as merge does) amortizes
      scheduling overhead. Denmark ~7K blocks → ~110 dispatches. Planet
      ~2.5M blocks → ~39K dispatches.

    **Implementation order:**
    1. Add `BlockBuilder::drain_blocks() -> Vec<Vec<u8>>`
    2. Build a `parallel_filter_write()` helper (pipeline → par process →
       reorder → write) that commands can call with a filter closure
    3. Convert `cat --type` first (simplest, no cross-block state)
    4. Convert `tags-filter -R`, `getid`/`removeid`
    5. Convert two-pass commands (tags-filter default, extract, add-locs)
    6. Convert `tags-count` (par + merge-reduce, read-only)

- [ ] **Remove prost dependency.** Replace prost-generated code with hand-rolled
  wire-format encoding/decoding for all remaining protobuf messages. Eliminates
  prost (runtime), prost-build + protox (build-time codegen), and the `build.rs`
  codegen step entirely.

  **Already hand-rolled (read hot path):**
  - `PrimitiveBlock` decode: `WireBlock`, `WireGroup`, `WireDenseNodes`,
    `WireNode`, `WireWay`, `WireRelation` in `src/read/wire.rs` + `block.rs`.
    Zero-copy, zero-alloc iteration via `PackedIter` and `WireMessageIter`.
  - Way/relation encode: direct wire-format in `src/write/wire.rs` +
    `block_builder.rs` (lines 794+). Already bypasses prost for the write
    hot path.

  **Still using prost (6 message types):**

  | Message | Where | Direction | Frequency |
  |---------|-------|-----------|-----------|
  | `BlobHeader` | `blob.rs:355,449`, `mmap_blob.rs:340` | decode | Every blob (~7K/Denmark, ~2.5M/planet) |
  | `Blob` | `blob.rs:449,763,776,788,797,855,951` | decode | Every blob |
  | `HeaderBlock` | `blob.rs:953,941` via `decode_blob<T>` | decode | Once per file |
  | `DenseNodes` + `DenseInfo` + `StringTable` + `PrimitiveBlock` | `block_builder.rs:775-850` | encode | Every node block written |
  | `Blob` + `BlobHeader` | `writer.rs:593-665` | encode | Every output blob |
  | `HeaderBlock` + `HeaderBBox` | `block_builder.rs:1342-1380` | encode | Once per output file |

  **Decode side — BlobHeader and Blob:** These are small, fixed-structure
  messages (~100 bytes each). `BlobHeader` has 3 fields (type, indexdata,
  datasize). `Blob` has 4 fields (raw, raw_size, zlib_data, zstd_data).
  Hand-rolling these is straightforward using the existing `read/wire.rs`
  primitives (varint, len-delimited field). Replaces `prost::Message::decode`
  with direct field-by-field parsing.

  **Decode side — HeaderBlock:** Parsed once per file. More fields (bbox,
  required_features, optional_features, writingprogram, source,
  osmosis_replication_*) but still a simple flat message. Low priority since
  it's not on the hot path.

  **Encode side — DenseNodes:** The last prost-encoded hot path. Currently
  builds `proto::DenseNodes`, `proto::DenseInfo`, `proto::StringTable`,
  wraps in `proto::PrimitiveGroup` + `proto::PrimitiveBlock`, then
  `Message::encode()`. Replace with direct wire-format encoding using
  `write/wire.rs` primitives, same pattern as ways/relations already use.
  This eliminates intermediate Vec allocations from the prost structs.

  **Encode side — Blob/BlobHeader framing:** `frame_blob()` in `writer.rs`
  builds `proto::Blob` + `proto::BlobHeader` then encodes. Simple flat
  messages, straightforward to hand-roll.

  **Benefits:**
  - Eliminates 3 dependencies: `prost` (runtime), `prost-build` + `protox`
    (build-time). Faster clean builds, smaller dependency tree.
  - Removes `build.rs` codegen step and `src/proto/*.proto` files.
  - Removes `pub(crate) mod proto` with its `#[allow(clippy::all)]` blanket.
  - Full control over DenseNodes encoding (potential for further optimization).
  - The `.proto` files are stable (OSM PBF format hasn't changed in years) —
    no need for codegen flexibility.

  **Implementation order:**
  1. BlobHeader decode (small, every-blob hot path, most impact)
  2. Blob decode (small, every-blob hot path)
  3. Blob + BlobHeader encode (writer framing)
  4. DenseNodes + StringTable + PrimitiveBlock encode (write hot path)
  5. HeaderBlock decode + encode (once per file, lowest priority)
  6. Remove prost/prost-build/protox deps, build.rs, proto files

- [ ] **Buffered merge at planet scale.** North America buffered merge is 43s (zlib)
  / 36s (none) vs io_uring's 33s/25s. The buffered path could be improved with
  read-ahead for passthrough blobs and reduced syscall overhead without requiring
  io_uring.

- [ ] **Large folios for mmap reads.** On 6.14+, file-backed mmap gets transparent
  2MB huge pages automatically. Low priority — mmap is not the production hot path
  and is already the slowest read mode. Only relevant at planet scale (80GB, 20M
  TLB entries). If implemented, should be opt-in to avoid regressing small files.

## Before crates.io publish

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Verify edition 2024 is intentional — most published crates use 2021 for broader compatibility
- [ ] Add `tests/test.osm.pbf` to version control (generated by `cargo run --example gen_test_pbf`)
- [ ] Fix crate-level doc example: says `pbfhogg = "0.1"` but Cargo.toml is 0.2.0
- [ ] Add doc comments to `writer.rs` public API (PbfWriter, Compression)
- [ ] Add doc comments to `block_builder.rs` public API (BlockBuilder, Metadata, MemberData)
- [ ] Add crate-level documentation for write/merge workflows (lib.rs)
- [ ] Tighten module visibility: `pub mod commands`, `pub mod osc`, `pub use
  read::file_reader`, `pub use write::file_writer` expose internals as public API
- [ ] Fix `error.rs:27` doc: says "when reading PBF files" but errors occur during writing too
- [ ] Publish to crates.io

## GitHub

- [ ] Write GitHub repo description and tags (openstreetmap, pbf, protobuf, osm, rust)
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

## Refactoring: duplicated metadata extraction

`dense_node_metadata()`, `element_metadata()`, `dense_node_raw_metadata()`,
`element_raw_metadata()`, `flush_block()`, and `rebuild_header()` are shared
helpers in `commands/mod.rs`.

sort.rs still has its own inline metadata extraction (uses `OwnedMetadata`
with owned `String` instead of borrowed `Metadata<'a>`).

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

## Benchmarking

- [ ] Track peak RSS during reads and merges at scale. Denmark for CI, planet for release validation.
- [ ] Run Germany full profiling suite (4.5 GB, ~500M elements). Currently only
  merge timing exists — missing read baselines (tags-count, check-refs),
  decode+write (cat --type), and allocations. Run:
  `scripts/profile-region.sh germany data/germany-20260224-seq4704.osm.pbf data/germany-20260225-seq4705.osc.gz`
  Then update `notes/region-profiles.md` with the results.

## Nice to have

- [ ] Consider adding `serde` feature for element serialization
