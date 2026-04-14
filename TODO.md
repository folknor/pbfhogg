# pbfhogg TODO

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` — it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    brokkr check -- --ignored

`tests/geocode_index.rs` has 6 `#[ignore]` tests — they build a geocode index from the
Denmark PBF and query it. ~154s in release mode. Run with:

    cargo test --release --test geocode_index -- --ignored

`sorted_flag_but_unsorted_nodes_panics` in `tests/read_paths.rs` is `#[ignore]` — it
verifies the debug monotonicity assertion fires on unsorted nodes when `Sort.Type_then_ID`
is declared. Requires `debug_assertions` to be enabled in the test profile. Nightly 1.95
(2026-02-25) has a regression where `debug_assertions` is off in test builds.

## Next up (2026-04-13)

- [ ] **Multi-extract way classify per-worker scratch** — line 868
  uses `|| ()` init, allocates `vec![Vec::new(); n]` per block.
  Node and relation phases already use per-worker state. Fix: change
  init to `|| vec![Vec::<i64>::new(); n]`, clear between blocks.
- [ ] **diff v3: non-overlapping block skip** — use indexdata min/max
  ID to skip decode for blocks entirely OldOnly or NewOnly (misaligned
  boundaries). Additive on shipped v1+v2. Low risk. Note:
  derive_changes must still decode OldOnly (needs element IDs for
  OSC XML delete output).
- [ ] **`--allow-missing` for apply-changes** — the single prerequisite
  for incremental extract (~10s vs 862s). Insert new elements that
  don't exist in the base PBF, then re-extract to filter to bbox.

## Performance

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  alternatives (`paralight`, `orx-parallel`, `chili`, `forte`, `spindle`).
  Revisit only if rayon becomes a proven bottleneck.

## Cross-pipeline optimization

Cross-thread buffer retention is **solved** — `DecompressPool` (commit
`8f6999b`) recycles decompression buffers in the pipelined reader. The
remaining architectural concern is thread oversubscription (two concurrent
rayon pools: decode + batch processing), not retention.

See [notes/altw-optimization-history.md](notes/altw-optimization-history.md)
for the complete plan: 20 items across 5 priority groups, covering infrastructure
fixes, planet blockers, external join P2b/P2c, and all affected commands.
See [notes/pipelined-reader-retention.md](notes/pipelined-reader-retention.md)
for the April 2026 audit. Sequential conversion was attempted for
getparents (commit `c912e4d`) and reverted — 4.7x regression on
Denmark (1400ms vs 300ms). Decompression dominates, not per-block
processing. **No remaining pipelined paths should be converted to
sequential.** Renumber converted separately (external join
architecture, not driven by retention/oversubscription).

## Milestone 1: Planet-safe production pipeline — COMPLETE

## Milestone 2: Performance supremacy

Goal: fastest or equal on every PBF transform operation, with published
benchmarks. The write path is the remaining frontier.

### Raw group passthrough

Raw frame passthrough is shipped for extract simple — the 3-phase barrier
pipeline classifies blobs in parallel and writes matching raw frames via
pread workers, bypassing decode+re-encode entirely. Simple extract now
beats osmium (4.4s vs 7.2s Japan, 100s vs 350s Europe sequential baseline).

Raw frame passthrough is now shipped for cat --type (matching blobs
written as raw compressed frames, planet 207s → 43s, 4.8x) and
getid --invert (blobs with no ID-range intersection pass through raw,
Denmark 1.9s → 0.5s, Japan 8.6s → 1.3s). getid include mode skips
decompression of non-intersecting blobs (planet 71.5s → 32.5s, 2.2x).

The remaining opportunity is extending raw passthrough to other
re-encoding commands: tags-filter, renumber, time-filter.
These still fully decode and re-encode via BlockBuilder.
For tags-filter: blobs where ALL elements match the tag expression
could be passed through raw (requires blob-level tag index check).
For renumber/time-filter: every element is modified, so raw passthrough
does not apply — the win here is write-path throughput instead.
See [notes/raw-group-passthrough.md](notes/raw-group-passthrough.md).

