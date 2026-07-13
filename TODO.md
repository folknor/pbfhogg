# pbfhogg TODO

## Roadmap at a glance

Open work is grouped here by what actually gates it. The completed
record (formerly interleaved with the open items) lives at the bottom
of this file, full text preserved. Detail stays in notes/ - closed
notes carry a CLOSED/STATUS header pointing back here.

**Quick wins (hours, or docs-only)**

- ALTW drift settle (optional, ~1 h machine time): both 2026-07-13
  planet runs sit +10-17 % above the April 546.0 s baseline, beyond
  the observed ~6 % single-sample noise - a `--bench 3` pair at HEAD
  vs `--commit 16e3694` would confirm or dismiss real drift
  (Performance section). The injected-prepass gate itself is CLOSED:
  no measurable regression.

- Geocode reader interior-hint PIP skip: investigated 2026-07-13,
  UNSOUND (wrong admin matches near boundaries). Fix A (always run
  the point-in-polygon test, drop the interior-flag bypass) LANDED
  2026-07-13. Fix B (builder-side interior erosion that would
  re-enable the skip as a sound optimization) remains open (Item
  plans / geocode entry).
- getparents blob-skip readout: MEASURED off existing run `21ed8d7c` -
  64.6 % of primary-planet blobs; the CHANGELOG "~85 %" discrepancy is
  RESOLVED as metric confusion (element share vs blob share), CHANGELOG
  stays untouched. Remaining tail = filter-semantics confirm only
  (Item plans / getparents).
- ElemKind reuse refactor: cat + repack DONE 2026-07-13; a third copy
  in degrade remains, deferred because its u8 kind is a `--drop-ids`
  hash/reproducibility contract (Item plans / repack follow-ups).

**Active arcs (large open engineering, pick deliberately)**

- Geocode builder rewrite G1-G5 + two S2 spikes
  ([notes/geocode-build-opportunities.md](notes/geocode-build-opportunities.md)).
- GeoJSON export v1 ([notes/geojson-export-design.md](notes/geojson-export-design.md)) -
  Milestone 3.
- Multi-extract v2 complete/smart strategies - Milestone 3.

**Decisions needed (forks in the road, not tasks)**

- CLI UX: scratch-dir posture + `--index-type` replacement (Milestone 3
  / Command surface).
- ~~diff `-j` default flip~~ DECIDED + IMPLEMENTED 2026-07-13:
  `-j 0` default, temp-disk cost documented in `--help` + README +
  CHANGELOG (Performance).
- ~~History-file support~~ DECIDED 2026-07-13: explicitly OUT of the
  1.0 scope; recorded in notes/time-filter-optimization.md + README
  footnote.
- ~~Writer API surface~~ RESOLVED 2026-07-13: OutputChunk/OutputSink
  shape settled-keep; deferred header write parked with a real-caller
  trigger; the PbfWriter consolidation closed as overtaken by code
  drift. Record in
  [notes/write-path-optimization-plan.md](notes/write-path-optimization-plan.md)
  "Current state"; no scheduled work.

**Deferred with explicit triggers - do not chase without the trigger**

- Shape-3 serial schedule walk / io_uring batched header walker
  ([notes/header-walk-batching.md](notes/header-walk-batching.md);
  trigger: a high-blob-count planet becomes a real workload).
- Reflink sort fast-path ([notes/sort.md](notes/sort.md) opp #6;
  trigger: a reflink-capable output fs matters in production).
- io_uring output rail on a real command (write-path plan items 5/5b).
- apply-changes #11 splice-in-place / #13 exact-membership metadata
  (see the apply-changes entry below).
- Milestones A/B/C + GPU (Non-traditional optimization research).
- renumber polish ([notes/renumber-optimization.md](notes/renumber-optimization.md));
  altw-external RAM/NVMe-gated tiers ([notes/altw-external.md](notes/altw-external.md));
  time-filter items ([notes/time-filter-optimization.md](notes/time-filter-optimization.md),
  gated on a history dataset).

**Blocked on dataset / config** - see the section of that name under
"Planet-scale validation coverage".

## Item/command-specific plans

Three new docs capturing a cross-cutting insight and two new commands that fall out of it. All scaffolding-level; details drift as work lands.

- [ ] **[reference/blob-density.md](reference/blob-density.md)** - the insight: Geofabrik-style PBFs (~8k elements/blob, ~522 k blobs on europe) scale very differently from `planet.openstreetmap.org`-style PBFs (~300k elements/blob, ~50 k blobs on planet). The `HeaderWalker`-touching commands split into three shapes (measured 2026-07-10 -> 2026-07-12 via `snapshot.8k`): **shape 1 selective header-walk** (`getid`, `getparents`) regresses on density and is now **RESOLVED** by ADR-0006 blob-count dispatch (150 k threshold); **shape 2 pure parallel-classify** (`tags-filter`) *gains* on density (-14 % at 8k, no action needed); **shape 3 hybrid** (`check --refs`, `check --ids`, `cat --clean`, `repack`, `degrade`, `extract --smart` - all via `build_classify_schedules_split`) carries a single-threaded serial schedule walk that regresses on high blob count (check-refs +185 % / 153.5 s at 8k, 66 % of it the serial walk; UUID `1851f73a`), but with near-zero production bite since production planet is 50 k blobs. `sort` pass 1 stays separately priced (seek-skip mechanism, excluded from ADR-0006). README's "Planet scale" table and `notes/*.md` "N seconds at planet" predictions remain measured on the sparse-blob (50 k) encoding. Shapes 1 and 2 need no further work; the one open lever is shape 3, below - deferred, near-zero production bite (production planet is 50 k blobs), do NOT chase unless a high-blob-count planet becomes a real workload.

  - [ ] **Shape-3 serial schedule walk (`build_classify_schedules_split`).** The single-threaded `HeaderWalker` loop in `src/scan/classify.rs` does one QD=1 pread per blob (~70 us/blob), invisible at 50 k blobs (~3.5 s) but a fixed ~97-109 s at 1.45 M blobs. **Full six-caller 8k sweep done 2026-07-12** (table in `reference/blob-density.md` "Full shape-3 sweep"): the walk fraction splits the family, which **narrows the payoff** from the earlier "helps six commands" framing. Read-only callers are ~2/3 walk - `check --refs` 66 % (`1851f73a`), `check --ids` 67 % (dirty) - so flattening it roughly halves their 8k wall (~2.7x -> ~1.3x). Re-encoding callers are only 18-29 % walk - `extract --smart` 29 % (`4b82686f`), `cat --clean` 19 % (`3f4c222c`), `repack` 18 % (`8f275ebf`), `degrade` 18 % (`6f8a3e94`) - their 8k regression is dominated by framing + writing 1.45 M tiny blobs, which the walk fix does not touch. So: **if the primitive is ever built, it is a win for the read-only pair only** (and those two are the same commands whose getid/getparents selective-scan cousins already got ADR-0006 dispatch). The fix is the same io_uring batched-header-walker primitive that sort pass 1, the getparents walk term, and the getid walker arm all want - the four-call-site convergence, per-site payoff, why it cannot be trivially parallelized, and the rejected alternatives are consolidated in [`notes/header-walk-batching.md`](notes/header-walk-batching.md). Still deferred: near-zero production bite (production planet is 50 k blobs).

- [x] ~~**[notes/repack.md](notes/repack.md)** - new command: re-encode a PBF with a configurable `--elements-per-blob N` cap.~~ **v1 + v2.1 LANDED** (`BlockBuilder::with_element_cap(n)`, cross-input-blob coalescing so grow caps fire correctly across input-blob boundaries). Planet 8k bench ~380-390 s. v2.2 (LocationsOnWays preservation) and v2.3 (osmium cross-validation) remain deferred - see the note.

  **v1.1 follow-ups:**
  - [ ] **`ElemKind` reuse audit (cross-command).** **cat + repack DONE 2026-07-13**: both commands' bare-`u8` `KIND_NODE/WAY/RELATION` constants replaced with `crate::blob_meta::ElemKind` throughout (phase params, `KindPayload`, worker matches - now exhaustive, deleting repack's dead `invalid kind constant` error arm and `KindPayload::empty` fallback). `brokkr check` tier1 both sweeps + the repack monotonicity tier-2 test pass. **Remaining: `degrade` is a THIRD copy the original audit missed** (`src/commands/degrade/mod.rs`, ~30 sites) and it is NOT a mechanical swap: `drop_hash` feeds the u8 kind *numerically* into the splitmix64 hash that selects `--drop-ids N:SEED` victims, and `DropKey` derives `Ord` over the u8 - the 0/1/2 mapping is a reproducibility contract (same seed must drop the same elements across builds). Correct shape if converted: plumb `ElemKind` through the phases but keep an explicit `fn hash_kind(ElemKind) -> u8` boundary pinning Node=0/Way=1/Relation=2 with a comment naming the contract. Deferred - requires the degrade design context (notes/degrade.md) to confirm no other consumer of the numeric values.

  **Correctness follow-up (found 2026-07-12 during the shape-3 8k sweep):**
  - [x] ~~**`repack` emits non-monotonic relations across blob boundaries (CONFIRMED bug, found + fixed 2026-07-12).**~~ Root cause (`run_kind_phase` in `src/commands/repack/mod.rs`): worker "full" blocks wrote directly via `write_raw_owned` in seq order, while the central builder's coalesced tail-blocks went through a `pending` buffer that only flushed every 32 blocks, so low-ID coalesced tails could land after later high-ID direct blocks. Fix: in each `pop_ready` iteration, when the incoming blob carries direct full-blocks and the central stream (`bb` or `pending`) is non-empty, flush the central stream first. **Accepted density trade:** on a coalescing shrink, output blob count is no longer the general `ceil(elements / cap)` - it now depends on input-blob boundaries, since each input blob whose count isn't a multiple of the cap emits its tail as its own possibly-under-cap block (documented in `notes/repack.md`). Regression tests in `tests/cli_repack.rs`: `repack_output_is_monotonic_across_coalesced_blob_boundaries`, `repack_output_is_monotonic_for_nodes_and_ways`, `repack_output_is_monotonic_with_pending_prepopulated_at_guard`.

  **Release / measurement follow-ups:**
  - [x] ~~**Register the 8 k-packed planet as a snapshot.**~~ Done 2026-07-10: UUID `8027765b` (377.5 s at `8c1cf03`, plantasjen) promoted its output to `data/planet-8k-with-indexdata.osm.pbf`, registered as `snapshot.8k` `pbf.indexed`. The two earlier bench runs (`0ae01c09`, `a4791ddc`) wrote to scratch and kept nothing. The snapshot is the input for every same-corpus-different-encoding pair the `reference/blob-density.md` matrix needs, and for the deferred `getparents` HeaderWalker dispatch decision (europe-regressing / planet-winning along the same blob-count axis); consumer commands reach it via `--snapshot 8k`.

