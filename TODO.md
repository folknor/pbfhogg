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

## Performance

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  alternatives (`paralight`, `orx-parallel`, `chili`, `forte`, `spindle`).
  Revisit only if rayon becomes a proven bottleneck.

## Cross-pipeline optimization

PrimitiveBlock cross-thread alloc/free retention affects every command using
the pipelined reader at 400K+ blocks (Europe/planet scale). The geocode builder
is the predicted next victim (16 GB DenseMmapIndex + 25 GB retention = OOM).

See [notes/altw-optimization-history.md](notes/altw-optimization-history.md)
for the complete plan: 20 items across 5 priority groups, covering infrastructure
fixes, planet blockers, external join P2b/P2c, and all affected commands.
See [notes/pipelined-reader-retention.md](notes/pipelined-reader-retention.md)
for the April 2026 audit: 6 remaining paths, renumber and cat --type are
the production-relevant ones still using `into_blocks_pipelined`.

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
  `into_blocks_pipelined` processing all blobs. 25+ GB retention at planet.
  Only triggers with `--force` on non-indexed PBFs. Niche but the last
  unmitigated retention path.

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
- [ ] **`renumber` external path — optimization roadmap (2026-04-11).** Six
  commits landed the external renumber implementation (pass 1 + stages
  2a-2d + relation R1/R2). First planet measurement on 2026-04-11
  (commit `e156e97`, UUID `c5d00c22`): **3,456 s (57.6 min)**, peak
  anon 2.79 GB, all element counts correct. That's 2.6× the design
  estimate of ~1,300 s. Memory is well under the 4 GB target; wall time
  is the outstanding issue.

  Reviewer brief sent to planet+perf+arch after the first measurement
  revised the optimization plan. Two new levers nobody had in the
  original 3-item list, plus honest revisions of the parallelization
  and radix-sort savings.

  **Latest measurement: 209 s (3m29s)** on commit `67c7960`
  (2026-04-12). **−94% vs the 3,456 s baseline.** IdSetDense rank
  fusion (eliminates all CooPair/bucket infrastructure), wire-format
  splice rewriters for all three element types (nodes, ways, relations),
  fused stage 2d (way resolve + splice in one pass), relation_map
  replaced by IdSetDense, zero temp disk, single shared input fd,
  atomic dispatch, zlib:1 output. Peak anon 7.0 GB.

  **Sub-6 minute plan (round 6+7, 2026-04-12):**

  Current: 401 s (6m42s). Target: 360 s (6 min). Need 41 s.

  - [ ] **Parallelize R1+R2A** (29 → ~8 s, save ~21 s). Pre-scan
    relation blob headers for per-blob element counts, compute prefix
    sums for starting relation IDs. TRIED in previous session and
    regressed (30 → 37 s) — 14K relation blobs is too few. Don't retry
    without a fundamentally different approach.

  **Round 7 reviewer findings (2026-04-12, all 5 archetypes):**

  Comprehensive sweep after reaching 401 s. Every actionable item from
  bugs-claude, bugs-codex, perf-codex, arch-claude, arch-codex,
  correctness-claude, correctness-codex, planet-codex.

  *Tier 2 — medium confidence, 5-20 s:*

  - [x] **Output compression: zlib:1 default** — `dd3f477`. Default output is
    now zlib:1 (fast compression), respects explicit `--compression` override.
    Zlib level 1 is ~3× faster than level 6 with ~10% worse ratio.
    (arch-claude, perf-codex)
  - [x] **Output compression: lower zlib level** — `dd3f477`. Done via zlib:1 default. (arch-claude, correctness-claude)
  - [ ] **Output compression: `--compression none` for benchmarking.**
    Eliminates compression entirely. Output is ~3× larger on disk.
    Useful as a zero-code-change diagnostic to confirm whether compression
    is the dominant ceiling. (arch-claude, arch-codex)
  - [ ] **Re-evaluate pass 1 worker count (4 → 5 or 6).** Previous
    session tried 6 and regressed (pread contention on 1.3M node blobs).
    But the current tree is much leaner. Measure again on current code.
    Arch reviewer warns consumer/compression may be the ceiling — adding
    workers only helps if compression keeps up. (correctness-claude,
    planet-codex)
  - [ ] **Re-evaluate stage 2d worker count (6 → 8).** decompress=152s,
    reframe=389s CPU. Reframe dominates. 8 workers: reframe wall 389/8 ≈
    49 s. But consumer write must keep up. Check `consumer_write_ms`.
    (bugs-claude, arch-codex)
  - [ ] **Wire-format scanner for R1** (est. −3 to −5 s). R1 currently
    does full PrimitiveBlock decode to iterate relation IDs. A lightweight
    scanner analogous to `scan_way_refs` would skip string table parsing
    and UTF-8 validation. (bugs-claude, perf-codex, arch-codex)
  - [ ] **Contiguous-range rank() fast path.** Cache last chunk_id and
    block_id; skip prefix recomputation when consecutive refs land in
    the same block. Marginal for ways (refs span full ID space), more
    impactful for relation members with nearby IDs. (bugs-claude)

  *Tier 3 — smaller wins, 2-5 s:*

  - [ ] **Varint encode lookup table.** 256-entry table for single-byte
    varints (values 0-127) in the reframe functions. Eliminates the loop
    for ~80% of varint encodes. Est. −2 to −3 s wall. (arch-claude)
  - [ ] **Longest-job-first scheduling for pass 1.** Pre-sort the schedule
    by decompressed size (descending) so the tail isn't dominated by one
    slow large blob. Improvement depends on blob size variance. Est. 0-10 s.
    (arch-claude)
  - [ ] **Shared `IdSetDense` with atomic byte operations in pass 1.**
    Instead of 4 independent bitsets merged after pass 1, have all workers
    write into a single shared bitset using `AtomicU8::fetch_or`. Eliminates
    the 3-way merge (~1-2 s) and reduces peak memory from 6 GB to 1.5 GB
    during pass 1. Different workers hit different cache lines most of the
    time (sorted PBF distributes node IDs). (correctness-claude)
  - [ ] **`posix_fadvise(SEQUENTIAL)` / `WILLNEED` on the main input fd**
    for the long pread phases. (arch-codex)
  - [x] **Stream relation member flat files** — superseded: flat files
    eliminated by IdSetDense rank fusion. R2d resolves inline via
    `resolve()`. (correctness-codex)
  - [ ] **Skip `way_id_set` IdSetDense if way rank derivable from schedule.**
    If input is strictly sorted and every way is renumbered, new way ID is
    `start_way_id + global_way_rank`. Build a cheaper index from
    schedule/blob boundaries instead of a full bitset+rank. (bugs-codex)

  *Tier 4 — speculative / high effort:*

  - [ ] **SIMD / table-driven varint decode in wire rewriters.** The
    reframe functions are varint-heavy. Group Varint Encoding (GVE) from
    Google's Procella paper shows 2-4× throughput but uses fixed-width
    layout incompatible with protobuf continuation bits. Requires
    `protohoggr` changes. (bugs-claude, bugs-codex, correctness-codex)
  - [ ] **io_uring for pread.** Workers use synchronous `read_exact_at`.
    io_uring batched pread could halve per-blob overhead. Est. −6 s pass 1,
    −3 s stage 2d. (bugs-claude, bugs-codex)
  - [ ] **Memory-map the input PBF instead of pread.** Eliminates pread
    syscall overhead, lets kernel readahead prefetch. But 87 GB mmap on
    32 GB host means heavy page faults. Concurrent worker access defeats
    sequential readahead. Net effect uncertain. (bugs-claude, arch-claude)
  - [x] **LZ4-compress the temp flat files** — superseded: zero temp disk.
    All flat files eliminated by IdSetDense rank fusion. (bugs-claude)
  - [ ] **Pipelined writer per phase + concatenate.** Run each phase with
    its own PbfWriter and concatenate output files. Would overlap pass 1
    compression with stage 2d decompress. Complex, uncertain benefit.
    (bugs-claude)
  - [ ] **Two-level parallelism: per-blob workers + per-element SIMD.**
    SIMD varint decode in protohoggr. Research-level. (bugs-claude)
  - [ ] **NUMA / affinity tuning.** Pin workers + scratch buffers to cores
    on dual-socket or big-core hosts. (bugs-codex, planet-codex)
  - [ ] **Dedicated allocator (jemalloc / mimalloc)** for this command.
    Already had to call `mallopt`; a better allocator may help with
    bounded cross-thread `Vec<u8>` traffic. (correctness-codex)
  - [ ] **Huge pages for `IdSetDense` chunks / rank index.** TLB behavior
    on dense rank bitsets. Low confidence. (planet-codex)
  - [ ] **`preadv2` / io_uring multishot read batching.** Read-side
    batching for pass1 and stage2d where many workers do smallish
    independent preads. (bugs-codex)

  *Instrumentation (non-blocking, informs further optimization):*

  - [x] **Sub-phase counters for `fused_way_resolve`** — superseded:
    `fused_way_resolve` fused into stage 2d. Stage 2d already has
    pread/decompress/reframe/send/consumer_write counters. (bugs-codex)
  - [ ] **Consumer drain-rate instrumentation.** Measure time blocking on
    `rx.recv()` vs time in `write_primitive_block_owned`. Distinguishes
    worker-bound vs consumer-bound. (planet-claude, bugs-codex)
  - [ ] **Finer stage 2d reframe instrumentation.** Split `reframe_ms`
    into `way_parse_ms`, `ref_lookup_ms`, `ref_encode_ms`, `frame_ms`.
    (perf-codex)
  - [ ] **Benchmark `--compression none` to isolate compression ceiling.**
    (arch-claude, arch-codex)

  **Next-round optimization levers (round 2 reviewer consensus, 2026-04-12):**

  - [ ] **`direct_io` flag honored in pread stages.** The shared input fd
    in `renumber_external` is opened with plain `File::open`. Should use
    O_DIRECT when the flag is set, for cache discipline on planet-scale
    hosts. (arch-codex)

  **Smaller / defensive followups (non-blocking for planet bench):**

  - [ ] **Add `scan_relation_members` fast-path** for R1/R2d, analogous
    to `scan_way_refs`. R1 currently does full PrimitiveBlock decode
    just to iterate relation IDs. A wire-format scanner would skip
    string table parsing and UTF-8 validation. Moderate win.
  - [x] **`MADV_DONTNEED` on mmap'd `new_refs` files** — superseded:
    `new_refs` files eliminated by IdSetDense rank fusion. No mmaps
    remain in renumber_external.
  - [x] **Clean up stale comments** — updated module doc, pass 1 / stage 2d /
    R1 / R2d doc comments, section headers, removed dead `bucket_emit_ms`
    from `StageCounters`, renamed `R1_R2A` markers to `R1`. (perf-codex)

  **Round 4 reviewer findings (2026-04-12, perf-codex + planet-claude):**

  (Consumer drain-rate instrumentation deduplicated — see above.)

  **Defensive asserts / hardening:**

  - [ ] **`relation_map.len()` upper-bound warning.** At planet we see
    ~14M relations; design doc targets `<4 GB` peak RSS. If OSM grows
    past ~50M relations, log a warning at R1 completion.
  - [x] **Scratch dir concurrent-from-same-process collision risk** —
    superseded: `renumber_external` no longer uses `ScratchDir` or temp
    files. All resolved inline via IdSetDense rank. Still applies to
    `external_join.rs` (ALTW external), but not a renumber concern.

  **Ergonomics / architecture:**

  - [x] **`BucketWriters::write_pair` helper** — superseded: renumber no
    longer uses BucketWriters. Only `external_join.rs` (ALTW) remains
    as a user. One caller doesn't justify the abstraction.
  - [x] **`RenumberStats.orphan_refs: u64` counter.** Both in-memory and
    external paths now count orphan refs (way refs and relation members
    whose old_id isn't in the corresponding ID set). `print_summary()`
    emits a warning when non-zero. Tracked via `AtomicU64` in parallel
    stages, `IdSetDense::get()` membership check.
  - [x] **Document orphan-ref policy** in `renumber_external.rs` module
    docs. Orphan refs pass through with their old ID, matching in-memory
    behavior and osmium's semantics.

  **Test gaps:**

  - [ ] **Non-indexed input test.** All current test PBFs are indexed
    (written via `write_test_pbf_sorted` which emits indexdata). Add a
    test that strips indexdata from the input so stage 2a / stage 2d /
    R2a / R2d hit the full-decode fallback path.
  - [ ] **Non-dense `Element::Node` element path.** Current test helpers
    always use DenseNode via `BlockBuilder::add_node`. Pass 1's
    `Element::Node(n)` branch (non-dense) is only reachable via
    externally-produced PBFs. Either construct such an input or
    document that the branch is dead-ish outside real-world inputs.

- [x] **`renumber` planet-scale refactor — COMPLETE.** External path
  Shipped with IdSetDense rank-based lookup for all
  three element types. Planet: 209 s (3m29s), 7.0 GB peak anon, zero
  temp disk.

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
  See [notes/incremental-extract.md](notes/incremental-extract.md)
  for 4 approaches. Recommended: apply-changes on region extract +
  re-extract to filter (approach 3). ~10s vs 862s. Needs
  `--allow-missing` flag for apply-changes.
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

See `notes/test-plan.md` for the full pre-release test matrix (feature permutations,
I/O modes, CLI commands) and `reference/performance.md` for consolidated baselines.

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