Four per-group raw passthrough primitives are committed as scaffolding
for partial-match blobs (e.g., extract boundary blobs where some groups
match and some don't). Currently unused — blob-level passthrough handles
the common case. See `notes/raw-group-passthrough.md` "Infrastructure":
- `PrimitiveBlock::raw_group_bytes(index)` — raw PrimitiveGroup bytes
- `PrimitiveBlock::raw_stringtable_bytes()` — raw StringTable bytes
- `PrimitiveBlock::block_scalars()` — granularity, lat/lon offset
- `frame_raw_block()` in `src/write/raw_passthrough.rs` — assemble
  PrimitiveBlock from raw components

### Write-path throughput

After raw group passthrough, `BlockBuilder` (`src/write/block_builder.rs`)
and `PbfWriter` (`src/write/writer.rs`) are the next bottleneck for commands
that must re-encode partial-match groups. Opportunities: SIMD varint encoding
in `src/write/wire.rs` (the write-side protobuf primitives), zlib compression
level tuning, and reducing per-element overhead in
`BlockBuilder::add_node/add_way/add_relation` (string table construction
is the hot path — FxHashMap lookup + Rc<str> alloc per unique string).
See [notes/SIMD.md](notes/SIMD.md) for the varint research.

**Zlib level tuning:** extremely low priority. Investigated multiple
times in the project's history with no actionable outcome. Default
level 6 matches osmium and is the right choice for interop. zstd is
better for internal pipelines but the production pipeline already
works. See [notes/zlib-level-tuning.md](notes/zlib-level-tuning.md).

### Reviewer findings (2026-03-29)

**Do later:**

- [ ] **Tags-filter raw passthrough via lightweight ID scanner** — the
  `count_in_range >= blob_count` check was unsound (extraneous IDs from
  other blobs inflate count). The correct approach: a cheap wire-format
  ID-only scanner per blob that verifies every element ID is in the
  included set without full PrimitiveBlock decode. If all match, raw
  passthrough. Only worth implementing if broad filters (e.g.,
  `building=*`) are a common use case. Flagged by 3/6 reviewers.

- [ ] **`pread_execute` opens a new `Arc<File>` per call** — simple extract
  calls it 3 times for the same input file. Could share the file handle
  across phases. Minor (~1µs per open). Flagged by 1/10 reviewers.

- [ ] **Simple extract phase 3 relation classify is sequential** — "needs
  full PrimitiveBlock (member access)" comment at `extract.rs` ~line 1472.
  Could use `parallel_classify_phase` like complete/smart phase 3.
  Relations are ~2K blobs at Europe — small gain but inconsistent with
  other strategies. Flagged by 1/10 reviewers.

- [ ] **No `fadvise(DONTNEED)` after pread in `parallel_classify_phase`** —
  external join's stage 2 workers call fadvise per pread, classify
  workers don't. At Europe scale (~2 GB compressed) this is fine. At
  planet scale (~87 GB) could accumulate page cache. Low priority since
  current planet-scale paths don't use `parallel_classify_phase` for
  heavy scans. Flagged by 1/10 reviewers.

- [ ] **Simple extract node_scanner skips non-dense Node messages** —
  `node_scanner.rs` only parses DenseNodes (line 15, 43). On legacy
  PBFs with field-1 Node messages, `bbox_node_ids` would be incomplete,
  cascading into missing ways and relations. Not reachable in practice
  (all modern PBFs use DenseNodes). Flagged by 1/10 reviewers.

### Smaller items

- [ ] **getid include: pread skip for non-matching blobs** — the include
  path now skips decompression via ID-range filtering (planet 71.5s →
  32.5s), but still sequentially reads the entire file to check each
  blob's header. A header-only scan + pread of only matching blobs
  would reduce planet from 32.5s to under 1s (only 3-9 blobs need
  reading). Low priority — 32.5s is already fast for planet-scale.
- [ ] `tags_count.rs` parallel path — `parallel_classify_phase` with
  per-worker CountMap accumulation. Tag counting is order-independent,
  so the merge is straightforward. Would restore parallel decode for
  unfiltered `inspect tags` on planet. Low priority.
- [ ] ALTW dense pass 2 decode-all fallback (`write_output_decode_all` in
  `src/commands/add_locations_to_ways.rs` ~line 1045) — uses
  `into_blocks_pipelined` processing all blobs. Retention solved by
  DecompressPool. Only triggers with `--force` on non-indexed PBFs.
  Pipelined decode + par_iter justified (heaviest per-block work).
  See retention audit for details.

## Milestone 3: Beyond the benchmark

Goal: the obvious choice for every OSM data processing task, not just
the fastest one.

### Multi-extract

Single-pass multi-extract shipped for simple strategy on sorted input
(commit `542aad0`). Reads PBF once, classifies each element against N
regions, writes to N sync-mode PbfWriters. 3-phase barrier (nodes →
ways → relations) with per-region IdSetDense + BlockBuilder. Memory:
N × ~1.5 GB at planet scale. Falls back to sequential for unsorted
input or --clean. Verified via `brokkr verify multi-extract`.

**Known issues:**

- [ ] **strip-4 verify failure** — `brokkr verify multi-extract --regions 5`
  on Denmark: strip-4 has 1 fewer node than sequential (41643 vs 41644).
  Passes with 3 and 4 regions. Only fails with 5 regions where strip
  boundaries fall at exact integer longitudes (8,9,10,11,12,13). Likely
  a floating-point rounding issue in brokkr's bbox strip generation,
  not a pbfhogg bug. Pre-existing since multi-extract shipped.

**v2 improvements:**
See [notes/multi-extract-optimization.md](notes/multi-extract-optimization.md)
for full analysis of 6 optimization opportunities.

- [x] **Parallel decode** — write phases converted from sequential
  BlobReader to pread-from-workers via `multi_extract_pread_write`.
  Workers decode blobs in parallel, classify against N regions, produce
  N × Vec<OwnedBlock>. Consumer routes to N sync-mode writers via
  ReorderBuffer. Denmark 5-region: 6.7s → 2.0s (3.4x). Japan 5-region:
  32.5s → 8.1s (4.0x). Single-pass now 2.7x faster than 5 sequential
  extracts at Japan scale (8.1s vs 22s).
- [ ] **Spatial index** — grid or R-tree over regions for O(1)
  per-element lookup instead of O(N). Required for 200+ regions where
  linear scan becomes the bottleneck. Simple grid (3600×1800 cells of
  0.1°, precompute overlapping regions per cell) is sufficient.
- [ ] **Complete/smart strategies** — per-region way/relation ID
  tracking. Memory: N × ~3 GB (bbox_node_ids + all_way_node_ids per
  region). Feasible for ~10 regions on 30 GB host, ~40 on 128 GB.
- [ ] **Raw passthrough** — infrastructure in place: `NodeBlobInfo`
  tracks per-region containment, `multi_extract_pread_write_nodes`
  handles passthrough via ReorderBuffer interleaving. Currently only
  fires when a blob is contained in ALL N regions (useful for N=1 or
  fully-overlapping regions). Per-region passthrough for disjoint
  strips needs hybrid decode+raw consumer path — decode once, write
  raw to contained regions, route elements to non-contained regions.

**Reviewer findings (2026-04-09):**

- [ ] **Raw passthrough unsafe for polygon regions** — `contained_in`
  is computed from each slot's bbox, not polygon geometry. For polygon
  or multipolygon extracts, "blob bbox contained in region bbox" does
  not prove every node is inside the polygon — can raw-copy
  out-of-polygon nodes. Pre-existing issue, not introduced by the
  allocation fixes. Flagged by sweep review (bugs/codex).
- [ ] **O(workers × regions) scaling for large N** — each worker
  allocates N BlockBuilders (~500 KB each). At N=50, ~200 MB across
  8 workers. At N=100+, ~400 MB. Monitor but acceptable for typical
  use (5-20 regions). Flagged by 2/6 reviewers.

### Export (GeoJSON/GeoPackage)

The bridge to the GIS ecosystem. Streaming PBF → GeoJSON/GeoJSONSeq
export. The pieces exist in the codebase:
- Reader: `ElementReader` for element iteration
- Geometry: `src/geo.rs` has point-in-polygon, ring assembly from way
  refs, Douglas-Peucker simplification
- Coordinates: `Way::node_locations()` from enriched PBFs (ALTW output),
  or inline coordinate resolution via the dense/external index
- Multipolygons: relation member assembly is in extract's smart strategy

The export command would iterate elements, resolve geometry (points for
nodes, linestrings for ways, polygons for multipolygon relations), and
write GeoJSON features to stdout or a file. Tag mapping (which tags
become GeoJSON properties) needs a configuration model.
See [notes/geojson-export-design.md](notes/geojson-export-design.md)
for the v1 design: GeoJSONSeq from ALTW-enriched PBFs, streaming
single-pass, tag expression and bbox filtering.

### Command surface

- [ ] Resolve or document known semantic differences in verify output.
  Three commands have known diffs: extract (relation inclusion criteria),
  diff (14-element version comparison), check-refs (occurrences vs unique).
  See `brokkr verify all` output and README cross-validation section.
- [ ] Auto-selection: `--index-type auto` exists (dense vs external).
  Extend to other decisions: sequential vs pread-from-workers based on
  available RAM and blob count; compression level based on output target;
  batch size based on core count. Config or heuristic, not manual flags.
- [ ] Migration guide from other tools — command mapping table, behavioral
  differences, indexdata workflow explanation. Build on existing
  `reference/osmium-parity.md`.
- [ ] **`renumber` — minor optimization (current: 194 s / 3m14s, planet).**
  Planet: 194 s, 3.3 GB peak anon, zero temp disk (commit `cb99106`).
  - [ ] **Varint encode lookup table.** 256-entry for single-byte varints
    in the reframe functions. Est. −2 to −3 s wall.
  - [ ] **Skip `way_id_set` if way rank derivable from schedule.** Sorted
    input means new way ID = `start_way_id + global_position`. Derive from
    schedule prefix sums instead of building a full IdSetDense. Saves ~160 MB.
  - [ ] **Finer stage 2d reframe breakdown.** Split `reframe_ms` into
    parse/lookup/encode/frame to identify which sub-step dominates.

- [ ] **`add-locations-to-ways --index-type external` — next round.**
  Planet: 1,462 s → 1,075 s (−26%). Europe: 608 s → 422 s → ~391 s (−36%).
  Peak anon: 16.7 GB → 8.7 GB → 5.9 GB (−65%).
  See `notes/altw-optimization-history.md`.

  Stage 4 per-ref floor is ~10.7 ns/ref (varint decode + mmap coord fetch +
  zigzag encode). Wire-format way reframe eliminated structural overhead.

  **Tried and failed**: in-memory `coords_by_rank` (`Vec<AtomicU64>`) — OOM'd
  at 28.2 GB on Europe (3.6B unique referenced nodes × 8 bytes). Does not
  fit on 30 GB host. See commit `2f41848` sidecar data.

  **Reviewer projection (planet-claude)**: after file-backed coords_by_rank
  + fused stage 2+3 + 12-byte records + varint skip: ~200 s Europe,
  ~530 s planet. That's 2× from current, 10× from original baseline.

  **Execution plan (ordered, each step validates before the next):**

  - [x] **Step 1: Shrink rank record to 12 bytes.** `(u32 local_rank, u64
    slot_pos)` = 25% I/O reduction. Shipped in `cfa916f`.

  - [x] **Step 2: File-backed coords_by_rank.** Dense `(lat, lon)` by rank
    to temp file during parallel node pass. Shipped in `6293ade`.

  - [x] **Step 3: Parallel stage 2.** AtomicUsize dispatch, N workers each
    claim rank buckets, load + counting-sort + pread coord slice + resolve.
    Shared slot bucket files with per-bucket mutexes (256 FDs total).
    Worker-local per-slot-bucket buffers flushed at 256 KB threshold.
    Europe: stage 2 124s → 91s (−27%), s2_resolve_ms 51s → 62s (summed
    across 6 workers). Shipped in `5e652f2` + `c7fdb4c`.

  - [x] **Step 4: Overlap stage 1B + node pass.** Shipped in `b1bddd5`.

  - [ ] ~~**Step 5: Fuse stage 2 + 3.**~~ **Ruled out.** Rank order and slot
    order are unrelated — direct scatter into coord_slots requires either
    billions of 8-byte pwrites (pathological syscall cost) or 37 GB of
    in-memory scatter buffers. Stage 3 exists precisely to bridge this gap
    with sequential I/O. The current architecture (slot bucket files +
    parallel stage 3 scatter) is the right design.

  - [x] **Step 6: Stage 4 — memory-fault bound, not arithmetic.
    Extensively tested, ~141s appears to be the floor for this
    architecture on this hardware.**

    Split timer (commit `b99af0c`, UUID `d7a08d2f`) proves coord mmap
    access is 25× more expensive than encode: `s4_way_coord_read_ms`=532s,
    `s4_way_delta_encode_ms`=21s. 374K major faults + 1.05M minor faults.

    **Experiments tried and reverted (all flat or worse on Europe):**

    | Experiment | Stage 4 | majflt | Result |
    |-----------|---------|--------|--------|
    | Baseline (MADV_SEQUENTIAL, 6 workers, work-stealing) | 141s | 374K | — |
    | Per-ref micro-opts: varint skip + batch reads (`70c87c1`) | ~141s | — | Flat, reverted |
    | Per-blob pread replacing mmap (`0330a9b`) | 145s | 19K | Flat — traded faults for syscall overhead |
    | Contiguous partitioning + 3 workers (`b26a172`) | 405s | 466K | 3× regression — starved consumer |
    | MADV_RANDOM (`0860164`) | 157s | 9,167K | Worse — killed readahead |
    | No madvise (`74ceba0`) | 143s | 197K | Tied — fewest faults, kept |

    **Conclusions:**
    - Per-ref arithmetic is negligible (encode 21s vs read 532s)
    - Per-blob pread eliminates faults but adds equivalent syscall overhead
    - Fewer workers starves the decompress/reframe pipeline
    - madvise tuning doesn't move wall time; no-advice has fewest faults
    - The ~141s floor is structural: 6 workers need CPU parallelism for
      decompress+reframe, but 6 concurrent mmap readers on 37 GB inevitably
      thrash. Reducing workers fixes thrashing but starves the pipeline.
    - TLB misses are irreducible with mmap at this scale (37 GB / 4 KB =
      9.5M pages, TLB has ~2K entries)
    - Breaking this floor requires a fundamentally different coord
      representation, not a different read primitive or scheduling tweak

  **Reviewer round 2026-04-13 (arch, perf, planet — commit `b99af0c`):**

  Instrumented run UUID `d7a08d2f`. Stage timings: S1 87s, S2 88s, S3 51s,
  S4 141s. Total 398s. All 5 reviewers recommended per-blob pread as the
  single next step — tested and found flat (see table above). The reviewer
  consensus was correct about the diagnosis (memory-fault bound, not
  arithmetic) but the recommended fix (pread) didn't help because it
  traded one I/O bottleneck for another.

  **Stage 1B write batching — tried and reverted (2026-04-14):**

  - [x] ~~Batch rank record writes per way or per blob.~~ Implemented
    as per-bucket blob-local byte staging (`bucket_staging: Vec<Vec<u8>>`),
    one `write_all` per non-empty bucket per blob. Reduced write_all
    call count 4.69B → 14.16M (-331×) as designed, but **stage 1 wall
    regressed +22.9s (76.977s → 99.887s, +30%) on Europe**, clean
    same-day comparison (baseline UUID `4acf4ed8` commit `091fc5b`, test
    UUID `4733d6b2` commit `e16674b`). Reverted.

    **Why it didn't work:** `BufWriter` (256 KB capacity) was already
    the right batching layer — each per-ref `write_all` was a cheap
    12-byte memcpy into the buffer, not a syscall. The staging layer
    added an extra memcpy (ref → `bucket_staging[bucket]` → BufWriter)
    and scattered writes across 256 `Vec<u8>` tails per blob, thrashing
    L1/TLB. Every CPU-bound counter regressed simultaneously:
    `s1b_scan_ms` +25.5s, `s1b_rank_ms` +20.9s, `s1b_encode_write_ms`
    +21.3s — consistent with shared-resource contention, not with the
    intended write-call reduction.

    **Lesson:** before batching on top of a buffered writer, confirm
    the per-call cost is actually a bottleneck. The TODO estimate
    (−6s wall) and multi-reviewer consensus (arch-claude, planet-claude,
    perf-codex) were both wrong here because they extrapolated from
    `s1b_encode_write_ms=137s cumulative` without accounting for
    BufWriter already amortizing the syscall cost. Reviewers and call
    counts don't prove a bottleneck; measurement does.

  - [ ] **Grouped-by-local-rank emission in stage 1B.** Workers sort or
    group output by local rank before flushing. Stage 2 counting sort
    becomes k-way merge or free. Second-best tier 1 item (perf-codex r1).

  **Still open — fuse stages 2+3 (planet-claude sketch):**

  - [ ] **Fuse stage 2+3 via sort-then-coalesced-pwrite.** Each stage 2
    worker resolves a rank bucket, sorts resolved entries by slot_pos,
    coalesces consecutive entries into larger pwrites directly into the
    coord_slots file. Eliminates slot bucket files + stage 3 I/O entirely.
    With ~10 consecutive slots per way: 1.4M pwrite calls per bucket,
    256 buckets / 4 workers = ~9s. Sort: radix sort 14M records × 16 bytes
    = ~70ms/bucket, ~4.5s total. Estimated: 88s + 14s = ~102s vs current
    88 + 51 = 139s. Saves ~37s + eliminates 75 GB temp disk.
    (planet-claude). Higher complexity.

  **Still open — structural opportunities (fundamentally different coord
  representation needed to break the stage 4 floor):**

  - [ ] **Postings-by-rank (CSR representation).** Replace flat
    `(rank, slot_pos)` records with `offsets[rank] + slot_positions[...]`.
    Stage 2 becomes trivial: load one coord, scatter to all slot positions
    in its posting list. Eliminates counting-sort. Higher design risk.
    (planet-codex r1).
  - [ ] **Way-ordered payloads instead of flat coord_slots.** Produce
    per-way-blob coordinate payloads directly. Stage 4 splices ready-made
    location fields instead of per-ref lookups. Hard part: assembling
    way-ordered payloads externally. (perf-codex r1). This is the only
    approach that could break the stage 4 floor — it eliminates the
    37 GB mmap entirely by producing coords in consumption order.
  - [ ] **Blob-local rank batching in stage 4.** In assembly, extract
    node IDs from way refs, compute ranks, gather coords from
    coords_by_rank file, splice locations. Could collapse stages 1B+2+3
    entirely. Risk: lookup locality. (planet-codex r1).
  - [ ] **Collapse pass A + pass B into single way scan.** Only possible
    with a staging model that doesn't need rank before emitting records.
    Postings representation might enable this. (planet-claude r1,
    planet-codex r1).

  **Still open — tuning:**

  - [ ] **More than 256 buckets.** Reduces per-bucket working set.
    Tradeoff: more files, more metadata. (arch-codex r1).
  - [ ] **io_uring for stage 2 and stage 4 preads.** (planet-codex r1,
    planet-claude).
  - [ ] **Output compression: Zstd(1).** Consumer write may be
    zlib-bottlenecked. Zstd(1) ~3× faster, similar ratio. (arch-claude r1).
  - [ ] **Reduce stage 4 decode workers by 1, give core to compression.**
    Check if consumer blocks on write_primitive_block_owned.
    (planet-claude r1).
  - [ ] **Stage 3 parse-and-scatter directly from raw bytes.** Currently
    parses all resolved entries into `Vec<ResolvedEntry>` before
    scattering. Could parse and scatter inline. Moot if stage 3 is fused
    into stage 2. (perf-codex).

  **Moot or resolved:**

  - [x] ~~Deeper bucket loader pipeline queue~~ — moot, stage 2 is parallel-for.
  - [x] ~~Direct mmap of shard files in stage 2~~ — moot, using coord slice pread.
  - [x] ~~Shrink ResolvedEntry~~ — was contingent on fused 2+3.
  - [x] ~~Partition node stream for parallel stage 2~~ — moot, coords_by_rank
    eliminated the merge.
  - [x] ~~Per-blob pread for coord_slots~~ — tried, flat (syscall overhead
    replaced fault overhead).
  - [x] ~~Contiguous range partitioning + fewer workers~~ — tried, 3×
    regression (starves decompress pipeline).
  - [x] ~~MADV_RANDOM~~ — tried, 24× more faults, 11% slower.
  - [x] ~~MADV_SEQUENTIAL~~ — replaced by no-advice (fewer faults, same wall).
  - [x] ~~Software prefetch~~ — irrelevant, bottleneck is disk faults not TLB.
  - [x] ~~SIMD/AVX2 for way reframe~~ — encode is 21s (1.5%), not worth it.

  **Correctness:**

  - [ ] **`(0, 0)` sentinel in `CoordSlots::get` is a latent bug.** Conflates
    "unresolved coord" with "coords happen to be (0°, 0°)." Should use
    a separate resolved/unresolved flag or a sentinel outside the valid
    coordinate range. (planet-codex).

### Ecosystem

- [ ] crates.io version badge — `https://img.shields.io/crates/v/pbfhogg`
- [ ] docs.rs badge — `https://img.shields.io/docsrs/pbfhogg`
- [ ] CI status badge — `https://img.shields.io/github/actions/workflow/status/folknor/pbfhogg/ci.yml`
  (requires GitHub Actions CI workflow)
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] CI with benchmark regression guard.
- [ ] API documentation for library consumers.
- [ ] PyO3 Python bindings (read/write API for the Python ecosystem).
- [ ] Packaged "planet on 32 GB" reference pipeline (documented, runnable).