- [ ] **[notes/degrade.md](notes/degrade.md)** - adversarial-test tool, v1 shipped (`--unsort`, `--unsort-intra`, `--strip-locations`, `--strip-indexdata`, `--strip-tagdata`, `--strip-bbox`, `--drop-ids`; deferred: `--recompress`). The `--unsort` cross-blob bug (found 2026-07-10) was **fixed 2026-07-11**: `--unsort` now suppresses the per-input-blob boundary flush so the swap straddles a genuine output-blob boundary, and the old intra-blob shape is preserved as the deliberate `--unsort-intra` flag (keyed to the first two elements so it stays intra-blob for any input blob size). **Correctness gate CLOSED 2026-07-11**: snapshot regenerated post-fix, `verify sort --snapshot unsorted` PASS, and the overlap-rewrite path fires on the regenerated data - `sort_blobs_overlap=6`, `sort_overlap_runs=3`, `sort_blobs_rewritten=6` (one adjacent same-kind pair per kind, exactly the designed shape; UUID `11062bdd` at `29e4eab`, plantasjen). The pre-fix run `f5cd6522` saw 0 overlaps / full passthrough.

- [ ] **[notes/sort.md](notes/sort.md)** - `sort` (repair unsorted PBFs into `Sort.Type_then_ID`). Drafted 2026-04-23. **Production reality**: Geofabrik / planet input is already sorted, so the overlap-count is ~zero and pass 2 is pure raw passthrough. The headline opportunity that helped the production case, **`copy_file_range` coalescing for passthrough runs**, LANDED (`244c6ec`: `try_extend_copy_run` + `flush_copy_run`; denmark run `11062bdd` coalesced 7387 passthrough blobs into 3 calls; see `notes/sort.md`). The bigger theoretical wins - parallel overlap-rewrite in pass 2 (1.5-3x) and HeaderWalker-based pass 1 (1.2-2x on non-indexed input) - only fire on genuinely-unsorted input, which has no dataset configured in `brokkr.toml` today. Planet hotpath + alloc captured 2026-04-27 overnight at `4fc8e35` (UUIDs `d64932d2` hotpath / `26fb329e` alloc): 115.4 s wall, **94 % in `pbfhogg::write::writer::flush`** (108.6 s) and 6 % in `build_blob_index` (6.77 s) - reaffirms the writer-side `copy_file_range` ceiling is the only lever for already-sorted input, with no allocation pressure (459 MB exclusive, all in `blob_wire::parse`). Hotpath wall sits below both the 124.6 s `68e1ba0` and 132.3 s `16e3694` bench baselines, softening the `+6-7 %` regression flag tracked in `reference/performance.md`. Anti-conversion rule (pipelined → sequential) explicitly off the table per `reference/pipelined-reader-paths.md:138`.

- [ ] **[notes/getparents.md](notes/getparents.md)** - `getparents` (whole-file scan listing ways / relations referencing a given ID set). Drafted 2026-04-23, headline experiment landed 2026-04-24 (`783970a`). The HeaderWalker + `parallel_classify_phase` rewrite shipped: planet 44.8 s -> **23.5 s** (-46 %, UUID `11bc44dc` at `16e3694`), europe 26.4 s -> **44.2 s** (+68 %, blob-density asymmetry - see [reference/blob-density.md](reference/blob-density.md)). Original 4-8x estimate was wrong: blob indexdata stores `(min_id, max_id)` of *elements in the blob*, not the *ref/member IDs* the typical "find ways referencing these nodes" query cares about, so `IdSet::any_in_range()` pre-screen does not apply. Actual win comes from IO byte reduction (74.8 GB -> 30 GB at planet) by skipping blob kinds structurally incapable of producing matches. Planet hotpath at `4fc8e35` (UUID `00253c7d`): 23.0 s wall, 78 % in `parallel_classify_phase` - that **is** the post-experiment state, not headroom. The c912e4d Denmark 4.7x sequential-decode regression rule remains explicitly off the table (it targets sequential-decode conversions; `parallel_classify_phase` keeps decompression parallel via pread workers). **The europe-regression question (revert / threshold-dispatch / accept) is RESOLVED**: the crossover was measured 2026-07-10 on the 8k-packed planet (HW 82.7 s vs scan 52.8 s; walk ~45 us/blob, linear in blob count) and the dispatch was ratified as [`ADR-0006`](decisions/0006-blob-count-threshold-dispatch.md) (full matrix in [notes/getparents.md](notes/getparents.md) "Crossover measured"; the RESOLVED paragraph moved to the Completed record below). Residual opportunities, absorbed here from the note (2026-07-13):
  - [ ] **#3 blob-filter skip-rate readout - MEASURED 2026-07-13** from the existing planet bench `21ed8d7c` (2026-07-12, `a65cecc`, `--bench 3`): `getparents_blobs_skipped` 32,835 / `walk_actual_osmdata_blobs` 50,816 = **64.6 % of blobs skipped** (schedule 17,981; counters sum exactly). The CHANGELOG 0.3.0 "~85 % of blobs at planet scale" claim vs this 64.6 % is **RESOLVED as metric confusion, not an error to correct** - three different metrics collide: nodes are **~90 % of elements** (encoding-independent; ~10.4 B of 11.6 B), **64.6 % of blobs on the byte-packed primary planet** (node blobs avg ~317 k elements vs way blobs ~66 k, so way blobs are overrepresented in blob count), **~90 % of blobs on the 8k-uniform encoding** (blob share = element share when every blob holds 8,000 elements), and **~75 % of planet bytes** (the module doc's metric). The "~85 %" was element share mislabeled as blob share, written 2026-04-27 - months before repack and any blob-density awareness. **Decision (2026-07-13): CHANGELOG.md stays untouched** - historical release record, defensible under the element-share reading; this entry is the durable explanation. Remaining tail: confirm the filter semantics for the "parent relations of a node still needs way blobs" case (a counter can't answer that; needs a targeted test or code read).
  - [ ] **#4 refs/members buf pre-sizing (<1 % wall).** Skip unless an `--alloc` profile shows churn.

