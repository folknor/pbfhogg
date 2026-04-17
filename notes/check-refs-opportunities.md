# `check --refs` - optimization plan

Target: `pbfhogg check --refs` on planet. Current: 20m25s (1225 s) wall, 1.8 GB peak RSS (commit `7e9c2e9`, 2026-04-17).

## Thesis

Unlike ALTW and the geocode builder, this command is a much simpler optimization story. It is not a pipeline reshape. It is not a barrier-removal. It is **one wrong data structure**. The author of [`check_refs.rs`](../src/commands/check_refs.rs) profiled the command, wrote the diagnosis down in a comment, and then reached for the wrong container to fix it.

From [check_refs.rs:76-80](../src/commands/check_refs.rs#L76):

> profiling shows check-refs is consumer-bound (main thread 100% CPU on `RoaringTreemap` insertions, decode workers idle at 1% CPU each). Faster parsing would not reduce wall time.

That is the complete diagnosis. Decompression is not the bottleneck. Parsing is not the bottleneck. The main thread is pegged on 11.6 B `RoaringTreemap::insert` + `contains` operations.

The fix is ~30 lines of diff: swap the three `RoaringTreemap`s for three `IdSetDense`. This is the codebase's own purpose-built dense-monotonic-ID set, used by renumber, ALTW, extract, tags_filter, and geocode_index. It is ~10× faster per op and has the same or slightly lower RSS. Parallelization (phase #2 below) is a secondary win that only becomes worth doing *after* the data-structure swap flips the profile.

Target after this plan: **~6-10 min at planet, RSS ~1.5-2 GB.**

## Yardstick

Workload at planet:

| Operation | Count | Current unit cost | Current total |
|---|---:|---:|---:|
| node_id inserts (monotonic) | ~10 B | ~70 ns | ~700 s |
| way_id inserts (monotonic) | ~1 B | ~70 ns | ~70 s |
| relation_id inserts (monotonic) | ~17 M | ~70 ns | ~1 s |
| way-ref contains (random) | ~4-6 B | ~40 ns | ~200 s |
| relation-member contains (random) | ~100 M | ~40 ns | ~4 s |
| Sum of consumer work | | | ~975 s |

That is ~16 min of the 21 min wall, matching the "100% CPU on treemap insertions" profile. The remaining ~5 min is blob I/O + decompression + `PrimitiveBlock` construction on the same sequential main thread.

## Measured baseline (Japan, commit `eebba66`, 2026-04-17, plantasjen)

Dev-loop A/B dataset is Japan. Planet is too long (21 min) for tight iteration; Denmark (8 s) is too short to escape noise on a 60 % win. Japan sits at ~57 s and shows all the same consumer work in the same proportion. Note: `brokkr check-refs` passes only `--refs` (no `--check-relations`), so the relation path is skipped - same as the 1225 s planet baseline.

**Instrumentation overhead:** adding unconditional `Instant::now()` brackets around every per-element op costs ~15 s at Japan (700 M timer fires × ~22 ns). This is constant across pre/post, so A/B comparisons remain valid, but absolute numbers are not directly comparable to pre-instrumentation baselines (prior cadc3e6 Japan: 42.1 s).

### Wall (UUID `09484939`, bench mode)

| Sub-op | Time | Share | Ops | Notes |
|---|---:|---:|---:|---|
| `node_insert_ms` | 17.9 s | 31.6 % | 301 M | RoaringTreemap inserts - swap target |
| `way_ref_check_ms` | 15.5 s | 27.4 % | 354 M | contains + dedup-insert - swap target |
| `decompress_ms` | 6.6 s | 11.7 % | 43 K blobs, 6.4 GB | won't change |
| `pread_ms` | 0.6 s | 1.1 % | 43 K | won't change |
| `block_build_ms` | 0.3 s | 0.6 % | 43 K | won't change |
| Timer overhead (unaccounted) | ~15.5 s | 27 % | ~700 M `Instant::now()` | constant across A/B |
| **Total wall** | **56.7 s** | | | |

Consumer work (inserts + contains) = **33.4 s, 59 % of wall**. Matches the plan's "consumer-bound" thesis exactly.

### Hotpath (UUID `0b1ab47e`)

| Function | Total | % |
|---|---:|---:|
| `check_refs` | 59.1 s | 100 % |
| `decompress_into` | 7.04 s | 11.92 % |
| `from_vec_with_scratch` | 385 ms | 0.65 % |
| `parse_and_inline_with_scratch` | 162 ms | 0.27 % |
| `blob::parse` | 2 ms | 0 % |

Read-path total ≈ 13 %. The remaining ~87 % lives inside `check_refs` itself (RoaringTreemap work, not coverable by hotpath - it's external crate code).

### Alloc (UUID `c8c6029f`)

| Function | Total Alloc | % |
|---|---:|---:|
| `parse_and_inline_with_scratch` | 12.0 GB | 55.4 % (scratch churn, won't change) |
| `decompress_into` | 6.0 GB | 27.7 % (won't change) |
| **`check_refs` exclusive** | **3.6 GB** | **16.9 %** (swap target) |
| main + others | 1 MB | 0 % |
| **Total** | **21.5 GB** | churn; **RSS diff +559 MB** |

### Measured post-swap (Japan, commit `8f0ccbb`, 2026-04-17, plantasjen)

Swap landed as a single commit: the three element-ID `RoaringTreemap`s become `IdSetDense` (pre-allocated to OSM ID ceilings), the three missing-ref sets become `Vec<i64>` with `sort_unstable + dedup` for unique counts, and the deferred relation-resolve path uses the same pattern. ~30 lines of diff. Cross-validated vs osmium on Denmark: all four check paths (ways-only + with-relations × nodes / ways / relations) return identical counts.

| Metric | Pre (`09484939`) | Post (`1fd77d78`) | Δ |
|---|---:|---:|---:|
| Wall | 56.7 s | **33.1 s** | **−41.6 %** |
| NODES phase | 35.7 s | 24.9 s | −30 % |
| WAYS phase | 20.9 s | 8.1 s | −61 % |
| `node_insert_ms` | 17.9 s | 6.6 s | −63 % (22 ns/op measured) |
| `way_ref_check_ms` | 15.5 s | 2.5 s | **−84 %** (7 ns/op measured) |
| `decompress_ms` | 6.6 s | 6.8 s | unchanged |
| `check_refs` exclusive alloc (alloc mode) | 3.6 GB | 3.9 GB | +0.3 GB (one-shot pre-alloc vs incremental churn) |
| RSS diff at exit | 559 MB | 1.6 GB | +1.0 GB (pre-alloc'd IdSetDense chunks stay resident) |
| after_nodes `hblkhd` | 10 MB | **1.76 GB** | 420 chunks × 4 MB = 1.68 GB (pre-allocated to 14 B node IDs) |

**Plan prediction vs reality:** projected ~27 s wall, measured 33.1 s. Gap is instrumentation: `Instant::now()` itself is ~20 ns, so the per-op readings ("22 ns node_insert", "7 ns way_ref_check") are hitting the timer-resolution floor. Underlying `IdSetDense` work is closer to the 5 ns/op the plan posited; we just can't see below ~20 ns with nanosecond-granularity wall timers. Dropping the per-op timers would likely recover the extra 5 s and land near the projection.

**Hotpath shape flip (`fc7bcfef`):**

| Function | Pre % | Post % |
|---|---:|---:|
| `decompress_into` | 11.9 % | **20.6 %** (now biggest-attributed) |
| `from_vec_with_scratch` | 0.65 % | 0.9 % |
| `parse_and_inline` | 0.27 % | 0.2 % |
| `check_refs` (consumer + timer OH) | ~87 % | ~78 % |

Of the ~78 % still inside `check_refs`, ~45 % is instrumentation timer overhead and ~33 % is real consumer work. Post-swap at planet the consumer share should drop well below decompression, validating the plan's claim that step #1 flips the bottleneck.

### Protocol artefacts

- Phase markers (nested in existing `CHECKREFS_SCAN_*`): `CHECKREFS_NODES_*`, `CHECKREFS_WAYS_*`, `CHECKREFS_RELATIONS_*`, `CHECKREFS_DEFERRED_RESOLVE_*`.
- Per-sub-op `_ms` counters: `pread`, `decompress`, `block_build`, `node_insert`, `way_insert`, `way_ref_check`, `rel_insert`, `rel_member_check`, `deferred_resolve`. Accumulated as nanos, emitted as millis.
- Structural counters: `node_blobs`, `way_blobs`, `relation_blobs`, `total_bytes_decompressed`, `way_refs_checked`, `rel_{node,way,rel}_members_{checked,deferred}`, `missing_*_occurrences`.
- `mallinfo2` snapshots at `checkrefs_after_nodes`, `checkrefs_after_ways`, `checkrefs_after_relations`, `checkrefs_final`.

## Current architecture

Sequential single-threaded loop at [check_refs.rs:152-244](../src/commands/check_refs.rs#L152). For each `OsmData` blob:

1. Pread the blob frame (sequential `BlobReader::next()`).
2. Decompress into a reused `Vec<u8>`.
3. Build a full `PrimitiveBlock` including string table, tag indexing, metadata.
4. Iterate `elements_skip_metadata()`:
   - **DenseNode / Node**: `node_ids.insert(id)`.
   - **Way**: `way_ids.insert(id)`; for each `ref` in `w.refs()`, if `!node_ids.contains(ref)` → `missing_node_refs_set.insert(ref)`.
   - **Relation**: `relation_ids.insert(id)`; for each member by type (Node / Way), check against the matching set; for Relation members, push onto a `deferred_relation_refs: Vec<u64>` for post-pass resolution.
5. After the main loop, iterate `deferred_relation_refs` and check each against the fully-built `relation_ids`.

Missing-ref sets (`missing_node_refs_set`, `missing_way_refs_set`, `missing_node_members_set`) are also `RoaringTreemap`s, used solely to deduplicate missing IDs to match osmium's "441 unique missing nodes" semantics.

Sequential reader comment at [check_refs.rs:107-111](../src/commands/check_refs.rs#L107):

> Sequential reader to avoid `PrimitiveBlock` cross-thread alloc/free retention (25+ GB at Europe/planet scale). check-refs does lightweight per-element work (`RoaringTreemap` inserts) - the pipelined reader's parallel decode creates cross-thread churn that dominates at scale.

Same glibc arena fragmentation issue described in Pass 2 of the geocode builder. Renumber has the two-line fix for it; check_refs did not adopt it.

## Central observation

The RSS justification at [check_refs.rs:82-102](../src/commands/check_refs.rs#L82) compares `RoaringTreemap` against `HashSet<i64>`:

> - `HashSet<i64>`: ~400 GB (infeasible)
> - `RoaringTreemap`: ~2-3 GB (fits comfortably on any server)

The baseline is a strawman. The codebase's native structure for exactly this workload - dense-monotonic-ID membership with fast insert + contains - is [`IdSetDense`](../src/commands/id_set_dense.rs). Concrete comparison at planet:

| Structure | `node_ids` RSS | insert cost | contains cost |
|---|---:|---:|---:|
| `HashSet<i64>` | ~400 GB | ~100 ns | ~100 ns |
| `RoaringTreemap` | ~2 GB | ~70 ns | ~40 ns |
| `IdSetDense` (pre-allocated to `MAX_NODE_ID`) | ~1.6 GB | ~5 ns | ~5 ns |

`IdSetDense` is strictly smaller *and* strictly faster for this workload. It's a 4 MB-chunked bitmap - `set(id)` is chunk-index + byte-offset + bitmask OR (~3-5 instructions, one cache line touched). `get(id)` is the same shape. No tree walk, no hash, no run-length decode.

check_refs needs only membership, never rank, so `build_rank_index()` is not called. The cheap path throughout.

That's a ~10× speedup on every one of the ~11 B insert operations and ~5 B contains operations. It is the whole game.

## Ranked opportunities

### #1 - Replace `RoaringTreemap` with `IdSetDense` (headline) — LANDED (commit `8f0ccbb`, 2026-04-17)

Japan bench confirmed the thesis: wall 56.7 s → 33.1 s (−41.6 %). Per-op `way_ref_check` dropped from 44 ns to 7 ns (timer-resolution limited), `node_insert` from 60 ns to 22 ns (same limit). `check_refs` now accounts for only ~33 % real consumer work, down from ~59 %. Decompression is the new biggest-attributed function in hotpath. Cross-validated against osmium on Denmark. See "Measured post-swap" section above for the full table. Planet bench deferred until a quiet-host window.

Scope: three local replacements in [`check_refs`](../src/commands/check_refs.rs#L105).

Pre-allocate conservative upper bounds at function entry, before the scan:

```rust
use super::id_set_dense::IdSetDense;

let mut node_ids = IdSetDense::new();
node_ids.pre_allocate(14_000_000_000);  // matches ALTW's MAX_NODE_ID
let mut way_ids = IdSetDense::new();
way_ids.pre_allocate(1_500_000_000);    // current way IDs top out ~1.3 B
let mut relation_ids = IdSetDense::new();
relation_ids.pre_allocate(25_000_000);
```

Swap `.insert(id.cast_unsigned())` → `.set(id)` (`IdSetDense::set` takes `i64` directly; the `cast_unsigned` dance disappears). Swap `.contains(id.cast_unsigned())` → `.get(id)`.

`build_rank_index()` is not called - check_refs uses only membership. Skip it.

**Missing-ref sets** (`missing_node_refs_set`, `missing_way_refs_set`, `missing_node_members_set`) are `RoaringTreemap`s used only to deduplicate missing IDs for the final count. At planet these are typically a few thousand to a few million IDs. Replace with `Vec<i64>` + `sort_unstable` + `dedup` at the end. Simpler, faster for small sets, and easier to concat-merge across workers in phase #2.

**Expected wall**: from 20m25s to roughly **10-13 min**. The consumer loop stops being the bottleneck; whatever surfaces next (blob I/O + decompression + `PrimitiveBlock` construction, all still on one thread) becomes the new limit.

**Expected RSS**: ~same or slightly lower (1.5-1.8 GB). Pre-allocating to `MAX_NODE_ID` allocates all ~400 chunks up front (~1.6 GB), versus `RoaringTreemap` growing as containers fill. Peak is comparable.

### #2 - Parallelize as a three-phase renumber-shaped scan — LANDED (commits `053def6`, `fbf591c`, 2026-04-17)

**Landed in three steps** plus one preparatory cleanup:

1. `d536466` — dropped per-element `Instant::now()` brackets (measurement tax we added in the instrumentation pass). Japan 33.1 s → 18.2 s; the tax was never real work.
2. `053def6` — parallel three-phase scan. Each phase builds a per-kind schedule and runs through `parallel_classify_phase`. Pre-allocation of all three `IdSetDense`s moved to function entry (required for `set_atomic`). `mallopt(M_ARENA_MAX, 2)` prelude added. Deferred relation-relation resolve stays serial post-loop.
3. `fbf591c` — follow-up: added `build_classify_schedules_split`, a one-pass header walk that emits all three per-kind schedules from a single file read. The naive rewrite called `build_classify_schedule` three times, doubling/tripling the header-chain walk cost at Europe scale (30.5 s of 51.2 s wall on the first parallel run). One-pass brings Europe to 33.6 s.

**Measured results (commits cumulative through `fbf591c`):**

| Dataset | Pre-#2 wall | Post-#2 wall | Speedup |
|---|---:|---:|---:|
| Japan (`09484939` → `4a347e3b`) | 18.2 s (timers dropped) | **2.1 s** | 8.7× |
| Europe (`fb042f27` → `70ff6c5d`) | 426.2 s (post-swap, instrumented) | **33.6 s** | 12.7× |

**Plan target:** ~6-10 min at planet. Projection from Europe × 5 file-size ratio = **~170 s ≈ 2.8 min**, already ~2× better than the plan's floor before any step #3 work.

**Europe phase breakdown (UUID `70ff6c5d`, commit `fbf591c`):**

| Phase | Wall |
|---|---:|
| SCHEDULE_SCAN_LOOP (one header walk, all kinds) | 14.8 s |
| CHECKREFS_NODES (parallel scan) | 11.2 s |
| CHECKREFS_WAYS (parallel scan) | 7.4 s |
| **Total** | **33.6 s** |

**Hotpath attribution at Europe (UUID `07d4d92b`):**

| Function | Total / wall | % |
|---|---:|---:|
| `decompress_blob_raw` | 162.3 s total | ~4× worker-concurrent |
| `build_classify_schedules_split` | 20.7 s wall | 52 % |
| `next_header_with_data_offset` | 20.6 s | 52 % (all of schedule-walk time is here) |
| `parallel_classify_phase` | 18.9 s wall | 48 % |
| `from_vec_pooled_with_scratch` | 9.1 s total | 23 % |
| `parse_and_inline` | 2.1 s | 5 % |

**Alloc at Europe (UUID `75840aff`, total 1.8 GB, no churn):**

| Function | Alloc |
|---|---:|
| `check_refs` exclusive (IdSetDense pre-alloc) | 1.6 GB |
| `parse_and_inline` (scratch) | 53 MB |
| schedule build vecs | 27 MB |
| worker scratch | 20 MB |
| `decompress_blob_raw` | 6 MB |

From 21.5 GB churn at Japan post-swap to 1.8 GB at Europe post-parallel — all the per-op incremental allocation is gone; only the one-shot pre-alloc remains.

**Cross-validation:** osmium parity still passes on Denmark across all four check paths (ways-only + nodes/ways/rels in relations) after each step.

**Planet confirmation (UUID `862547e4`, commit `0d71b3b`, 2026-04-17, plantasjen):** **1225 s → 72.5 s = 16.9× speedup.** Plan target was 6–10 min; measured 1 min 12 s, roughly 5–8× under the plan floor. Phase breakdown: SCHEDULE_SCAN_LOOP 16.8 s, NODES parallel 35.4 s, WAYS parallel 20.2 s. Peak RSS 2.17 GB, p95 2.13 GB, 0 major page faults - comfortable on a 27 GB host.

**Off-plan discoveries logged in `TODO.md` Performance section:** `BlobReader::seek_raw` goes through stdlib `Seek::seek` which always discards the `BufReader` buffer; fixing this to use `BufReader::seek_relative` is codebase-wide perf work beyond check-refs scope. Buffer-size tuning alone is unsafe without that fix (verified: 16 MB buffer caused a 13× regression when the seek path still discards per blob; reverted in `86761d6`).

### Original #2 plan (preserved for reference)

Once #1 lands, the profile flips. `IdSetDense::set_atomic` is essentially free (a relaxed atomic OR per ID), so per-element work goes from the dominant cost to negligible. At that point the sequential reader at [check_refs.rs:112](../src/commands/check_refs.rs#L112) is the new binding constraint, and decode workers - currently "idle at 1% CPU" - can actually do useful work.

The structure maps cleanly onto `renumber_external`'s three-phase shape, because check_refs's phases have the same dependencies renumber's do: each type's ID set must be fully built before the *next* type's ref-checks against it can run.

**Prelude**: `mallopt(M_ARENA_MAX, 2)` at function entry ([renumber_external.rs:95-98](../src/commands/renumber_external.rs#L95)). Prevents the glibc arena retention the current code's comment calls out. Two lines.

**Phase 1 - node scan.** Pattern: [`pass1_parallel_scan`](../src/commands/renumber_external.rs#L615). Work-stealing dispatch over node blobs (schedule from [`build_classify_schedule`](../src/commands/mod.rs#L429) filtered to `ElemKind::Node`). Each worker: pread → decompress → `PrimitiveBlock` → walk DenseNode IDs → `node_ids.set_atomic(id)`. No contains checks yet. `node_ids` is the single shared pre-allocated `IdSetDense`.

**Phase 2 - way scan.** Work-stealing over way blobs. Each worker: `way_ids.set_atomic(way.id())`; for each `ref`, `if !node_ids.get(ref) { local_missing_node_refs.push(ref); local_missing_refs.push(MissingRef { ... }); }` if `show_ids`. Per-worker `Vec<i64>` for missing IDs; merged at end.

**Phase 3 - relation scan.** Work-stealing over relation blobs. Each worker: `relation_ids.set_atomic(rel.id())`; check node members against `node_ids`, way members against `way_ids`, push relation-type members onto a per-worker `deferred_relation_refs: Vec<(u64, i64)>` (member id + referencing rel id, for `show_ids`).

**Post-pass**: merge per-worker deferred vectors, scan sequentially, check each against the fully-built `relation_ids`. Small - at planet, ~10 M total deferred relation refs. Cheap.

**Merging missing-ref vecs**: concatenate per-worker `Vec<i64>`s, `sort_unstable`, `dedup`, take `len()` for the unique-missing count. If `show_ids`, concatenate per-worker `Vec<MissingRef>`s in worker order; the output order no longer matches file order, but the current contract doesn't promise file order (deferred relation-relation refs are already resolved out-of-order in the post-pass).

**Expected wall**: another 2-4 min saved. Target end state: **~6-10 min** at planet, primarily decompression-bound.

### #3 - Selective wire-format parser (conditional on #1 + #2)

The comment at [check_refs.rs:69-80](../src/commands/check_refs.rs#L69) explicitly rejects this:

> A pure "ID-only scan mode" that skips refs/members would not work here. A selective parse that skips stringtable, tags, coordinates, and metadata but keeps IDs + refs + members was considered but is **not worth it**: profiling shows check-refs is consumer-bound …

That premise changes after #1 and #2. With `IdSetDense` + parallel decode, consumer cost and decompression cost both drop. The remaining wall is in `PrimitiveBlock::from_vec_*` construction + the iteration in `elements_skip_metadata()` - including string-table UTF-8 validation, tag indexing, and metadata decode that check_refs never uses.

Build a wire-format `scan_ids_refs_members(decompressed, callbacks)` that extracts only:

- **Nodes**: the packed DenseNode ID delta stream (`DenseNodes` field 1). Nothing else - skip lat (fields 8), lon (field 9), keys_vals (field 10), info (field 5).
- **Ways**: way field 1 (id) + field 8 (packed refs). Skip field 2 (keys), field 3 (vals), field 4 (info), and fields 9/10 (LocationsOnWays lat/lon).
- **Relations**: field 1 (id) + field 9 (memids) + field 10 (types). Skip field 2 (keys), 3 (vals), 4 (info), 8 (roles_sid).

Skip the `PrimitiveBlock::StringTable` entirely - we never resolve any key or value string in check_refs.

Template: [`scan_way_refs`](../src/commands/way_scanner.rs#L24) is the existing shape for way refs; add node-ID and relation-member variants.

**Expected**: cuts per-blob decode + parse cost roughly in half. Whether this matters depends on where #2 leaves things. If post-#2 wall is ~8 min evenly split between decompression and `PrimitiveBlock` parse, this saves ~2 min. If decompression dominates, this saves less.

Worth building **after measuring** post-#2 to know whether the parse or the decompression is the new limit. Not worth building first - if #1 and #2 together hit the target wall, this is unnecessary complexity.

### #4 - (Already folded into #1) Missing-refs sets as `Vec<i64>` + dedup

Folded into #1's diff. Called out separately because it's a distinct micro-decision: the `RoaringTreemap` → `IdSetDense` swap for the three main ID sets is about hot-path speed; the missing-refs vec-and-dedup is about merging per-worker results cleanly in phase #2.

## What to leave alone

- **The two-phase deferred relation-relation pattern.** Forward references are real (relations legitimately reference later relations). Deferred-collect-then-post-pass-check is the correct shape. Run it with `IdSetDense::get` instead of `RoaringTreemap::contains`.
- **`show_ids` path.** Off by default; the `MissingRef` vec it populates is large only when the input is broken. Don't redesign around it.
- **The `check_relations` flag and its blob-filter skip.** Pass-through correctly; phase #2 honors it by simply not launching Phase 3.
- **`elements_skip_metadata()` vs full element iteration.** Already the right shape for what `check_refs` needs (when it goes through `PrimitiveBlock` at all). If #3 lands, the wire-format scanner replaces this entirely; if not, leave as-is.
- **`RoaringTreemap` as a dependency elsewhere.** This analysis is local to `check_refs.rs`. Other callers may genuinely have sparse sets.
- **Sorting way refs before contains-checks.** Considered for cache locality. With `IdSetDense` each contains is a single cache-line touch already, and the bitmap is dense enough that sorted access doesn't help much. Skip.

## Memory budget (planet, post-#1 + #2)

At phase-3 peak (the heaviest - all three ID sets live, plus per-worker scratch):

| Component | Size |
|---|---:|
| `node_ids` `IdSetDense` (pre-allocated to 14 B) | ~1.6 GB |
| `way_ids` `IdSetDense` (pre-allocated to 1.5 B) | ~175 MB |
| `relation_ids` `IdSetDense` (pre-allocated to 25 M) | ~3 MB |
| Per-worker read + decompress buffers × 6 | ~300 MB |
| Per-worker missing-ref `Vec<i64>` × 6 (typical case: small) | <50 MB |
| Per-worker deferred relation-relation vecs (Phase 3 only) | <100 MB |
| **Total** | **~2.1-2.3 GB** |

Host budget: unchanged (1.8 GB current is comfortable; 2.3 GB post-parallelization is still trivial).

**Why this plan's sizing is robust where altw-as-renumber's was not.** `IdSetDense` pre-allocated to `MAX_NODE_ID` is a fixed 1.6 GB regardless of how many IDs are actually set; it does not scale with the unique-referenced count. The [altw-as-renumber](altw-as-renumber.md) reshape (attempted 2026-04-16) OOM'd on Europe because its `coord_table` scaled as `unique_referenced × 8 bytes` and the real count was ~4-5× the estimate. check-refs avoids that failure mode by construction: the bitmap size is bounded by the ID space, not the population, and the ID space is a global OSM constant.

## Plan of attack

1. **Add per-phase `_ms` counters** unconditionally (not `cfg(feature = "hotpath")`). Measure current planet to fix the 20m25s baseline, then re-measure after each step. Each step's proof is "wall went down and result is identical."
2. **Land #1 alone first** - `RoaringTreemap` → `IdSetDense` + missing-refs vec-and-dedup. ~30 lines of diff. This is most of the win. Cross-validate against current `main` on Denmark, Europe, planet - `RefCheckResult` fields should be identical (node_count, way_count, all four missing counts) plus identical `missing_refs` vec contents (sort both sides before comparing if order matters).
3. **Land #2** - `mallopt` + three-phase parallel scan. Confirm no RSS regression on planet (expect +300-500 MB from per-worker scratch). Re-measure wall.
4. **Measure post-#2 breakdown**. If decompression dominates, stop - #3 would not help. If `PrimitiveBlock` construction is a significant share, land #3.

## Correctness invariants

- **Dedup semantics.** The command reports *unique missing IDs* ("441 nodes missing" = 441 distinct IDs that don't exist). Preserved by vec-sort-dedup at end, same cardinality as the current `RoaringTreemap::len()`.
- **Deferred relation-relation refs.** Sorted PBF does not guarantee relations are referenced only after their definition; forward references exist. The post-pass check after `relation_ids` is fully built is the correct shape; preserve it across parallelization by merging per-worker deferred vecs before the check.
- **`MissingRef` output order.** Currently produced in PBF blob order within a single pass, but the deferred relation-relation refs are already appended out-of-order by the post-pass. So callers cannot rely on full file-order. Phase #2's per-worker concatenation preserves that contract - the order within each worker's block is PBF order; across workers is undefined, same as the existing deferred tail.
- **`check_relations = false` skip.** `skip_field(relation_kind)` at [check_refs.rs:157-162](../src/commands/check_refs.rs#L157). Preserved by simply not running Phase 3 when the flag is false, and by filtering the relation blob schedule appropriately.
- **Negative-ID handling.** Current code uses `id.cast_unsigned()` because `RoaringTreemap` is `u64`. `IdSetDense::set` takes `i64` and rejects negative IDs silently via the `if id < 0 { return; }` guard (see id_set_dense.rs:45). Production planet files never contain negative IDs (comment at [check_refs.rs:94-98](../src/commands/check_refs.rs#L94)); JOSM-local negative IDs are out of scope. If negative-ID support is required, `IdSetDense` would need extension - but this is a non-goal for check-refs against official planet dumps.

## Open questions

- **Exactly how much wall time does #1 alone save?** My estimate (~10 min) is based on a per-op cost model. Actual speedup depends on where the `RoaringTreemap` ops land in the cache hierarchy - `IdSetDense` at 1.6 GB doesn't fit in L3, so every contains is an L3 miss at minimum. This is still much cheaper than a tree walk, but the absolute numbers want verification.
- **Does `pre_allocate(14_000_000_000)` cost visibly at startup?** That's a ~1.6 GB contiguous memset on Phase-1 entry. At ~10 GB/s DDR bandwidth, ~160 ms. Negligible against a 6-10 min wall, but worth noting.
- **Is phase #3's decompression genuinely faster than phase #2's?** Relation blobs are a tiny fraction of total bytes. Phase 3 might complete in seconds regardless; if so, the end-of-pipeline tail is dominated by the post-pass merge, not relation decode. Neutral either way.
- **Do we gain anything by fusing phase 2 and phase 3?** The dependency chain says way_ids must be built before relation-ref-to-way checks. But relation member checks are deferred to the post-pass anyway for relation-relation refs, and the node/way checks are small. Could collapse phase 2 and phase 3 into one parallel scan that reads both kinds of blobs and relies on the sorted-PBF ordering for the dependency. Not critical; leave as two phases for clarity.