### Non-traditional optimization research

Ordered by reviewer consensus (6 reviewers, 3 archetypes: perf, arch, planet).
The first three form a dependency chain. The last two are independent
hardware-level tuning. Investigate allocators and columnar together as
Milestone A, SIMD as Milestone B, huge pages and NUMA as Milestone C.

**Milestone A: data layout + allocation (investigate together)**

- [ ] **Global allocator investigation** — jemalloc and mimalloc were
  previously benchmarked at <1% wall time difference on Denmark (483 MB)
  and removed as CLI features (they broke `--all-features` builds due to
  duplicate `#[global_allocator]` definitions). Re-investigate at planet
  scale where allocator behavior under cross-thread free patterns and
  high churn may differ. Meta/Facebook has restarted active jemalloc
  development — revisit `tikv-jemallocator` and `mimalloc` when the
  arena/scratch work is complete and the remaining alloc profile is
  clearer. Measure RSS and wall time on planet add-locations-to-ways,
  merge, and build-geocode-index.

- [ ] **1. Custom allocators (per-block arena)** — 4/6 reviewers ranked 1st.
  See [notes/arena-allocator-research.md](notes/arena-allocator-research.md)
  for full landscape, alloc profiling data, and 5-step implementation plan.
  Key finding: `parse_and_inline` generates ~829 MB alloc churn (Japan) /
  ~14 GB (planet est.) from two temp `Vec<(u32, u32)>` per block. Step 1
  (thread-local scratch Vecs) eliminates ~97% of this with zero risk.
  Steps 2-5 escalate to bumpalo, columnar layout, pipelined reader
  re-enablement. Top crate candidates: `bumpalo` (v3.20, zero deps,
  stable), `bump-scope` (v2.2, scoped sub-allocations), or hand-rolled
  50-line bump allocator.