- [x] ~~**[notes/geocode-build-opportunities.md](notes/geocode-build-opportunities.md)**~~ - `build-geocode-index`. **ARC LANDED 2026-04-18**, planet 1255 s -> 432.9 s (-65%/2.9x), all 10 ranked items shipped. Remaining follow-ups in the note: Pass 2 interp resolve still sequential (30.6 s planet), interpolation endpoint CSR for RSS hygiene.
  - [ ] **Geocode reader interior-hint PIP skip was UNSOUND - investigated 2026-07-13, Fix A landed 2026-07-13, Fix B open.** The admin-cell interior high bit used to be honored as a PIP bypass (`is_interior || admin_polygon_contains(...)` in `src/geocode_index/reader.rs`, both `search_admin_ranked` and `search_admin_all`), introduced at `f365f7f` with a comment claiming "accepted approximation per spec". Investigation findings:
    - **The spec said the opposite.** The retired spec (`reverse-geocoding-spec.md`, deleted 2026-07-13, in git history; sections 3.4 and 4.11) defined the flag as a hint only - the reader "still performs a point-in-polygon test" on interior-flagged cells, using the flag for priority and same-level early exit, so a false flag costs wasted work, never a wrong answer. The "per spec" comment was a misreading.
    - **What `f365f7f` actually restored is the traccar-geocoder precedent.** `research/traccar-geocoder/server/src/main.rs` does the identical `is_interior || point_in_polygon(...)` over the query cell plus all 8 neighbors. Reader-side, pbfhogg was traccar parity.
    - **But traccar's builder makes that skip sound and pbfhogg's does not.** traccar (`builder/src/build_index.cpp` `cover_polygon`) uses S2's geometric `GetInteriorCovering(polygon)` AND erodes the interior by one cell ("only mark a cell interior if all its edge neighbors are also interior"). With erosion, any query cell adjacent to a flagged cell is itself fully inside the polygon, so neighbor-flag acceptance is correct (residual hole: erosion uses 4 edge neighbors while the reader iterates 8 including diagonals - a diagonal-neighbor gap even traccar's erosion doesn't close). pbfhogg's pass 3 (`src/geocode_index/builder/pass3.rs`) instead flood-fills from the centroid, tests only cell centers, excludes edge cells derived from the sampled 256-step-clamped `cover_segment` - no erosion, no geometric covering. pbfhogg inherited the optimization while dropping both of its preconditions.
    - **Two concrete wrongness modes that existed in the pre-fix reader.** (1) Neighbor-cell acceptance: a query point in boundary cell C, outside polygon P, was accepted into P via an adjacent genuinely-interior cell N - fired for near-boundary queries within one admin-cell width (~8 km at level 10), and ranked mode's smallest-area-per-level then preferred the wrong-side polygon when it was smaller (e.g. standing in Germany near the border, Denmark won level 2). Wrong even with a perfect builder. (2) False interior flags: an edge cell missed by the sampled `cover_segment` whose center lies inside got flagged interior with the boundary running through it - the exact gap the open "exact S2 segment coverage" spike below tracks.
    - **Fix A landed 2026-07-13: restore spec behavior.** Dropped the bypass at both call sites in `src/geocode_index/reader.rs` - both now always run `admin_polygon_contains`; `is_interior` is read but no longer influences the result. Regression test `admin_interior_hint_does_not_bypass_point_in_polygon` in `tests/geocode_index.rs` pins the neighbor-cell case: it asserts a query's own admin cell carries no interior flag while at least one cell in its 8-neighbor S2 neighborhood does, then checks both `query()` and `candidates()` reject the wrong-side match. Sound regardless of builder quality; cost is one ray-cast against a max-500-vertex simplified polygon per candidate entry.
    - **Fix B (open, re-enables the optimization): traccar-parity builder.** Add interior erosion - using all 8 neighbors, not traccar's 4, to close the diagonal-neighbor hole confirmed above - and geometric interior covering to pass 3; then the reader skip becomes sound and can return as a measured optimization. Format-compatible (same flag bit) but requires an index rebuild; dovetails with the open "Spike: exact S2 segment coverage for Pass 3" item below.
  - [ ] **Spike: exact S2 segment coverage for Pass 3.** `cover_segment` currently samples intermediate lat/lon points and clamps the walk at 256 steps. Prototype a proper S2 edge/cell traversal using `s2` 0.1.0 primitives (`RegionCoverer`, `Cell`, `Point`, `edgeutil::simple_crossing`, or equivalent direct cell-edge tests), then compare fine/coarse cell counts and geocode query results against the sampling path on Denmark/Europe before replacing it.
  - [ ] **Spike: admin interior hints via S2 region coverage.** Admin indexing currently edge-covers rings and flood-fills from an arithmetic exterior vertex mean; concave polygons whose mean falls outside skip interior hints and pay more query-time PIP checks. Prototype a polygon `Region` or other `RegionCoverer::interior_covering` path that preserves the current edge-vs-interior on-disk semantics, handles holes, and measure admin cell count/PIP-hit changes before any format or behavior change.

