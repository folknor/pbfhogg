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
  3. ~~**`Mixed | Empty` handler is a full sequential fallback.**~~
     Split Empty from Mixed (commit `ff29c1f`). Empty blocks now skip without
     flushing batches. Mixed blocks retain sequential fallback — rare in
     production PBFs, not worth parallelizing (perf reviewer consensus).
  4. ~~**Vec-per-block allocation in batch helpers.**~~ Closed. Alloc profiling
     showed 1.7 GB cumulative churn (52% of extract budget), but RSS stayed
     under 200 MB — the allocator recycles pages efficiently. Three fix
     attempts failed: IdSetDense fold+merge (15x wall time regression,
     merge is O(id_space)), Vec fold (rayon splits at same granularity,
     no reduction), map_init (can't return ownership without losing capacity).
     The original per-block Vec pattern is the correct tradeoff.
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

## ~~BlobReader fadvise: gate on `target_os = "linux"` instead of `linux-direct-io`~~ DONE

Done in commit `7acbb1a`. libc now non-optional, fadvise gated on `target_os = "linux"`.

## Cross-pipeline optimization

PrimitiveBlock cross-thread alloc/free retention affects every command using
the pipelined reader at 400K+ blocks (Europe/planet scale). The geocode builder
is the predicted next victim (16 GB DenseMmapIndex + 25 GB retention = OOM).

See [notes/altw-optimization-history.md](notes/altw-optimization-history.md)
for the complete plan: 20 items across 5 priority groups, covering infrastructure
fixes, planet blockers, external join P2b/P2c, and all affected commands.

## ALTW external join — COMPLETE

Planet validated: **1,462s (24.4 min), 16.7 GB peak anon, 3.9x faster than dense.**
See [notes/altw-optimization-history.md](notes/altw-optimization-history.md).

## ALTW memory optimization — COMPLETE

External join ships as `--index-type external` (or `auto`).
Dense remains the "fast when RAM fits" path. See [notes/altw-optimization-history.md](notes/altw-optimization-history.md).

### Measured baselines (commit `69a127f`, plantasjen, 30 GB RAM + 8 GB swap)

| Dataset | Size | Elements | Time | Notes |
|---------|------|----------|------|-------|
| Europe | 33.6 GB | 4.2B (3.7B nodes, 454M ways, 8.2M rels) | 2565s (43m) | buffered, commit `69a127f` (no pass 0) |
| Europe | 33.6 GB | 4.2B | 2611s (43m) | `--direct-io` (+2%, no benefit), commit `69a127f` |
| Europe | 33.6 GB | 4.2B | 2631s (44m) | buffered, post `3677069` (with pass 0), +2.6% noise |
| Planet | 87.7 GB | 11.6B (10.4B nodes, 1.17B ways, 14.1M rels) | 5773s (96m) | buffered, memory-latency-bound, commit `69a127f` |

## Next (from 10-reviewer sweep)

### Planet validation runs (run when AFK — takes 25-60 min each)

- [ ] **Planet geocode build** — estimated 25 min, ~20 GB RSS. Validates compact
  rank-indexed array at planet scale. `brokkr build-geocode-index --dataset planet --bench 1`
- [ ] **Planet merge/apply-changes** — production daily path. Batched pattern
  should bound retention. `brokkr merge --dataset planet --bench 1`
- [ ] **Planet cat --type** — fixes landed (batch byte budget + bounded writer).
  `brokkr cat-way --dataset planet --bench 1`

### merge --locations-on-ways node scanner (6/10 reviewers)

Still builds full PrimitiveBlock to mine node coordinates from passthrough
node blobs (merge.rs ~line 1437). Node-coordinate scanner (`extract_node_tuples`)
already exists and fits directly. Localized, low-risk, production-relevant
(nidhogg steady state once locations_on_ways is enabled).

### Hybrid pipeline for extract (7/10 reviewers, separate project)

Only credible path to closing the 1.65x osmium gap on simple extract.
New `PipelineOutput` enum in pipeline.rs: `Decoded(PrimitiveBlock)` or
`Passthrough(RawBlobFrame)`. Pipeline's blob filter checks `BlobBbox::contains`
and skips decompression for fully-contained node blobs. Consumer writes
passthrough blobs via `write_raw_owned`, decoded blobs via existing batch path.

Benefits extract, cat --type, tags-filter — any command with spatial selectivity.
~200 lines in pipeline.rs + blob.rs. Two attempts without this regressed (lost
pipelined decode parallelism). See [notes/extract-passthrough-findings.md](notes/extract-passthrough-findings.md).

### Smaller items (from reviewer sweep)

- [ ] `node_stats.rs` — still on `for_each_pipelined`, pure node-coordinate
  analytics. Natural node-only scanner candidate. Low user impact.
- [ ] `getid::parse_ids_from_pbf` — full-block decode for ID-only scan.
  Could use a simpler scanner. Low priority.
- [ ] `tags_count.rs` — still pipelined. Analytics-only, low priority.
- [ ] ALTW dense pass 2 decode-all fallback (`write_output_decode_all`) — last
  pipelined path processing all ~1.4M blocks. Only triggers with `--force` on
  non-indexed PBFs. Niche but the last 25+ GB retention path.
- [ ] Write-path throughput (arch-Codex) — BlockBuilder/PbfWriter is the next
  bottleneck after read-path optimizations. SIMD varint, compression tuning.
  See [notes/SIMD.md](notes/SIMD.md).
- [x] Dead code cleanup (commit `428e73b`). Zero lib warnings.

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

**merge** (low impact, low risk):
- [x] Use `elements_skip_metadata()` in `block_overlaps_diff`. Commit `b90e8ef`.

**extract --smart** (verified — already optimized):
- [x] Audit: no std HashMap/HashSet in hot paths. Uses IdSetDense throughout.
- [x] Verify: all classification passes use `elements_skip_metadata()` (confirmed:
  lines 1242, 1305, 1382, 723, 742, 752, 763, 1022, 1054, 1086).
- [ ] Check for opportunities to reduce repeated full-file traversals in relation
  closure expansion. (Inherent to transitive closure — may not be reducible.)

**tags_filter** (verified — already optimized):
- [x] Verified: tag-first classification in place. Way refs collected only after tag
  match (line 580). `elements_skip_metadata()` in all collection passes.
- [x] Audit: std HashSet only in cold-path expression parsing (line 28-29, once at
  startup). Not worth changing.

**add-locations-to-ways** (verified — already optimized):
- [x] Audit: `elements_skip_metadata()` used in all scan passes (lines 411, 839,
  859, 882, 1072). Only the write path (line 1129) uses `elements()` (correct —
  needs full metadata for output).
- [x] Audit: FxHashMap already used in all hot paths (lines 1028, 1035, 1066).
  IdSetDense for ID sets.
- [ ] Tag-first rejection in rewrite phase: ALTW processes all ways unconditionally
  (no tag-based filtering). Not applicable — every way gets location enrichment.
- [ ] Clone/allocation in batch processing: passthrough coalescing uses raw bytes,
  no cloning. Batch slot dispatch is enum-based. Already well optimized.

**inspect** (verified — already optimized):
- [ ] `elements_skip_metadata()` in `--locations` without `--extended`: minor, deferred.
  Index-only fast path already skips decompression for the common case.
- [x] Audit: `inspect tags` uses FxHashMap for counting (tags_count.rs). No std hash
  in hot paths.

**check_refs** (verified — no action):
- Consumer-bound (RoaringTreemap insertions, decode workers idle at 1% CPU).
  Block-pipelined + skip_metadata would not reduce wall time.
- [x] Audit: uses RoaringTreemap for all ID sets (optimal). No std hash in hot paths.
- [ ] Re-evaluate if consumer bottleneck shifts after RoaringTreemap improvements.

**sort, cat** (no action):
- Already optimal — blob-level passthrough, single-pass, or need full metadata for output.

### Geocode index builder — DONE (planet validation pending)

Europe: 568s (9.5 min), 7.5 GB RSS. Planet estimated ~20 GB RSS.
Planet validation listed above under "Planet validation runs".

### README badges (after publishing)

- [ ] crates.io version badge — `https://img.shields.io/crates/v/pbfhogg`
- [ ] docs.rs badge — `https://img.shields.io/docsrs/pbfhogg`
- [ ] CI status badge — `https://img.shields.io/github/actions/workflow/status/folknor/pbfhogg/ci.yml`
  (requires GitHub Actions CI workflow)

### Other

- [ ] Add LICENSE-APACHE copyright header (currently has upstream b-r-u only)
- [ ] Add a CHANGELOG.md before first tagged release
- [ ] Add GitHub Actions CI — clippy, tests, rustfmt, doc build on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Write a small 1-page project website (what it does, benchmarks, usage, link to repo)
- [ ] Host via GitHub Pages