**Scratch buffer reuse audit (step 1 of arena research):**

`parse_and_inline` scratch is done (829 MB → 48 MB, -94%). The following
per-iteration allocations remain across the codebase, ordered by impact:

- [x] **`scan_block_ids` / `scan_block_tags` groups Vec** — NOT FEASIBLE.
  `Vec<&[u8]>` borrows from function parameter `raw: &[u8]`, lifetime
  changes each call. Cannot pass scratch from outer scope. Typically
  1-3 entries — negligible allocation.

- [ ] **Geocode pass 3 stage A par_iter** — per-way `Vec::new()` inside
  `flat_map_iter` closure (`builder.rs` ~line 1226). Hard to fix due to
  parallel iterator ownership semantics. `SmallVec` could avoid heap
  allocation for ways with few segments. Low priority.

- [ ] **Per-relation members_scratch** — 14M relations × ~10 members ×
  24 bytes = 3.4 GB cumulative at planet. All allocator fast-path, no
  RSS impact. Skipped during v0.1 review (4 planet reviewers: not worth
  the API complexity). Revisit only if allocator profiling shows it
  matters after arena/columnar work.

- [ ] **Borrowed XML writer Vec elimination** — `write_borrowed_way_xml`
  and `write_borrowed_relation_xml` in `elements_xml.rs` still collect
  refs and members into `Vec`s. Could use `.peekable()` like tags to
  iterate directly. Low priority (~8 refs/way, ~10 members/relation).

