# ALTW external join — historical probe record

Historical record of probes attempted on the `add-locations-to-ways --index-type external` path before the 2026-04-16 structural re-plan.

**This is not an active plan.** The active plan is [`altw-structural-reports.md`](altw-structural-reports.md).

Retained for:

- measured failure-mode evidence (shelved results future work can reference)
- baseline UUIDs for before/after comparison
- concrete "what actually went wrong in the probe" notes that distinguish *the idea was wrong* from *the probe was too timid*

Most entries below describe a narrow probe on the existing architecture. "Shelved" means the probe did not beat `main` under its own keep-gate; it does **not** mean the underlying structural idea is wrong. Several of these are reopened as proper rewrites in the re-plan.

## Measured baselines (reference)

Recent clean normal baselines on current `main`:

| Dataset | UUID | Wall | Stage 1 | Stage 2 | Stage 3 | Finalize | Stage 4 |
|---|---:|---:|---:|---:|---:|---:|---:|
| Europe | `ffdf5f69` | 375.9 s | 71.0 s | 97.0 s | 37.2 s | 17.8 s | 121.1 s |
| Planet | `4f059b67` | 867.7 s | 148.5 s | 266.6 s | 100.2 s | 46.4 s | 231.6 s |

Europe is stage-4-led; planet is stage-2-led with stage 4 second.

## Already shipped on `main`

These are in tree and reflected in the baselines above:

- `coords_by_rank` removal: stage 2 decodes node blobs directly via `NodeBlobInfo`
- Stage-3 direct scatter from raw `ResolvedEntry` bytes (no `Vec<ResolvedEntry>` materialization)
- Parallel finalize tail in `coord_payloads.rs` — per-blob pread+pwrite work-stealing
- Stage-4 per-way refcount sidecar consumption in the way reframe path
- Stage-4 raw passthrough for relation blobs (always) and node blobs when `keep_untagged_nodes`
- `PerWayRcs` lazy per-blob decode via blob-offset sidecar
- Slot-bucket `ResolvedEntry` record shrunk 16 → 12 bytes: `fcd4fa2`
- Shared header-scan sidecar replacing three header-only passes: `f864b64f`

## Probes — summary

### Rank-bucket sweep beyond 256

- Implementation: `2168a7e` (land), reverted after Japan
- UUIDs: 256 baseline `6453221b`; 384 `800de5c2`; 512 `d3a320de`
- Japan stage 2+3+finalize: 256 9116 ms → 384 9711 ms (+6.5%) → 512 10377 ms (+13.8%)
- Stage 1 essentially flat across the sweep (3307 → 3342 → 3229 ms)
- Structural counters scaled linearly with bucket count:
  - `s2_open_calls`: 5632 → 8448 → 11264
  - `s2_node_straddler_blobs`: 510 → 766 → 1022
  - `s3_integrated_straddler_count`: 255 → 383 → 511
- Verdict: keep `NUM_BUCKETS = 256`. Failure mode is structural: reopens and straddlers grow faster than cache-fit gains.

### Slot-bucket 16 → 12 byte record (KEPT)

- Landed in `fcd4fa2`
- UUIDs: Denmark `d285275e`, Japan `a065f776`, Europe `e03dff10`
- Japan: `s2_slot_bytes_written` 5.66 → 4.25 GB (−25%); `s3_bytes_read` −25%; stage 2+3+finalize 9116 → 8529 ms (−6.4%)
- Europe: scratch −25% on both sides; combined stage 2+3 flat; including finalize +0.6% (within gate)
- Fault signal: Europe total major faults 378,816 → 249,097 (−34%); stage-4 majors 122,545 → 9,353 — less downstream page-cache pressure

### Epoch-spill / slot-space epochs (env-var-gated prototype)

- Denmark correctness: passed
- Europe E=4: won locally and on wall
- Planet E=4: **OOMed**
- Planet E=8: fit, but lost to same-commit normal
- Verdict: shelved **as a narrow probe**. The structural re-plan's opportunity #1 is the non-timid version — delete the disk-backed `SlotBuckets` path entirely, auto-tune `num_epochs` against `/proc/meminfo`, collapse finalize into the final epoch's emit. The OOM at E=4 planet is evidence that the probe needed memory-conscious defaults, not evidence that the idea does not work.

### Per-worker local `IdSetDense` in pass A

- Europe: `s1a_idset_local_chunks = 8932` vs `s1a_idset_final_chunks = 406` — excessive fragmentation
- Verdict: shelved on this design.

### Stage-1 pass-A direct-set fusion / pass-B ranked-vector fusion

- Both tried and reverted on the existing emission shape; details in `altw-optimization-history.md`.

### Stage-1B per-blob bucket staging (batched `write_all` per bucket per blob)

