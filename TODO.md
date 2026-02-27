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

  | Command | pbfhogg | osmium | ratio | notes |
  |---------|---------|--------|-------|-------|
  | cat --type way | **1.06s** | 2.23s | **0.48x** | parallel + blob-skip (indexdata) |
  | tags-filter amenity=restaurant -R | **0.46s** | 1.16s | **0.40x** | parallel + blob-skip |
  | getid (9 elements) | **0.38s** | 0.84s | **0.45x** | parallel |
  | tags-count --type way | **0.35s** | 0.60s | **0.58x** | fold+reduce + blob-skip (indexdata) |
  | tags-filter w/highway=primary -R | **0.44s** | 0.55s | **0.80x** | parallel + blob-skip |
  | tags-filter highway=primary 2pass | 2.69s | 2.42s | 1.11x | two-pass, parallel Pass 2 |
  | add-locations-to-ways | 11.42s | 11.98s | 0.95x | Pass 1 hash build is bottleneck |
  | extract --simple | 4.08s | 1.65s | 2.47x | incremental state, cannot parallelize |
  | extract (complete-ways) | 4.45s | 2.74s | 1.62x | Pass 2 parallel, Pass 1 sequential |
  | extract --smart | 6.12s | 3.20s | 1.91x | Pass 3 parallel, Passes 1-2 sequential |

  All commands use pipelined reader + pipelined writer. All write passes use
  parallel element processing via rayon batches (64 blocks per dispatch).
  Blob-type skipping via indexdata provides additional gains for type-filtered
  commands. Ratios below 1.0 = pbfhogg is faster. Numbers from
  `scripts/bench-commands.sh all` on Denmark 483 MB, commit `5d2d759`.

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
    with ordered output, not just parallel decompression.

    **Pattern:** Collect decoded `PrimitiveBlock`s into batches of 64, then
    `batch.par_iter().map_init(BlockBuilder::new, ...)` processes blocks in
    parallel. Each rayon thread owns a `BlockBuilder`, flushes serialized
    blocks to `Vec<Vec<u8>>` via `flush_local()`. Sequential phase writes
    results in order. Stats are per-thread, merged after `collect()`.

    **Per-command status:**

    | Command | Status | Notes |
    |---------|--------|-------|
    | `merge` | DONE | `rewrite_block_parallel` (original precedent) |
    | `cat --type` | DONE | `process_batch` + `process_block`, 2.63s → 1.23s |
    | `tags-filter -R` (single-pass) | DONE | `process_filter_batch`, 2.8s → 0.46s |
    | `tags-count` | DONE | `par_iter().fold().reduce()` merge pattern, 0.81s → 0.35s |
    | `getid` / `removeid` | DONE | `process_filter_batch`, getid 2x faster than osmium |
    | `add-locs-to-ways` Pass 2 | DONE | `process_batch` with `&NodeLocationIndex` shared across threads |
    | `tags-filter` (two-pass) | DONE | Pass 2 parallel batch, `Pass2IdSets` shared across threads |
    | `extract` complete-ways | DONE | Pass 2 parallel batch, `ExtractPass2IdSets` shared across threads |
    | `extract` smart | DONE | Pass 3 parallel batch, `ExtractPass3IdSets` shared across threads |
    | `extract` simple | BLOCKED | Single-pass incremental state — see below |
    | `sort` | N/A | Already specialized (blob-level permutation) |

    **Extract parallelization constraints.** Extract is the one command where osmium
    is significantly faster (~2-3x on Denmark). The bottleneck is the *collection*
    passes, not the write passes (which are now parallelized):

    - **Simple strategy:** Single-pass, builds node/way ID sets incrementally as
      elements are encountered. Each way lookup depends on the full node ID set
      collected so far; each relation lookup depends on the full way ID set.
      The OSM element ordering dependency (nodes→ways→relations) makes this
      inherently sequential.

    - **Complete-ways Pass 1 / Smart Pass 1:** Same incremental ID collection as
      simple. `collect_pass1_matches()` sorts bbox_node_ids lazily on first way
      encounter, then matched_way_ids lazily on first relation encounter. Cannot
      parallelize without a fundamentally different approach.

    - **Smart Pass 2 (scan-only):** Collects extra_node_ids from ways matching
      extra_way_ids. Read-only lookups on sorted slices but mutates a shared Vec.
      Could be parallelized with thread-local Vecs + merge, but the pass is I/O
      bound (only visiting way blobs) so the gain would be minimal.

    **Possible approaches to close the gap:**
    - [ ] **Parallel ID collection with IndexedReader.** Use blob-level indexdata
      to partition the file into node/way/relation ranges. Process all node blobs
      in parallel to build bbox_node_ids (embarrassingly parallel — each node is
      independent). Then process way blobs in parallel (bbox_node_ids is immutable
      by then). Then relation blobs in parallel. This is a different architecture
      from the current pipelined approach and would require IndexedReader-based
      scanning rather than sequential `into_blocks_pipelined`.
    - [ ] **Concurrent ID sets.** Use `dashmap` or atomic bitsets to allow
      parallel writes to the ID collection during Pass 1. Overkill for the
      current single-type-per-phase approach but could work with a redesigned
      multi-phase collector.
    - [x] **Profile osmium's approach.** DONE — see analysis below.

    **Osmium extract analysis (source: `data/osmium-tool/`, `data/libosmium/`).**
    Osmium uses the same algorithmic structure as pbfhogg (simple=1 pass,
    complete-ways=2 passes, smart=3 passes) with identical element ordering
    dependencies. The speed difference comes from data structures and decode
    optimizations, not a fundamentally different algorithm.

    **Finding 1: O(1) chunked bitset vs O(log n) binary search (DOMINANT).**
    Osmium stores ID sets in `IdSetDense` (`libosmium/include/osmium/index/id_set.hpp`),
    a chunked sparse bitset: `Vec<Option<Box<[u8; 4MB]>>>` with `chunk_bits=22`.
    Each chunk covers 33M IDs. `set()` and `get()` are 3 instructions: one array
    index, one byte offset, one bitmask. Memory: 1 bit per ID present in the
    chunk, 4MB per allocated chunk, zero for empty ranges.

    pbfhogg uses sorted `Vec<i64>` + `binary_search()`. For 52M Denmark nodes,
    that's a ~400MB contiguous array with O(log n) ≈ 25 comparisons per lookup,
    each a potential L2/L3 cache miss. On the complete-ways Pass 2 hot path
    (52M nodes × 2 ID set lookups), this is ~300ns/node vs osmium's ~6ns/node —
    a **~50x difference on the lookup-dominated inner loop**.

    The sorted Vec was chosen over BTreeSet (5x memory savings) and HashSet
    (cache-friendly sequential access), which was the right tradeoff at the time.
    But a bitset is strictly better: O(1), cache-friendly (sequential bit scanning),
    and comparable memory for dense ID ranges (node IDs are dense 1..12B).

    - [ ] **Replace `Vec<i64>` + `binary_search` with a chunked dense bitset.**
      Hand-roll a Rust equivalent of osmium's `IdSetDense`: chunked
      `Vec<Option<Box<[u8; CHUNK_SIZE]>>>` with bit-level set/get. ~50 lines.
      CHUNK_SIZE=4MB (matching osmium's `chunk_bits=22`) covers 33M IDs per
      chunk. For Denmark's 52M nodes: 2 chunks allocated = 8MB total vs current
      ~400MB sorted Vec. Planet (12B node IDs): ~364 chunks = ~1.5GB bitset vs
      ~96GB sorted Vec (impossible). This is the single highest-impact change.
      Roaring bitmaps (`roaring` crate, already a dependency) are an alternative
      but heavier — the hand-rolled bitset is simpler, zero-dep, and matches
      the proven osmium design exactly.

    **Finding 2: Metadata skipping in collection passes.**
    Osmium passes `read_meta::no` for all non-write passes:
    - complete-ways Pass 1 (`strategy_complete_ways.cpp:175`)
    - smart Pass 1 (`strategy_smart.cpp:310`)
    - smart Pass 2 (`strategy_smart.cpp:324`)

    The PBF decoder has a dedicated `decode_dense_nodes_without_metadata()`
    path (`pbf_decoder.hpp:632`) that skips version, timestamp, changeset, uid,
    user, and visible fields entirely. For ways and relations, individual fields
    are skipped via `pbf_node.skip()`. Metadata is ~30-40% of dense node data
    by byte volume.

    pbfhogg always decodes all fields via the wire-format parser. The `WireInfo`
    struct is parsed for every element even when only IDs and coordinates are
    needed (collection passes) or only IDs and refs are needed (way matching).

    - [ ] **Add skip-metadata mode for collection passes.** Add a flag or
      alternative code path in the wire-format decoder that skips `WireInfo`
      fields when the consumer only needs IDs + coordinates (Pass 1) or
      IDs + refs (Pass 2 way scanning). Estimated ~15-25% decode speedup
      on collection passes. Could be a `BlobFilter`-style builder method
      on `ElementReader`, or a per-block decode flag.

    **Finding 3: Integer bbox containment vs float conversion.**
    Osmium's `Box::contains()` (`libosmium/include/osmium/osm/box.hpp:224-229`)
    does 4 int32 comparisons on raw decimicrodegree coordinates:
    ```
    location.x() >= bottom_left().x() && location.y() >= bottom_left().y() &&
    location.x() <= top_right().x() && location.y() <= top_right().y();
    ```
    The `Location` class stores `int32_t m_x, m_y` natively — no conversion.

    pbfhogg converts nanodegrees → f64 via `1e-9 * self.nano_lat() as f64`
    before each bbox test (`extract.rs:479,486`). The i64→f64 cast + f64
    multiply + 4 f64 comparisons cost ~5ns/node vs ~2ns/node for int32.
    At 52M nodes this is ~156ms vs ~104ms — small but free to fix.

    - [ ] **Use integer bbox containment.** Convert the bbox to
      decimicrodegree int32 once at startup. Compare `dn.decimicro_lat()` /
      `dn.decimicro_lon()` directly (already available, `elements.rs:53-60`).
      Eliminates the i64→f64 conversion and uses integer comparison.
      Polygon containment can keep f64 (rare path, already has bbox
      fast-rejection).

    **Remaining bottleneck: `add-locations-to-ways` Pass 1 (hash index building).**
    Pass 2 is now parallel, but Pass 1 (building the FxHashMap node index) is
    sequential and dominates wall time. Denmark: ~11.3s total vs osmium ~10.3s.
    The hash index build is ~6-7s (52.5M node inserts into FxHashMap). Options:
    - Parallel hash map build: partition nodes by ID range across threads, each
      builds a sub-map, then merge. Or use a concurrent map (dashmap/flurry).
    - The Dense mmap index variant avoids this entirely (direct indexing, no
      hash table) but requires `vm.overcommit_memory=1` for planet-scale capacity.

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