- [x] **2. Columnar batch processing** — shipped for extract node
  classification. `DenseNodeColumns` decodes IDs/lats/lons into
  contiguous arrays. `collect_matching_ids_multi_bbox` does single-pass
  N-region bbox test. Used in multi-extract and single-extract.
  Measured: multi-extract Japan node classify 1081ms → 748ms (-31%).
  See [notes/columnar-integration.md](notes/columnar-integration.md).

- [x] **Smart-extract planet memory blocker — CLOSED 2026-04-11, ship
  as-is.** The 2026-04-10/11 investigation (4 reviewer rounds, 6
  commits) shipped a 29% wall improvement on Europe smart extract
  (254s → 181s) and also delivered complete −17% and simple −15% via
  the same `0b085b1` PASS1 schedule reuse. Planet measured on 2026-04-11
  at commit `cadc3e6`, UUID `2d028196`, plantasjen (32 GB, 27.9 GB
  avail), Europe bbox, `--bench 1` single sample: **279s wall / 11.17
  GB peak anon RSS.** The Europe×2.6 = 26-28 GB projection was wrong
  by ~2.4× because peak anon is dominated by PASS3 write work
  (bbox-sized), not PASS1 scanning the input file. Per the round-4
  decision tree, < 25 GB = ship as-is. The reusable packet pool,
  compact payload, malloc_trim-at-boundary, and bumpalo arena options
  from the round-4 mitigation menu are all **not needed** for this
  workload and have been closed out.

  Caveat: measured with Europe bbox. A substantially larger bbox
  (beyond continent scale) would grow PASS3's touched working set
  and could push peak anon higher. If extract-on-planet ever becomes
  a recurring operation for bboxes > Europe, re-measure. Whole-planet
  bbox isn't a real workload — use `cat` passthrough.

  See [notes/parallel-classify-regression.md](notes/parallel-classify-regression.md)
  for the full investigation history, mechanism analysis (cold-arena-page
  residency cascade), and the historical mitigation menu preserved
  as reviewer-context rather than outstanding work.

