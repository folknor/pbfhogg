# ALTW Optimization History

Research path from mmap thrashing to bounded-memory external join for
`add-locations-to-ways` on memory-constrained hosts (30 GB RAM, plantasjen).

## The problem

ALTW needs random access to node coordinates by ID. OSM node IDs reach ~13B,
so a direct-mapped array is 128 GB virtual. After pass 0 filtering (way-
referenced nodes only), ~2B nodes remain = ~16 GB touched pages. On a 30 GB
host, 16 GB mmap + 33 GB input page cache exceeds physical memory. The kernel
constantly evicts and re-faults pages: planet took 96 minutes, CPU mostly idle.

**Key structural insight**: ALTW reconstructs a sparse relationship from two
streams sorted on different keys (nodes by node ID, ways by way ID). The 16 GB
mmap is a hash table for this join. Random access is inherent to the structure,
not an implementation flaw.

## Approaches tried

### 1. Pass 0: referenced-nodes-only filter (commit `b3a98b0`)

Scans way blobs to build `IdSetDense` bitset (~1.6 GB for planet). Only inserts
way-referenced nodes into the dense index. Reduces touched pages from ~80 GB to
~16 GB. **Result**: no improvement at Europe scale (2631s vs 2565s baseline).
16 GB mmap + 33 GB input still exceeds 30 GB RAM. Useful on 64 GB hosts.

### 2. Sparse index: Planetiler-inspired chunk array (commit `52d6273`)

`--index-type sparse`. Chunk-indexed (size 256) sparse array. ~540 MB RAM for
chunk index, ~16 GB on-disk values file via mmap. Way lookups batched and sorted
by file offset via `FxHashMap`.

**Hypothesis**: sorted batched lookups would convert random mmap access into
sequential scans. **Disproven** at Europe scale: 2.5x slower than dense.

**Cross-scale results** (plantasjen):

| Scale | Dense | Sparse | Ratio | Insight |
|-------|-------|--------|-------|---------|
| Denmark (465 MB) | 6,826 ms | 14,105 ms | 2.07x | CPU overhead dominates (small batches) |
| Japan (2.4 GB) | 72,284 ms | 71,837 ms | **1.00x** | CPU overhead amortized; proves overhead is negligible |
| Europe (33.6 GB) | 2,565s | 6,453s | **2.5x** | Page-cache thrash on values mmap, plus sort overhead |

The Japan result was key: sparse = dense when data fits in cache proves the
sort+hash CPU cost is negligible. The Europe failure is purely I/O -- same
page-cache thrashing as dense, but with overhead on top.

### 3. Coalesced pread (commit `4fbf7a8`, reverted `034422c`)

Replaced mmap with explicit coalesced `read_exact_at` calls over 128 KB spans.

| Backend | Japan best of 3 | vs dense |
|---------|----------------|----------|
| mmap | 71,837 ms | 1.00x |
| pread | 79,370 ms | 1.10x |

**10% slower** on hot data. Syscall overhead per read outweighs I/O benefits
when pages are cached. Rejected without Europe testing.

### 4. Partitioned multi-pass (measured, rejected)

Split node ID range into N partitions; skip way blobs not touching the current
partition. **Hypothesis disproven** by measurement.

Measured with `examples/partition_stats.rs`:

| Dataset | Way blobs | N=2 single-part | N=64 median touched |
|---------|-----------|-----------------|---------------------|
| Denmark | 828 | 0 (0.0%) | 62-63 of 64 |
| Japan | 5,363 | 3 (0.1%) | 61-62 of 64 |

**Every way blob touches nearly every partition.** Root cause: sorted PBFs group
ways by way ID. A blob of ~8000 ways from different eras references nodes
spanning the entire chronological ID space. Node-ID partitioning is
fundamentally misaligned with PBF blob layout. This kills any approach depending
on selective way-blob skipping.

## Solution: external join via double radix permutation

**Commit**: `034422c`+ (`--index-type external`). Pre-compute the way-node join
using sequential I/O and bounded memory.

### Pipeline

1. **Way pass**: stream ways, emit `(node_id, slot_pos)` COO pairs into 256
   node buckets partitioned by high bits of node_id. Memory: ~64 MB.
2. **Node join**: for each bucket, sort by node_id, single-pass merge-join with
   node stream, emit `(slot_pos, lat, lon)` into 256 slot buckets. Memory:
   ~500 MB (one bucket). Nodes read exactly once across all buckets.
