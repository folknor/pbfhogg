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

- [ ] **`add-locations-to-ways --index-type external`.**
  Planet 953 s, Europe 400 s (commit `3d977a0`, 2026-04-14). 99 GB
  `coord_slots` mmap retired in favour of 55 GB blob-ordered
  delta-varint `coord_payloads`; stage 4 majflt 555K → 3.2K.
  See `notes/altw-optimization-history.md` for the full history
  (prototype, integration measurement, Stage 6 cleanup) and
  `notes/altw-optimization-history.md` "Stage 4 bottleneck isolated"
  for the NVMe-floor analysis that closed the structural-rearrangement
  family of optimizations.

  **Bugs (must fix soon):**

  - [x] ~~Small-input external mode is structurally rejected.~~ Fixed
    in two commits. First commit (`cdb97e7`) introduced
    `slot_bucket_count = min(NUM_BUCKETS, max(1, total_slots / max_blob_slots))`
    and plumbed it through stages 2/3, but kept `range_size =
    total_slots.div_ceil(slot_bucket_count)` — which makes the LAST
    bucket smaller, not larger, so the smallest bucket could still be
    narrower than `max_blob_slots`. Reviewer counterexample
    (`total_slots=10, max_blob_slots=3` → buckets [0,4)[4,8)[8,10),
    last is 2 wide) caught it. Followup: switched `range_size` to
    floor division (in both `ResolvedEntry::slot_bucket` and
    `stage3_slot_reorder`), making the LAST bucket *absorb the
    remainder* (and be wider, not narrower). Smallest bucket = floor
    `range_size` = `total_slots / slot_bucket_count` ≥ `max_blob_slots`
    by construction. Out-of-range high `slot_pos` values get clamped
    to the last-bucket index. (External review 2026-04-14 #2 + #3
    followup.)
  - [x] ~~External `Stats.missing_locations` always reports 0.~~ Fixed
    in three commits (`25031a1` + manifest-persistence followup).
    Computed in `external_join` after stage 4 as
    `total_slots − resolved_count`, where `resolved_count` comes from
    stage 2's `is_resolved = lat != 0 || lon != 0` aggregation. When
    stage 2 was skipped via `--start-stage >= 3`, `resolved_count` is
    recovered from the manifest (extended to 16 bytes:
    `[u64 total_slots][u64 resolved_count]`) — so resume runs populate
    the field consistently with fresh runs.

    **Caveat (acknowledged tradeoff, not parity with dense):** this
    counts a real OSM node at exact (0,0) decimicrodegrees as missing,
    while dense doesn't. The closed "Null Island" item accepted that
    tradeoff because (0°, 0°) is in the Atlantic and no real OSM node
    has those exact coords; the count is correct in practice but the
    semantics differ in principle. Remove this caveat only if a
    user-visible (0,0)-sentinel collision is observed. (External review
    2026-04-14 #2 + #3 followup.)

  **Bugs (lower priority, dev-time only):**

  - [ ] **`--start-stage` resume is fragile.** Manifest stores only
    `total_slots` (`mod.rs:157`); resume rescans input and silently
    swallows read/decompress errors (`mod.rs:184`); worker count
    inferred from scratch filenames (`mod.rs:217`). Stale or
    mismatched scratch degrades into partial metadata instead of
    erroring. Fix: persist `unique_nodes`, `num_shard_workers`,
    `rank_bucket_counts`, and an input PBF fingerprint in the
    manifest; hard-error on mismatch; drop the rescan. Affects
    `--start-stage` only (dev/profiling). (External review
    2026-04-14 #2.)
  - [ ] **No integration tests for `IndexType::External`,
    `--keep-scratch`, or `--start-stage`.** This gap hid the
    small-input bug above. Add a CLI test that runs external on a
    small fixture + a `--keep-scratch` + `--start-stage` round-trip.
    (External review 2026-04-14 #2.)

  **Still open — small / quick:**

  - [ ] **Stage 4 `coord_payload` trailing-bytes assert.** One-line
    defensive check after the de-interleave loop:
    `payload_pos == coord_payload.len()`. Catches stage 3 bugs,
    truncated payloads, and version skew at the boundary. (External
    review 2026-04-14 #1.)
  - [ ] **Drop `IdSetDense` after stage 1.** `stage1_way_pass` returns
    the full set; downstream stages never read it. Holding it costs
    ~2+ GB RSS through stage 2/3/4 unnecessarily. Resume path rebuilds
    it for nothing — only `unique_nodes` is needed and that can be
    persisted in the manifest (see `--start-stage` fix above).
    (External review 2026-04-14 #2.)
  - [ ] **Sparse stage 2 slot buffers.** `stage2.rs:285` allocates
    `Vec<Vec<u8>>` of 256 entries × 6 workers eagerly. Worst-case
    capacity at 256 KB threshold = 384 MB. Use a sparse map (or
    `Vec<Option<Vec<u8>>>` with lazy init) so unused buckets cost
    nothing. (External review 2026-04-14 #1.)
  - [ ] **`coord_slice` resize moved out of per-bucket loop**
    (`stage2.rs:317`) — pre-size once per worker; trivial.
    (External review 2026-04-14 #1.)
  - [ ] **Per-way refcounts threaded into stage 4** so
    `reframe_way_blob_with_locations` can stop re-counting refs from
    field 8 varints. Modest CPU win, not transformative. (External
    review 2026-04-14 #2 hypothetical.)
  - [ ] **Stale "coord_slots" comment sweep** across `altw/*` after
    the rename. Cosmetic. (External review 2026-04-14 #1.)

  **Still open — measurable wins inside the current architecture:**

  - [ ] **Stage 4 raw passthrough for non-way blobs.** Currently every
    non-way blob goes through full `PrimitiveBlock` decode +
    `BlockBuilder` re-encode (`stage4.rs:337`). Dense path already has
    raw passthrough machinery for relation blobs (always) and untagged
    node blobs (when `keep_untagged_nodes=true`). Planet
    `s4_nonway_assemble_ms` = 922 s cumulative (~154 s wall ÷ 6
    workers); even partial passthrough is meaningful. Plumb the same
    machinery into the external stage 4. (External review
    2026-04-14 #2.)
  - [ ] **`PerWayRcs` lazy per-blob decode via blob-offset index.**
    Currently parses entire varint sidecar into a flat `Vec<u32>`
    held resident through stage 3 + finalize (~4.7 GB on planet). Add
    a per-blob byte-offset index over the varint stream and decode
    each blob's refcounts on demand. Saves ~3.5 GB RSS. (External
    review 2026-04-14 #2.)
  - [ ] **io_uring (SQPOLL) for stage 2 + stage 4 preads.** Stage 4
    now does ~17K (planet) / ~57K (Europe) per-blob preads on
    `coord_payloads`; stage 2 does its rank-bucket coord-slice
    preads. SQPOLL kernel-thread submission would zero-syscall the
    hot paths. Higher leverage post-coord_payloads than pre-.
  - [ ] **More than 256 buckets.** Smaller scatter_buf per bucket
    (better L2 fit during scatter+encode), at the cost of more
    straddler boundaries (~255 today × 2 at 512 buckets — still well
    under 1 GB staging). Tractable bench. Note: also affects the
    small-input bug fix (dynamic bucket count is one fix candidate).
  - [ ] **Fuse the intermediate vectors in stage 1's hot loops.**
    Pass A copies refs into `blob_node_ids` before setting bits
    (`stage1.rs:151`); pass B builds `ranked` then immediately
    serializes it (`stage1.rs:362`); coord pass builds
    `ranked_coords` before turning into extents (`stage1.rs:613`).
    Each is a per-blob `Vec` allocation feeding a single downstream
    consumer — fuse to remove memory traffic. (External review
    2026-04-14 #2.)
  - [ ] **Stage 3 parse-and-scatter directly from raw bytes.**
    Currently parses slot-bucket records into `Vec<ResolvedEntry>`
    before scattering into `scatter_buf` (`stage3.rs:223`). Same
    family as the stage-1 vector-fusion items above. Micro: ~1–3 s
    wall on planet. (Already in TODO; reaffirmed by external review
    2026-04-14 #2.)
  - [ ] **Stage 1B grouped-by-local-rank emission (segmented sort
    + k-way merge in stage 2).** Desk estimate ~9 s wall on Europe by
    skipping `s2_prepare_scatter_ms`. Track record on stage-1B desk
    estimates is poor (the batching one regressed 30% vs predicted
    −6 s) — only commit after a Denmark-scale prototype.

  **Still open — large structural redesigns (paired):**

  - [ ] **Eliminate `coords_by_rank` by merging stage 1's coord pass
    into stage 2.** Today: stage 1's coord_pass writes 82 GB
    `coords_by_rank` (planet) overlapped with stage 1B; stage 2
    preads the whole file back as rank-sorted slices. Hypothetical:
    stage 2 workers each read the node-blob range covering their rank
    bucket and resolve inline. Saves 82 GB write + 82 GB read
    (−164 GB scratch I/O, planet), adds ~61 GB compressed node-blob
    read. Net I/O −103 GB. Concurrency complication: rank ranges map
    to contiguous-but-not-trivially-aligned node-blob ranges.
    Effort: large. (External review 2026-04-14 #1.)

  - [ ] **Eliminate slot-bucket files by partitioning stage 2 output
    per-blob in memory.** Today: stage 2 writes ~50 GB slot-bucket
    files (planet, `s2_slot_bytes_written`); stage 3 reads them back
    to scatter into per-bucket buffers, which then feed coord_payloads
    encoding. Hypothetical: stage 2 streams resolved entries into
    per-blob staging in memory, `way_slot_starts`-classified;
    coord_payloads emission happens inline. Slot-bucket files
    disappear. Memory cost: streaming straddler-style buffering ≤
    bucket-sized (~388 MB worst case per blob spanning current bucket
    sizes). Architecturally adjacent to coord_payloads but at the
    other end of the pipeline. Effort: very large. (External review
    2026-04-14 #1.)

    Note: distinct from the closed sort-then-coalesced-pwrite fuse
    (which kept `coord_slots` and tried to write into it from many
    rank buckets — rejected on coalescing-ratio grounds). This
    proposal eliminates the slot-bucket *intermediate*, not the
    final output, so the NVMe-floor argument doesn't transfer.

  - [ ] **Parallelize the finalize tail.** Currently sequential:
    walk blobs in order, pread each worker temp's bytes, append to
    `coord_payloads`, encode straddlers (`coord_payloads.rs:155`).
    26.5 s on Europe, ~68 s on planet — the next-largest measured
    wall bite after stage 4. Two-pass: stage 3 workers track
    per-blob byte sizes alongside their temp writes; coordinator
    computes cumulative offsets; workers then `pwrite` directly into
    `coord_payloads` at known offsets in parallel. Eliminates the
    serial read+write tail. Smaller scope than the per-blob in-memory
    redesign above. (External review 2026-04-14 #2.)

  - [ ] **Per-worker local `IdSetDense` + merge in pass A.** Today
    pass A workers all hit a single shared `IdSetDense` via
    `set_atomic` (`stage1.rs:154`). Eager full-space preallocation
    plus N-way `fetch_or` contention. Per-worker local sets +
    bitwise-OR merge at end would remove the atomic contention and
    let preallocation be sized to the actual touched range per
    worker. Reviewer flags this as the only stage-1 architectural
    change worth measuring; do not trust desk estimates without a
    bench. Effort: medium. (External review 2026-04-14 #2 hypothetical.)

  **Still open — dev convenience:**

  - [ ] ~~Serialize `IdSetDense` for `--start-stage` resume.~~
    Superseded by the `--start-stage` robustness fix in Bugs above:
    persist `unique_nodes` (and other metadata) in the manifest,
    drop the rescan, and drop `IdSetDense` after stage 1 (no
    downstream reader). Reviewer #1 wanted to keep IdSetDense around
    for resumes; reviewer #2 noted nothing reads it downstream, so
    the right answer is to drop it and persist the scalar instead.

  **Conditional on output compression** (currently `compression:none`
  in production, so both moot):

  - [ ] **Output Zstd(1).** ~3× faster than zlib(6), similar ratio.
  - [ ] **Reduce stage 4 decode workers by 1, give core to compression.**

  **External review rediscoveries (already considered or measured-and-closed):**

  - [x] ~~Stage 2 mutex contention on shared slot writers.~~ Measured:
    `s2_slot_flush_lock_wait_ms` = 8,765 ms cumulative on planet
    (1.5 s wall ÷ 6 workers, 0.7% of stage 2). Below the >5%
    threshold the reviewer named as actionable. (Review #1.)
  - [x] ~~`ResolvedEntry` varint-packing.~~ Already noted as moot in
    history — only worth doing under a fused-2+3 architecture, which
    is the structural redesign above. Reviewer #1 agrees.
  - [x] ~~Straddler `Mutex<Option<...>>` → `AtomicPtr`.~~ Reviewer #1
    themselves notes "negligible gain, contention is rare." Skip.

  **History.** All measurement details, rejected alternatives, and the
  full integration narrative live in
  [`notes/altw-optimization-history.md`](notes/altw-optimization-history.md).
  The architectural family of "rearrange bytes inside the rank-bucketing
  pipeline" optimizations (postings-by-rank CSR, blob-local rank
  batching in stage 4, sort-then-coalesced-pwrite, pwritev) is closed
  by the NVMe-floor analysis and won't be reopened without a different
  hardware envelope or a different intermediate representation.

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
    - **jemalloc 5.3.1 (released 2026-04)** — wait for `tikv-jemallocator`
      to tag a release pointing at 5.3.1, then rerun the bench.
      Specifically relevant to the pipelined reader's cross-thread free
      pattern (`src/read/pipeline.rs:70` — decode workers allocate
      `PrimitiveBlock`s dropped on the consumer thread, the exact reason
      the prior jemalloc bench only saved RSS and not wall time):
        - tcache for deallocation-only threads (most on-point)
        - locality-aware tcache GC (`experimental_tcache_gc`, default on)
        - `calloc_madvise_threshold`, `process_madvise_max_batch`,
          `tcache_ncached_max` for ~MB-sized block allocations
      Check tikv-jemallocator releases; when 5.3.1 lands, run planet read
      + ALTW external + merge.

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