**Milestone B: vectorization (after columnar layout stabilizes)**

- [ ] **3. SIMD** — universal agreement: comes after columnar. Columnar
  now shipped for extract (single + multi-region). ASM inspection
  confirms LLVM does NOT autovectorize the bbox classify loop — the
  `push()` side effect prevents vectorization entirely.

  **Codegen finding:** explicit AVX2 intrinsics are the only path.
  The multi-bbox loop is a better SIMD target than single-bbox: N
  region tests per node amortizes setup (N=5 with AVX2 8-wide ≈ 1.6
  nodes of all 5 tests per vector op). Single-bbox is only 2.8% of
  total Europe extract time — not worth it alone.

  SIMD becomes worthwhile when:
  - The classify loop is a larger fraction of runtime (after write-path
    optimization makes classify the bottleneck)
  - Multiple consumers use columnar arrays (multi-region, polygon PIP)
  - Batch varint decode in protohoggr (different SIMD target, broader
    impact across all commands)

  Varint SIMD research (notes/SIMD.md) previously closed — scalar beats
  SIMD for individual LEB128 varints. Batch varint decode into contiguous
  arrays is a different problem (columnar enables this).

**Milestone C: hardware-level tuning (where perf counters justify it)**

- [ ] **4. Huge pages** — `MAP_HUGETLB` (2 MB pages) for large mmap'd
  structures. Dense ALTW index (128 GB virtual, ~16 GB touched): 4 KB
  pages cover 8 MB via TLB, 2 MB pages cover 4 GB. Geocode index mmap
  reader, external join temp files. 5-15% speedup for random-access
  patterns. Note: dense ALTW is deprecated at planet scale in favor of
  external join. Requires hugepage availability (`sysctl` config) or
  `madvise(MADV_HUGEPAGE)` for THP. Linux-only.