3. **Slot reorder**: for each slot bucket, sort by slot_pos, scatter-write to
   final coord_slots file. Memory: ~375 MB.
4. **Assembly**: stream PBF, read matching coords from coord_slots sequentially,
   emit enriched ways. Passthrough for nodes/relations.

Every stage: bounded memory (<1 GB), all sequential I/O, no mmap, no page faults.

### Optimization history

| Optimization | Commit | Impact |
|-------------|--------|--------|
| Single-pass node merge (stage 2) | `a334c72` | 302s -> 25s Denmark (12x) |
| fadvise(DONTNEED) + mmap coord_slots | `165cbb2` | 25s -> 22s Denmark |
| Node-only wire-format scanner (stage 2) | `cf350a9` | Eliminated 25+ GB heap retention |
| Scatter buffer (stage 3) | `cf350a9` | 15x speedup, eliminated 4.69B pwrite calls |
| Sequential readers (stages 1, 4) | `4daf995`, `2873919` | Eliminated 11 GB PrimitiveBlock retention |
| P2b-v2 pread-from-workers (stage 2) | `80e227b` | 301s -> 216s (-28%), anon 20.4 GB -> 1.4 GB |
| P2c parallel assembly (stage 4) | `6b09796` | 432s -> 136s (-68%) |

### Final results (plantasjen, 30 GB RAM)

| Index | Denmark (465 MB) | Japan (2.4 GB) | Europe (33.6 GB) | Planet (87.7 GB) |
|-------|-----------------|----------------|------------------|------------------|
| dense | 8,168 ms | 72s | 2,565s (43m) | 5,773s (96m) |
| sparse | 14,105 ms | 72s | 6,453s (107m) | not tested |
| external | **12.3s** | 143s | **577s (9.6m)** | **1,462s (24.4m)** |
| ext/dense ratio | 1.5x slower | 2.0x slower | **4.5x faster** | **3.9x faster** |

**Crossover**: between Japan (2.4 GB, dense 2x faster) and Europe (33.6 GB,
external 4.5x faster). Dense thrashes when mmap working set + input page cache
exceeds physical memory.

### Memory profile (Europe, sidecar `bc38a079`, commit `6b09796`)

| Stage | Duration | Anon peak | Notes |
|-------|----------|----------|-------|
| Stage 1 (way pass) | 128s | 70 MB | Sequential reader |
| Stage 2 (node join) | 221s | 1.4 GB | Pread-from-workers, bucket sort |
| Stage 3 (slot reorder) | 91s | -- | Scatter buffer |
| Stage 4 (assembly) | 136s | 7.3 GB | Pread-from-workers, parallel assembly |

### Planet-validated (commit `abcc736`)

**1,075s (17.9 min), 8.7 GB peak anon. Dense planet: 5,773s (96 min). 5.4x faster.**

| Stage | Baseline (planet) | Optimized (planet) | Improvement |
|-------|------|------|------|
| Stage 1 (two-pass: IdSetDense + rank-bucketed emission) | 333s | 136s | −59% |
| Stage 2 (pipelined counting-sort merge) | 612s | 469s | −23% |
| Stage 3 (parallel pwrite scatter) | 247s | 167s | −32% |
| Stage 4 (parallel P2c assembly + wire-format way reframe) | 269s | 280s | — |
| **Total** | **1,462s** | **1,075s** | **−26%** |

### April 2026 optimization sprint

Europe: **608s → 422s → ~391s (−36%)**. Planet: **1,462s → 1,075s (−26%)**.
Peak anon: 16.7 GB → 8.7 GB → 5.9 GB (Europe, −65% from original).

