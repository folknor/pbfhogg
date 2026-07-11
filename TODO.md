# pbfhogg TODO

## Item/command-specific plans

Three new docs capturing a cross-cutting insight and two new commands that fall out of it. All scaffolding-level; details drift as work lands.

- [ ] **[reference/blob-density.md](reference/blob-density.md)** - the insight: Geofabrik-style PBFs (~8k elements/blob, ~522 k blobs on europe) scale very differently from `planet.openstreetmap.org`-style PBFs (~300k elements/blob, ~50 k blobs on planet). Every `HeaderWalker`-based command (`sort`, `getid`, `getparents`, `inspect`, `apply-changes::scanner`, `check --refs`, `extract --smart`, `tags-filter`, `build-geocode-index`, `renumber_external`) has an implicit blob-count scaling dependency silently shaped by the encoder on the producer side. README's "Planet scale" table and all `notes/*.md` "N seconds at planet" predictions are measured on the sparse-blob encoding. Needs same-corpus-different-encoding measurements once `repack` exists.

- [x] ~~**[notes/repack.md](notes/repack.md)** - new command: re-encode a PBF with a configurable `--elements-per-blob N` cap.~~ **v1 + v2.1 LANDED.** Per-kind parallel scan mirrors `cat --clean`; `BlockBuilder::with_element_cap(n)` is the cap-plumbing primitive; CLI flags `--elements-per-blob`, `--compression`, `--direct-io`, `--io-uring`, `--force`. v2.1 added cross-input-blob coalescing (hybrid worker/central-builder shape), so grow caps fire correctly across input-blob boundaries. Planet 8 k bench validated twice: 380 s / 1.36 GB (UUID `0ae01c09`, `48685ba`, 2026-04-28) and 389.6 s / 1.31 GB (UUID `a4791ddc`, `8c1cf03`, 2026-07-10), both plantasjen. v2.2 (LocationsOnWays preservation) and v2.3 (osmium cross-validation) remain deferred - see the note.

  **v1.1 follow-ups (small, no benchmarking):**
  - [x] ~~**Cap-message parity for `BlockBuilder::with_element_cap`.**~~ Landed: assertion text is now `"BlockBuilder::with_element_cap: --elements-per-blob must be > 0"`, sharing the substring `"--elements-per-blob must be > 0"` with the CLI Err for grep parity.
  - [x] ~~**Warn on no-op grow cap.**~~ Landed: `run_kind_phase` returns a `cap_fired` flag (true whenever a worker emits >1 output blob from one input blob). `repack()` aggregates across phases and emits a stderr warning at end-of-run when the cap never fires and at least one element was written. Tier-1 tests in `tests/cli_repack.rs` cover both directions (warning fires on cap=8000 vs ~20-element input blobs, does NOT fire on cap=10 shrink).
  - [ ] **`ElemKind` reuse audit (cross-command, low priority).** Both `cat --clean` (`src/commands/cat/mod.rs:433-435`) and `repack` (`src/commands/repack/mod.rs:35-37`) invented their own bare-`u8` `KIND_NODE/WAY/RELATION` constants instead of reusing the project's `pub(crate) enum ElemKind` (`src/blob_meta/mod.rs:20`). Repack mirrors cat by design - changing one without the other would just diverge them. Worth doing as one refactor across both commands when someone is in there for another reason; not worth a dedicated PR.

  **Release / measurement follow-ups:**
  - [x] ~~**Register the 8 k-packed planet as a snapshot.**~~ Done 2026-07-10: UUID `8027765b` (377.5 s at `8c1cf03`, plantasjen) promoted its output to `data/planet-8k-with-indexdata.osm.pbf`, registered as `snapshot.8k` `pbf.indexed`. The two earlier bench runs (`0ae01c09`, `a4791ddc`) wrote to scratch and kept nothing. The snapshot is the input for every same-corpus-different-encoding pair the `reference/blob-density.md` matrix needs, and for the deferred `getparents` HeaderWalker dispatch decision (europe-regressing / planet-winning along the same blob-count axis); consumer commands reach it via `--snapshot 8k`.