- [ ] **5. NUMA-aware memory placement** — last by unanimous agreement
  (6/6). Only matters on multi-socket servers. Current benchmark host
  (plantasjen) is single-socket. Pread-from-workers pattern already has
  natural NUMA affinity (thread-local allocations, first-touch policy).
  `set_mempolicy(MPOL_BIND)` / `mbind()` for explicit placement.
  Candidates: pipelined reader decode pool, dense ALTW index interleave,
  external join scatter buffers. 10-20% on dual-socket, 0% on
  single-socket. Requires per-host tuning and NUMA hardware to validate.

**Separate track (GPU, independent of milestones A-C):**

- [ ] **GPU-accelerated point-in-polygon for geocode builder** — Pass 2
  tests billions of nodes against admin boundary polygons. NVIDIA's
  cuSpatial has production-quality PIP (winding number, handles holes).
  Depends on columnar batch processing for efficient host-to-device
  transfer. Rust interop via `cudarc`. Feature-gate behind `cuda`.
  Planet: 2.5B nodes, polygon set ~100 MB. Only worthwhile at
  Europe/planet scale. No precedent in OSM tooling.

### Research / stretch ideas

- [ ] Incremental geocode index update (daily diff → index patch, no full rebuild).
  See [notes/incremental-geocode-index.md](notes/incremental-geocode-index.md)
  for 4 approaches analyzed. Recommended: v1 append-only delta index with
  query-time merge (simplest, no format changes), v2 S2 cell-level partial
  rebuild (better query perf, proportional to diff size).