| Optimization | Commit | Impact (Europe) |
|-------------|--------|----------------|
| Parallel stage 1 (per-worker bucket shards, AtomicUsize dispatch) | `de75000` | 117s → 45s (−62%) |
| Rank-bucketed counting sort (O(n) replaces O(n log n) comparison sort) | `df09a62` | stage 2: 262s → 218s |
| Parallel stage 3 (pwrite to pre-sized coord_slots) | `74edbfd` | 108s → 64s (−41%) |
| Pipelined stage 2 bucket loader | `e1ba970` | stage 2: 218s → 181s |
| Fused rank_if_set + parse-free bucket prep | `06f2a30` | stage 2: 181s → 140s |
| Wire-format way reframe (stage 4) | `a705fde` | stage 4 assemble: −40% CPU |
| Shard consolidation (reverted — net loss) | — | +67s overhead |
| Shrink rank record to 12 bytes | `cfa916f` | 25% I/O reduction stages 1B+2 |
| File-backed coords_by_rank | `6293ade` | Eliminates streamed node merge |
| Overlap stage 1B + coord pass | `b1bddd5` | Concurrent independent scans |
| Parallel stage 2 (AtomicUsize dispatch, shared slot buckets) | `5e652f2`+`c7fdb4c` | stage 2: 124s → 91s (−27%) |
| Stage 4 micro-opts (tried and reverted) | `70c87c1` → `3c59471` | flat — not arithmetic-bound |

**Parallel stage 2 details** (`5e652f2`, `c7fdb4c`): replaced sequential
producer/consumer (loader thread + sync_channel + single consumer) with N
workers via AtomicUsize dispatch. Shared 256 slot bucket files with per-bucket
Mutex<BufWriter> (256 FDs total, FD-safe). Worker-local per-slot-bucket buffers
flush at 256 KB threshold — avoids both the OOM from unbounded buffering
(28.2 GB peak anon before fix) and the contention from per-entry locking
(`s2_resolve_ms` 409s → 62s summed across 6 workers).

**Stage 4 optimization experiments (all tried and reverted or flat):**

Split timer (commit `b99af0c`, UUID `d7a08d2f`) proves the bottleneck:
`s4_way_coord_read_ms`=532s vs `s4_way_delta_encode_ms`=21s (25× ratio).
374K major page faults during stage 4 vs 44 in stage 2.

| Experiment | Stage 4 | majflt | Result |
|-----------|---------|--------|--------|
| Baseline (MADV_SEQUENTIAL, 6 workers, work-stealing) | 141s | 374K | — |
| Per-ref micro-opts: varint skip + batch reads | ~141s | — | Flat, reverted |
| Per-blob pread replacing mmap | 145s | 19K | Flat — syscall overhead replaced fault overhead |
| Contiguous partitioning + 3 workers | 405s | 466K | 3× regression — starved consumer |
| MADV_RANDOM | 157s | 9,167K | Worse — killed readahead |
| No madvise | 143s | 197K | Tied — fewest faults, kept as new default |

**Conclusions**: stage 4 at ~141s is a structural floor for this architecture.
6 workers are needed for CPU parallelism (decompress+reframe); fewer workers
starve the consumer pipeline. But 6 concurrent mmap readers on 37 GB inevitably
thrash the page cache. Per-blob pread eliminates faults but replaces them with
equivalent syscall overhead. madvise tuning doesn't move wall time. TLB misses
are irreducible with mmap at this scale (37 GB / 4 KB = 9.5M pages). Breaking
this floor requires a fundamentally different coord representation (e.g.,
way-ordered payloads that eliminate the 37 GB mmap entirely).

**Fuse stage 2+3 (evaluated, ruled out)**: direct pwrite-scatter from stage 2
into coord_slots would require either billions of 8-byte pwrites (pathological
syscall cost) or 37 GB of in-memory scatter buffers. Rank order and slot order
are unrelated — stage 3 exists precisely to bridge this gap with sequential I/O.

**Stage 1B write batching (tried and reverted 2026-04-14, commit `e16674b`)**:
Replaced per-ref `write_all(12 bytes)` to `BufWriter` with per-bucket blob-local
byte staging (`Vec<Vec<u8>>` sized 256), one `write_all` per non-empty bucket
per blob. Call count reduced 4.69B → 14.16M (-331×) as designed.

| Metric (Europe) | Baseline `091fc5b` | Batched `e16674b` | Δ |
|---|---|---|---|
| `EXTJOIN_STAGE1` wall | 76,977 ms | 99,887 ms | **+22.9 s (+30%)** |
| `s1b_shard_write_calls` | 4,690,095,140 | 14,158,764 | -331× |
| `s1b_encode_write_ms` | 136,465 | 157,788 | +21.3 s |
| `s1b_scan_ms` | 9,990 | 35,490 | +25.5 s |
| `s1b_rank_ms` | 113,625 | 134,535 | +20.9 s |