- [ ] **[notes/degrade.md](notes/degrade.md)** - adversarial-test tool, v1 shipped (`--unsort`, `--strip-locations`, `--strip-indexdata`; deferred: `--strip-tagdata`, `--strip-bbox`, `--recompress`, `--drop-ids`). **BUG found 2026-07-10** (note's "Known bugs" section): `--unsort` produces an intra-blob inversion instead of the documented cross-blob overlap - the merge loop's per-input-blob sort-preservation flush means the central builder never spans input blobs, so the cap-keyed swap lands inside one blob. Consequence: `sort`'s overlap-rewrite path has STILL never been exercised on real unsorted data (`verify sort --snapshot unsorted` run `f5cd6522`: 0 overlaps detected, full passthrough). Fix: suppress the boundary flush under `--unsort` (or key the swap to output-blob boundaries); keep the accidental intra-blob shape as a deliberate second flag (`--unsort-intra`) since it exposed the sort blind spot below. After the fix: re-run `degrade --unsort --as-snapshot unsorted --replace-snapshot` + `verify sort --snapshot unsorted` as the overlap-rewrite correctness gate.

- [ ] **`sort` correctness hole: intra-blob disorder is invisible** (see [notes/sort.md](notes/sort.md) "Correctness finding", found 2026-07-10 via the degrade bug above). Blobs internally unsorted but with non-overlapping ranges pass straight through and the output header claims `Sort.Type_then_ID` - silent corruption of the sorted invariant. Decision needed: add a ~free monotonicity check to the non-indexed pass-1 fallback (it already decodes payloads; covers real third-party unsorted files), and for indexed inputs either document intra-blob sortedness as a producer precondition or add opt-in `--verify-blobs`. Record the outcome in CORRECTNESS.md.

- [ ] **`read` parallel variant OOM on high-blob-count encoding** (found 2026-07-10): `brokkr read --dataset planet --snapshot 8k --bench 1` - the parallel variant was killed by signal on the 1.45 M-blob encoding while surviving primary planet (50.8 k blobs); 3 of 4 variants completed. Per-blob memory accumulation implicated. Synthetic bench surface, but the underlying parallel read path is library code - instrument before assuming shape (tags-filter lesson). Evidence in `reference/blob-density.md` "Measured evidence".

- [x] ~~**`diff --format osc` metadata fidelity - joint look with brokkr dev**~~ - **RESOLVED 2026-07-10, two distinct pbfhogg bugs found.** The failing roundtrip nodes were the multi-line-`inscription` memorials: (A) the OSC writer emitted raw newline/tab/CR inside XML attribute values; XML attribute-value normalization turns those into spaces at parse time, so applying a derived OSC silently corrupted multi-line tag values (byte-proven: `new` had `%0a%`, roundtrip had `%20%` via osmium OPL dump of n5900269010). **Fixed same day**: `push_attribute_escaped` in `src/osc/write.rs` emits `&#10;`/`&#13;`/`&#9;` character references (osmium parity), applied to tag k/v, member roles, and user names, with write->parse roundtrip regression tests. The `v0` in the verify printout was a red herring for the roundtrip failure but exposed bug B below.

- [x] ~~**apply-changes drops OSC element metadata**~~ - **FIXED 2026-07-10, same day as found.** `CompactDiffOverlay` gained a fixed 29-byte metadata block per arena record (`flags`/`version`/`timestamp`/`changeset`/`uid`/interned `user`); the OSC parser reads the attributes (lenient, single pass, RFC 3339 parse via the library-ified `parse_rfc3339_utc`); all nine apply-changes write paths pass the metadata through. Companion fix: `diff --format osc` now emits the full metadata set (was version-only), so the derive -> apply circle is metadata-lossless end-to-end - pinned by `derive_then_apply_preserves_metadata` (tier 1) plus the updated `merge_metadata_preservation` (which had pinned the OLD lossy behavior as expected). Byte-proven on real data: n5900269010 in the verify-recreated `new` now carries `v2 t2026-02-20T21:39:49Z` matching the source OSC. Note: Geofabrik public diffs strip changeset/uid/user (GDPR), so those stay 0/empty on OSC-sourced elements with public-diff input - that is source data, not loss.

**Open decision on `getparents`** (see [notes/getparents.md](notes/getparents.md) "Crossover measured"): the 8k-packed planet matrix landed 2026-07-10. Three cells: planet primary HW **23.5 s** vs scan 44.8 s; europe HW 44.2 s vs scan **26.4 s**; planet-8k HW 82.7 s (`425d1f1e`; walk 64.8 s at ~45 us/blob, decode 17.8 s) vs scan **52.8 s** (`2b3e496e` via `--commit 68e1ba0`). HeaderWalker loses on BOTH high-blob-count encodings; walk cost is linear in blob count, decode is encoding-invariant. Recommendation in the note: **threshold-dispatch** on blob count (crossover bracket 51 k-522 k; pick ~150-250 k). Awaiting ratification, then implementation.

- [ ] **[notes/sort.md](notes/sort.md)** - `sort` (repair unsorted PBFs into `Sort.Type_then_ID`). Drafted 2026-04-23. **Production reality**: Geofabrik / planet input is already sorted, so the overlap-count is ~zero and pass 2 is pure raw passthrough. The headline opportunity that helps the production case is **`copy_file_range` coalescing for passthrough runs** (hours-scope, transplant from apply-changes drain, 1.1-1.5x via syscall reduction). The bigger theoretical wins - parallel overlap-rewrite in pass 2 (1.5-3x) and HeaderWalker-based pass 1 (1.2-2x on non-indexed input) - only fire on genuinely-unsorted input, which has no dataset configured in `brokkr.toml` today. Planet hotpath + alloc captured 2026-04-27 overnight at `4fc8e35` (UUIDs `d64932d2` hotpath / `26fb329e` alloc): 115.4 s wall, **94 % in `pbfhogg::write::writer::flush`** (108.6 s) and 6 % in `build_blob_index` (6.77 s) - reaffirms the writer-side `copy_file_range` ceiling is the only lever for already-sorted input, with no allocation pressure (459 MB exclusive, all in `blob_wire::parse`). Hotpath wall sits below both the 124.6 s `68e1ba0` and 132.3 s `16e3694` bench baselines, softening the `+6-7 %` regression flag tracked in `reference/performance.md`. Anti-conversion rule (pipelined → sequential) explicitly off the table per `reference/pipelined-reader-paths.md:138`.

- [ ] **[notes/getparents.md](notes/getparents.md)** - `getparents` (whole-file scan listing ways / relations referencing a given ID set). Drafted 2026-04-23, headline experiment landed 2026-04-24 (`783970a`). The HeaderWalker + `parallel_classify_phase` rewrite shipped: planet 44.8 s -> **23.5 s** (-46 %, UUID `11bc44dc` at `16e3694`), europe 26.4 s -> **44.2 s** (+68 %, blob-density asymmetry - see [reference/blob-density.md](reference/blob-density.md)). Original 4-8x estimate was wrong: blob indexdata stores `(min_id, max_id)` of *elements in the blob*, not the *ref/member IDs* the typical "find ways referencing these nodes" query cares about, so `IdSet::any_in_range()` pre-screen does not apply. Actual win comes from IO byte reduction (74.8 GB -> 30 GB at planet) by skipping blob kinds structurally incapable of producing matches. Planet hotpath at `4fc8e35` (UUID `00253c7d`): 23.0 s wall, 78 % in `parallel_classify_phase` - that **is** the post-experiment state, not headroom. The c912e4d Denmark 4.7x sequential-decode regression rule remains explicitly off the table (it targets sequential-decode conversions; `parallel_classify_phase` keeps decompression parallel via pread workers). **Open question is the europe regression**: revert / threshold-dispatch / accept - the crossover was measured 2026-07-10 on the 8k-packed planet (HW 82.7 s vs scan 52.8 s; walk ~45 us/blob, linear in blob count) and the note now recommends threshold-dispatch on blob count; see the "Open decision" paragraph above for the full matrix. Smaller residual opportunities in `notes/getparents.md`: blob-filter skip-rate verification (#3, hours scope), refs/members buf pre-sizing (#4, <1 % wall).

- [x] ~~**altw-as-renumber (in-RAM coord-table thesis)**~~ - **EXPERIMENT FAILED (2026-04-16).** Implemented as `src/commands/altw_v2.rs`, OOM-killed at Europe. Measured unique-referenced count was 3.6 B → 29 GB coord table (plan estimated 2 B / 16 GB at planet; real planet ~10 B / ~80 GB). The in-RAM-coord-table thesis is disproven for Europe+; the existing 4-stage external-sort shape is load-bearing and correct. Post-mortem and numbers now live in [notes/altw-optimization-history.md](notes/altw-optimization-history.md). **Active ALTW work moves to** [notes/altw-external.md](notes/altw-external.md) **(live leads).**

- [x] ~~**[notes/geocode-build-opportunities.md](notes/geocode-build-opportunities.md)**~~ - `build-geocode-index`. **ARC LANDED 2026-04-18.** Planet 1,255 s (20.9 min, TAINTED baseline) -> **432.9 s (7m12s)**, -65 % / 2.9x. Pass 1.5 peak anon 29.5 GB -> 3.0 GB (-90 %); governing peak migrated to Pass 3 Stage B at ~25 GB, comfortable on 27 GB hosts. All 10 ranked items (#1 Phase 2a+2b, #2, #3, #4, #5, #6, #7, #8, plus header-walk consolidation and direct coord_mmap writes) shipped. Remaining follow-ups in the note: #4 "needs another pass" (fused Stage A delivered only 2.8 s at Europe vs the 40-60 s planet prediction), Pass 2 interp resolve still sequential at 30.6 s planet, interpolation endpoint CSR for RSS hygiene.
  - [ ] **Spike: exact S2 segment coverage for Pass 3.** `cover_segment` currently samples intermediate lat/lon points and clamps the walk at 256 steps. Prototype a proper S2 edge/cell traversal using `s2` 0.1.0 primitives (`RegionCoverer`, `Cell`, `Point`, `edgeutil::simple_crossing`, or equivalent direct cell-edge tests), then compare fine/coarse cell counts and geocode query results against the sampling path on Denmark/Europe before replacing it.
  - [ ] **Spike: admin interior hints via S2 region coverage.** Admin indexing currently edge-covers rings and flood-fills from an arithmetic exterior vertex mean; concave polygons whose mean falls outside skip interior hints and pay more query-time PIP checks. Prototype a polygon `Region` or other `RegionCoverer::interior_covering` path that preserves the current edge-vs-interior on-disk semantics, handles holes, and measure admin cell count/PIP-hit changes before any format or behavior change.

- [x] ~~**check --refs**~~ - landed 2026-04-17 across commits `8f0ccbb` (step #1: `RoaringTreemap` → `IdSetDense`), `053def6` + `fbf591c` (step #2: three-phase parallel scan + one-pass schedule walk). Japan 56.7 s → **2.1 s** (27×). Europe 426.2 s → **33.6 s** (12.7×). Planet **1225 s → 53.8 s** (22.8×, UUID `7d9f5dfd` at commit `16e3694`, 2026-04-26; previously 72.5 s at `862547e4`), ~5-8× better than the 6-10 min plan floor. Peak RSS 2.17 GB. Step #3 (selective wire-format parser) was predicated on decompression and parse landing roughly co-equal; actual post-parallel split is ~162 s decompress vs ~2 s parse at Europe, putting the selective-parser ceiling at fractions of a second - so the next lever for check-refs perf is decompression throughput (zstd, io_uring, direct I/O), not selective parse. Load-bearing pin in `src/commands/check/refs.rs::check_refs` doc comment. Plan doc retired.

- [ ] **[notes/apply-changes-opportunities.md](notes/apply-changes-opportunities.md)** - `apply-changes --locations-on-ways`. **P1 + P1.5 landed 2026-04-21 (`719f306`)**; parallel writer made the default 2026-04-21 (buffered path removed, `--parallel-writer` flag deleted). **Planet best: 80.9 s cross-disk + zstd:1** (-44 % vs 144.4 s pre-flip baseline; parallel pwrite, unaffected by the CopyRange bug). Same-disk zstd:1 best: **104.5 s** with parallel pwrite. The same-disk `--io-uring` column was re-measured 2026-04-26 at `16e3694` after the `fa8251d` CopyRange fix and is now uniformly slower than parallel pwrite at every same-disk compression level (none 137.5 s, zlib:6 137.4 s, zstd:1 126.3 s; UUIDs `9a5c25a7` / `70e5414b` / `0e6a5918`); the original 108.6 / 137.1 / 99.4 s numbers were tainted by the writer dropping a zero-page between OSMHeader and first OSMData blob. Cross-disk `--io-uring` rows (93.0 / 127.9 / 82.8 s) still need re-measurement on the fixed writer. Same-disk `--io-uring` no longer the recommended override; cross-disk `--compression none` + `--io-uring` is open until re-measured. Remaining open items: splice-in-place (#11, deferred - doesn't reduce output bytes on compressed output), multi-file output / RAID-0 (unlanded and lower priority given 80.9 s is comfortably inside any realistic production budget).

- [x] ~~**getid include mode**~~ - landed 2026-04-20 via a shared `pread`-only `HeaderWalker` primitive (`src/read/header_walker.rs`). Planet **43.7 s → 6.1 s (7.2×, UUID `24362e36`)**, germany 200 ms, disk read 88 GB → 601 MB. Initial HeaderWalker landing hit 7.0 s with two preads per blob; the follow-up 1-pread probe walker (commit `d263d76`) trimmed a further 0.9 s (-13 %). Walker is syscall-bound; going lower would need io_uring batching - not pursued. Plan doc retired. **2026-07-10 blob-density follow-up**: full three-cell matrix measured - europe HW 40.2 s (`57ffbf49`) vs scan 17.9 s (`bc96d15d`), planet-8k HW 102.6 s (`aa5bc158`) vs scan 33.2 s (`c0d89d8f`); HW regresses +125 % / +209 % on high-blob-count encodings while the scan arm actually IMPROVES with density (indexdata prescreen selectivity). The resulting threshold dispatch is recorded in [`ADR-0006`](decisions/0006-blob-count-threshold-dispatch.md); sort pass 1 remains a separate follow-on.

- [x] ~~**diff-snapshots (text and `--format osc`)**~~ - landed 2026-04-20. Planet baselines: text **2134 s / 35m34s**, osc **2225 s / 37m06s**. ID-range sharded parallel block-pair merge for both paths: text planet **227.5 s / 3m48s at `-j 16` (UUID `22a5eb55`, 9.5× speedup, temp-file shape, 586 MB peak anon)**, osc planet **293.8 s / 4m54s at `-j 16` (UUID `cdcaa4f1` at commit `16e3694`, 2026-04-26; 7.6× speedup, was 313.8 s at `9b3fc2b9` before the parallel `assemble_osc` gzip landed, 663 MB peak anon at the prior bench)**. CLI flag `-j/--jobs N` on `pbfhogg diff`. Germany text 16.5 s, germany osc 20.4 s at `-j 8`. Both paths now stream shard output to per-shard scratch temp files; an interim text-shape buffered each shard in a `Vec<u8>` (208.6 s, UUID `b02d86bc`, 2.29 GB peak anon) and was replaced with the temp-file shape for a 74 % RSS drop at a 10 % wall cost. Shard balance within 1.03× max/min. Both paths beat the 8-min aspirational target. Parallel `assemble_osc` (was the single-threaded gzip + concat of ~45 GB of XML fragments at 32.8 s) closed the OSC tail. Remaining follow-up: auto-enable parallel by default. Plan doc retired.

- [ ] **[notes/altw-external.md](notes/altw-external.md)** - `add-locations-to-ways --index-type external`. Current planet baseline: **546.0 s `--bench 1`** (UUID `7fd04130`, commit `16e3694`, 2026-04-26; was 603.7 s at `aa0dc719` post-A1, 661.2 s pre-A1) - **−115.2 s / −17.4 % vs pre-A1**, an extra **−9.6 %** since the post-A1 measurement attributable to commits between `0dc8ae1` and `16e3694`. Europe **270.8 s** post-A1 (was 291.6 s at `6d71053`). **Europe compression sweep landed 2026-04-27 overnight at `4fc8e35`** (`reference/performance.md` "Compression axis" subsection): `none` 246.8 s (UUID `16c35911`, ~6.5 GB anon), `zstd:1` **233.3 s** (UUID `e2fba1bf`, ~6.6 GB anon), vs the cross-commit zlib:6 reference 270.8 s at `0dc8ae1` - so zstd:1 is **−14 % vs default**, refreshing the stale 419→379 / −9.5 % claim from the older `f3c53a34`/`66e43a11` baselines. Same mechanism as before: relieves consumer/compression saturation in stage 4 with similar output size. A1 (rankless node-ID bucketed join) landed 2026-04-25 across 8 commits + 4 review fixups; pass B and the IdSet rank machinery deleted. Doc lists 20 live leads grouped by blocker (Tier 1 actionable now, Tier 2 speculative, Tier 3 hardware-gated, Tier 4 deep stretch) plus correctness invariants + implementation conventions. Failed attempts, measured numbers, physical floors, and meta-lessons live in [`notes/altw-optimization-history.md`](notes/altw-optimization-history.md). Dominant theme post-A1: the stage 2 → stage 3 → stage 4 disk-seam chain (now ~56 GB id shards instead of ~80 GB rank shards + ~112 GB slot buckets) and the new stage-2 sort cost (~87 s wall at planet, comparison-sort on `local_node_id`). Five ranked items landed earlier this sprint (#4 stage-2 de-ranking, #8 BlobLocationRouter, #9 L1+L2 relation scan, #2 streaming stage 3 → 4); remaining seam-shaped items are blocked on RAM (~25 GB host) or a faster second NVMe.

  **Apply-changes work that might transfer (speculative, 2026-04-21, no deep ALTW research):**
  - **Worker-emits-framed-bytes (P1.5 pattern).** If ALTW Stage 4 still dispatches framing via `rayon::spawn` per output block and funnels through `write_primitive_block_owned`, moving framing inline into the worker (call `frame_blob_pipelined` directly, ship the framed `Vec<u8>` to the writer thread via `write_raw_owned`) would save the same `writer_pipeline_send_wait_ns` we shaved in apply-changes (-86% at planet `--compression none`). Pattern transfers cleanly; trigger is whether that counter is large in ALTW.
  - **Cross-disk scratch (no code, pure config).** Apply-changes planet dropped 31% just by moving bench output to a different physical NVMe (single-NVMe read+write contention removed). Worth a single `brokkr.toml` edit + bench to see if ALTW's 661 s shows similar shape - if so, it's an immediate runtime recommendation rather than code work.
  - **`zstd:1` for internal pipelines.** Already documented in apply-changes plan doc and in the ALTW notes (`notes/altw-optimization-history.md` mentions `--compression zstd:1` Europe 419 s → 379 s, -9.5%). Confirmed the same mechanism in apply-changes (workers parallelize zstd cheaply; smaller bytes → less writer wall). Should lift to ALTW Stage 4's writer config without changes.

  **Probably doesn't transfer:**
  - **Descriptor-first scanner + drain shape.** ALTW external is multi-pass external-sort; the design premise (reader/classify/rewrite in one pass) doesn't match.
  - **Node→way barrier + coord fusion.** Apply-changes-specific (coords needed mid-run to resolve OSC way refs); ALTW's whole job IS the coord scatter.
  - **BTreeMap seq reorder buffer at drain.** ALTW Stage 4 has its own output ordering.

  **Already shared:**
  - **HeaderWalker** (scan-audit round migrated both commands).
  - **`copy_file_range` coalescing** (apply-changes drain *ported from* `altw/passthrough.rs`, so the flow direction already ran).
  - **`IdSetDense::set_atomic_if_new`** primitive (used by both for parallel set-membership).

  Rough prioritization if a day were available: cross-disk bench first (10 min, tells us where the ALTW ceiling actually lives), then worker-framed-bytes if Stage 4 is writer-bound.

- [ ] **[notes/injected-prepass.md](notes/injected-prepass.md)** - cross-repo contract from elivagar's `injected-prepass-spec.md` (their H2a+H2b): altw computes relation membership and exact shared-node pins at enrichment time and injects them as BlobHeader field 5 (way-member bitmap, superset semantics, presence = validity) + Way field 20 (per-ref pin bitmap, omitted when empty), declared via `pbfhogg.WayMembers-v1` / `pbfhogg.SharedNodePins-v1` optional_features. Public surface: `Blob::way_members()`, `Way::shared_node_pins()`, opt-in `parse_waymembers` toggle on `BlobReader::new`. Survey landed 2026-07-09; the note restates the full contract (self-contained, no reach into the elivagar repo), maps it onto the post-A1 external pipeline (relation-scan fusion at ~zero wall, stage-2 run-length shared bit, two zero-widening escapes that keep both 12-byte records at 12 bytes), and prices it against the standing **external <= 3% regression bound** (~16 s at planet, baseline 546.0 s `7fd04130`). Decisions 1 and 2 RATIFIED 2026-07-10: (1) steady state is option (a) - altw moves into the daily refresh loop after each merge, the production merge drops `--locations-on-ways`, both injected fields stay fresh every cycle (recorded in `reference/pipeline.md`, rewritten the same day for the new loop shape plus general staleness); (2) injection is opt-in flag-gated (working name `--inject-prepass`) with sparse parity to keep the backend-parity canary. The paired spec now waits only on (3) elivagar's Brick 1 superset screen (superset <= 1.5x needed_ways on germany locations) - if it fails, the membership semantics gain a `tag_expr` relation filter. Sequencing: the decode-backpressure fix landed and was validated 2026-07-10 (commit `a0a2e3b`; verdict read, all gates kept), so Brick 2 baselines are clean. Risk-free early slice available now: the format/reader layer (field-5 parse/encode, accessors, toggle) is invariant under the remaining gate.

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
planet-safe" table is now empty.

- [x] ~~**`check --ids` (streaming default mode)**~~ - **fixed
  2026-04-27** (commit `516129e`). Rewrote streaming entry to use
  `parallel_classify_phase` mirroring the co-located `--full` mode,
  minus the IdSet population. Re-bench: planet **57 s wall, 504 MB
  peak anon** (UUID `02595428`) vs the failed 26 s SIGKILL at
  29.2 GB. Now the lowest peak RSS of any `check --ids` variant on
  planet (`--full` is 2.17 GB). Behavior change on non-indexed
  input: element-level type-order detection (mixed-type-blob
  ordering) dropped, matching `--full`'s existing semantics on
  non-indexed input.
- [x] ~~**`cat --clean`**~~ - **fixed 2026-04-27** (commits
  `6184602` + `b347c0a`). Rewrote `cat_filtered` to use
  `parallel_classify_phase` per kind (mirroring the verify_ids
  template) with the per-blob framed output streamed via
  `ReorderBuffer` so that planet-scale phase output isn't
  accumulated up-front (the first cut accumulated all framed
  blobs before writing - 47K node blobs × ~1.6 MB framed each
  ≈ 75 GB ceiling - and OOM'd at 28.9 GB on its own). Re-bench at
  `4fc8e35`: planet **5m34s wall, 750 MB peak anon** (UUID
  `f2315551`, 2026-04-27 overnight; was `7c4e03eb` 5m48s/835 MB
  at the `b347c0a` re-bench) vs the failed 32 s SIGKILL at
  28.9 GB - 38× peak-RSS reduction. Output ordering is type-sorted
  (nodes, then ways, then relations); preserves structure on
  already-type-sorted input, re-sorts unsorted input.
- [x] ~~**`time-filter`**~~ - **LANDED 2026-04-28** (commit `83183fb`).
  Snapshot path migrated from
  `for_each_primitive_block_batch + par_iter().map(thread_local BB)`
  + drain to `parallel_classify_phase` + `ReorderBuffer`, mirroring
  the `cat --clean` (`b347c0a`) and `check --ids` (`516129e`)
  precedents. Planet `--bench 1` (UUID `6d905564`):
  **4m30s wall, 812 MB peak anon** (was 5x SIGKILL at ~28 GB).
  Europe **2m27s / 324 MB** (was 1m32s / 16.9 GB; +59 % wall,
  −98 % RSS). The 2026-04-28 instrumentation campaign across three
  SIGKILL'd attempts (UUIDs `4800c0a` / `e06c6ad` / `9fffdc4`) ruled
  out the iter-5 alloc-profile hypothesis (per-decode-thread scratch
  was 60 MB total, not 4.4 GB) and ruled out allocator knobs
  (`malloc_trim`, `M_MMAP_THRESHOLD=64K`, `decode_ahead=8` all
  hit the same ceiling); mallinfo2 confirmed the ~28 GB working set
  is structural to the parallel-decode + batch-collect + parallel-
  write architecture, not retention. Migration also deleted the
  iter-4 / iter-5 pool infrastructure (`buf_pool` module,
  `take_owned_swap`, `write_primitive_block_owned_pooled`) since
  nothing else in-tree referenced it. Plan doc:
  [`notes/time-filter-optimization.md`](notes/time-filter-optimization.md).
  History-path pending-group state machine (`time_filter_history`)
  is sequential by design and would need a real refactor for
  parallelism, but no history PBF is configured in `brokkr.toml`
  so it doesn't show up in the snapshot-path planet bench - keep
  separate from this entry.
- [x] ~~**`tags-filter --invert-match w/highway=primary`**~~ -
  **LANDED 2026-04-28** (commit `17b116c`). The 28.3 GB peak from
  the earlier 16e3694 bench (UUID `6665605a`) was misattributed
  to pass 2 in the prior diagnosis. The 2026-04-28 reproduction
  on the time-filter-migration commit `4f16591` (UUID `9044c456`)
  showed pass 2 only peaks at **7.04 GB** - it already uses the
  right shape (custom pread-from-workers + ReorderBuffer, lines
  852-961 of `tags_filter/mod.rs`). The actual 24.09 GB peak was
  in **`collect_way_node_dependencies`**, which used
  `parallel_classify_accumulate` with a per-worker `IdSet`: the
  bitmap is sized by the node ID space (~1.5 GB at planet),
  multiplied by ~30 decode threads. Documentation at
  `scan/classify.rs:300-308` already flagged this exact pattern
  as a known concern at 14.59 GB at planet. Migration: switched
  to `parallel_classify_phase` so workers emit per-blob
  `Vec<i64>` of way node-refs (bounded by blob size, ~640 KB max),
  consumer merges into one shared IdSet through the 32-slot
  result channel. Re-bench (UUID `7e74981a`): **planet 7m57s
  wall, 6.97 GB peak anon** (was 8m08s / 24 GB; **−71 % RSS**,
  wall ~unchanged). Default mode `tags-filter w/highway=primary`
  re-bench (UUID `258a2e9a`): 1m57s / 2.59 GB (was 1m48s / 2.6 GB;
  +8 % wall, RSS unchanged). `collect_relation_member_closure`
  was NOT migrated despite using the same per-worker pattern -
  its merge step needs `&mut included_relation_ids` while
  classify needs `&included_relation_ids`, so they cannot
  co-exist in one parallel_classify_phase invocation; the
  per-worker accumulation there is also bounded enough (6.8 GB
  peak even at planet invert-match). Pinned in a comment block
  inside the function.

### Latent same-shape risks (not gating 1.0)

Two commands share the
`for_each_primitive_block_batch` + `par_iter().map_init(BlockBuilder)`
+ `collect` + drain pattern that drove the pre-migration
time-filter snapshot OOM. Neither has been benched at planet
RSS-wise; neither blocks 1.0 today.

**Critical lesson from the tags-filter investigation (2026-04-28):
shape ≠ root cause. Always instrument first.** The pre-migration
TODO entry for tags-filter `--invert-match` claimed the par_iter+collect
shape was the 28.3 GB peak's root cause, by analogy with time-filter.
The actual measurement showed pass 2 (which has that exact shape)
peaks at only ~7 GB at planet. The 24 GB came from a sibling phase
(`collect_way_node_dependencies`) using `parallel_classify_accumulate`
with a per-worker `IdSet` bitmap whose size scales with ID space
not element count. Both bugs are real, but they live in different
phases and have different fixes. **Before assuming either of these
two commands needs the par_iter+collect migration, run a planet bench
with full sidecar instrumentation and read the per-phase RSS table.**
The actual blocker may be a sibling phase (e.g. an
`parallel_classify_accumulate` caller that's now visible because
the headline phase isn't dominating).

When you do bench, the data lives in `brokkr sidecar <UUID> --human`
and won't survive a subsequent forced/failed run from any other
command (the `dirty` alias rotates). If the run OOM/SIGKILL'd before
`writer.flush()`, mid-run `WRITER_METRICS.emit()` calls inside the
batch boundary leave fresh state in the FIFO - the time-filter
migration set up that pattern in `src/commands/time_filter/mod.rs`;
mirror it if you expect SIGKILL on the first attempt.

- [x] ~~**`getid --add-referenced` pass 2**~~ - **PLANET-SAFE AT
  CURRENT WORKLOAD, no migration needed.** Benched 2026-04-28 at
  commit `afe3139` (UUID `dirty`, brokkr's hardcoded ID set: 3 ways
  + 3 relations + ~76 referenced nodes via pass 1): planet wall
  **96.3 s**, peak anon **1.26 GB** at the GETID_PASS2 phase
  (par_iter+collect shape). Pass 1 (`parallel_classify_accumulate`
  with per-worker `IdSet`) peaks at 499 MB - the per-worker bitmap
  stayed small because only 3 way IDs needed scanning. The europe
  9.03 GB pass 2 peak observed 2026-03-29 (UUID `c0d364c3` at
  commit `7cf002c`) was on pre-`DecompressPool` /
  pre-`parse_and_inline`-fix infrastructure; HEAD's pipelined-reader
  memory profile is dramatically smaller. **Same lesson as
  tags-filter (2026-04-28): shape ≠ root cause. The par_iter+collect
  shape did not trigger the predicted peak because brokkr's tiny
  hardcoded ID set yields ~zero output blobs per batch (29 framed
  output items total at planet, 8 KB), so the `Vec<OwnedBlock>`
  collected per batch is almost empty.** Pass 2 is read-bound, not
  memory-bound: `writer_recv_wait_ns=72.4 s` of the 72.9 s pass 2
  wall, `pipeline_decoded_recv_wait_ns=43.7 s`. Future risk is a
  workload with a much larger / wider-spread input ID set
  (millions of IDs hitting many blobs at high keep-rate), which
  would re-introduce the result accumulation peak - but that's the
  same axis as the deferred TODO line below ("Custom ID set
  distributions for `getid` / `getparents`") and not gating 1.0.
  Counter `getid_dep_node_ids` added 2026-04-28 (commit forthcoming)
  emits the dep-set size between pass 1 and pass 2, so a future
  run with a different ID set surfaces the size up-front before
  pass 2 starts.

- [ ] **`altw` sparse path** (`src/commands/altw/mod.rs:485-510`
  + `process_batch:692-736`). Identical par_iter+collect shape.
  Currently masked because `add-locations-to-ways --index-type auto`
  selects `external` for sorted+indexed planet inputs, and external
  uses entirely different scatter/gather code (`altw/external/`) -
  this pattern doesn't fire on the planet recommended path.
  Forcing `--index-type sparse` at planet is the trigger. Same
  investigative discipline as getid above: bench first, instrument
  the sidecar, identify the actual peak phase before assuming the
  par_iter+collect step is the culprit. Other altw stages also worth
  checking: any `parallel_classify_accumulate` caller in the sparse
  pipeline is suspect at planet keep-rates (the documented caution
  at `src/scan/classify.rs:300-317` lists the criteria).

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

- [ ] **History PBF for `time-filter`**. pbfhogg supports per-element
  version history and visibility, but `brokkr.toml` has no history
  variant on any dataset. `time-filter` benches on a regular PBF
  record near-no-op walls (every timestamp compare decides keep).
  Configure a history PBF variant (planet history is ~120 GB; europe
  history is more realistic for iteration) to unlock the actual
  workload.
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

- [ ] **Consider auto-enabling diff `-j`**. Currently `pbfhogg diff`
  defaults to `-j 1` (sequential). `-j 0` maps to
  `available_parallelism()`. Evaluate flipping the default from 1
  to 0 once the parallel path has more field miles. Wait until
  Milestone 3.

- [ ] **Expose phase events as a proper Rust event/hook API** - wrap
  every instrumentation call in per-command `probes` modules, then swap
  the backend from the current FIFO sink to `tracing` spans/events so
  library consumers can subscribe. Full rollout (call-site shape,
  coverage sweep, brokkr `--probes`, backend migration) in
  [`notes/instrumentation-layering.md`](notes/instrumentation-layering.md).

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
works. See [notes/zlib-level-tuning.md](notes/zlib-level-tuning.md).

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
input or --clean. Verified via `brokkr verify multi-extract`.

**Known issues:**

- [ ] **strip-4 verify failure** - `brokkr verify multi-extract --regions 5`
  on Denmark: strip-4 has 1 fewer node than sequential (41643 vs 41644).
  Passes with 3 and 4 regions. Only fails with 5 regions where strip
  boundaries fall at exact integer longitudes (8,9,10,11,12,13). Likely
  a floating-point rounding issue in brokkr's bbox strip generation,
  not a pbfhogg bug. Pre-existing since multi-extract shipped.

**v2 improvements:**

- [ ] **Spatial index** - grid or R-tree over regions for O(1)
  per-element lookup instead of O(N). Required for 200+ regions where
  linear scan becomes the bottleneck. Simple grid (3600×1800 cells of
  0.1°, precompute overlapping regions per cell) is sufficient.
- [ ] **Complete/smart strategies** - per-region way/relation ID
  tracking. Memory: N × ~3 GB (bbox_node_ids + all_way_node_ids per
  region). Feasible for ~10 regions on 30 GB host, ~40 on 128 GB.
- [x] ~~**Raw passthrough**~~ - CLOSED 2026-04-20 via shadow counter
  (planet 5-region `--config --simple` at commit `57b01f9`, UUID
  `dad573cb`): 0 / 32,835 node blobs qualify under any partial-passthrough
  gate. Same outcome as tags-filter's earlier 0 / 50,364. Structural:
  ID-sorted PBFs put chronologically-adjacent (geographically-scattered)
  nodes in each blob, so a blob's geographic bbox is ~planet-wide and
  cannot fit in a sub-planet region. The all-N-contained path stays
  for the N=1 / fully-overlapping niche. Load-bearing pin in
  `src/commands/extract/multi.rs::try_extract_multi_single_pass`.

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
- [ ] **Document the `merge-changes -> apply-changes` pipeline pattern in
  README.** When applying accumulated dailies (e.g. a week worth), squashing
  them with `merge-changes` first and then running `apply-changes` once on
  the result is the recommended shape - cheaper than running `apply-changes`
  N times. The 5x speedup at planet 7-OSC (commit `99057fa`, 267 s -> 55 s)
  makes this an unambiguous recommendation now; pre-parallel the squash itself
  was 4m27s and the calculus was murkier. Original suggestion from the
  apply-changes Q7 reviewer round (2026-04-21), retired from the now-deleted
  `notes/merge-changes.md` plan doc 2026-04-28 when the parallel-drain work
  shipped. Pure documentation; no code change needed.
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
  See [notes/columnar-integration.md](notes/columnar-integration.md).

- [x] **Smart-extract planet memory blocker - CLOSED 2026-04-11, ship
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
  bbox isn't a real workload - use `cat` passthrough.

  Mechanism: cold-arena-page residency cascade. Post-PASS1 header
  scans touched glibc's bloated free-list pages that were previously
  reserved but not resident; the fix (commits `d4ea760`, `0b085b1`)
  plumbs the PASS1 schedule forward so PASS2/PASS3 don't rescan.

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
  See [notes/spatial-index-in-pbf.md](notes/spatial-index-in-pbf.md)
  and [notes/way-blob-bbox-speculation.md](notes/way-blob-bbox-speculation.md).
  Node blob header scan is already fast (~0.5s planet). Way blob spatial
  bboxes are limited by chronological ID ordering (~30% skip for Denmark,
  not 50-80%). Geography-sorted way blobs (Hilbert curve) would give
  90%+ skip but breaks Sort.Type_then_ID. Multi-extract benefits most.
- [x] Streaming pipeline composition - CLOSED, limited benefit.
  The codebase already does the most valuable composition (inline
  indexdata in all write paths). Multi-pass commands can't consume
  streams. See [notes/streaming-pipeline-composition.md](notes/streaming-pipeline-composition.md).
- [x] ~~Dense ALTW compact rank-indexed array~~ - CLOSED 2026-04-30 by
  commit `c6f08ff` (sparse rank-indexed flat) + `b70dd8c` (dense removed).
  The proposed rank-indexed layout landed as the new sparse encoding,
  which dominated dense at every measured scale (japan 4.3x faster) and
  worked in regimes dense did not (europe survives where dense OOMs);
  dense was then removed entirely. Reviewer items "parallel pass 1" and
  "rank-compacted index" would have converged dense to the same encoding,
  same access pattern, same wall - so maintaining two near-identical
  implementations was not earned.
- [ ] Verify GeoJSON polygon format coverage for extract (does `--polygon`
  accept GeoJSON, or only .poly format?).
- [ ] History-file support - decide in-scope or explicitly out-of-scope.