- [ ] Incremental extract update (`extract --apply-changes` — base extract + OSC +
  region → updated extract without re-reading planet).
  Recommended: compose two existing commands — `apply-changes` on
  the region extract (with `--allow-missing` for new elements not in
  the base), then `extract` to re-filter to the bbox. ~10s vs 862s
  for the full-planet pipeline. Works for simple strategy immediately.
  Complete/smart strategies need planet access for newly referenced
  elements outside the bbox.
- [ ] Spatial indexing in PBF format (R-tree over blob offsets for
  O(log N) spatial queries on planet files).
  See [notes/spatial-index-in-pbf.md](notes/spatial-index-in-pbf.md)
  and [notes/way-blob-bbox-speculation.md](notes/way-blob-bbox-speculation.md).
  Node blob header scan is already fast (~0.5s planet). Way blob spatial
  bboxes are limited by chronological ID ordering (~30% skip for Denmark,
  not 50-80%). Geography-sorted way blobs (Hilbert curve) would give
  90%+ skip but breaks Sort.Type_then_ID. Multi-extract benefits most.
- [x] Streaming pipeline composition — CLOSED, limited benefit.
  The codebase already does the most valuable composition (inline
  indexdata in all write paths). Multi-pass commands can't consume
  streams. See [notes/streaming-pipeline-composition.md](notes/streaming-pipeline-composition.md).
- [ ] Zstd as default compression for internal pipelines — extremely
  low priority. Investigated multiple times, production pipeline works.
- [ ] Dense ALTW compact rank-indexed array (same pattern as geocode builder —
  better locality on hosts where dense currently works, reviewers split 1/8).
- [ ] Verify GeoJSON polygon format coverage for extract (does `--polygon`
  accept GeoJSON, or only .poly format?).
- [ ] History-file support — decide in-scope or explicitly out-of-scope.

## Release prep

### Testing

See `reference/performance.md` for consolidated baselines.

- [ ] **Diff element_stream fallback path untested** — all test PBFs are
  indexed because `PbfWriter::write_primitive_block` unconditionally adds
  indexdata. The `diff_element_stream` fallback (non-indexed inputs) has
  no direct coverage. Needs a `write_test_pbf_non_indexed` helper that
  either strips indexdata post-write or uses `write_blob` directly.

- [ ] **Test fixture infrastructure** — current `write_test_pbf` /
  `write_test_pbf_sorted` helpers create minimal PBFs (1-3 elements per
  type, single block). Needed: (1) a sorted+indexed fixture generator
  for commands that require indexdata (merge, extract, diff, ALTW),
  (2) larger multi-block fixtures (~100 elements, 3-5 blocks) to exercise
  batch boundaries, blob classification, and passthrough coalescing,
  (3) a fixture with metadata (version, changeset, timestamp, uid, user)
  for CleanAttrs / time_filter / diff verbose testing.

- [ ] **Fuzz testing** — PBF parsing (`PrimitiveBlock::from_vec`), OSC
  parsing (`parse_osc_file`), and wire-format decoders (`Cursor`,
  `WireBlock`, `WireInfo`) accept untrusted input. `cargo-fuzz` targets
  for these entry points would catch panics, OOM, and logic errors on
  malformed data. Also fuzz the roundtrip path (write → read → compare).

### Cross-pipeline optimization audit (commit `398b1a4`)

Findings from code audit + outside review of transferring geocode builder
optimizations (block-pipelined + skip_metadata, tag-first classification,
FxHash, pass fusion, clone/alloc cleanup) to other commands.

**getid** (moderate impact, low risk):
- [x] Replace `dep_node_ids: BTreeSet<i64>` with `IdSetDense` in `getid_with_refs`.
  O(log n) → O(1) per node lookup. Also removed dead `strip_tags_ids` parameter.
  Commit `a704f5c`.
- [x] Use `elements_skip_metadata()` in `getid_with_refs` pass 1 and
  `parse_ids_from_pbf`. Commits `a704f5c`, `58e38d8`.
- [ ] Audit pass fusion for `--add-referenced` / `--invert` flows — checked:
  cannot fuse (pass 2 needs complete dep_node_ids before deciding which nodes
  to emit). Two-pass structure is inherent to the data dependency.

**extract --smart** (verified — already optimized):
- [ ] Check for opportunities to reduce repeated full-file traversals in relation
  closure expansion. (Inherent to transitive closure — may not be reducible.)

**add-locations-to-ways** (verified — already optimized):
- [ ] Tag-first rejection in rewrite phase: ALTW processes all ways unconditionally
  (no tag-based filtering). Not applicable — every way gets location enrichment.
- [ ] Clone/allocation in batch processing: passthrough coalescing uses raw bytes,
  no cloning. Batch slot dispatch is enum-based. Already well optimized.

**check_refs** (verified — no action):
- Consumer-bound (RoaringTreemap insertions, decode workers idle at 1% CPU).
  Block-pipelined + skip_metadata would not reduce wall time.
- [ ] Re-evaluate if consumer bottleneck shifts after RoaringTreemap improvements.

**sort, cat** (no action):
- Already optimal — blob-level passthrough, single-pass, or need full metadata for output.