- Implementation: `e16674b` (land), `950c22d` (revert), 2026-04-14
- Europe stage 1: 77.0 → 99.9 s (+30%); every CPU-bound counter regressed together (`s1b_scan_ms`, `s1b_rank_ms`, `s1b_encode_write_ms`)
- `write_all` call count: 4.69 B → 14.16 M (−331×, as designed) — but the syscall was never the cost
- Root cause: `BufWriter` was already amortizing syscalls; the staging layer added an extra memcpy and scattered writes across 256 `Vec<u8>` tails, thrashing L1/TLB
- Lesson: `s1b_encode_write_ms` cumulative looked like a syscall pile-up but was a `BufWriter`-amortized memcpy. **Cumulative-ms numbers are not evidence of a bottleneck — measurement is.** Reviewer consensus is not either.
- Verdict: shelved *as framed*. The re-plan's #3 (node-ID scratch spool) is a different mechanism — replaces pass B's zlib decompression entirely, rather than reshaping an already-cheap write path.

### Stage-2 hot-loop batch (monotonic rank + callback node scanner)

- Implementation: `237cb2e`; Japan `36615411`
- Japan `EXTJOIN_STAGE2`: 5900 → 5922 ms (flat)
- Subcounters: `s2_coord_fill_ms` −16%, `s2_resolve_ms` −19%, `s2_node_extract_ns` down sharply, `s2_node_rank_ns` up correspondingly
- Verdict: shelved. Attribution shuffle without a wall win.

### Stage-4 wire-format DenseNodes filter

- Implementation: `4910fd9`
- Denmark correctness: `pbfhogg diff --summary --suppress-common` reported `same=10175884 different=0`. MD5 differed because the encoding shape changed without changing semantic content — MD5 was the wrong gate for this item.
- Japan: `s4_nonway_assemble_ms` 1113 → 557 ms (−50%); `EXTJOIN_STAGE4` 9002 → 8759 ms (−2.7%)
- Europe (normal `7ab12b2a` vs wire `d0ffd614`): `s4_nonway_assemble_ms` 78501 → 36940 (−53%); `s4_assemble_ms` 520947 → 426199 (−18%); but `EXTJOIN_STAGE4` 122.7 → 127.6 s (worse); `s4_send_ms` cumulative 560971 → 671809 — freed worker CPU refilled the writer queue
- Europe `zstd:1` (normal `e3f3ec1b` vs wire `774fe74b`): `s4_nonway_assemble_ms` −13%; `EXTJOIN_STAGE4` −1.3%; total wall 5m40s → 5m48s (worse)
- Verdict: shelved. **Writer-ceiling diagnostic retained as evidence** for re-plan #2 — real stage-4-local CPU wins are invisible on wall under a writer-bound output mode.

### Stage-1B grouped-by-local-rank emission

- Japan: normal `3b5fcc08` vs grouped `856a7bb9`
- `s2_prepare_scatter_ms` 3761 → 3629 ms (−3.5%, below the 20% gate)
- `EXTJOIN_STAGE1` 3212 → 4239 ms (+31.9%)
- Combined stage 1+2 9221 → 9840 ms (+6.7%)
- `s1b_bytes_written` 4.25 → 5.31 GB (+25%); `s1b_shard_write_calls` 354 M → 664 M; `s1b_grouped_headers` 310 M
- Verdict: shelved. Per-group headers did not pay back; most emitted runs were too short.

### Shared header-scan sidecar (KEPT)

- Landed in `f864b64f`
- Baseline `7ab12b2a` → sidecar `f864b64f`: Europe 6m37s → 5m33s (−64 s)
- Replaced three header-oriented passes:
  - `s1_way_schedule_build_ms` 24762 → 80
  - `s1_node_map_build_ms` 30887 → 123
  - `s4_schedule_scan_ms` 31537 → 144
- New single pass: `extjoin_meta_scan_ms` 30852 ms
- Phase movement: `EXTJOIN_STAGE1` 91.4 → 36.0 s; `EXTJOIN_STAGE4` 122.7 → 90.6 s
- Net: 87.2 s of scan work → 31.2 s (saved ~56 s Europe)

### Relation-member node wire scanner

- Baseline `f864b64f` vs wire-scanner `603b1043`
- Denmark: byte-identical outputs (MD5 `2d3c901a40eec6bf3bfb2084641519f4`)
- Europe: `EXTJOIN_RELATION_SCAN` 14294 → 15172 ms (worse); total wall moved from unrelated noise
- Verdict: shelved. Parsing just the relation memids/types arrays was not enough to beat the existing full-block path.

### Node-blob double-decode across stage 2 and stage 4 (deferred)

- Stage 2 decodes the kept node-blob set to populate bucket-local `coord_slice`; stage 4 decodes the same kept node blobs again on the non-way path
- Planet cumulative work: `s2_node_decompress_ms = 192356`; stage 4 processes all 32835 / 32835 node blobs again
- Verdict at the time: deferred structural item, not a next-sprint candidate. Fusing is architecturally awkward — stage 2 is rank-bucket ordered while stage 4 is file-ordered and consumer/writer-bound
- Reopened as re-plan opportunity #6.