BufWriter (256 KB capacity) was already the right batching layer — each
per-ref `write_all` was a cheap 12-byte memcpy into the buffer, not a syscall.
The staging layer added an extra memcpy (ref → `bucket_staging[bucket]` →
BufWriter) and scattered writes across 256 `Vec<u8>` tails per blob, thrashing
L1/TLB. All CPU-bound counters regressed simultaneously — consistent with
shared-resource contention, not write-call reduction. The TODO estimate
(−6s wall) and multi-reviewer consensus (arch-claude, planet-claude,
perf-codex) were both wrong because they extrapolated from cumulative
`s1b_encode_write_ms` without accounting for BufWriter already amortizing
the syscall cost.

**Stage 2+3 fuse ideas evaluated 2026-04-14 (desk analysis only, not
measured)**: after the stage 1B batching regression, three stage 2+3
ideas were analyzed on paper and either rejected or downgraded. No
benchmarks were run; the reasoning below is desk math about memory,
coalescing ratios, and syscall primitives.

1. **sort-then-coalesced-pwrite fuse** (`planet-claude` sketch) —
   rejected structurally. Stage 3 already does 256 pwrites, each
   150 MB, hitting the pwrite floor for a 37.5 GB positioned-write
   output. A rank bucket holds ~18M entries drawn from 4.69B slot
   positions → average gap 260 slots; a way's ~10 refs land in 10
   different rank buckets, so within-bucket slot adjacency is near
   zero. Realistic coalescing ratio ~1×, giving ~3B+ pwrites vs a
   budget of ~240M. Cannot win under rank-bucketing.