- [ ] **apply-changes** (plan note `apply-changes-opportunities.md` deleted 2026-07-13, content absorbed here; git history has the full record) - `apply-changes --locations-on-ways`. **P1 + P1.5 landed 2026-04-21 (`719f306`)**; parallel writer made the default 2026-04-21 (buffered path removed, `--parallel-writer` flag deleted). **Planet best: 80.9 s cross-disk + zstd:1** (-44 % vs 144.4 s pre-flip baseline; parallel pwrite, unaffected by the CopyRange bug). Same-disk zstd:1 best: **104.5 s** with parallel pwrite. The same-disk `--io-uring` column was re-measured 2026-04-26 at `16e3694` after the `fa8251d` CopyRange fix and is now uniformly slower than parallel pwrite at every same-disk compression level (none 137.5 s, zlib:6 137.4 s, zstd:1 126.3 s; UUIDs `9a5c25a7` / `70e5414b` / `0e6a5918`); the original 108.6 / 137.1 / 99.4 s numbers were tainted by the writer dropping a zero-page between OSMHeader and first OSMData blob. Cross-disk `--io-uring` rows (93.0 / 127.9 / 82.8 s) still need re-measurement on the fixed writer. Same-disk `--io-uring` no longer the recommended override; cross-disk `--compression none` + `--io-uring` is open until re-measured. Remaining open items: splice-in-place (#11, deferred - doesn't reduce output bytes on compressed output), multi-file output / RAID-0 (unlanded and lower priority given 80.9 s is comfortably inside any realistic production budget). **Note closed 2026-07-13** - its remaining items are tracked here now:
  - [x] ~~**#15 document zstd:1 as the internal-pipeline recommendation**~~ - DONE (found landed 2026-07-13): README.md and reference/performance.md both carry the `--compression zstd:1` recommendation for pipelines that skip osmium interop.
  - [ ] **#11 splice-in-place for low-touch rewrites (deferred).** For `NeedsRewrite` blobs with <=K affected elements (K~64), splice raw wire bytes for unmodified element runs instead of full decode + re-encode (~1.5-2 s at daily; scaffolding in `src/write/raw_passthrough.rs`). Saves classify/rewrite CPU but not output bytes, so it will not move the writer-bound wall. Revisit if the writer ceiling moves.
  - [ ] **#13 exact-membership metadata / sidecar (format project).** Per-blob ID-range-only metadata forces slow-path decode on pure creates inside an existing range: 15,224 FalsePositive blobs / 92,677 slow-path = 16 % of slow-path work wasted at planet. The false-positive distinction is documented on `block_overlaps_diff` in `src/commands/apply_changes/classify.rs` (`range_overlaps` true for create IDs inside the blob's range, `block_overlaps_diff` false because no element in the block matches; the old `classify_only` "FalsePositive" comment the note pointed at was removed in the streaming refactor). Two fix shapes: (a) wire-format exact-overlap scanner on decompressed bytes; (b) per-blob membership sketch in indexdata. Not a quick cleanup.
  - Open questions preserved from the note: actual overlap-blob ratio under larger OSCs (re-measure if input diffs grow; governs worker-pool load); byte-budget reorder capacity sizing (current setting works at planet; revisit on RSS/stall signals); scanner HeaderWalker vs worker throughput balance (no signal it's an issue).

- [ ] **[notes/altw-external.md](notes/altw-external.md)** - `add-locations-to-ways --index-type external`. Current planet baseline: **546.0 s `--bench 1`** (UUID `7fd04130`, commit `16e3694`, 2026-04-26; was 603.7 s at `aa0dc719` post-A1, 661.2 s pre-A1) - **−115.2 s / −17.4 % vs pre-A1**, an extra **−9.6 %** since the post-A1 measurement attributable to commits between `0dc8ae1` and `16e3694`. Europe **270.8 s** post-A1 (was 291.6 s at `6d71053`). **Europe compression sweep landed 2026-04-27 overnight at `4fc8e35`** (`reference/performance.md` "Compression axis" subsection): `none` 246.8 s (UUID `16c35911`, ~6.5 GB anon), `zstd:1` **233.3 s** (UUID `e2fba1bf`, ~6.6 GB anon), vs the cross-commit zlib:6 reference 270.8 s at `0dc8ae1` - so zstd:1 is **−14 % vs default**, refreshing the stale 419→379 / −9.5 % claim from the older `f3c53a34`/`66e43a11` baselines. Same mechanism as before: relieves consumer/compression saturation in stage 4 with similar output size. A1 (rankless node-ID bucketed join) landed 2026-04-25 across 8 commits + 4 review fixups; pass B and the IdSet rank machinery deleted. **Doc rewritten 2026-07-13** (full code re-read + codex critique) around the measured planet cost model at `856efc3`: queue is now N1-N7 (packed u64 IdRecord + cat metadata enabler at the top, then payload RAM handoff, stage-2 worker sweep, scratch split), with a disposition ledger mapping the old L1-L20 and output-compression knob sweeps moved to do-not-retry. 2026-07-13 planet runs measured 636.6 s plain / 602.9 s `--inject-prepass` (`abe2ebf2` / `b3b79a62`, commit `856efc3`) - +90 s vs the 546.0 s baseline, attributed entirely to scratch I/O throughput on identical byte volumes (stage 1 +32 s, stage 2 +52 s, streaming flat); drive-state vs code verdict pending the overnight bench. Failed attempts, measured numbers, physical floors, and meta-lessons live in [`notes/altw-optimization-history.md`](notes/altw-optimization-history.md).

  **Apply-changes transfer candidates (all three resolved since 2026-04-21):**
  - **Worker-emits-framed-bytes (P1.5 pattern): tried and reverted** (A2 milestone 1, `b641095` -> `1050111`, 2026-04-25). Mechanism worked exactly as designed (permit/send waits -99 %) but planet stage-4 wall regressed +30 s: the pattern removed the implicit parallelism between decode workers and the writer's rayon pool. Do not re-try without a coordinated multi-stage rework; details in `notes/altw-optimization-history.md`.
  - **Cross-disk scratch: probed and blocked on hardware** (2026-04-22, +30 % regression on the slower 970 EVO Plus). Lives on as `notes/altw-external.md` N5, gated on a 990-PRO-class second drive.
  - **`zstd:1` for internal pipelines: measured and documented** (europe compression axis 2026-04-27, -13.9 %; shipped guidance in README.md / `reference/performance.md`).

  **Probably doesn't transfer:**
  - **Descriptor-first scanner + drain shape.** ALTW external is multi-pass external-sort; the design premise (reader/classify/rewrite in one pass) doesn't match.
  - **Node→way barrier + coord fusion.** Apply-changes-specific (coords needed mid-run to resolve OSC way refs); ALTW's whole job IS the coord scatter.
  - **BTreeMap seq reorder buffer at drain.** ALTW Stage 4 has its own output ordering.

  **Already shared:**
  - **HeaderWalker** (scan-audit round migrated both commands).
  - **`copy_file_range` coalescing** (apply-changes drain *ported from* `altw/passthrough.rs`, so the flow direction already ran).
  - **`IdSet::set_atomic_if_new`** primitive (used by both for parallel set-membership; type formerly named `IdSetDense`).

  Rough prioritization if a day were available: cross-disk bench first (10 min, tells us where the ALTW ceiling actually lives), then worker-framed-bytes if Stage 4 is writer-bound.

Measurement-first on every one: turn on `#[cfg(feature = "hotpath")]` counters (or add unconditional `*_ms` counters) to ground-truth the inferred per-phase breakdowns before committing to the order of landing items within a plan.

## Important: ignored tests

`roundtrip_denmark` in `tests/roundtrip_real.rs` is `#[ignore]` - it roundtrips the entire
Denmark PBF (~54s) and is too slow for the normal edit-test cycle. **Must be run before any
release and after completing major work** (especially changes to reader, writer, block_builder,
or BlockBuilder/PbfWriter APIs):

    brokkr check -- --ignored

`tests/geocode_index.rs` has 6 `#[ignore]` tests - they build a geocode index from the
Denmark PBF and query it. ~154s in release mode. Run with:

    cargo test --release --test geocode_index -- --ignored

`sorted_flag_but_unsorted_nodes_panics` in `tests/read_paths.rs` is `#[ignore]` - it
verifies the debug monotonicity assertion fires on unsorted nodes when `Sort.Type_then_ID`
is declared. Requires `debug_assertions` to be enabled in the test profile. Nightly 1.95
(2026-02-25) has a regression where `debug_assertions` is off in test builds.

`fault_injection_parallel_writer_pool_panic_surfaces_error` in
`tests/apply_changes_invariants.rs` and
`fault_injection_parallel_gzip_worker_panic_surfaces_via_finish` /
`fault_injection_uring_writer_dispatch_panic_surfaces_via_flush` /
`fault_injection_diff_parallel_shard_panic_surfaces_and_sweeps_scratch` /
`fault_injection_derive_parallel_shard_panic_surfaces_and_sweeps_scratch` /
`fault_injection_altw_stage3_bucket_panic_surfaces_and_cleans_scratch` /
`fault_injection_geocode_pass3_streets_panic_sweeps_bucket_dirs` in
`tests/fault_injection.rs` are `#[ignore]`d because their fault-injection hooks are
**process-global static atomics** that race with any concurrently-running test that
uses the same pipeline (most apply-changes / derive-changes / diff tests do). They
require single-threaded execution to be deterministic. Run via `brokkr test <name>`
(which always adds `--test-threads=1`) or `cargo test -- --ignored --test-threads=1`.
The canonical `fault_injection_worker_panic_surfaces_error_and_leaves_scratch_clean`
test in the same file is **not** ignored because its hook is per-instance
(`MergeOptions::panic_at_blob_seq`) and has no shared-state hazard.

The uring fault-injection test additionally skips gracefully on hosts whose
`RLIMIT_MEMLOCK` soft limit is below 16 MB (needed for io_uring's registered
buffers). To actually exercise it on a dev host, raise the limit system-wide
via `/etc/security/limits.conf`:

    @<your-group>    -    memlock    unlimited

then log out/in. The same limit constrains the existing
`roundtrip_uring_*` tests (they also skip when MEMLOCK is too low).

## Planet-scale validation coverage

README's planet table is the source of truth for "this command runs
cleanly on the 32 GB-RAM reference host". `overnight.sh` fills most
of the reachable gaps as bench runs (produces
`.brokkr/results.db` entries that get promoted into the README once
they land). This section tracks the remaining axes and dataset gaps
that are not currently driven by `overnight.sh`.

### 1.0 blockers (planet OOM or RSS-exceeds-ceiling)

**All resolved 2026-04-27 / 2026-04-28** via `parallel_classify_phase`
+ `ReorderBuffer` migrations: `check --ids` streaming (`516129e`),
`cat --clean` (`b347c0a`), `time-filter` snapshot (`83183fb`),
`tags-filter` way-deps phase (`17b116c`). README's "Not yet
planet-safe" table is now empty. Per-item record (numbers, root
causes, the origin of the "shape != root cause" lesson) moved to the
Completed record at the bottom of this file.

### Latent same-shape risks (not gating 1.0)

Two commands share the
`for_each_primitive_block_batch` + `par_iter().map_init(BlockBuilder)`
+ `collect` + drain pattern that drove the pre-migration
time-filter snapshot OOM. Neither has been benched at planet
RSS-wise; neither blocks 1.0 today.

**Critical lesson (2026-04-28): shape != root cause, always instrument
first.** See the tags-filter bullet above. Before assuming either of
these two commands needs the par_iter+collect migration, run a planet
bench with full sidecar instrumentation and read the per-phase RSS
table - the actual blocker may be a sibling `parallel_classify_accumulate`
caller instead.

When you do bench, the data lives in `brokkr sidecar <UUID> --human`
and won't survive a subsequent forced/failed run from any other
command (the `dirty` alias rotates). If the run OOM/SIGKILL'd before
`writer.flush()`, mid-run `WRITER_METRICS.emit()` calls inside the
batch boundary leave fresh state in the FIFO - the time-filter
migration set up that pattern in `src/commands/time_filter/mod.rs`;
mirror it if you expect SIGKILL on the first attempt.

- [x] ~~**`getid --add-referenced` pass 2**~~ **PLANET-SAFE AT CURRENT
  WORKLOAD, no migration needed.** Benched 2026-04-28: planet 96.3 s
  wall, 1.26 GB peak anon (par_iter+collect shape) - didn't trigger
  the feared peak because brokkr's tiny hardcoded ID set yields
  ~zero output blobs per batch. Pass 2 is read-bound, not
  memory-bound (`writer_recv_wait_ns` = 99% of wall). Same lesson as
  tags-filter: shape != root cause. Future risk is a much larger /
  wider-spread input ID set re-introducing the accumulation peak -
  see "Custom ID set distributions" below, not gating 1.0. Counter
  `getid_dep_node_ids` (2026-04-28) surfaces dep-set size before
  pass 2 starts.

- [ ] **`altw` sparse path** (`src/commands/altw/mod.rs:485-510`
  + `process_batch:692-736`). **BENCHED (partial) 2026-07-12, commit
  `a65cecc`, UUID `dirty` - the par_iter+collect shape is NOT the
  problem; shape != root cause a third time.** The overnight rider
  `add-locations-to-ways --dataset planet --index-type sparse --bench 1`
  was terminated by the operator ~72 min into `ALTW_PASS2` (a manual
  kill, NOT a crash and NOT OOM: RSS was bounded at ~21.3 GB with only
  ~3.2 GB anon, nowhere near the host ceiling; brokkr's `exit 137
  "(OOM?)"` label is the SIGKILL heuristic misfiring). The failure mode
  is SOFT: unboundedly slow progress, not death. The pass-2 sidecar
  phase shows the actual peak is random-access mmap thrash on the sparse
  rank-indexed flat scratch during location write-back:
  **~181M major page faults, ~13 TB disk re-read** (against a scratch
  file orders of magnitude smaller), **2.5 avg cores** (I/O-stalled, not
  compute-bound), still far from done at kill. The index build itself is
  fine (rank buckets written in ~80 s); the pathology is purely pass-2
  read-back locality. So the par_iter+collect `Vec<OwnedBlock>`
  accumulation the prior TODO framing feared never dominated - RSS
  stayed bounded exactly as getid's pass 2 did.
  Currently masked because `add-locations-to-ways --index-type auto`
  selects `external` for sorted+indexed planet inputs, and external
  uses entirely different scatter/gather code (`altw/external/`) -
  this pattern doesn't fire on the planet recommended path.
  Forcing `--index-type sparse` at planet is the trigger. **Conclusion:
  sparse is not planet-viable, but the fix axis is pass-2 mmap access
  locality (or simply keep `auto` steering planet to `external`), NOT a
  par_iter+collect migration.** Other altw stages still worth checking:
  any `parallel_classify_accumulate` caller in the sparse pipeline is
  suspect at planet keep-rates (the documented caution at
  `src/scan/classify.rs:300-317` lists the criteria).

### Other `parallel_classify_accumulate` callers (audit checklist)

The pattern that bit tags-filter (`parallel_classify_accumulate` +
per-worker `IdSet`) lives in at least one other place that's
already documented:

- **geocode pass 1.5** - per-worker IdSet of way node refs. Documented
  at `src/scan/classify.rs:302-308` as "shipping at 14.59 GB peak RSS
  (planet) - OK in practice, but on the rewrite list in
  `notes/geocode-build-opportunities.md`." Migration template applies
  identically: per-blob `Vec<i64>` of node refs through the bounded
  result channel. **Borrow caveat:** the geocode pass 1.5 merge
  step's mutability vs. the classify step's read access has not been
  audited in this context - if the same `&X` / `&mut X` conflict
  arises that prevented `tags_filter::collect_relation_member_closure`
  from migrating, fall back on `parallel_classify_accumulate` and
  size the per-worker state explicitly. The
  `tags_filter::collect_relation_member_closure` precedent at
  `src/commands/tags_filter/mod.rs:984-1066` shows the unmigratable
  shape and the trade-off (bounded per-worker `Vec<i64>` is fine
  when state grows with element count, not ID space).

- **`tags_filter::collect_relation_member_closure`** itself - kept
  on `parallel_classify_accumulate` *deliberately* (per the borrow
  caveat above; pinned in code).

If you discover another caller while investigating getid or altw,
add it here with the per-worker upper bound at planet scale.

### Blocked on dataset / config

- [x] ~~**Injected-prepass flag-ON planet verdict**~~. The producer landed
  2026-07-11 (`29e4eab`, ADR-0007) with flag-OFF neutrality shown. The
  brokkr `--inject-prepass` passthrough LANDED the same day (brokkr
  `e50a679`; the flag lands in recorded cli_args like `--direct-io`
  does, no separate variant column needed). **VERDICT REACHED
  2026-07-13** via a same-commit `--bench 1` A/B at `856efc3`
  (plantasjen): flag-ON **602.9 s** (`b3b79a62`) vs flag-OFF
  **636.6 s** (`abe2ebf2`) - flag-ON measured 33.7 s FASTER despite
  doing strictly more work, so single-sample noise on this command is
  at least ~35 s / ~6 % and **the injection cost is below `--bench 1`
  resolution: no measurable planet regression; the ADR-0007 gate is
  closed**. The four injection counters exercised end-to-end for the
  first time, all plausible: `altw_member_ways` 37.2 M,
  `altw_pinned_refs` 2.63 B (21 % of the 12.44 B way refs),
  `altw_field20_ways_emitted` 535.3 M (46 % of 1.166 B way messages),
  `altw_field5_bytes` 145.8 MB. Full record in
  `reference/performance.md` "ALTW drift flag + inject-prepass A/B";
  gate context in `reference/pipeline.md`. **Follow-up spun off
  below**: both runs sit +10-17 % above the April 546.0 s baseline -
  see the ALTW drift item under Performance.
- [x] ~~**History PBF for `time-filter`**~~ - RETIRED 2026-07-13:
  history-file support declared OUT OF SCOPE for 1.0. Full decision,
  rationale, and re-entry trigger in
  [notes/time-filter-optimization.md](notes/time-filter-optimization.md);
  README's planet table carries the user-facing footnote.
- [ ] **Additional planet snapshots** for `diff-snapshots`. Current
  `brokkr.toml` has only one alternate (`snapshot.20260411`), so the
  snapshot-range axis is a single pairing. Downloading another
  snapshot 2-4 weeks away would let us measure diff-wall vs
  snapshot-delta-size empirically.

### Un-benched permutations (low priority)

Known to work, no performance question open, but not in the results DB:

- [ ] **Custom ID set distributions for `getid` / `getparents`**.
  brokkr's ID set is baked in; no way to test different distributions
  (sparse vs dense, forward vs spread across the ID range, cold-cache
  vs hot-cache). Add a CLI pass-through if ID-set shape becomes a
  perf question. Not needed for general validation.
- [ ] **`--direct-io` at planet for commands beyond apply-changes**.
  `apply-changes` has coverage. Every other command supporting
  `--direct-io` (cat, sort, extract, add-locations, merge-changes
  where applicable, ...) has no `--direct-io` planet number. Only
  matters if direct-io becomes a default on any of them.
- [ ] **`renumber` with non-default flags**. Has no non-default flags
  in pbfhogg today (just the one variant since the in-memory path
  was retired). If a future variant adds flags this reopens.
- [ ] **`bench-read` / `bench-write` / `bench-merge` at planet**.
  Synthetic benchmarks, intentionally excluded from the README user
  surface. Periodically-useful diagnostic tools; not a validation
  target.

## Performance

- [ ] **ALTW planet drift: confirm or dismiss (+10-17 % vs April).**
  Both 2026-07-13 `--bench 1` runs at `856efc3` (flag-OFF 636.6 s
  `abe2ebf2`, flag-ON 602.9 s `b3b79a62`) sit well above the 546.0 s
  baseline (`7fd04130` at `16e3694`, 2026-04-26). The A/B pair proved
  single-sample noise is at least ~35 s / ~6 % on this command, which
  does NOT cover the 57-91 s gap - so real drift across the ~2.5
  months of commits is plausible but unconfirmed (cache state and the
  32 GB host's memory pressure differ across runs too). To settle: a
  `--bench 3` pair, HEAD vs `--commit 16e3694`, grouped per the
  build-thrash rule (~1 h machine time). Full record in
  `reference/performance.md` "ALTW drift flag + inject-prepass A/B".

- [x] ~~**Auto-enable diff `-j`**~~ - **DECIDED + IMPLEMENTED
  2026-07-13.** Flipped the default from `-j 1` to `-j 0`
  (`available_parallelism()`). Implementation note: `diff-snapshots`
  is brokkr's name for `pbfhogg diff` on two independent files - one
  command, one flag, so a single default flip covers both bench
  surfaces, and it applies to both `--format text` and `osc` (the old
  `--help` claim that parallelism was text-only was stale; both
  dispatchers resolve `jobs == 0`). Rationale: 9.5x/7.6x planet wins,
  ~3 months of field miles, fault-injection coverage on the shard
  path. The one real cost is scratch surprise (parallel diff writes
  ~30-45 GB of shards to the output's parent at planet; sequential
  needs zero), handled by documentation not code: `--help`, README's
  planet-table note, and a CHANGELOG "Changed" entry all state the
  temp-disk requirement and that `-j 1` restores the scratch-free
  sequential path. No size-based auto-threshold. Existing
  `derive_changes_jobs_parity_roundtrips_to_same_output` pins
  sequential == parallel output. **Gap found by the flip:** the
  parallel text path never implemented `-v/--verbose` per-field
  detail lines (acceptable while parallel was opt-in; four cli_diff
  tests caught it the moment parallel became the default). Fix:
  verbose diffs always dispatch to the sequential path
  (`src/commands/diff/mod.rs`, `!options.verbose` in the parallel
  guard), documented in `--help` and CHANGELOG. If parallel verbose
  is ever wanted, the shard workers need per-field detail emission -
  new work, not scheduled.

- [ ] **Expose phase events as a proper Rust event/hook API** - wrap
  instrumentation calls in per-command `probes` modules, then swap the
  backend from the current FIFO sink to `tracing` spans/events so
  library consumers can subscribe. **History (recovered 2026-07-13):**
  a probes-module PoC on time-filter landed (`2eae6ff`) and was
  reverted (`ddd6dc7`) after stress-testing against altw's ~140
  counters / 26 markers showed the design does not absorb the real
  aggregation shapes (AtomicU64 + thread::scope, channel-merged Stats,
  post-blob nanosecond accumulators); the plan doc
  (`notes/instrumentation-layering.md`) was retired with it (deleted in
  `9770abc`, recoverable from git history). If revisited, start from
  the revert message's guidance - cfg-gated empty-twin + ZST tuple-tail
  composition, shaped per command's actual question - not from the old
  rollout plan. No `probes` module or `tracing` usage exists in the
  tree today.

- [ ] **Reclaim europe's lost prefetch win after scan-audit.** The
  2026-04-20 scan-audit swap to `HeaderWalker` gave up ~14 s of
  downstream decompression benefit at europe scale because the old
  buffered header walk was accidentally warming blob-body pages via
  the kernel's sequential readahead - pages the downstream phases
  then reused. `posix_fadvise(POSIX_FADV_RANDOM)` deliberately skips
  that. A deliberate `posix_fadvise(POSIX_FADV_WILLNEED)` over the
  exact blob ranges that the scan result flagged for later pread
  (`(data_offset, data_size)` for the schedule entries we're about
  to hit) would reclaim the prefetch without re-introducing the old
  walk's I/O waste. Only matters for mid-size workloads - planet is
  larger than RAM so prefetched pages evict before reuse, and
  germany is already fully cached. Measure on europe `check-refs` /
  `tags-filter` / `extract --simple` where the phase-level win is
  huge but the full-command wall is currently flat.

## Cross-pipeline optimization

Cross-thread buffer retention is **solved** - `DecompressPool` (commit
`8f6999b`) recycles decompression buffers in the pipelined reader. The
remaining architectural concern is thread oversubscription (two concurrent
rayon pools: decode + batch processing), not retention.

See [notes/altw-optimization-history.md](notes/altw-optimization-history.md)
for the complete plan: 20 items across 5 priority groups, covering infrastructure
fixes, planet blockers, external join P2b/P2c, and all affected commands.
See [reference/pipelined-reader-paths.md](reference/pipelined-reader-paths.md)
for the April 2026 audit. Sequential conversion was attempted for
getparents (commit `c912e4d`) and reverted - 4.7x regression on
Denmark (1400ms vs 300ms). Decompression dominates, not per-block
processing. **No remaining pipelined paths should be converted to
sequential.** Renumber converted separately (external join
architecture, not driven by retention/oversubscription).

## Milestone 1: Planet-safe production pipeline - COMPLETE

## Milestone 2: Performance supremacy

Goal: fastest or equal on every PBF transform operation, with published
benchmarks. The write path is the remaining frontier.

### Raw group passthrough

Raw frame passthrough is shipped for extract simple - the 3-phase barrier
pipeline classifies blobs in parallel and writes matching raw frames via
pread workers, bypassing decode+re-encode entirely. Simple extract now
beats osmium (4.4s vs 7.2s Japan, 100s vs 350s Europe sequential baseline).

Raw frame passthrough is now shipped for cat --type (matching blobs
written as raw compressed frames, planet 207s → 43s, 4.8x) and
getid --invert (blobs with no ID-range intersection pass through raw,
Denmark 1.9s → 0.5s, Japan 8.6s → 1.3s). getid include mode skips
decompression of non-intersecting blobs (planet 71.5s → 32.5s, 2.2x).

The remaining re-encoding commands - tags-filter, renumber, time-filter -
still fully decode and re-encode via BlockBuilder. Of these:

- **tags-filter** is closed: blob-level raw passthrough was measured on
  2026-04-18 (shadow counter, commit `a5c6854` reverted in `0ef4107`,
  UUID `8c786794` at `w/highway=primary` on planet) and 0 / 50,364
  pass-2 blobs qualified. The load-bearing pin is the comment block
  at the pass-2 worker in `src/commands/tags_filter.rs`.
- **renumber / time-filter**: every element is modified, so raw
  passthrough does not apply - the win here is write-path throughput
  instead.

Four per-group raw passthrough primitives are committed as scaffolding
for partial-match blobs (e.g., extract boundary blobs where some groups
match and some don't). Currently unused - blob-level passthrough handles
the common case. Design tradeoffs and the measurement prerequisite live
in the module doc comment at `src/write/raw_passthrough.rs`. The
primitives themselves:

- `PrimitiveBlock::raw_group_bytes(index)` - raw PrimitiveGroup bytes
- `PrimitiveBlock::raw_stringtable_bytes()` - raw StringTable bytes
- `PrimitiveBlock::block_scalars()` - granularity, lat/lon offset
- `frame_raw_block()` in `src/write/raw_passthrough.rs` - assemble
  PrimitiveBlock from raw components

### Write-path throughput

After raw group passthrough, `BlockBuilder` (`src/write/block_builder.rs`)
and `PbfWriter` (`src/write/writer.rs`) are the next bottleneck for commands
that must re-encode partial-match groups. Opportunities: SIMD varint encoding
in `src/write/wire.rs` (the write-side protobuf primitives), zlib compression
level tuning, and reducing per-element overhead in
`BlockBuilder::add_node/add_way/add_relation` (string table construction
is the hot path - FxHashMap lookup + Rc<str> alloc per unique string).
See [notes/SIMD.md](notes/SIMD.md) for the varint research.

**Zlib level tuning:** extremely low priority. Investigated multiple
times in the project's history with no actionable outcome. Default
level 6 matches osmium and is the right choice for interop. zstd is
better for internal pipelines but the production pipeline already
works. The measured level matrix and closure verdict live in
[notes/write-path-optimization-plan.md](notes/write-path-optimization-plan.md)
item 2 (the `zlib-level-tuning.md` note was deleted 2026-07-13).

**Zstd:1 vs zlib:6 for ALTW external** (measured 2026-04-14): for
pipelines that can opt out of osmium interop, `--compression zstd:1`
is a substantial wall win on the external join path. Europe ALTW
external: 419 s (zlib:6, UUID `f3c53a34`) → 379 s (zstd:1, UUID
`66e43a11`), **−40 s, −9.5 %**. Stage 4 wall drops 28 % (132 s →
95 s); `s4_send_ms` cumulative drops 81 % (270 s → 51 s) and
`s4_channel_high_water` falls far below capacity - confirming that
zlib compression throughput was the steady-state stage-4 ceiling
under the consumer-owned raw-passthrough pipeline. The wall win
comes entirely from relieving consumer/compression saturation
downstream of the decode workers, not from any change in the
encode/decode code path. Zstd is not safe as the library default
(osmium and most consumers still expect zlib-compressed blobs;
[wiki: PBF specifies zlib](https://wiki.openstreetmap.org/wiki/PBF_Format))
but the flag is right there for internal-pipeline users. Output
file size stays within a few percent of zlib:6 at zstd:1, so the
knob is pure wall/interop trade-off, not a size trade-off.

## Milestone 3: Beyond the benchmark

Goal: the obvious choice for every OSM data processing task, not just
the fastest one.

### Multi-extract

Single-pass multi-extract shipped for simple strategy on sorted input
(commit `542aad0`). Reads PBF once, classifies each element against N
regions, writes to N sync-mode PbfWriters. 3-phase barrier (nodes →
ways → relations) with per-region IdSet + BlockBuilder. Memory:
N × ~1.5 GB at planet scale. Falls back to sequential for unsorted
input or --clean. Verified via `brokkr verify multi-extract`. Node
classify (the only coordinate-driven phase) is grid-pruned at N ≥ 16
regions via a CSR region grid (256 MiB coverage budget, linear
fallback above it); way/relation classify stay linear (IdSet
membership, not spatial).

**Known issues:**

- [ ] **strip-4 verify failure** - `brokkr verify multi-extract --regions 5`
  on Denmark: strip-4 has 1 fewer node than sequential (41643 vs 41644).
  Passes with 3 and 4 regions. Only fails with 5 regions where strip
  boundaries fall at exact integer longitudes (8,9,10,11,12,13). Likely
  a floating-point rounding issue in brokkr's bbox strip generation,
  not a pbfhogg bug. Pre-existing since multi-extract shipped.

**v2 improvements:**

- [ ] **Complete/smart strategies** - per-region way/relation ID
  tracking. Memory: N × ~3 GB (bbox_node_ids + all_way_node_ids per
  region). Feasible for ~10 regions on 30 GB host, ~40 on 128 GB.
- [x] ~~**Raw passthrough**~~ - CLOSED 2026-04-20: shadow counter
  measured 0/32,835 node blobs qualify (same outcome as tags-filter's
  0/50,364). Structural: ID-sorted PBFs scatter geography per blob,
  so a blob's bbox is ~planet-wide and can't fit a sub-planet region.
  Pin in `src/commands/extract/multi.rs::try_extract_multi_single_pass`.

### Export (GeoJSON/GeoPackage)

The bridge to the GIS ecosystem. Streaming PBF → GeoJSON/GeoJSONSeq
export. The pieces exist in the codebase:
- Reader: `ElementReader` for element iteration
- Geometry: `src/geo.rs` has point-in-polygon, ring assembly from way
  refs, Douglas-Peucker simplification
- Coordinates: `Way::node_locations()` from enriched PBFs (ALTW output),
  or inline coordinate resolution via the sparse/external index
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
- [ ] **CLI UX: scratch dir + mode naming, unified across the CLI** (raised
  2026-04-23, unresolved). Two related decisions, both of which should be
  applied uniformly across every command that carries the pattern, not
  one-off per command.

  (A) **Scratch-dir argument presence.** Today `add-locations-to-ways
  --index-type external` infers scratch as `output.parent()` with a `.`
  fallback (silent cwd footgun at 112-224 GB scale; see the `altw/external/mod.rs:191`
  bug-sweep entry). Sparse follows the same pattern. Other large-scratch
  paths (extract complete/smart, geocode builder, renumber stage 2d) need
  auditing: do they infer scratch the same way, and would a unified policy
  apply to all of them?

  Three postures for the unified policy, from least to most strict:
  1. **Fail-on-unsafe-default.** Infer from output.parent(); error cleanly
     if the derivation falls back to `.`. Catches the footgun, no new flag,
     no friction for the common "output on big disk" case.
  2. **Balanced: add a `--scratch DIR` override everywhere.** Default to
     output.parent(). Error on bare filename without `--scratch`. Gives
     users who want scratch on a different disk than output an explicit
     lever. Same footgun protection as (1).
  3. **Strict: require `--scratch` on every large-scratch command.**
     Self-documenting; every invocation names the scratch dir. Script-
     breaking for existing users; friction even when the inference would
     have been right.

  Pick one posture, apply to altw (all three backends) + extract complete/smart
  + geocode builder + any other commands that land >1 GB of scratch. The
  per-command bug-sweep LOW ticket for altw folds in once the posture
  is picked.

  (B) **Replace `--index-type` on `add-locations-to-ways` with a single
  user-facing override flag.** Today: `--index-type sparse|external|auto`,
  default `sparse`. The three-value flag exposes implementation names
  (`sparse`, `external`) that don't mean anything to a user picking a
  PBF tool, and the `auto` value is what the default should have been
  in the first place.

  Proposed shape:
  - Default behaviour: today's `auto` logic. Pick external when the input
    is sorted + indexed (the fast path at planet scale), pick sparse
    otherwise.
  - Single override flag, opting INTO the in-memory path: `--in-memory`
    (working name; alternatives considered: `--low-disk`,
    `--minimal-disk`, `--no-spill`). The override only matters when auto
    would have picked external - i.e., the input is sorted + indexed but
    the user doesn't have ~256 GB of temp disk for external's scratch.
    Forcing external the other direction is pointless: auto would have
    already picked it when conditions were met, and external can't run
    when they aren't (it requires sorted + indexdata).

  Why one flag is enough: the asymmetry above. Two flags
  (`--force-sparse` + `--force-external`) was the obvious symmetric
  shape, but `--force-external` is either redundant (auto picks it) or
  fails (preconditions not met), so it earns nothing.

  Why `--in-memory` over `--low-disk`: framing the override by what the
  user gets ("keep the index in process memory, don't spill to a giant
  scratch file") reads more naturally than framing by what they avoid.
  Slight imprecision since sparse still mmaps a values file
  (`referenced_count * 8` bytes; ~29 GB at europe), but that file is
  dwarfed by external's ~256 GB planet scratch and the user's mental
  model is "don't make a huge temp file."

  Library-side API change: `IndexType` enum loses its `FromStr` (no
  string parsing) and the dense-removal migration hint goes with it -
  users on `--index-type dense` would get clap's "unrecognized
  argument" error rather than the friendly pointer at sparse. The
  `altw_dense_index_type_rejected_with_migration_hint` test goes
  away in the same change. Acceptable cost: dense has been gone since
  `b70dd8c` (2026-04-30); by the time `--index-type` itself goes, the
  migration hint has done its job.

  Other "mode-like" flags (`extract --strategy`, `bench-read --mode`,
  `bench-write --writer`, `bench-merge --io-mode`, `diff --format`) are
  inconsistent but each picks a value out of a closed set that DOES
  matter to the user (e.g., extract strategies have different output
  semantics, not just performance). Leave them alone.

  Breaking CLI change. No urgency.
- [ ] Auto-selection: `--index-type auto` exists (sparse vs external).
  Extend to other decisions: sequential vs pread-from-workers based on
  available RAM and blob count; compression level based on output target;
  batch size based on core count. Config or heuristic, not manual flags.
- [ ] Migration guide from other tools - command mapping table, behavioral
  differences, indexdata workflow explanation. Build on existing
  `reference/osmium-parity.md`.
- [x] ~~**Document the `merge-changes -> apply-changes` pipeline pattern in
  README.**~~ **DONE 2026-07-13** - README's CLI section now documents
  squash-then-apply-once with the measured numbers. Original rationale
  preserved: when applying accumulated dailies (e.g. a week worth),
  squashing them with `merge-changes` first and then running
  `apply-changes` once on the result is the recommended shape - cheaper
  than running `apply-changes` N times. The 5x speedup at planet 7-OSC
  (commit `99057fa`, 267 s -> 55 s) made this an unambiguous
  recommendation; pre-parallel the squash itself was 4m27s and the
  calculus was murkier. Original suggestion from the apply-changes Q7
  reviewer round (2026-04-21), retired from the now-deleted
  `notes/merge-changes.md` plan doc 2026-04-28 when the parallel-drain
  work shipped.
- [ ] **`renumber` - maintenance polish** (current: 204.5 s / 3m25s planet at `aee7727`,
  historical 194 s at `cb99106`, 3.3 GB peak anon, zero temp disk).
  Three candidate items (varint fast path, `way_id_set` vs schedule, reframe
  breakdown instrumentation) captured in
  [`notes/renumber-optimization.md`](notes/renumber-optimization.md) with
  per-item regression analysis and disposition. Not today; revisit if
  renumber becomes critical path or the +10 s drift vs `cb99106` grows.

### Ecosystem

- [ ] CI status badge - `https://img.shields.io/github/actions/workflow/status/folknor/pbfhogg/ci.yml`
  (requires GitHub Actions CI workflow)
- [ ] Add GitHub Actions CI - clippy, tests, rustfmt, doc build on Linux
  (`.github/workflows/` has only `deploy.yml` + `docs.yml` today; no
  clippy/test workflow)
- [ ] Add GitHub Actions release pipeline - build binaries on tag push, attach to GitHub release
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

- [ ] **Global allocator investigation** - jemalloc and mimalloc were
  previously benchmarked at <1% wall time difference on Denmark (483 MB)
  and removed as CLI features (they broke `--all-features` builds due to
  duplicate `#[global_allocator]` definitions). Re-investigate at planet
  scale where allocator behavior under cross-thread free patterns and
  high churn may differ. Meta/Facebook has restarted active jemalloc
  development - revisit `tikv-jemallocator` and `mimalloc` when the
  arena/scratch work is complete and the remaining alloc profile is
  clearer. Measure RSS and wall time on planet add-locations-to-ways,
  merge, and build-geocode-index.
    - **jemalloc 5.3.1 (released 2026-04)** - wait for `tikv-jemallocator`
      to tag a release pointing at 5.3.1, then rerun the bench.
      Specifically relevant to the pipelined reader's cross-thread free
      pattern (`src/read/pipeline.rs:70` - decode workers allocate
      `PrimitiveBlock`s dropped on the consumer thread, the exact reason
      the prior jemalloc bench only saved RSS and not wall time):
        - tcache for deallocation-only threads (most on-point)
        - locality-aware tcache GC (`experimental_tcache_gc`, default on)
        - `calloc_madvise_threshold`, `process_madvise_max_batch`,
          `tcache_ncached_max` for ~MB-sized block allocations
      Check tikv-jemallocator releases; when 5.3.1 lands, run planet read
      + ALTW external + merge.

- [ ] **1. Custom allocators (per-block arena)** - 4/6 reviewers ranked 1st.
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

- [ ] **Geocode pass 3 stage A par_iter** - per-way `Vec::new()` inside
  `flat_map_iter` closure (`builder.rs` ~line 1226). Hard to fix due to
  parallel iterator ownership semantics. `SmallVec` could avoid heap
  allocation for ways with few segments. Low priority.

- [ ] **Per-relation members_scratch** - 14M relations × ~10 members ×
  24 bytes = 3.4 GB cumulative at planet. All allocator fast-path, no
  RSS impact. Skipped during v0.1 review (4 planet reviewers: not worth
  the API complexity). Revisit only if allocator profiling shows it
  matters after arena/columnar work. Shape of the fix (for when /
  if it's ever needed): change `BlockBuilder::add_relation` from
  `members: &[MemberData<'_>]` to `impl IntoIterator<Item = MemberData<'_>>`,
  add three parallel packed scratches on the builder
  (`member_roles_scratch`, `member_ids_scratch`, `member_types_scratch`)
  so the single-pass iteration can write all three protobuf member
  fields without re-scanning. Most callers already reuse a buffer
  (`members_buf.clear(); members_buf.extend(...)`) so the saving is
  small; the concentrated win is `apply-changes`
  (`rewrite_block.rs`, `element_writes.rs`) which builds fresh
  `Vec<MemberData>` per relation from OSC input with no reuse.

- [x] **2. Columnar batch processing** - shipped for extract node
  classification. `DenseNodeColumns` decodes IDs/lats/lons into
  contiguous arrays. `collect_matching_ids_multi_bbox` does single-pass
  N-region bbox test. Used in multi-extract and single-extract.
  Measured: multi-extract Japan node classify 1081ms → 748ms (-31%).
  Record (including the IdSet 29x accumulation pin and the per-path
  planet-safety analysis) consolidated in
  [reference/performance-history.md](reference/performance-history.md)
  "Parallel classify" (the `columnar-integration.md` note was deleted
  2026-07-13).

- [x] **Smart-extract planet memory blocker** - CLOSED 2026-04-11,
  ship as-is. The investigation shipped a 29% wall improvement on
  Europe smart extract (254s -> 181s), plus complete -17% and simple
  -15% via the same PASS1 schedule reuse (`0b085b1`). Planet: 279s
  wall / 11.17 GB peak anon (bbox-sized PASS3 write work dominates,
  not PASS1 scan). Mechanism: cold-arena-page residency cascade,
  fixed by plumbing the PASS1 schedule forward so PASS2/PASS3 don't
  rescan. **Caveat: measured with Europe bbox** - a substantially
  larger bbox would grow PASS3's working set and could push peak
  anon higher; re-measure if extract-on-planet with a bbox beyond
  Europe scale becomes a recurring operation.

**Milestone B: vectorization (after columnar layout stabilizes)**

- [ ] **3. SIMD** - universal agreement: comes after columnar. Columnar
  now shipped for extract (single + multi-region). ASM inspection
  confirms LLVM does NOT autovectorize the bbox classify loop - the
  `push()` side effect prevents vectorization entirely.

  **Codegen finding:** explicit AVX2 intrinsics are the only path.
  The multi-bbox loop is a better SIMD target than single-bbox: N
  region tests per node amortizes setup (N=5 with AVX2 8-wide ≈ 1.6
  nodes of all 5 tests per vector op). Single-bbox is only 2.8% of
  total Europe extract time - not worth it alone.

  SIMD becomes worthwhile when:
  - The classify loop is a larger fraction of runtime (after write-path
    optimization makes classify the bottleneck)
  - Multiple consumers use columnar arrays (multi-region, polygon PIP)
  - Batch varint decode in protohoggr (different SIMD target, broader
    impact across all commands)

  Varint SIMD research (notes/SIMD.md) previously closed - scalar beats
  SIMD for individual LEB128 varints. Batch varint decode into contiguous
  arrays is a different problem (columnar enables this).

**Milestone C: hardware-level tuning (where perf counters justify it)**

- [ ] **4. Huge pages** - `MAP_HUGETLB` (2 MB pages) for large mmap'd
  structures. Sparse ALTW rank-flat values file (japan 2 GB, europe
  ~29 GB), geocode index mmap reader, external join temp files. 5-15%
  speedup for random-access patterns. Requires hugepage availability
  (`sysctl` config) or `madvise(MADV_HUGEPAGE)` for THP. Linux-only.

- [ ] **5. NUMA-aware memory placement** - last by unanimous agreement
  (6/6). Only matters on multi-socket servers. Current benchmark host
  (plantasjen) is single-socket. Pread-from-workers pattern already has
  natural NUMA affinity (thread-local allocations, first-touch policy).
  `set_mempolicy(MPOL_BIND)` / `mbind()` for explicit placement.
  Candidates: pipelined reader decode pool, sparse ALTW rank-flat
  interleave, external join scatter buffers. 10-20% on dual-socket,
  0% on single-socket. Requires per-host tuning and NUMA hardware to
  validate.

**Separate track (GPU, independent of milestones A-C):**

- [ ] **GPU-accelerated point-in-polygon for geocode builder** - Pass 2
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
- [ ] Incremental extract update (`extract --apply-changes` - base extract + OSC +
  region → updated extract without re-reading planet).
  Recommended: compose two existing commands - `apply-changes` on
  the region extract (current apply-changes already tolerates OSC ops
  referencing elements outside the region; see reference/osmium-parity.md), then
  `extract` to re-filter to the bbox. ~10s vs 862s
  for the full-planet pipeline. Works for simple strategy immediately.
  Complete/smart strategies need planet access for newly referenced
  elements outside the bbox.
- [ ] Spatial indexing in PBF format (R-tree over blob offsets for
  O(log N) spatial queries on planet files).
  (The two research notes, `spatial-index-in-pbf.md` and
  `way-blob-bbox-speculation.md`, were deleted 2026-07-13 - speculative
  only, conclusions retained here, full analysis in git history.)
  Node blob header scan is already fast (~0.5s planet). Way blob spatial
  bboxes are limited by chronological ID ordering (~30% skip for Denmark,
  not 50-80%). Geography-sorted way blobs (Hilbert curve) would give
  90%+ skip but breaks Sort.Type_then_ID. Multi-extract benefits most.
- [x] Streaming pipeline composition - CLOSED, limited benefit.
  The codebase already does the most valuable composition (inline
  indexdata in all write paths). Multi-pass commands can't consume
  streams. (Note deleted 2026-07-13; analysis in git history.)
- [x] ~~Dense ALTW compact rank-indexed array~~ - CLOSED 2026-04-30
  (`c6f08ff` + `b70dd8c`). The proposed rank-indexed layout landed as
  the new sparse encoding, dominating dense at every measured scale
  (japan 4.3x faster, europe survives where dense OOMs); dense
  removed entirely.
- [ ] Verify GeoJSON polygon format coverage for extract (does `--polygon`
  accept GeoJSON, or only .poly format?).
- [x] ~~History-file support - decide in-scope or explicitly
  out-of-scope.~~ DECIDED 2026-07-13: explicitly OUT OF SCOPE for 1.0,
  code kept, best-effort thereafter; see
  [notes/time-filter-optimization.md](notes/time-filter-optimization.md).

## Completed record

Resolved items moved out of the open sections above (2026-07-13
restructure), full text preserved. Nothing here is actionable; it
exists so measured numbers, commit hashes, and pins against
re-attempting stay greppable in one place.

### Item-level completions

- [x] ~~**`sort` correctness hole: intra-blob disorder is invisible**~~ **FIXED 2026-07-11.** Pass 1 now tracks intra-blob monotonicity (`scan_block_ids_checked`) keyed on the header's `Sort.Type_then_ID` claim, not indexdata presence, and routes any out-of-order blob into pass 2's decode + re-encode path. Recorded in CORRECTNESS.md; tests in `tests/cli_sort.rs`.

- [x] ~~**`read` parallel variant OOM**~~ **FIXED 2026-07-11.** Rewritten as a one-pass byte-and-count-bounded worker-resident parallel fold (256 MiB in-flight budget). Planet: SIGKILL -> 54 s / 616 MB peak anon, exact element-count parity with sequential. Zero production consumers of `par_map_reduce` existed, so no planet-safe pipeline was touched. Full numbers and the fadvise follow-up aside are in [reference/performance-history.md](reference/performance-history.md) "Env-gated read-path batch".

**Read-path follow-ups from the 2026-07-11 architecture reports** - outcomes settled in [reference/performance-history.md](reference/performance-history.md) "Env-gated read-path batch" (full detail there; verbatim reports live in git history). Summary: `BlobHeader.datasize` validation FIXED (`MAX_BLOB_DATASIZE` bound rejects oversized declared sizes pre-allocation, CORRECTNESS.md); ordered-pipeline batch rebuild BUILT then REVERTED (`aabb696`, no isolated win); command-transform fusion into decode workers KEPT ([ADR-0009](decisions/0009-fused-command-transforms.md), `aabb696`, -6.5 to -7.7% at 8k across getid/getparents/tags-filter); sequential-path double copy FIXED (`3ccc580`, owned-Vec scratch constructors, no regression across 16 cells); count-only buffer knobs BUILT env-gated then REVERTED (`629d9ca`, no knob improved anything).

- [x] ~~**`diff --format osc` metadata fidelity**~~ **FIXED 2026-07-10.** OSC writer emitted raw newline/tab/CR inside XML attribute values, silently corrupting multi-line tags on apply (osmium parity fix: `push_attribute_escaped` in `src/osc/write.rs` now emits `&#10;`/`&#13;`/`&#9;`). Roundtrip regression tests added.

- [x] ~~**apply-changes drops OSC element metadata**~~ **FIXED 2026-07-10.** `CompactDiffOverlay` gained a per-record metadata block (version/timestamp/changeset/uid/user); all apply-changes write paths pass it through, and `diff --format osc` now emits the full metadata set (was version-only) - the derive -> apply circle is metadata-lossless end-to-end. Note: Geofabrik public diffs strip changeset/uid/user (GDPR) - that's source data, not loss. Pinned by `derive_then_apply_preserves_metadata`.

**Open decision on `getparents`** - **RESOLVED 2026-07-11**: threshold-dispatch on blob count landed for both `getparents` and `getid` (150k-blob threshold, bounded header-probe estimator; see [`ADR-0006`](decisions/0006-blob-count-threshold-dispatch.md)). Matrix in [notes/getparents.md](notes/getparents.md) "Crossover measured".

- [x] ~~**altw-as-renumber (in-RAM coord-table thesis)**~~ **EXPERIMENT FAILED (2026-04-16)** - OOM-killed at Europe (real planet unique-referenced ~10B / ~80GB vs 2B/16GB estimate). Disproven for Europe+; the 4-stage external-sort shape is load-bearing. Post-mortem in [notes/altw-optimization-history.md](notes/altw-optimization-history.md). Active work moved to [notes/altw-external.md](notes/altw-external.md).

- [x] ~~**check --refs**~~ - landed 2026-04-17. Planet 1225 s -> 53.8 s (22.8x, peak RSS 2.17 GB). Selective wire-format parser (step #3) was rejected: post-parallel split is ~162 s decompress vs ~2 s parse at Europe, so the next lever is decompression throughput, not selective parse. Pin in `src/commands/check/refs.rs::check_refs` doc comment.

- [x] ~~**getid include mode**~~ - landed 2026-04-20 via the shared `pread`-only `HeaderWalker` primitive (`src/read/header_walker.rs`). Planet 43.7 s -> 6.1 s (7.2x), disk read 88 GB -> 601 MB. Walker is syscall-bound (io_uring batching not pursued). 2026-07-10 blob-density follow-up found HW regresses on high-blob-count encodings; threshold dispatch recorded in [`ADR-0006`](decisions/0006-blob-count-threshold-dispatch.md).

- [x] ~~**diff-snapshots (text and `--format osc`)**~~ - landed 2026-04-20. ID-range sharded parallel block-pair merge, `-j/--jobs N` flag: planet text 2134 s -> 227.5 s (9.5x, `-j 16`), osc 2225 s -> 293.8 s (7.6x). Both beat the 8-min aspirational target. Remaining follow-up: auto-enable parallel by default.

### 1.0 blockers, per-item record (all resolved 2026-04-27 / 2026-04-28)

- [x] ~~**`check --ids` (streaming default mode)**~~ **fixed 2026-04-27**
  (`516129e`) - rewrote to `parallel_classify_phase`. Planet: SIGKILL
  at 29.2 GB -> 57 s wall / 504 MB peak anon.
- [x] ~~**`cat --clean`**~~ **fixed 2026-04-27** (`6184602` + `b347c0a`)
  - rewrote to `parallel_classify_phase` per kind with `ReorderBuffer`
  streaming output. Planet: SIGKILL at 28.9 GB -> 5m34s wall / 750 MB
  peak anon (38x RSS reduction). Output stays type-sorted (nodes,
  ways, relations).
- [x] ~~**`time-filter`**~~ **LANDED 2026-04-28** (`83183fb`) - migrated
  to `parallel_classify_phase` + `ReorderBuffer`, mirroring the
  `cat --clean`/`check --ids` precedents. Planet: SIGKILL at ~28 GB
  -> 4m30s wall / 812 MB peak anon. Instrumentation confirmed the
  ~28 GB working set was structural to the old parallel-decode +
  batch-collect architecture, not buffer retention. Plan doc:
  [notes/time-filter-optimization.md](notes/time-filter-optimization.md).
- [x] ~~**`tags-filter --invert-match w/highway=primary`**~~
  **LANDED 2026-04-28** (`17b116c`). The 28.3 GB peak was
  misattributed to pass 2 (which only peaks at ~7 GB - it already
  used the right shape). Actual culprit: `collect_way_node_dependencies`
  used `parallel_classify_accumulate` with a per-worker `IdSet` sized
  by node-ID-space (~1.5 GB) x ~30 decode threads. Migrated to
  `parallel_classify_phase` (workers emit bounded per-blob `Vec<i64>`,
  consumer merges into one shared IdSet). Planet: 24 GB -> 6.97 GB
  peak anon (-71%), wall unchanged. `collect_relation_member_closure`
  was NOT migrated (borrow conflict: merge needs `&mut`, classify
  needs `&`) - pinned in a comment in the function. This is the
  origin of the "shape != root cause" lesson recorded under "Latent
  same-shape risks" above.