2. **grouped-by-local-rank emission in stage 1B** (`perf-codex` r1) —
   deferred, design drafted but not implemented. Naive version requires
   55 GB of per-worker per-bucket buffering (doesn't fit). Segmented
   version (~10 blobs buffered per worker, local counting-sort, k-way
   merge in stage 2) is feasible with ~920 MB/worker and estimated
   ~9s wall savings by eliminating `s2_prepare_scatter_ms`. Estimate
   is theoretical; given that the last stage-1B "improvement" theory
   predicted −6s and measured +22.9s, this number is not trustworthy
   without a run. Complexity is moderate-high.

3. **pwritev scatter-gather** — not applicable. `pwritev` writes from
   multiple source buffers into a **contiguous** file range; stage 3's
   one contiguous 150 MB buffer per bucket degenerates to `pwrite`.
   Would only help a design with many small discontinuous writes,
   which is exactly the design #1 rejected for other reasons.

4. **io_uring SQPOLL + registered buffers + IOPOLL** — filed as future
   option. Only relevant for a design that legitimately has many small
   pwrites. Stage 3's current 256-pwrite floor doesn't qualify. Noted
   for completeness if a future structural change (e.g., way-ordered
   payloads) changes the write pattern.

**Meta-lesson:** desk estimates on this code path have been
systematically optimistic. Both the stage 1B batching (measured: 30%
regression vs −6s estimate) and the proposed fuse (desk analysis
contradicts original sketch by an order of magnitude) suggest the
bottleneck mental model for external_join is unreliable. Future
estimates should be bounded by micro-benchmarks or skipped in favor
of direct measurement on a small dataset.

**Stage 4 bottleneck isolated 2026-04-14 — measurement-backed.**
The 0330a9b per-blob-pread experiment already carried the diagnostic
we needed. Its sidecar (UUID `44135291`) split `s4_way_coord_read_ms`
into two counters:

| Variant | `s4_coord_pread_ms` cumul | `s4_way_coord_read_ms` cumul | Combined wall | Stage 4 wall |
|---|---|---|---|---|
| mmap (`e151e5e8`) | n/a | 370,200 | ~62 s (fused) | 141 s |
| pread (`44135291`) | 306,999 | 42,507 | 58 s (51 + 7) | 145 s |

Interpretation:

- Inner-loop byte copy: 7 s wall. Already cheap.
- Pread I/O: 51 s wall for 37 GB = **720 MB/s aggregate**. 6 concurrent
  workers on disjoint regions of a 37 GB file on consumer NVMe.
- Mmap ≈ pread on total coord work (62 s vs 58 s). Mmap stage 4 wall
  is 4 s better because async fault handling overlaps with worker
  thread other work; sync preads don't.
- **The 141 s stage 4 floor is NVMe sequential read cost for 37 GB
  across 6 workers.** Not mmap mechanics, not fault count, not
  inner-loop work.

This closes the large family of "change the coord access mechanism"
designs: per-blob pread, way-ordered payload layouts, postings-by-rank
CSR, blob-local rank batching. None of these reduce bytes-read, so
none can beat 720 MB/s × 37 GB ≈ 51 s I/O alone.

**The only remaining lever is reading fewer bytes.** Two options
sketched (not measured, ~10% total improvement ceiling):

1. Delta-encoded varint coords (3–4× smaller file). Projected stage 4
   save ~27 s wall. New on-disk format, new decode path in stage 4,
   stage 2 emits per-way deltas.
2. Wire-format-ready payloads (stage 4 splices bytes, no per-ref
   encode/reassemble). Projected save ~8–10 s wall. Couples stage 2
   to PBF wire format.

Combined: Europe 392 → ~350–360 s; planet 1,075 → ~950–1,000 s.
Reasonable to ship or defer.

**Blob-ordered coord payload prototype 2026-04-14 (commits a13a6a8,
e9e1d77, 7738642).** Built as a measurement-first prototype before
committing to a stage-3 integration, per the lesson that desk
estimates on this code path over-predict by 5–10×.

Scope — three commits:

1. **Per-way refcount sidecar** (stage 1A). Captures `refs.len()` per
   way during the existing `scan_way_refs` pass. Emits varint stream
   per blob: `[varint num_ways][varint rc0][varint rc1]...`. Europe
   sidecar size: 455 MB.

2. **`coord_slots → coord_payloads` transform pass** (`coord_payloads.rs`).
   Reads coord_slots + per-way refcount sidecar, emits per-blob
   delta-varint payloads. File format:
   `[u64 num_way_blobs][u64 total_payload_bytes][u64*(N+1) blob_offsets][payloads]`.
   Within each blob: for each way, 2×ref_count zigzag-varints
   interleaved (lat, lon, lat, lon, ...) with deltas reset per way.

3. **Stage 4 alternate path** (gated by `PBFHOGG_COORD_PAYLOADS_PROTOTYPE`
   env var). When enabled, each way-blob worker preads its blob's
   payload into a worker-local buffer and de-interleaves raw varint
   bytes into PBF's `packed_lats` / `packed_lons` fields — no zigzag
   decode, no re-encode, because the payload byte layout matches PBF
   wire format 1:1. Combines "fewer bytes to read" (option 1) with
   "skip delta encode" (option 2) in a single format.

Measured results (Europe, commit `7738642`, UUID `99f6b8bc`):

| | Baseline `e151e5e8` | Prototype `99f6b8bc` | Δ |
|---|---|---|---|
| Total wall | 392.7 s | 465 s | **+72 s (regression)** |
| Stage 1 | 81 s | 94 s | +13 s |
| Stage 2 | 87 s | 88 s | +1 s |
| Stage 3 | 51 s | 52 s | +1 s |
| Transform pass | — | 65 s | — |
| Stage 4 | 141 s | 130 s | **−11 s** |
| `s4_way_coord_read_ms` cumul | 370,200 | 77,316 | −5× |
| `s4_way_delta_encode_ms` cumul | 52,000 | 0 | eliminated |

**Compression ratio: 1.81× (37.5 GB → 20.8 GB).** Confirmed at both
Denmark (486 MB → 268 MB, same ratio) and Europe. The 3–4× estimate
was wrong: absolute lat/lon values (first ref per way) remain 5-byte
varints, and typical 1-km-scale deltas are 2–3 bytes. This format's
ceiling is ~1.8×, not higher.

**Correctness: SHA256 match** between baseline and prototype output
PBFs on Denmark. The byte-copy de-interleave in stage 4 is exactly
equivalent to the mmap-read + delta-encode path.

**Interpretation of the −72 s Europe regression.** Stage 4 genuinely
saves ~11 s wall (coord_read 62 s → 13 s, plus zero delta_encode).
The transform pass costs 65 s wall end-to-end, dominated by the
20.8 GB sequential output write to NVMe. This is the prototype tax
— if stage 3 emitted coord_payloads directly (integrated design),
this pass goes away.

**Net projection if integrated:**

- Europe: 392 s → ~373 s (−5%). Marginal.
- Planet (scaling the measured stage-4 coord-work saving):
  982 s → **~900 s (−8%)**.

Substantially less than the 15% I projected when assuming 3–4×
compression. Closer to my worst-case "might be ±0" from the
same-day regression lesson.

**What the prototype answered:**

1. Format is sound; bit-identical output.
2. Compression ratio is 1.81×, not 3–4×.
3. Stage 4 I/O reduction works: coord read cumul dropped 5×.
4. `s4_way_delta_encode_ms` can be eliminated entirely via
   wire-format-ready payloads.
5. Best-case integrated win is ~8% on planet, not ~15%.

**Resolved 2026-04-14: integrated as default.** After walking the
architectural option table (dense, sparse, LocationsOnWays input,
streaming, spatial partition, chunk-parallel, hybrid), coord_payloads
was the only candidate that was measured, credible, and uncontested
under the (27 GB RAM, consumer NVMe, standard-format PBF) envelope.
Rank-bucketed external join is the local architectural optimum;
coord_payloads is the last remaining incremental direction inside
that architecture.

**Shipping measurement (commit `3d977a0`).**

Final integrated-as-default results on plantasjen, same-day baseline
comparisons:

| | Baseline | Integrated default | Δ |
|---|---|---|---|
| Europe `e151e5e8` / `768d3d4e` | 392.7 s | **400 s** | **+7 s (+1.8%)** |
| Planet `b55b5605` / `c021dd91` | 982 s | **953 s** | **−29 s (−3.0%)** |

Planet scale delivered an unexpected wall-time *win*, not just parity.
Reason: at planet scale the 99 GB coord_slots mmap thrashes harder
than at Europe (37 GB), so eliminating it gives back real wall time
on top of the CPU and I/O wins.

**Europe stage breakdown (UUID `768d3d4e`, commit `3d977a0`):**

| Stage | Baseline | Integrated | Δ |
|---|---|---|---|
| Stage 1 | 81 s | 79 s | −2 s |
| Stage 2 | 87 s | 90 s | +3 s |
| Stage 3 | 51 s | 42.5 s | **−8.5 s** (no coord_slots pwrite) |
| Finalize | — | 26.5 s | +26.5 s (new) |
| Stage 4 | 141 s | 129 s | **−12 s** (smaller coord read, no delta-encode) |

**Planet stage breakdown (UUID `c021dd91`, commit `3d977a0`):**

| Stage | Baseline | Integrated | Δ |
|---|---|---|---|
| Stage 1 | 231 s | 232 s | +1 s |
| Stage 2 | 235 s | 212 s | −23 s |
| Stage 3 | 154 s | 108 s | **−46 s** (no coord_slots pwrite) |
| Finalize | — | 68 s | +68 s (new) |
| Stage 4 | 291 s | 259 s | **−32 s** |

**Non-wall-time benefits (all measured on planet, UUID `c021dd91`):**

| Metric | Baseline planet | Integrated planet | Δ |
|---|---|---|---|
| coord_slots file | 99 GB | 0 (not created) | eliminated |
| coord_payloads file | — | 54.8 GB | (replaces coord_slots) |
| Scratch peak | ~300 GB | ~256 GB | **−44 GB** |
| `s3_bytes_written` (coord_slots pwrite) | 99 GB | 0 | eliminated |
| `s4_majflt_delta` | 555,141 | 3,256 | **−99.4%** |
| `s4_minflt_delta` | 3,170,288 | 1,026,905 | −68% |
| `s4_way_delta_encode_ms` cumul | 68,582 | 0 | eliminated |
| Stage 4 mmap virtual | 99 GB | — | eliminated |

The 99 GB coord_slots mmap across 6 workers was the dominant cause
of cross-workload page-cache disruption in the baseline; integrated
replaces it with bounded per-blob preads into ~6 MB worker buffers.
Stage 4's major-fault count dropped by two orders of magnitude.

**Verification**: `brokkr verify add-locations-to-ways --dataset
denmark --mode external` with and without `PBFHOGG_COORD_SLOTS=1`
(the escape hatch) produce bit-identical verify logs — integrated
default and pre-integration paths yield byte-for-byte identical
output PBFs.

**Ship structure (commits):**

1. `77490b7` — extract per-blob delta-encode helper (Stage 1 of plan).
2. `c96566f` — blob↔slot-bucket classification helper (Stage 2).
3. `c12a642` — dual-emit stage 3 + finalize pass behind
   `PBFHOGG_COORD_PAYLOADS_INTEGRATED=1` env var (Stage 3).
4. `3d977a0` — default flip to integrated, remove prototype transform
   and `PBFHOGG_COORD_PAYLOADS_INTEGRATED` / `PBFHOGG_COORD_PAYLOADS_PROTOTYPE`
   env vars, add `PBFHOGG_COORD_SLOTS=1` pre-integration escape hatch
   (Stage 5).
5. **Stage 6 cleanup (same-day):** dropped the `PBFHOGG_COORD_SLOTS`
   escape hatch and the entire `CoordSlots` struct + mmap path. Stage
   3's `shared_file`/`/dev/null` dummy gone — the function no longer
   touches coord_slots at all. Stage 4's `assemble_block` Way branch
   becomes a hard error (it was already unreachable under the
   sorted-PBF + indexed-PBF requirement). Counters renamed
   `s3_integrated_*` → `s3_*`. Inner `EXTJOIN_STAGE3_INTEGRATED_*`
   markers dropped (outer `EXTJOIN_STAGE3_START/END` cover it). Net
   −271 source lines; `brokkr verify --mode external` Denmark log
   bit-identical to pre-Stage-6.

The stability-window gate was waived: same-day cleanup felt safer
than carrying a months-long escape hatch through unrelated changes,
and the implementation is fully covered by the verify diff.

Key architectural changes:
- **COO pair format**: `(node_id, slot_pos)` → `(rank, slot_pos)`. Dense rank
  space enables O(n) counting sort instead of O(n log n) comparison sort on
  sparse i64 node IDs.
- **Two-pass stage 1**: pass A builds shared atomic `IdSetDense` of referenced
  node IDs; pass B emits rank-bucketed records with `slot_pos` pre-computed
  from sidecar prefix sums. Workers are fully independent — no global
  sequential allocator.
- **Wire-format way reframe**: stage 4 way blobs skip full PrimitiveBlock
  decode + BlockBuilder re-encode. Only decodes field 8 (refs) to count refs
  and look up coords; appends fields 9+10 (lat/lon) directly. Saves ~40% of
  way assembly CPU. Node/relation blobs stay on full decode path.
- **IdSetDense::rank_if_set()**: fused get+rank in one lookup, eliminating
  double chunk traversal in the merge loop.
- **File-backed coords_by_rank**: dense `(lat, lon)` array written by rank
  during a parallel node pass overlapped with stage 1B. Stage 2 reads one
  contiguous coord slice per bucket via pread. Eliminates streamed node merge.

Stage 4 sub-phase profiling (Europe, commit `b99af0c`, UUID `d7a08d2f`):
- `s4_way_coord_read_ms`: 532s cumulative (mmap access)
- `s4_way_delta_encode_ms`: 21s (zigzag + varint encode + Vec push)
- `s4_way_ref_parse_ms`: 12s (varint skip to count refs)
- `s4_way_reassemble_ms`: 11s
- `s4_way_parse_way_ms`: 11s
- `s4_majflt_delta`: 374K major faults (MADV_SEQUENTIAL), 197K (no advice)
- Volume: 4.69B refs across 453M way messages

The remaining stage 4 cost is memory-fault bound at ~141s on this hardware.
Extensively tested: per-blob pread, contiguous partitioning, MADV_RANDOM,
no-advice — none moved wall time. The coord_slots mmap access pattern (6
workers × 37 GB × scattered reads) is a structural bottleneck. Breaking it
requires a fundamentally different coord representation, not read-primitive
or scheduling changes.

### Temp disk (rank-bucketed architecture)

| Structure | Count | Entry size | Total |
|-----------|-------|------------|-------|
| Rank records | 4.69B (Europe) | 16 bytes | ~75 GB |
| Coord slots | 4.69B | 8 bytes | ~37 GB |
| Slot buckets (temp) | 256 | varies | ~37 GB |

Peak temp disk: ~112 GB (Europe). After cleanup: 37 GB (coord slots only).

## Implementation

`src/commands/external_join.rs`. Correctness verified identical to dense output
on Denmark (10,175,884 elements, 0 differences) and cross-validated against
osmium via `brokkr verify add-locations-to-ways`.
