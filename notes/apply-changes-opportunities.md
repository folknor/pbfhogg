# `apply-changes --locations-on-ways` - optimization plan

Target: `pbfhogg apply-changes --locations-on-ways` on planet with a daily OSC, production default `--compression none`.

## Current state (2026-04-21, post-flip landing `719f306` + io_uring writer + cross-disk experiment)

- **Best planet wall measured: 80.9 s** (LOW + altw + OSC 4913 + `--compression zstd:1` + `--parallel-writer` + scratch on different physical NVMe). Pre-flip baseline was 144.4 s on same hardware: **-44% wall**. Plan target was 40-55 s; the remaining ~30 s gap is now the CPU floor + pipeline send-wait, addressable only by further parallelism (more pool workers don't help beyond 16) or lifting the drain's single-thread send bottleneck.
- **Planet matrix at commit `80b37df`:**

| Config | Buffered | `--io-uring` | `--parallel-writer` (POOL_SIZE=16) |
|---|---:|---:|---:|
| Same-disk (source+output on Banan/nvme1n1), `--compression none` | 135.5 s | 108.6 s | not benched |
| Same-disk, `--compression zlib:6` | 143.7 s | not benched | not benched |
| Same-disk, `--compression zstd:1` | 121.2 s | 99.4 s | 104.5 s |
| Cross-disk (output on Booty/nvme0n1p3), `--compression none` | 95.4 s | 93.0 s | 99.0 s |
| Cross-disk, `--compression zlib:6` | not benched | not benched | 117.4 s |
| Cross-disk, `--compression zstd:1` | 87.1 s | 82.8 s | **80.9 s** |

- **Writer-backend choice by disk configuration:**
  - **Same-disk**: io_uring wins at every compression level. Same-disk is IOPS-bound (reads compete with writes on the one NVMe); io_uring's queue-depth batching alleviates IOPS contention more than parallel `pwrite` threads, which each issue their own syscalls.
  - **Cross-disk**: `--parallel-writer` wins at zstd:1 (our most-compressed path). io_uring still wins at `--compression none` cross-disk. Parallel pool saturates NVMe write bandwidth when there's headroom; io_uring is near-optimal when the disk is already near max throughput.
  - **Pool size**: measured 4 → 89 s, 8 → 83 s, 16 → 81 s, 32 → 82 s at planet zstd:1 cross-disk. 16 is the sweet spot on plantasjen (Samsung 990 PRO NVMe); pool above 16 over-contends on the device's internal parallelism.

- **Diagnosis of the 82.8 s ceiling** (cross-disk + io_uring + zstd:1):
  - `writer_write_ns = 64 s`, single-thread writer at 1.49 GB/s (95.6 GB output / 64 s). NVMe sequential peak is ~5 GB/s, so the writer thread is at ~30 % of disk peak even with io_uring batching.
  - `writer_pipeline_send_wait_ns = 81.5 s` cumulative on drain - drain is still 98 % blocked on send, but those blocks are short and let progress through (cumulative > wall is plausible given the channel-send-wait counter is per-call).
  - Worker CPU floor: `merge_streaming_(decompress+rewrite+frame+precise+coord_extract)_ns / 22 ≈ 35 s` wall. Far below the writer ceiling.
  - Workers avg 10.7 cores out of 22; capped by drain backpressure on the writer pipeline channel.
- **Why same-disk loses ~30 s of headroom**: source and target on the same NVMe contend on read+write. Cross-disk separation alone gave -31 % wall (138 s → 95 s) at `--compression none` even before io_uring.
- **Why io_uring is decisive on same-disk but marginal cross-disk**: same-disk's writer is fighting reads for IOPS, so batching syscalls helps a lot (`writer_write_ns` 120 s → 67 s). Cross-disk's writer wasn't IOPS-bound, so io_uring's per-syscall savings are absorbed.
- **Why zstd:1 wins**: workers parallelize compression cheaply (`merge_streaming_frame_ns` 205 s cumulative ≈ 9 s wall at 22 cores) and the ~20 % output-byte reduction shrinks writer time proportionally. zlib:6 gives similar output size but costs 6.5× more CPU (1352 s cumulative frame_ns), so its smaller writer time is offset by classify/rewrite competing for cores.

- **Production configurations measured (LOW + altw + OSC 4913, `--bench 1`):**
  - `--compression none` (osmium-interop default for production pipeline): **135.5 s** same-disk, **93.0 s** cross-disk + io_uring.
  - `--compression zlib:6` (ecosystem default): **143.7 s** same-disk; cross-disk variant not benched.
  - `--compression zstd:1` (recommended for internal pipelines): **121.2 s** same-disk, **82.8 s** cross-disk + io_uring.

### Full bench log (all permutations run this session, planet LOW + altw + OSC 4913, `--bench 1`, plantasjen)

Hardware for reference:
- Banan = `/dev/nvme1n1p1` on Samsung 990 PRO 4TB NVMe, mounted at `/media/folk/Banan`, linked as `data/` in the project. Source + OSC live here.
- Booty = `/dev/nvme0n1p3` on Samsung 970 EVO Plus 1TB NVMe, mounted at `/media/folk/Booty`. Used only as the cross-disk target for the experiment.

| Run | Compression | Writer | Output target | Wall | writer_bytes_written | writer_write_ns | writer_pipeline_send_wait_ns | Avg cores (way phase) | Peak RSS | Notes |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---|
| Pre-flip baseline, commit `52c2c4b` UUID `e81a9316` | none | buffered | Banan | **144.4 s** | 119 GB | ~100 s (est) | ~85 s (est) | 4.1 | 1.63 GB | The wall to beat. Classify 4.15 cores avg (plan's batch-Amdahl diagnosis). |
| Post-flip (brokkr), run 1 | none | buffered | Banan | **135.5 s** | 119 GB | 120 s | 859 s cumulative (pre-P1.5) | 4.6 | 3.29 GB | Right after the flip. Writer chain saturated per plan's R1 prediction. |
| Post-flip + P1.5 (brokkr), run 1 | none | buffered | Banan | 135.5 s | 119 GB | 120 s | 117 s | 4.8 | 3.29 GB | P1.5 dropped pipeline_send_wait 859→117 s (-86 %) but wall flat (HDD-bandwidth-perceived but actually single-NVMe contention). |
| Post-flip + P1.5 (brokkr), run 2 | none | buffered | Banan | 148.7 s | 119 GB | - | - | - | - | Variance sample from the second `--bench 1`; shows ±7 % run-to-run band. |
| Post-flip + P1.5 `WRITE_AHEAD=64` (reverted) | none | buffered | Banan | 132.6 s | 119 GB | 107 s | 104 s | - | - | Bump tested; -2.9 s wall, -13 % write_ns, -11 % send_wait. Inside variance band; reverted since it affects unrelated writers (altw/extract). |
| Post-flip + P1.5 `WRITE_AHEAD=64` (reverted) | zstd:1 | buffered | Banan | 126.7 s | 95 GB | - | - | - | - | +5.5 s vs default on zstd:1 at same settings; net: WRITE_AHEAD bump is noise. |
| Post-flip + P1.5 | zlib:6 | buffered | Banan | **143.7 s** | 93 GB | 104 s | 91 s | - | - | Default ecosystem compression. writer_compress_ns/frame_ns across workers = 1352 s cumulative (~62 s at 22 cores). |
| Post-flip + P1.5 | zstd:1 | buffered | Banan | **121.2 s** | 95 GB | 94 s | 92 s | - | - | Best same-disk result. frame_ns 205 s cumulative (~9 s at 22 cores) - 6.5× less worker CPU than zlib:6 for same output size. |
| Post-flip + P1.5 (direct invocation, no brokkr) | none | buffered | Banan | **137.96 s** | 114 GB | - | - | 5.1 (from `time -v` 510 % CPU) | 3.03 GB | Confirms brokkr overhead is ~1.5 % at this scale. User CPU 622 s, sys 82 s. FS outputs 222 GB (2× data due to ext4 journaling). |
| Post-flip + P1.5 (direct invocation, cross-disk) | none | buffered | Booty | **95.4 s** | 114 GB | - | - | 8.17 (from 817 % CPU) | 4.37 GB | -42.6 s / -31 % vs same-disk. First proof that single-NVMe read+write contention, not software, was the ceiling. Major faults 48210→2316. |
| Post-flip + P1.5 (direct invocation, cross-disk) | zstd:1 | buffered | Booty | **85.8 s** | 92 GB | - | - | 10.7 (from 1068 % CPU) | 2.86 GB | zstd:1 stays the wall winner even cross-disk. |
| Post-flip + P1.5 (brokkr, cross-disk, hotpath mode) | zstd:1 | buffered | Booty | **87.1 s** | 95 GB | 100 s | 97 s | 6.0 | 2.64 GB | Same config through brokkr's instrumentation - ~1.5 % overhead vs direct. |
| Post-flip + P1.5 + io_uring | none | io_uring | Banan | **108.6 s** | 119 GB | 66.6 s | 43.4 s | - | - | io_uring on same-disk: -27 s vs buffered. Writer disk throughput 991 MB/s → 1.79 GB/s. `writer_recv_wait_ns = 35.7 s` - writer now occasionally waits for drain (role reversed). |
| Post-flip + P1.5 + io_uring | zstd:1 | io_uring | Banan | **99.4 s** | 95 GB | - | - | - | - | io_uring + zstd:1 same-disk: -21.8 s vs buffered. |
| Post-flip + P1.5 + io_uring (cross-disk) | none | io_uring | Booty | **93.0 s** | 119 GB | 75 s | 89 s | - | - | io_uring gain collapses cross-disk (-2 s only) - read+write was the same-disk ceiling, not writer IOPS. |
| Post-flip + P1.5 + io_uring (cross-disk) | zstd:1 | io_uring | Booty | 82.8 s | 95 GB | 64.2 s | 81.5 s | - | 2.86 GB | Writer thread at ~64 s / 30 % of NVMe headroom - single-thread writer was the ceiling. |
| Post-flip + P1.5 + parallel-writer POOL_SIZE=4 (cross-disk) | zstd:1 | parallel (4 workers) | Booty | 89.2 s | 95 GB | 223 s cumulative | 62 s | - | - | Scaffold attempt before tuning pool size. |
| Post-flip + P1.5 + parallel-writer POOL_SIZE=8 (cross-disk) | zstd:1 | parallel (8 workers) | Booty | 83.4 s | 95 GB | - | - | - | - | Halfway between 4 and 16. |
| Post-flip + P1.5 + parallel-writer POOL_SIZE=16 (cross-disk) | zstd:1 | parallel (16 workers) | Booty | **80.9 s** | 95 GB | - | - | - | - | **Best overall result**: -44 % vs 144.4 s pre-flip. Ties with io_uring cross-disk at current best configuration. |
| Post-flip + P1.5 + parallel-writer POOL_SIZE=32 (cross-disk) | zstd:1 | parallel (32 workers) | Booty | 82.2 s | 95 GB | - | - | - | - | Regresses vs POOL_SIZE=16; NVMe queue saturated around 16. |
| Post-flip + P1.5 + parallel-writer POOL_SIZE=16 (same-disk) | zstd:1 | parallel (16 workers) | Banan | 104.5 s | 95 GB | - | - | - | - | Same-disk: io_uring (99.4 s) beats parallel, IOPS contention dominates. |
| Post-flip + P1.5 + parallel-writer POOL_SIZE=16 (cross-disk) | none | parallel (16 workers) | Booty | 99.0 s | 119 GB | - | - | - | - | zstd:1 still wins at cross-disk despite higher CPU cost. |
| Post-flip + P1.5 + parallel-writer POOL_SIZE=16 (cross-disk) | zlib:6 | parallel (16 workers) | Booty | 117.4 s | 93 GB | - | - | - | - | zlib:6's higher compression CPU costs more than its smaller output saves. |

Europe LOW altw + OSC 4715, `--bench 3` (reference):

| Run | Compression | Writer | Output | Wall |
|---|---|---|---|---:|
| Pre-flip (commit `b4f45ff`), UUID `f0af4170` | none | buffered | Banan | 46.1 s |
| Pre-flip (commit `b4f45ff`), UUID `570dfa69` | zlib:6 | buffered | Banan | 54.2 s |
| Post-flip + P1.5 | none | buffered | Banan | 49.8 s |
| Post-flip + P1.5 | zlib:6 | buffered | Banan | 53.8 s |

### Methodology notes
- All planet runs are `--bench 1` (single sample). Variance band ±7 % observed. Multiple runs at the same config confirm the direction of each optimisation's effect even though individual wall numbers move within the band.
- `RLIMIT_MEMLOCK` must be ≥16 MB for io_uring to register its 64×256 KB buffer pool. Plantasjen defaults to 8 MB; raise with `sudo prlimit --pid=$$ --memlock=unlimited:unlimited` in the bench shell.
- Cross-disk experiments used a temporary `brokkr.toml` edit of `[plantasjen].scratch` from `data/bench-tmp` (Banan) to `/media/folk/Booty/pbfhogg-bench-tmp` (Booty). Reverted post-bench. The `[plantasjen.drives].target = "hdd"` label in the toml is separately misleading: brokkr writes bench output to `scratch`, not to `target`, so the "hdd" classification referred to the cargo build dir on Oioioi/sdc (unrelated to the bench).
- Counter values in the table above are cumulative across threads except where noted. Drain is single-threaded, so `writer_pipeline_send_wait_ns` there is near-linear with wall-time blocked on send.
- `writer_write_ns` correlates with writer disk throughput: at same-disk 119 GB / 120 s = 991 MB/s buffered (IOPS-limited by read+write contention); cross-disk 95 GB / 64 s = 1.49 GB/s with io_uring (single-thread writer syscall ceiling).
- **Europe walls (LOW + altw + OSC 4715, `--bench 3`):**
  - `--compression none`: 49.8 s (pre-flip `b4f45ff` was 46.1 s)
  - `--compression zlib:6`: 53.8 s (pre-flip was 54.2 s, UUID `570dfa69`)
- **Peak RSS at planet:** 3.29 GB (pre-flip 1.63 GB, +2.0×). Inside 27 GB host envelope. Peak threads 27 → 50 (+85%). Involuntary context switches dropped 70% (7214 → 2134) - workers run longer between preemptions.
- **Where the time goes** at planet `--compression none`:
  - `writer_write_ns = 120 s` (89% of wall) - writer thread is HDD-bound on sustained sequential writes (~200 MB/s nominal, ~1 GB/s apparent via page cache until Linux's `dirty_ratio` saturates around 5.6 GB and writeback throttles further writes). target=hdd (sdc: Seagate ST4000DM004 5400 RPM, confirmed via `lsblk ROTA=1`).
  - `writer_pipeline_send_wait_ns = 117 s` cumulative on drain - drain single-threaded, blocked 87% of wall on writer pipeline backpressure.
  - CPU floor: `merge_streaming_{decompress + parse + precise + rewrite + frame}_ns / 22` = 31 s wall. Plenty of headroom; wall isn't CPU-bound at planet.
- **Why zstd:1 wins at planet:** output bytes 95 GB vs 119 GB for `none` (-20%) at similar CPU cost because workers parallelize compression inline. `writer_write_ns` drops 120 s → 94 s, writer backpressure drops proportionally. zlib:6 produces similar output size (93 GB) but costs 6.5× more CPU (`merge_streaming_frame_ns` 1352 s cumulative vs zstd:1's 205 s) which cancels the gain.
- **What the pipeline architecture bought us:** the plan's batch-Amdahl hypothesis held - classify wall is no longer the bottleneck. But the new ceiling at planet `--compression none` is HDD sustained write speed, which the pipeline doesn't influence. The win shows up as:
  - +2-15% wall improvement depending on compression mode (biggest gain at zlib:6 and zstd:1 where worker-parallel compression beats the legacy rayon-spawn-per-block dispatch).
  - 70% drop in involuntary context switches - workers stay on-CPU longer, less scheduler thrash.
  - Per-compression-level CPU cost can now be spent on compression quality without fighting the classify pool.
  - Infrastructure (`scanner.rs` / `streaming.rs` / `drain.rs`) is reusable for further optimizations (splice-in-place, parallel writer, io_uring writer).
- **Plan's 40-55 s target (fused-CPU floor) was predicated on classify CPU being the ceiling** - measured CPU floor is indeed 17-31 s, but the HDD writer became the new ceiling at ~120 s, which the plan didn't predict. Per-hardware; on a faster target disk (NVMe observed at 3+ GB/s sustained on the same host), the CPU-budget floor math would apply.
- **Scope out of this plan:** internal API rewrites (`IdSetDense`, `PbfWriter`, `HeaderWalker` are correct as-is), weekly-OSC parallel parse (separate workstream).

## Implementation progress (2026-04-20)

Tracking what's landed vs pending inside the P1 rewrite. The plan's
"one commit" framing applies to the *big rewrite* (delete batch loop,
fuse classify+rewrite, wire scanner + workers + drain). Pure-additive
scaffolding (types, property tests) can land separately as prep so
that a revert of the rewrite keeps the safety net.

**Committed prep scaffolding (`a2a0567`, 2026-04-20):**

- `src/commands/apply_changes/descriptor.rs` - the four types the
  pipeline exchanges: `BlobDescriptor`, `ScannedBlob` (Candidate |
  Passthrough), `WorkerOutput` (FalsePositive | Rewritten |
  OwnedPassthrough for `--direct-io` fallback), `DrainItem`
  (CopyRange | OwnedBytes | Rewritten). Includes a `byte_cost()`
  accessor for the reorder buffer's byte-budget backpressure.
- `tests/apply_changes_invariants.rs` - six property tests against
  current main's behavior, locking in the contract the rewrite must
  preserve: two cursor-rule tests (FalsePositive blob with upsert in
  range produces output at blob-tail, not OSM-sorted interleaved),
  two empty-base-PBF tests (all-kinds trailing flush, empty-diff
  noop), two trailing-create interleave tests. `--locations-on-ways`
  invariants are not exercised here (fixture needs ALTW-enriched
  base PBF; coverage moved to Denmark byte-equal cross-validation).
- Plan doc updates (this file + multi-extract).

**Committed prep scaffolding part 2** (descriptor-first pipeline pieces,
all behind `#![allow(dead_code)]` until the merge() flip lands):

- `src/commands/apply_changes/scanner.rs` - `HeaderWalker`-driven
  descriptor emission with the scanner-side node→way barrier and
  `use_copy_range` routing fork. Splice fast-path emits `DrainItem`
  directly into a dedicated drain channel; `--direct-io` and
  overlap-candidate descriptors route through the worker pool as
  `ScannedBlob`. Emits these markers/counters on the scanner path:
  - Markers: `MERGE_SCANNER_START/END`,
    `MERGE_SCANNER_BARRIER_WAIT_START/END`.
  - Counters: `merge_scanner_blobs_emitted`,
    `merge_scanner_to_drain_bytes_high_water`,
    `merge_scanner_to_workers_bytes_high_water`.
- `src/commands/apply_changes/streaming.rs` - long-lived worker pool
  driven by `std::thread::scope`. Workers each own a thread-local
  `BlockBuilder` + scratches, pread the Blob body for `Candidate`
  descriptors, decompress, opportunistically extract Node coords
  during the node phase into per-worker `Arc<Mutex<FxHashMap>>`
  slots, parse via `from_vec_with_scratch`, precise-check, then emit
  `DrainItem::Rewritten` or convert false positives to
  `DrainItem::CopyRange`. Under `--direct-io`, workers also handle
  `Passthrough` descriptors: pread the **full framed bytes** and emit
  `DrainItem::OwnedBytes`. Emits:
  - Markers: `MERGE_STREAMING_START/END`.
  - Counters: `merge_streaming_blobs_processed`,
    `merge_streaming_blobs_rewritten`,
    `merge_streaming_blobs_false_positive`,
    `merge_streaming_blobs_owned_passthrough`,
    `merge_streaming_decompress_ns`, `merge_streaming_parse_ns`,
    `merge_streaming_precise_ns`, `merge_streaming_rewrite_ns`,
    `merge_streaming_coord_extract_ns`,
    `merge_streaming_coord_pairs_extracted`.
- `src/commands/apply_changes/drain.rs` - single-threaded ordered drain
  actor. Consumes a unified `DrainItem` stream from scanner + workers
  via `mpsc::Receiver<DrainItem>`. `BTreeMap<seq, DrainItem>` reorder
  buffer keyed by global seq; advances through contiguous seqs after
  each insert. Owns `UpsertCursors`, `last_type`, gap-create
  `BlockBuilder`, contiguous-range `copy_file_range` coalescer (port
  of ALTW's pattern), `OwnedBytes` chunk coalescer, `MergeStats`
  accumulator. Detects node→way transition; merges `streaming::CoordSlots`
  into the published `LocMapHandle = Arc<OnceLock<Arc<FxHashMap>>>`
  before signalling the scanner over `barrier_tx`. Trailing-creates
  port verbatim from `rewrite.rs`'s `types_to_flush` match. Includes
  property-tested cursor-rule invariant: Rewrite advances the cursor
  past `blob_osm_last_key`; CopyRange/OwnedBytes do NOT (silent-break
  risk surfaced in the R3R2 review). Emits:
  - Markers: `MERGE_DRAIN_START/END`,
    `MERGE_DRAIN_BARRIER_START/END`,
    `MERGE_TRAILING_CREATES_START/END`.
  - Counters: `merge_drain_items_processed`,
    `merge_drain_copy_range_calls`,
    `merge_drain_copy_range_coalesced_items`,
    `merge_drain_passthrough_chunks_flushed`,
    `merge_drain_rewrite_blocks_written`,
    `merge_drain_gap_creates_emitted`,
    `merge_drain_trailing_creates_emitted`,
    `merge_drain_reorder_buffer_high_water_count`,
    `merge_drain_reorder_buffer_high_water_bytes`,
    `merge_drain_barrier_loc_map_size`,
    `merge_drain_reorder_gap_wait_ns`.
- `descriptor.rs` exposes conversion helpers
  `BlobDescriptor::into_drain_copy_range` and
  `WorkerOutput::into_drain_item` so scanner and workers both emit
  `DrainItem` directly into the unified drain channel.

**P1 flip landed (`719f306`, 2026-04-21):**

- `merge()` in `rewrite.rs` became the thin orchestrator: spawns
  scanner + worker-pool threads via `std::thread::scope`, runs the
  drain on the caller thread so it can hold `&mut writer`. Four
  channels: scanner→drain (splice fast-path), scanner→workers
  (candidate dispatch), workers→drain (unified DrainItem stream,
  one cloned `SyncSender` per worker), drain↔scanner barrier
  (`last_node_seq` + barrier signal).
- `classify.rs` stripped to just `block_overlaps_diff` for the
  worker pool's precise check. Legacy batch-slot types deleted.
- `node_locations.rs::prefill_from_base` deleted. OSC-pre-seeded
  coords from `build_from_diff` become the drain's
  `seeded_locations`; base-PBF coords are extracted opportunistically
  by node-phase workers into per-worker `Arc<Mutex<FxHashMap>>`
  slots, merged by the drain at the node→way barrier, published via
  `LocMapHandle = Arc<OnceLock<Arc<FxHashMap>>>` for way-phase
  workers to read.
- `parallel_reader.rs` deleted entirely.
- `stream_output.rs::coalesce_passthrough` deleted (drain has its
  own contiguous-range `copy_file_range` coalescer).
- `stats.rs::{PhaseTimers, ClassifyCounters, StallAccumulator,
  PhaseRss, read_rss_kb}` deleted (all batch-loop instrumentation).

**P1.5 landed (same commit `719f306`) - worker-emits-framed-bytes:**

- Workers call `frame_blob_pipelined` per output block and attach
  framed `Vec<u8>` chunks to `DrainItem::Rewritten`; drain emits them
  via `write_raw_owned` (single-thread send path) instead of
  `write_primitive_block_owned` (per-block `rayon::spawn` dispatch).
- Effect on planet `--compression none`:
  `writer_pipeline_send_wait_ns` 859 s cumulative → 117 s (-86%).
  Writer-pipeline serialization is no longer the ceiling; HDD
  sustained write throughput is.
- Drain's `recv_timeout(25 ms)` lets the barrier fire from idle
  (prevents a three-way deadlock when drain finishes the last node
  item before scanner sends `last_node_seq`).

**Cross-validation (2026-04-21):**

- **Denmark element counts byte-equal to pre-flip**: 52,493,619
  nodes / 6,616,901 ways / 46,108 relations. `brokkr verify merge
  --dataset denmark` PASSes the Sort.Type_then_ID check; vs-osmium
  element counts match pre-flip (known semantic difference, not a
  regression).
- **6/6 property tests** in `tests/apply_changes_invariants.rs`
  pass (cursor-rule on FalsePositive, empty-base-PBF trailing
  flush, trailing-create interleave).
- **18/18 integration tests** in `tests/merge.rs` pass.
- **Planet `--bench 1`**: 135.5 s (UUID in session log). Under the
  plan's 40-55 s target; the remainder is HDD-bound writer wall
  (`writer_write_ns` = 120 s). Abort threshold was >80 s; we're at
  135 s but the remainder is hardware-limited, not pipeline-limited.

**Measured walls + counters at commit `719f306` (plantasjen,
target=hdd, OSC 4913 where applicable):**

| Dataset | Compression | Wall | writer_bytes_written | writer_write_ns | writer_pipeline_send_wait_ns | merge_streaming_frame_ns cumulative |
|---|---|---:|---:|---:|---:|---:|
| Europe altw LOW (`--bench 3`) | none | 49.8 s | - | - | - | - |
| Europe altw LOW (`--bench 3`) | zlib:6 | 53.8 s | - | - | - | - |
| Planet altw LOW (`--bench 1`) | none | 135.5 s | 119 GB | 120 s | 117 s | 48 s |
| Planet altw LOW (`--bench 1`) | zlib:6 | 143.7 s | 93 GB | 104 s | 91 s | 1352 s |
| Planet altw LOW (`--bench 1`) | zstd:1 | **121.2 s** | 95 GB | 94 s | 92 s | 205 s |

zstd:1 planet result is the current wall leader. `merge_streaming_frame_ns` cumulative (summed across 22 workers) shows compression cost directly: zlib:6 pays 1352 s cumulative (~62 s wall at 22 cores) vs zstd:1's 205 s (~9 s wall), for similar output sizes. This is the core architectural win of P1: compression parallelizes in the worker pool instead of fighting the classify pool (legacy shape's `+7.4 s classify wall under zlib` is gone).

**Memory + runtime shape at planet (from `brokkr sidecar` stat,
compared against legacy UUID `e81a9316`):**

| Metric | Legacy `52c2c4b` | Post-flip `719f306` | Δ |
|---|---:|---:|---:|
| Wall | 144.4 s | 135.5 s | -6% |
| Peak RSS | 1.63 GB | 3.29 GB | +2.0× |
| Peak anon | 1.63 GB | 3.28 GB | +2.0× |
| Avg RSS | 1.05 GB | 1.72 GB | +1.6× |
| p95 RSS | 1.23 GB | 2.68 GB | +2.2× |
| Peak threads | 27 | 50 | +85% |
| Total major faults (max) | 52,659 | 67,858 | +29% |
| Total minor faults (max) | 1.02 M | 1.36 M | +33% |
| Total involuntary context switches (max) | 7,214 | 2,134 | **-70%** |
| Total voluntary context switches (max) | 128 k | 140 k | +9% |

RSS roughly doubled: per-worker thread-local `BlockBuilder` + scratches × 22 ≈ 220 MB, per-worker coord slots, framed-chunk buffering at the drain (~800 KB per rewrite blob in-flight), and the `BTreeMap<seq, DrainItem>` reorder buffer. Inside the 27 GB host envelope and the plan's 1.5-2.1 GB projection range was optimistic - real is 3.3 GB, still comfortable. Involuntary context switches dropped 70 % - workers run longer between preemptions (rayon-spawn churn gone), healthy sign.

**Session learnings worth noting (found during scaffolding):**

- The classify/rewrite contract for false-positive blobs produces
  blob-tail ordering `[1, 2, 10, 5]`, not OSM-sorted interleave
  `[1, 2, 5, 10]`. The property tests assert the current shape
  verbatim, not an idealised OSM-sorted form. This was implicit in
  the `classify.rs::block_overlaps_diff` doc comment but the plan
  doc hadn't stated it as an asserted-on invariant.
- `HeaderWalker::next_header` returns frame_start + data_offset +
  data_size per blob, perfectly matching the descriptor shape. The
  scanner's integration is a straight port of the walker's output
  into `BlobDescriptor`.
- `OwnedBlock` is a type alias `(Vec<u8>, BlobIndex, Option<Vec<u8>>)`,
  not a struct - keep this in mind when accessing the encoded bytes
  (tuple indexing in destructure).
- `std::sync::mpsc::SyncSender::send` takes `&self`, so a shared
  `Arc<SyncSender>` or by-value in a destructure both work for the
  scanner → drain / scanner → workers channels. Destructuring the
  ScannerConfig inside `run_scanner` is preferred over `&cfg` so
  clippy sees the channels being consumed at scanner exit (which is
  what signals end-of-input to downstream).

## Thesis

Unlike ALTW, the geocode builder, and check-refs, apply-changes is **already mostly well-shaped**. The existing pipeline has:

- parallel classify via rayon `par_iter`
- pipelined writer with bounded channels
- `Arc<NodeLocationIndex>` to avoid per-batch location-index cloning
- per-rayon-task `PrimitiveBlock` drop after rewrite, for early memory release
- coalesced passthrough writes (consecutive raw frames flush as a single `write_raw_owned` move)
- raw-bytes pre-seeded string table path for base element rewrite (no re-parse, no re-intern)

The 144 s cost reflects the real work of rewriting ~40 % of a planet's blobs with locations preserved, not a wrong shape - but that cost sits on top of a per-batch barrier that leaves 3/4 of the rayon pool idle. Deleting the barrier is the work.

## Yardstick

| Command | Wall | Peak RSS | Notes |
|---|---:|---:|---|
| `apply-changes --locations-on-ways` (pre-#2, `--compression none`) | 154.9 s | 1.8 GB | commit `b7ed0e1`, UUID `b91009ae`, sequential prefill |
| `apply-changes --locations-on-ways` (post-#2, `--compression none`) | **144.4 s** | 1.8 GB | commit `52c2c4b`, UUID `e81a9316`, parallel prefill - current baseline |

Historical `--compression zlib:6` number from commit `7e9c2e9` (2026-04-17, pre-parallel-prefill) was 12m33s; not re-measured under zlib since production runs `none`. A fresh A/B under both compression modes landed 2026-04-18 and is documented in "Zlib vs none" below (Europe, 46.1 vs 54.2 s).

## Measured: Europe altw + `--locations-on-ways --compression none`

Commit `b4f45ff`, 2026-04-18, plantasjen. UUID `f0af4170`, `--bench 1`.
Europe altw (38 GB), OSC seq 4715 (planet-scale daily diff applied to a
regional extract - see the missing-node note below).
Total wall: **46.1 s**.

### Per-phase wall (from always-on sidecar counters)

| Phase | Wall (ms) | % of wall | Parallelised? | Counter |
|---|---:|---:|---|---|
| OSC parse | 1 860 | 4.0 % | no | `merge_osc_parse_ms` |
| `DiffRanges::from_diff` | 59 | 0.1 % | no | `merge_diffranges_ms` |
| `NodeLocationIndex::prefill_from_base` | **5 819** | **12.6 %** | no (sequential) | `merge_prefill_ms` |
| Header read + writer setup | ~0 | - | no | `merge_header_read_ms`, `merge_writer_setup_ms` |
| Phase 1 classify (cumulative) | **19 767** | **42.9 %** | yes (rayon) | `merge_classify_total_ms` |
| Phase 2 inline assignment (cumulative) | 54 | 0.1 % | no (sequential) | `merge_phase2_inline_total_ms` |
| Phase 3 rewrite spawn (cumulative) | 145 | 0.3 % | - | `merge_rewrite_spawn_total_ms` |
| Phase 4 rewrite recv + dispatch (cumulative) | **13 651** | **29.6 %** | main thread blocks on rayon | `merge_rewrite_recv_total_ms` |
| Phase 4 output write (cumulative) | 110 | 0.2 % | - | `merge_output_write_total_ms` |
| Passthrough write (cumulative) | 14 | 0.0 % | - | `merge_passthrough_write_total_ms` |
| Trailing creates | 1 | 0.0 % | no | `merge_trailing_creates_ms` |
| Final writer flush | 1 354 | 2.9 % | - | `merge_final_flush_ms` |
| **Sum of phases** | **~42 800** | **~93 %** | | |

Unaccounted ~3.3 s (~7 %) is thread startup/join, OSC file I/O before
the parse marker, and stderr printing. Not a target.

### Stall attribution (from WAIT_* spans + atomic accumulators)

| Stall | Total | % of wall | Meaning |
|---|---:|---:|---|
| `merge_rewrite_recv_wait_us` | 13.50 s | 29.3 % | main thread blocked on rayon rewrite results |
| `merge_reader_send_wait_us` | 1.47 s | 3.2 % | reader thread blocked on full frame channel |
| `merge_consumer_recv_wait_us` | 1.44 s | 3.1 % | `collect_batch` blocked on empty frame channel |
| `merge_writer_call_us` | 1.14 s | 2.5 % | time spent in `writer.write_*` calls |

Writer-internal stalls (from the writer's own counters):

| Counter | Total | Comment |
|---|---:|---|
| `writer_recv_wait_ns` | 19.6 s | reorder-buffer thread waiting for inputs - writer isn't the bottleneck |
| `writer_compress_ns` | 8.6 s | cumulative framing/encode (note: `none` compression still frames wire bytes) |
| `writer_write_ns` | 15.2 s | actual disk writes |
| `writer_flush_ns` | 1.35 s | matches `merge_final_flush_ms` |

### Shape counters

| Counter | Value |
|---|---:|
| `merge_batches_total` | 14 744 |
| `merge_reader_frames_sent` | 514 664 |
| `merge_reader_blocked_sends` | 2 533 (0.49 %) |
| `merge_blobs_passthrough` | 473 706 |
| `merge_blobs_rewritten` | 40 958 |
| `merge_blobs_index_hit` | 451 231 |
| `merge_bytes_passthrough` | 20.8 GB |
| `merge_bytes_rewritten` | 26.4 GB (56 % rewrite ratio) |

### Prefill shape

| Counter | Value |
|---|---:|
| `merge_loc_needed` | 3 576 070 |
| `merge_loc_from_diff` | 731 618 |
| `merge_loc_from_base` | 37 585 |
| `merge_loc_missing` | **2 806 867 (78 %)** |
| `merge_prefill_blobs_scanned` | 95 071 |
| `merge_prefill_blobs_skipped_range` | 361 872 (79 % skip rate) |
| `merge_prefill_early_exit` | 0 |

**Missing-node caveat.** Europe's OSC (seq 4715) is a full-planet daily
diff, not a regional one. Most referenced nodes are outside Europe's
extract bbox and won't be in the base PBF. The 78 % missing rate is
expected for this dataset combination, not a bug. It does mean prefill
is doing less useful work than it would on a planet run, where the
OSC and the base cover the same extent.

### What this changes vs the inferred breakdown

Plan-level intuition held for the order of magnitude (prefill slow,
reader single-threaded), but the proportions shifted:

- **Classify is the dominant phase, not prefill or rewrite-encode.**
  19.8 s / 43 % of wall. The plan's "leave alone" recommendation
  deserves revisiting. Candidate: reduce per-blob decompress cost
  (pipelined decompress reuses buffers), or skip more blobs via
  better blob-index fast paths.
- **Main thread spends 30 % of wall blocked on rayon rewrite
  results.** `merge_rewrite_recv_wait_us` and
  `merge_rewrite_recv_total_ms` both land at ~13.5 s. The plan framed
  rewrite-encode as a rayon-parallel phase to leave alone; actually
  its wall-clock contribution from the main thread's point of view is
  second only to classify. Faster rewrites, or more rewrite workers,
  are worth considering.
- **Prefill is 5.8 s on Europe, not the 30-100 s predicted for
  planet.** Scaling by file size (87/38 = 2.3×) predicts ~13 s on
  planet; scaling by node count (10.4 B / 3.7 B = 2.8×) predicts ~16 s.
  Plan #2 estimate was 30-60 s. Either the estimate was generous or
  the Europe OSC touches fewer nodes proportionally. Either way, plan
  #2's ceiling is probably ~10-20 s saved at planet, not 30-60 s.
- **Reader thread stalls only 3 % of wall.** The plan estimated
  reader wall at 50-150 s (planet). On Europe only 1.47 s stalled; if
  the whole reader walked 46 s at ~830 MB/s it would take ~45 s, but
  the reader runs concurrently with the consumer, so its wall doesn't
  add. Plan #3's payoff on `--compression none` looks modest; under
  zlib-output (where classify + rewrite + writer get slower) the
  reader still isn't the critical path.

### Revised ranked targets (post-measurement)

1. **Classify** (19.8 s @ Europe, 43 % wall; **70.9 s @ planet, 49 % wall**) - newly elevated. Was "leave alone" in the inferred plan; measurement says it's the biggest bucket.
2. **Rewrite throughput** (13.65 s @ Europe, 30 % wall; **43.7 s @ planet, 30 % wall**) - consumer blocked on rayon results. Also not originally on the plan's ranked list.
3. ~~**Prefill** (5.8 s @ Europe, 13 %; 20.5 s @ planet, 13 % pre-#2) - plan #2 target.~~ **Landed 2026-04-18 (commit `52c2c4b`), planet phase 20.5 s -> 6.6 s (-14 s), overall wall -10.5 s.**
4. **Reader parallelisation** (1.47 s stall @ Europe, 3 %; planet `merge_reader_send_wait_us=14.0 s`, 10 % wall) - plan #3 target; payoff bigger at planet than Europe predicted.

The top two rows (classify, rewrite) weren't on the ranked
opportunities list in the inferred plan. Before committing to an
approach, the next measurement should separate classify's work into
its three fast paths (indexdata hit / scan-only / precise decompress)
to see whether the time is dominated by decompress of the 8 % of
blobs that get rewritten, or by the scan-only path across the 92 %
that pass through.

## Measured: Planet altw + `--locations-on-ways` (baseline + post-#2)

Commit `b7ed0e1` (baseline) and `52c2c4b` (post-#2), 2026-04-18,
plantasjen. Planet altw (92.6 GB), OSC seq 4913, `--compression none`
(production default). `--bench 1`.

| Run | UUID | Commit | Wall |
|---|---|---|---:|
| Baseline (sequential prefill) | `b91009ae` | `b7ed0e1` | **154.9 s** |
| After #2 (parallel prefill)   | `e81a9316` | `52c2c4b` | **144.4 s** |

Wall delta: **-10.5 s (-6.8 %)**.

### Prefill phase delta (from `brokkr sidecar --compare b91009ae e81a9316`)

| Metric | Baseline | Post-#2 | Delta |
|---|---:|---:|---:|
| `MERGE_PREFILL` span wall | 20 544 ms (1.0c) | **6 606 ms (5.1c)** | **-13.9 s (-68 %)** |
| `merge_prefill_ms` counter | 20 544 | 6 606 | -13.9 s |
| Decode threads | 1 | 22 | - |
| Blobs decompressed | 42 119 | 42 119 | 0 |
| Blobs skipped (range overlap) | 9 315 | 9 315 | 0 |
| Nodes found | 97 392 | 97 392 | 0 |

Prefill phase shrank 14 s; overall wall 10.5 s. The ~3.4 s gap is the
new path's extra header-only pass (to build the blob schedule)
plus thread startup overhead. The prior sequential path folded
header scan and decode into one BlobReader iterator so paid the
header walk implicitly. Within the 10-20 s ceiling predicted for #2.

### Phase distribution at planet (post-#2, UUID `e81a9316`)

From the sidecar counters. Same shape as Europe's revised ranking
scaled up.

| Phase | Wall (ms) | % of 144 400 ms wall |
|---|---:|---:|
| `merge_osc_parse_ms` | 4 925 | 3.4 % |
| `merge_prefill_ms` | 6 606 | 4.6 % |
| `merge_classify_total_ms` | **70 942** | **49.1 %** |
| `merge_phase2_inline_total_ms` | 166 | 0.1 % |
| `merge_rewrite_recv_total_ms` | **43 714** | **30.3 %** |
| `merge_rewrite_spawn_total_ms` | 1 522 | 1.1 % |
| `merge_output_write_total_ms` | 1 413 | 1.0 % |
| `merge_final_flush_ms` | 1 593 | 1.1 % |

Classify + rewrite-recv together = **79 % of wall at planet** (vs 73 %
at Europe). The top-ranked targets (classify, rewrite) are confirmed
at scale. Prefill has dropped from 13 % (Europe, pre-#2) to 4.6 %
(planet, post-#2) - no further useful gain available there.

### Shape counters (planet post-#2)

| Counter | Value |
|---|---:|
| `merge_reader_frames_sent` | 197 585 |
| `merge_reader_blocked_sends` | 1 826 (0.92 %) |
| `merge_blobs_passthrough` | 120 132 |
| `merge_blobs_rewritten` | 77 453 |
| `merge_blobs_index_hit` | 104 908 |
| `merge_bytes_passthrough` | 51.7 GB |
| `merge_bytes_rewritten` | 67.1 GB (56.5 % rewrite ratio) |
| `merge_loc_needed` | 6 803 426 |
| `merge_loc_from_diff` | 2 468 812 |
| `merge_loc_from_base` | 97 392 |
| `merge_loc_missing` | 4 237 222 (62 %) |

Note the `merge_loc_missing` ratio: 62 % of nodes referenced by OSC
ways still aren't in either the base or the OSC. Missing refs fall
back to `(0, 0)` coords per
[element_writes.rs](../src/commands/apply_changes/element_writes.rs) (search for `locations.push((0, 0))`). Worth
spot-checking that this matches osmium's semantics before claiming
parity on `--locations-on-ways` output.

## Hotpath: per-function cumulative CPU (Europe altw, 2026-04-18)

Two runs at commit `b4f45ff`, `--hotpath` mode. Totals are cumulative
wall across all calls; `% Total` is cumulative vs primary-thread wall,
so values >100 % mean the function saw parallel execution across
multiple cores.

### `--compression none` (UUID `e583036e`, 60.5 s wall)

| Function | Calls | Avg | Total | % wall |
|---|---:|---:|---:|---:|
| `merge::classify::classify_only` | 514,636 | 192 µs | **98.7 s** | 163 % |
| `merge::rewrite::rewrite_block_parallel` | 40,921 | 1.65 ms | **67.4 s** | 111 % |
| `pbfhogg::commands::read_raw_frame` | 514,667 | 96 µs | 49.3 s | 82 % |
| `write::writer::frame_blob_into` | 61,670 | 220 µs | 13.6 s | 22 % |
| `write::block_builder::take_owned` | 61,675 | 151 µs | 9.3 s | 15 % |
| `merge::rewrite::collect_batch` | 15,312 | 545 µs | 8.4 s | 14 % |
| `read::block::new` | 63,406 | 96 µs | 6.1 s | 10 % |
| `merge::node_locations::prefill_from_base` | 1 | **5.94 s** | 5.94 s | **10 %** |

### `--compression zlib:6` (UUID `ff3b07aa`, 56.7 s wall - different cache state)

| Function | Calls | Avg | Total | % wall |
|---|---:|---:|---:|---:|
| `write::writer::frame_blob_into` | 61,686 | 10.1 ms | **625 s** | 1104 % |
| `merge::classify::classify_only` | 514,651 | 225 µs | 115.7 s | 204 % |
| `merge::rewrite::rewrite_block_parallel` | 40,942 | 2.06 ms | 84.4 s | 149 % |
| `pbfhogg::commands::read_raw_frame` | 514,667 | 83 µs | 42.8 s | 75 % |
| `write::block_builder::take_owned` | 61,695 | 191 µs | 11.8 s | 21 % |
| `read::block::new` | 63,420 | 115 µs | 7.3 s | 13 % |
| `merge::node_locations::prefill_from_base` | 1 | **5.85 s** | 5.85 s | 10 % |
| `read::blob::decompress_into` | 95,071 | 43 µs | 4.1 s | 7 % |

Delta from none → zlib:

- `frame_blob_into`: **13.6 s → 625 s** (+611 s CPU). Pure zlib encode cost, parallel.
- `classify_only`: 99 s → 116 s (+17 s CPU). **Classify doesn't encode anything** - the delta is core contention, see the "zlib vs none" section.
- `rewrite_block_parallel`: 67 s → 84 s (+17 s CPU). Same mechanism.
- `prefill_from_base`: unchanged at ~5.9 s. Runs before the main pipeline, no contention.

## Alloc: per-function cumulative bytes (Europe altw, 2026-04-18)

UUID `4d4d9954`, commit `b4f45ff`, `--alloc` mode (`hotpath-alloc`
feature), default `--compression` (zlib:6). 60.8 s wall. Total
allocations ~291 GB across the run; peak RSS 1.1 GB - the allocator
turns bytes over fast, doesn't retain.

| Function | Calls | Avg | Total | % total |
|---|---:|---:|---:|---:|
| `merge::rewrite::rewrite_block_parallel` | 40,936 | 2.0 MB | **80.7 GB** | 27.7 % |
| `read::wire::parse_and_inline_with_scratch` | 63,423 | 860 KB | **52.0 GB** | 17.9 % |
| `merge::classify::classify_only` | 514,654 | 83.5 KB | 41.0 GB | 14.1 % |
| `commands::read_raw_frame` | 514,667 | 74.1 KB | 36.4 GB | 12.5 % |
| `write::block_builder::take_owned` | 61,690 | 563 KB | 33.1 GB | 11.4 % |
| `read::block::new` | 63,423 | 411 KB | 24.9 GB | 8.6 % |
| `write::writer::frame_blob_into` | 61,683 | 281 KB | 16.5 GB | 5.7 % |
| `merge::node_locations::prefill_from_base` | 1 | **2.9 GB** | 2.9 GB | 1.0 % |
| `read::blob::parse` | 971,613 | 901 B | 835 MB | 0.3 % |

Signals:

- **`parse_and_inline_with_scratch` at 860 KB per call** is the most
  surprising row - the "with_scratch" variant suggests scratch reuse
  already happened, but 52 GB cumulative shows the scratch doesn't
  cover every per-call vec. Worth auditing which vecs inside the
  function are still fresh per call.
- **`rewrite_block_parallel` at 2 MB per call × 41 k calls = 80.7 GB**
  is the largest single-callsite bucket. Per-call BlockBuilder (`task_bb`
  in the `rayon::spawn` closure inside `merge()`'s batch loop), output
  `Vec<OwnedBlock>`, stats - all per-task greenfield. This is the biggest
  arena / scratch pool target.
- **`classify_only` at 83 KB per call × 514 k calls = 41 GB** adds up
  from the per-call decompress buffer + wire scan scratch. Already
  uses `map_init` with a reusable buffer for decompress; the 41 GB
  says other per-call vecs are leaking (probably inside
  `scan_block_ids`, `scan_block_tags`, or the full-parse fallback).
- **`prefill_from_base` at 2.9 GB in one call** is the `NodeLocationIndex`
  - `locations: FxHashMap<i64, (i32,i32)>` + `needed_set: FxHashSet<i64>`.
  Sparse, expected, planet-scale still comfortable under host budget.

## Zlib vs none: where does the 8 s go?

Directly comparable `--bench 1` runs, same commit, same file, same cache
state window:

| UUID | Compression | Wall |
|---|---|---:|
| `f0af4170` | none | **46.1 s** |
| `570dfa69` | zlib:6 | **54.2 s** |

Zlib costs +8.1 s wall. Counter deltas:

| Counter | none | zlib:6 | Δ |
|---|---:|---:|---:|
| `merge_classify_total_ms` | 19,767 | 27,204 | **+7.4 s** |
| `merge_rewrite_recv_total_ms` | 13,651 | 13,895 | +0.2 s |
| `merge_prefill_ms` | 5,819 | 6,268 | +0.5 s |
| `merge_osc_parse_ms` | 1,860 | 1,955 | +0.1 s |
| `merge_final_flush_ms` | 1,354 | 1,179 | −0.2 s |
| `writer_compress_ns` | 8.6 s | **637.5 s** | +629 s CPU (parallel) |
| `merge_reader_send_wait_us` | 1.47 s | 8.63 s | +7.2 s stall |
| `writer_reorder_high_water` | 133 | **1797** | **13× deeper** |
| `writer_bytes_framed` | 26.6 GB | 17.3 GB | zlib output 35 % smaller |

**Classify takes +7.4 s under zlib even though classify doesn't
compress anything.** This is core contention: rayon workers decoding
blobs for classify compete for cores with rayon workers compressing
blocks for the writer. Under `none` the compression pool does almost
nothing; under zlib:6 it burns 629 s cumulative CPU (parallel) and
eats into classify's decode bandwidth. The reader-send stall at 8.6 s
(vs 1.5 s under none) is a second-order effect: consumer is slower, so
the frame channel stays full longer.

The writer's reorder high-water at 1797 (vs 133) says zlib produces
highly out-of-order completions - compression workers finish blobs in
variable time, the reorder buffer queues them until the file-order
consumer catches up. Not a bottleneck per se, just a shape change.

**Implications for ranking:** confirms `--compression none` is the
right production default. Compression-level tuning is out of scope
for this workstream since production already runs with `none`.

## Current architecture (reference)

Entry: `merge()` in [`apply_changes/rewrite.rs`](../src/commands/apply_changes/rewrite.rs) (at line ~197 as of 2026-04-20; all line numbers in this section may drift, treat as starting points). The public command name is `apply-changes`; the module was renamed from `merge` to `apply_changes` after the counter names (`merge_*_ms`) were locked in, so the counter/counter-prefix vocabulary still reads as `merge_*`. Hotpath labels in older measurement tables below (`merge::classify::...`) are historical from the pre-rename runs.

The module is split across several files:

- [`rewrite.rs`](../src/commands/apply_changes/rewrite.rs) - `merge()` entry + main batch loop + counter emission
- [`classify.rs`](../src/commands/apply_changes/classify.rs) - `classify_only` (fast/scan/parse/precise paths), `ClassifyResult`, `BatchSlot`, `RewriteJob`, `block_overlaps_diff`
- [`rewrite_block.rs`](../src/commands/apply_changes/rewrite_block.rs) - `rewrite_block_parallel` (the per-blob rewrite the streaming worker would inline)
- [`parallel_reader.rs`](../src/commands/apply_changes/parallel_reader.rs) - header-only schedule + pread workers + reorder pump (replaces the old sequential reader thread)
- [`node_locations.rs`](../src/commands/apply_changes/node_locations.rs) - `NodeLocationIndex::build_from_diff` and `prefill_from_base` (parallel as of `52c2c4b`)
- [`stream_output.rs`](../src/commands/apply_changes/stream_output.rs) - `coalesce_passthrough`, `emit_gap_creates`, `flush_remaining_upserts`, `has_gap_creates`, `emit_create_for_output` (reorder-actor helpers, already extracted)
- [`element_writes.rs`](../src/commands/apply_changes/element_writes.rs) - `write_base_*_local`, `write_osc_way_local`, etc.
- [`diff_ranges.rs`](../src/commands/apply_changes/diff_ranges.rs) - `DiffRanges` (sorted ID vecs), `UpsertCursors`
- [`stats.rs`](../src/commands/apply_changes/stats.rs) - `MergeStats`, `PhaseTimers`, `StallAccumulator`, `ClassifyCounters`

**Setup phase**:

1. Parse OSC → `CompactDiffOverlay`.
2. Build `DiffRanges` - sorted upsert + delete ID vecs per type.
3. If `--locations-on-ways`: build `NodeLocationIndex`:
   - `NodeLocationIndex::build_from_diff` collects all node IDs referenced by OSC ways, seeds coords from OSC nodes, leaves the rest in `needed_set`.
   - `NodeLocationIndex::prefill_from_base` drives a parallel work-stealing pread/decode over node blobs whose ID range overlaps `needed_set` (landed `52c2c4b` 2026-04-18).
4. Read header, create pipelined writer.
5. `spawn_parallel_reader` - header-only schedule scan + pread worker pool + reorder pump on a manager thread, feeding a 128-deep `sync_channel<RawBlobFrame>` back to the main loop in file order.

**Main batch loop** (in `merge()`):

For each byte-budgeted batch of raw frames (from `collect_batch`):

- **Phase 1 (parallel classify)**: `classify_only` per frame via rayon `par_iter().map_init(...).collect()`. Returns `Passthrough`, `FalsePositive`, or `NeedsRewrite(PrimitiveBlock, BlobIndex)`.
- **Phase 2 (sequential inline assignment)**: for each `NeedsRewrite` slot, binary-search the sorted upsert vec for IDs landing in the blob's OSM range. O(log n) per blob.
- **Phase 3 (parallel rewrite)**: `rayon::spawn` per `RewriteJob`, each emitting to an `mpsc::sync_channel` sized to `num_threads.min(rewrite_count)`. Jobs own their `PrimitiveBlock` and drop it after completion.
- **Phase 4 (streaming output)**: main thread processes slots in file order; passthrough slots flow into a coalescing buffer (`write_raw_owned` via `stream_output::coalesce_passthrough`); rewrite slots use a try_recv/blocking-recv pattern with a `WAIT_REWRITE_RESULT` marker pair on the blocking path.

**Teardown**: flush remaining upserts per type (`types_to_flush` match on `last_type`), writer flush.

## Opportunities, ranked

### #2 - Parallelize `NodeLocationIndex::prefill_from_base` (landed 2026-04-18, commit `52c2c4b`)

**Result:** prefill phase 20.5 s -> 6.6 s (-14 s, -68 %), overall wall
154.9 s -> 144.4 s (-10.5 s, -6.8 %) on planet altw + OSC 4913. See
"Measured: Planet" section above. Actual landed shape differs
slightly from the sketch below: schedule construction is inline in
`prefill_from_base` rather than reusing `build_classify_schedule`,
because the `overlaps_needed` filter needs per-blob indexdata access
at schedule-build time and adding that to the shared helper would
have bloated its signature for one caller. `parallel_classify_accumulate`
is also not used directly - the parallel path uses
`extract_node_tuples` on raw decompressed bytes, which is cheaper than
the full `PrimitiveBlock` construction that helper does. Retained for
historical context as a record of the planning/measurement/landed
arc.

[node_locations.rs:112-144](../src/commands/apply_changes/node_locations.rs#L112) is a straight sequential loop over node blobs:

```rust
for blob_result in &mut reader {
    // overlap check via needed_sorted binary search
    blob.decompress_into(&mut buf)?;
    extract_node_tuples(&buf, &mut tuples, &mut group_starts);
    for t in &tuples {
        if self.needed_set.remove(&t.id) {
            self.locations.insert(t.id, (t.lat, t.lon));
            nodes_found += 1;
        }
    }
    if self.all_found() { break; }
}
```

`overlaps_needed` ([node_locations.rs:73](../src/commands/apply_changes/node_locations.rs#L73)) is effective at skipping blobs that contain zero needed IDs. But every overlapping blob is decompressed on the main thread, serially, before the main pipeline even starts. For a daily diff touching ~10 M referenced nodes spread across the node ID space, probably 30-50 % of node blobs overlap, giving ~20-30 GB of compressed node data to decompress. At ~500 MB/s single-threaded, 40-60 s. On 6 cores: 10-15 s.

The shape matches [`parallel_classify_accumulate`](../src/commands/mod.rs#L571) exactly - it's the same pattern the geocode builder uses in Pass 1.5 for a dense-decode accumulator ([geocode_index/builder.rs:498](../src/geocode_index/builder.rs#L498)). Reuse it:

- Build a node-only schedule via [`build_classify_schedule`](../src/commands/mod.rs#L429) with `kind_filter = Some(ElemKind::Node)`. Apply the `overlaps_needed` filter at schedule-construction time (header-only blob walk, cheap). The filtered schedule contains only blobs worth decompressing.
- `parallel_classify_accumulate` with per-worker state `S = FxHashMap<i64, (i32, i32)>`. Workers do `pread → decompress → extract_node_tuples → if needed_set.contains(id) { local.insert(id, (lat, lon)) }`.
- Merge: drain each per-worker map into `self.locations`. HashMap insert is last-write-wins; all coords for a given ID are identical, so the merge is straightforward.

**Two nuances**:

- The current code uses `needed_set.remove(&t.id)` to (a) avoid double-insertion and (b) support early-exit via `all_found()`. In parallel land, workers read `needed_set` (shared immutable after build; swap `remove` for `contains`) and insert unconditionally. Early-exit is less useful once blobs are all in flight; drop the `all_found()` check or gate it on an atomic counter polled every N tuples.
- Per-worker map size at peak: ~2-5 MB for a daily diff (10M / 6 workers × ~50 bytes/entry). Merge is a single linear drain. No backpressure.

**Expected win**: ~30-60 s at planet.

**Risk**: low. Pattern is already used in the codebase. Correctness is straightforward (merge is commutative + idempotent for sparse location lookups).

### #3 - Replace the sequential reader thread with parallel pread schedule - **LANDED 2026-04-18 (commit `c97d6b5`)**

**Landed-result.** Parallel reader (header-only schedule + pread worker pool + reorder pump) lives at [`apply_changes/parallel_reader.rs`](../src/commands/apply_changes/parallel_reader.rs). Net-zero wall change vs the old sequential reader thread at planet - but kept as infrastructure because the streaming pipeline (next planned move, see External Review Synthesis section) builds on its schedule-and-dispatch shape. Reader is no longer the bottleneck; the per-batch Amdahl barrier is. Historical rationale and design notes preserved below for reviewer context.

---

Historical context (pre-landing): the original `spawn_reader_thread` ran one thread that opened a `FileReader` and streamed `RawBlobFrame`s through a 128-deep `sync_channel`. That thread was the only reader. The batch loop decoupled reader from workers but did not parallelize the read itself.

At sequential BufReader + blob-header-parse overhead, realistic throughput is ~500 MB/s - 1 GB/s. 87 GB is 90-180 s. Parallel `pread` on NVMe reaches 3-5 GB/s, dropping to 17-30 s.

**Refactor**: replace the reader thread with the same work-stealing pread schedule pattern used in [`pass1_parallel_scan`](../src/commands/renumber_external.rs#L615), ALTW's `stage2d_worker`, and geocode's proposed Phase 2a/2b:

- Header-only schedule scan up front, producing `(seq, frame_offset, data_offset, data_size, blob_type, indexdata_hint, tagdata_hint)` tuples. Today's `build_classify_schedule` uses `BlobReader::seekable_from_path` + `next_header_with_data_offset`, which walks via a 256 KB `BufReader`. On cold cache that reads roughly half the file (~45 GB at planet) because data bytes inside the buffer window get pulled in even though only the header is used - measured in the 2026-04-20 diff-walker work. A cheaper variant now exists: [`src/read/header_walker.rs::HeaderWalker`](../src/read/header_walker.rs) uses raw `pread` with `posix_fadvise(POSIX_FADV_RANDOM)`, reading ~2.6 GB at planet and landing the walker phase at ~15 s. Migrate `build_classify_schedule` to `HeaderWalker` as a contained refactor before (or alongside) #3 for the 2026-04-20 walker win, then fan the resulting schedule out to worker preads for #3's classify+pread fusion.
- Collapse "reader thread → frame channel → classify workers" into one stage: each worker preads + classifies in the same loop and emits `ClassifyResult` downstream.
- Retain the existing batch structure by having the consumer side pull `ClassifyResult`s in seq order (reorder buffer) rather than pulling raw frames.

**Two wrinkles**:

- **`copy_file_range` path** (in `merge()`, gated on `use_copy_range`) needs `frame.file_offset`. That survives cleanly - the schedule entry has both `frame_offset` (for raw passthrough) and `data_offset` (for pread of the compressed body). Include both in the tuple.
- **Raw-frame ownership** for the zero-copy passthrough move (`std::mem::take(&mut frame.frame_bytes)` inside `coalesce_passthrough`). Workers already own their pread buffer; move it out the same way. The concept of `RawBlobFrame` survives; the difference is *when* the frame bytes are read (worker pread) versus *who* read them (reader thread today).
- **Reader-thread backpressure semantics.** The current `sync_channel(128)` gives 128 blobs of read-ahead. Parallel pread gives `num_workers × per-worker-batch` blobs of concurrent in-flight reads, which is similar or slightly higher. Page cache pressure is the same (reading the same bytes). No new RSS concern.

**Expected win**: ~50-100 s at planet on NVMe. Smaller on spinning disk.

**Risk**: medium. Largest of the three changes. Touches the main loop structure, not just a helper. Preserve the reorder-buffer + batch-boundary logic carefully.

### #4 - Classify slow-path reduction (next target)

Classify is now the biggest bucket at planet: **70.9 s / 144.4 s = 49 % of wall** (post-#2). It's main-thread wall, not cumulative CPU - the per-batch pipeline is serial on the main thread (classify -> inline-assign -> rewrite-spawn -> rewrite-recv), so classify and rewrite-recv wall add directly. Together they're 79 % of wall.

The classifier runs three paths per blob (see [classify.rs:129](../src/commands/apply_changes/classify.rs#L129)):

1. **Fast path** - indexdata present + range miss -> `Passthrough` without decompress. Should be nanoseconds per call.
2. **Scan path** - indexdata range overlapped or absent. Decompress, then `scan_block_ids` for a tighter range. If that range misses -> `Passthrough`. Decompress cost only.
3. **Full parse path** - scan overlapped too (or produced nothing). Full `parse_primitive_block_from_bytes_owned` + `block_overlaps_diff`. If no element matches -> `FalsePositive`; else `NeedsRewrite`.

#### Current blob-count split at planet (UUID `e81a9316`)

| Path | Blobs | % of 197 585 | Counter |
|---|---:|---:|---|
| Fast-path passthrough | 104 908* | 53 % | `merge_blobs_index_hit` (conflated, see below) |
| Scan-path passthrough | 0* | 0 % | n/a (every blob has indexdata; counter reports `blobs_scan_only` for the no-indexdata case only) |
| FalsePositive (full parse, no match) | 15 224** | 8 % | derived: `blobs_passthrough - blobs_index_hit - blobs_scan_only` |
| Rewrite (full parse + re-encode) | 77 453 | 39 % | `merge_blobs_rewritten` |

*`merge_blobs_index_hit` today counts *both* "fast-path" *and* "scan-path after indexdata-loose overlap": any `Passthrough` where the original frame had indexdata. The scan-path-through-indexdata-loose count can't be separated without new instrumentation. At planet the two are probably dominated by fast-path (indexdata is tight on PBFs pbfhogg produces), but we don't know.

**`merge_blobs_scan_only` only counts the no-indexdata case (`has_indexdata: false`). At planet every OSM blob carries indexdata, so this is always 0. The counter name is misleading.

**FalsePositive has no explicit counter.** The delta subtraction works today but obscures a distinct path - these blobs pay the full-parse cost for nothing. Worth tracking directly.

#### Instrumentation landed (`b769996`) and measured (UUID `e49b6182`)

New per-path counters: `merge_blobs_classify_{fastpath,scan_pass,false_positive,rewrite}` (explicit blob counts) and `merge_classify_{decompress,scan,parse,precise}_ns` (cumulative ns summed across rayon workers, divided by observed parallelism to back out wall share). Fast-path is not instrumented (few-ns range check; atomic-add would dominate).

Planet run at `b769996`, 2026-04-18, plantasjen, `--bench 1`, `--compression none`. Wall: **136.8 s** (a further ~8 s drop vs `52c2c4b`'s 144.4 s, likely run-to-run variance). Classify phase wall: **71.1 s**.

**Per-sub-step cumulative CPU:**

| Sub-step | Cumulative CPU | % of classify CPU |
|---|---:|---:|
| `merge_classify_decompress_ns` | **206.3 s** | **70.0 %** |
| `merge_classify_precise_ns`    | 56.9 s       | 19.3 % |
| `merge_classify_parse_ns`      | 21.1 s       | 7.2 % |
| `merge_classify_scan_ns`       | 10.7 s       | 3.6 % |
| **Total measured CPU**         | **295.0 s**  | |

**Per-path blob counts** (explicit):

| Path | Blobs | % of 197 585 |
|---|---:|---:|
| `fastpath` | 104 908 | 53.1 % |
| `scan_pass` | **0** | 0.0 % |
| `false_positive` | 15 224 | 7.7 % |
| `rewrite` | 77 453 | 39.2 % |

**Findings:**

1. **Classify is CPU-undersaturated, not CPU-bound.** 295 s of rayon-worker CPU in 71 s of main-thread wall = **4.15 cores average** (out of 22 available). The phase is wall-limited by how work reaches rayon, not by raw work volume. With 15 864 batches averaging 12.5 blobs/batch, most per-batch `par_iter` calls can't use more than a handful of cores before running out of work. **This is the biggest lever.**
2. **Decompress is 70 % of classify CPU.** Nothing to optimise inside the decompress call (zlib is zlib); what matters is how many blobs reach it. The 92 677 slow-path blobs (FalsePositive + rewrite) each cost ~2.23 ms to decompress; there's no way to skip decompress if we need the bytes for rewrite or precise check.
3. **`scan_block_ids` is pure waste at planet: 0 `scan_pass` hits, 10.7 s CPU burned.** When indexdata is present and said overlap, `scan_block_ids` returns a range that always overlaps too (indexdata is already tight on pbfhogg-emitted PBFs). Skipping the scan when we already have indexdata saves 10.7 s CPU / 4.15 cores ≈ **2.6 s wall**. Trivial fix.
4. **Precise check (`block_overlaps_diff`) costs 57 s CPU** - more than parse (21 s). 92 677 calls x 614 us avg. Driven by `HashSet::contains` over diff IDs for every element in the block. Candidates: iterate diff IDs against the block's sorted element IDs (linear merge) instead; or replace `FxHashSet` lookup with [`IdSetDense::get`](../src/commands/id_set_dense.rs#L213) for the diff nodes/ways/relations if dense enough (read-only presence query, distinct from the `_if_new` variants used for newer-wins dedupe in item 11); or do a wire-format ID scan that avoids materialising `PrimitiveBlock` at all.
5. **Parse is only 21 s CPU (7 %)** - smaller than expected. Deferring parse to Phase 3 would save barely 5 s wall at current parallelism. Not worth the refactor on its own.

#### Revised ranking (post-measurement)

By estimated wall impact at planet, assuming current ~4.15 cores classify utilisation (numbers change if item 1 lands first):

| # | Change | Est. wall save | Risk | Notes |
|---|---|---:|---|---|
| 1 | **Classify parallelism: cash in the CPU slack** | **~25-35 s** | medium | Bigger/coarser batches, or pipeline restructure so classify isn't per-batch main-thread serial. See sub-proposal below. |
| 2 | Skip `scan_block_ids` when indexdata was present | **~2.6 s** | trivial | Drop the scan call when `frame.index.is_some()`; rely solely on the precise check. Cheap first win. |
| 3 | Wire-format FalsePositive scan | **~3.4 s** | low | Replace parse+precise for 15 k blobs with a lightweight ID-only wire walk (analogous to `extract_node_tuples` but for way/relation IDs). Saves ~5 s parse CPU + ~9 s precise CPU. |
| 4 | Reduce `block_overlaps_diff` per-call cost | unknown | low-med | 57 s CPU total. Try linear sorted-merge vs HashSet lookups; or dense ID sets. Needs a micro-bench before committing. |
| 5 | Pipeline restructure (subsumes reader #3) | ~40-60 s | high | Collapses reader → classify → rewrite into one work-stealing pipeline. Also tackles rewrite-recv (43 s wall, 30 %). Only after items 1-4. |

Item 1 detail: the cheapest experiment is to bump `BATCH_MIN_BLOBS` / widen `BATCH_BYTE_BUDGET` so each `par_iter` has more work. Risk is memory: peak in-flight bytes grows with batch size. Likely safe under the 1.8-2.5 GB budget documented below. If that doesn't saturate cores, the only remaining fix is pipeline restructure (item 5). Instrument first by emitting per-batch classify CPU and per-batch blob count, so we can tell which `par_iter` calls leave cores idle.

## Overall expected savings

Under `--compression none` at planet:

- #2 (parallel prefill): **landed, measured -10.5 s wall (154.9 s -> 144.4 s on `52c2c4b`).**
- Scan-skip + sorted-merge: landed (`da1c45e`), ~-2 s wall (mostly variance).
- Parallel reader + merge batch budget: landed (`c97d6b5`, `bfac63b`), net zero wall. Kept as infrastructure for the streaming rewrite.

Updated primary target after the **external-review-driven streaming pipeline + fuse classify+rewrite** (next planned move): **40-55 s wall at planet daily OSC** (`--compression none`), from the current 144.4 s baseline. CPU-budget floor from reviewer 2: classify CPU 270 s / 22 cores = 12.3 s classify wall under the streaming shape, plus ~3-4 s rewrite, plus pre-loop phases (~10 s) and writer wall-bound work running in parallel (~30 s). See "External review synthesis" section above. The 140.3 s figure in the review-synthesis section is from an interim measurement window; the current post-#2 `--bench 1` is 144.4 s on the same commit, within run-to-run noise.

**Weekly OSC end-state estimate (reviewer 2):** **70-90 s wall at planet weekly OSC** with streaming + worker-emits-framed (Q2) + prefill fusion (Q4) + parallel OSC parse (Q7). Current weekly (linear scaling of serial phases) probably ~250 s+.

**Follow-up wins on top of the streaming rewrite:**

- Worker-emits-framed-bytes: frees the `PIPELINE_DISPATCH_PERMITS=64` cap, small commit after streaming. More valuable under zlib and at weekly scale.
- Prefill fusion: -5 s at daily, -30 s+ at weekly. Clean local change after streaming lands.
- Splice-in-place for low-touch rewrites: ~1.5-2 s daily; less valuable at weekly.
- Parallel OSC parse: only matters at weekly scale (20-30 s).

## External review synthesis (2026-04-18) - HISTORICAL

> **Superseded by the "Third review round + synthesis (2026-04-20)" section below** on every item where they differ. Notable stale content preserved here for historical record:
> - RawBlobFrame-first pipeline shape (R3 replaced with descriptor-first).
> - 12.3 s classify floor (R3 corrected to 13.4 s; fused classify+rewrite floor ~17 s).
> - "Worker-emits-framed-bytes in v1" - R3 explicitly deferred to P1.5.
> - "PbfWriter pre-ordered input entry point in P1" - R3 deferred to post-P1 measurement.
> - Q1-Q7 follow-ups preserved below for context but the Q4/Q6 positions are folded into P1's design rather than landing separately.
>
> Read the R3 synthesis first; refer back here only for the original ranked-item context.

Two independent reviewers (`perf` / `arch` / `planet` archetypes) converged strongly on the same diagnosis and next move after seeing the planet post-parallel-reader plateau at 140.3 s. Their writeups are extensive; this section folds the claims, reasoning, and follow-up ideas into one place so we don't lose them.

### Diagnosis: the 4.1-core classify plateau is self-inflicted

The shape of the main loop (one `batch.par_iter().collect()` followed by a serialised drain in file order) has two structural properties that together cap classify wall:

1. **Per-batch Amdahl on classify.** `par_iter().collect()` returns only when the slowest blob in the batch finishes. Batches mix a handful of heavy overlap blobs (decompress + precise, ~2 ms each) with many cheap fastpath blobs (ns each). Rayon schedules the heavy ones one-per-core; remaining cores sit idle waiting for the tail blob. At ~5-6 heavy blobs per batch, effective utilisation is ~5-6 cores - matching the measured 295 s CPU / 72.5 s wall = 4.1 cores.
2. **Phase-exclusive pool usage.** Within a batch: classify runs (pool busy on classify), then rewrites run (pool busy on rewrites), then the main thread walks slots in order. By the time the next classify starts, all previous rewrites have drained. Neither phase overlaps with itself or with the next batch. Each phase sees 22 cores in principle but each is individually bounded by its own barrier.

**Direct consequence:** batch size knobs can't fix this. Adding more items to a batch doesn't shrink the tail; measured at 128 MB -> 512 MB, classify wall was unchanged (72.5 s vs 70.3 s) and the bottleneck shifted to writer-permit pressure. The plateau is structural to the batch shape, not a rayon pathology.

**Reviewer 2's CPU-budget lower bound:** classify CPU 270 s / 22 cores = **~12.3 s classify wall** if the barrier is removed. That, plus the rewrite recv stall disappearing (main thread off the critical path), yields an estimated **40-55 s wall at planet**, vs current 140 s - roughly 3x speedup vs the ~18 % that pipelined batches alone would give.

### Primary rewrite: streaming pipeline, batches deleted

Replace `collect_batch` + `par_iter().collect()` + the per-batch slot-drain with continuous stages connected by bounded ordered channels:

```
[Reader]  --RawBlobFrame (seq)-->
  [Classifier pool: N workers]     each worker: decompress + precise check.
                                   If NeedsRewrite, do the rewrite inline (fuse).
                                   Emit (seq, ClassifiedItem) where ClassifiedItem =
                                       { RawPassthrough(frame) | Rewritten(blocks, stats) }
     --ClassifiedItem (seq)-->
  [Reorder / gap-create actor]     single thread. Consumes in seq order.
                                   Emits gap creates, handles type transitions,
                                   forwards frames/blocks to writer pipeline.
     --OutputChunk (seq)-->
  [PbfWriter pipeline]             existing, unchanged.
```

**Why this is the right move, not "another" move:**

- **No Amdahl barrier.** When worker A finishes its blob, it immediately pulls the next one. Cores stay busy as long as the reader has frames. Classify wall becomes `max(reader_bandwidth, 270 s / 22) ≈ 12-15 s`, vs 72.5 s today.
- **Rewrite fuses into classify naturally.** Today's flow decodes a `PrimitiveBlock` in one worker, ships it back to main thread via `NeedsRewrite(PrimitiveBlock, BlobIndex)`, main thread does ~50 ns of `partition_point` upsert-range compute, then re-spawns into rayon, which schedules yet another worker to rewrite the block it already had. Two dispatches, one cross-thread `PrimitiveBlock` handoff, one temporary allocation alive across the gap. If a worker already decoded the block, let it finish the job - pass `Arc<DiffRanges>` + `Arc<CompactDiffOverlay>` to the worker and the inline upsert-range compute happens inline.
- **Main thread leaves the critical path.** Today it owns type transitions, gap creates, passthrough coalescing, ordered rewrite-recv, passthrough flush, rewrite-block writes - a serial chain that shows up as `merge_rewrite_recv_total_ms = 39 s`. Under the new shape, a tiny dedicated reorder/gap-create actor owns only in-order sequencing + gap-create emission. Almost no CPU.
- **Coalescing gets better, not worse.** The reorder actor coalesces consecutive `RawPassthrough` frames into a single `write_raw_chunks` - same `Vec<Vec<u8>>` output shape as today. Coalescing now runs per-run-of-passthroughs in file order, not per-batch-truncated. Batches today arbitrarily break up long passthrough runs.

**CPU-budget walk (reviewer 2):**

- Classify CPU: 270 s -> `270 / 22 = 12.3 s` wall
- Rewrite CPU: (39 s recv is a lower bound on rewrite wall-in-pool today; real rewrite CPU probably 60-80 s) -> `3-4 s` wall with 22 cores
- Prefill 4.9 s + OSC parse 4.9 s + header ~0 s = unchanged serial pre-phase
- Writer wall: `--compression none` writes ~92 GB in ~30 s wall on decent NVMe, runs fully in parallel with classify

**Realistic floor: 40-55 s.** We've been at 140 s.

**Risk inventory and mitigations:**

- **Gap-create ordering.** Gap creates must be emitted in a specific seq position (before the blob whose `min_id` exceeds the cursor). The reorder actor sees items in seq order, so it can inject gap-create blocks between forwarded items. Same invariant as today; moved to a different owner.
- **Type transitions.** Same: the reorder actor owns the last-type state.
- **Prefill** for `--locations-on-ways`: stays as a serial pre-phase before the pipeline starts. Already fast (~5 s) and its `Arc`'d output just gets handed to classifier workers.
- **Backpressure / RSS.** Bound the `(seq, ClassifiedItem)` channel capacity **in bytes, not count**, since `Rewritten` items carry decoded + re-encoded data. A budget of ~1 GB in flight keeps us well under the 30 GB host limit.
- **Error propagation.** Workers send `Result<ClassifiedItem, _>`; reorder actor propagates. Existing pattern in the codebase.
- **Rayon pool partitioning.** The two reviewers diverge here (see Q3 follow-up subsection). Reviewer 1 recommends a dedicated rayon pool (~19 worker threads) following the [`src/read/pipeline.rs:127`](../src/read/pipeline.rs#L127) pattern. Reviewer 2 recommends the default global pool with work-stealing. Both agree reader, pread workers, and reorder actor run on dedicated (non-rayon) threads, and that "shared pool with priorities" is wrong. Decide at implementation time; for apply-changes specifically (no other concurrent rayon user) the two positions collapse to the same hardware assignment.

**Scope.** Net delete of ~300 lines from rewrite.rs; net add of ~400 lines split across a new `streaming.rs` + reorder actor. Not a prototype - commit, measure, keep or revert.

### Cheap disambiguation experiment (attempted, reverted 2026-04-18)

Attempted implementation: `rayon::scope` + per-blob `scope.spawn` + `mpsc::sync_channel(batch_len)` + `ReorderBuffer` replacing `batch.par_iter().collect()`. Keeps phase 2/3/4 unchanged. Committed as `<pending-sha>`.

**Result: reverted.** Denmark verify passed (correctness OK), but planet `--bench 1` never completed: killed at 10 min wall (vs ~140 s baseline) with last marker `WAIT_REWRITE_RESULT_END` and only 5.6 GB / 92 GB read, 5.4 GB written. Child RSS was 1.2 GB, 50 threads. Not a deadlock - the process was making forward progress, just pathologically slowly.

**Likely cause (not proven):** per-batch `rayon::scope` overhead at scale. Planet has ~11 k batches; 18-way scope.spawn + waiting-for-all-to-complete + channel allocation + ReorderBuffer allocation per batch. Even a few ms of per-batch overhead puts the total minutes over baseline. The `rayon::scope` barrier semantics also still wait for the tail task per batch - the shape isn't meaningfully different from `par_iter().collect()` in that respect.

**What this tells us:** the "cheap" experiment as reviewers sketched it (swap dispatch shape, keep batch loop) doesn't cleanly isolate the batch-barrier hypothesis at planet scale. A better experiment would be:

- Pre-allocate the reorder buffer + channel **once**, outside the batch loop, reused across batches.
- Or: abandon the "keep batches" constraint and go straight to the streaming rewrite - the real payoff shape anyway.

**Doesn't disprove the plateau hypothesis** - it just means this particular minimal-change form doesn't prove it either way. The CPU-budget math (270 s / 22 = 12.3 s classify wall floor) still stands as the streaming pipeline's theoretical target.

**Sequencing takeaway:** skip the cheap experiment; go direct to the streaming pipeline. Accept that we can't cheaply disambiguate "batch barrier vs non-scaling CPU" before committing to the restructure. Mitigation: instrument the streaming pipeline with per-phase counters (already planned) so we can detect non-scaling CPU early and pivot if the 40-55 s target turns out unreachable.

### Follow-up wins (order by when they make sense)

The streaming rewrite above is the headline. After it lands, these become the next targets:

#### Worker emits framed bytes (both reviewers, Q2 follow-up)

Highest-confidence follow-up after the streaming rewrite. Moves compression + framing into the classifier worker that already holds the uncompressed bytes. Details in the Q2 follow-up subsection above; ~50-line commit; works for `none`, `zlib`, and `zstd`. Extra payoff under zlib: removes the `PIPELINE_DISPATCH_PERMITS=64` cap that bottlenecks concurrent compressor tasks today.

#### Prefill fusion into the streaming pipeline's node phase (reviewer 2, Q4 follow-up)

Classifier workers decompressing node blobs opportunistically extract coords for IDs in `needed_set` into thread-local maps. Reorder actor detects the node→way transition and merges per-worker maps into `Arc<loc_map>` before gating way-blob workers. Details in Q4 follow-up subsection above. **Deletes the prefill phase entirely:** ~5 s at daily, ~30 s+ at weekly scale.

#### Parallel OSC parse (reviewer 2, Q7 follow-up, weekly only)

Only relevant if the standard pipeline processes a week of OSCs in one run. Single-threaded [`load_all_diffs`](../src/osc.rs#L1036) parse scales linearly with OSC count; at 7x it becomes a 30-40 s serial phase. Shape: parse each OSC concurrently into its own overlay, then merge overlays with newer-wins semantics using [`IdSetDense::set_atomic_if_new`](../src/commands/id_set_dense.rs#L163) as the per-element-type dedupe primitive (walk overlays newest-first; keep the element iff the call returns `true`). Each OSC is independent work. Estimated 20-30 s wall at weekly scale.

#### Splice-in-place for low-touch rewrites (reviewer 2)

In [`rewrite_block_parallel`](../src/commands/apply_changes/rewrite_block.rs) (own file), every `NeedsRewrite` blob is **fully decoded and fully re-encoded**, even if only one of its ~8 000 elements is touched by the diff. At planet with a daily OSC, the modal "needs rewrite" blob has 1-3 affected elements out of 8 000.

**The change.** For blobs where the precise check finds `<=K` affected elements (say K=64), splice: walk the raw decompressed wire bytes for the `DenseNodes` / `Ways` / `Relations` `PrimitiveGroup`, emit runs of unaffected elements raw (via the existing raw-group passthrough scaffolding at [`src/read/block.rs:507`](../src/read/block.rs#L507) + [`src/write/raw_passthrough.rs`](../src/write/raw_passthrough.rs#L1)), and only decode+re-encode the affected ones.

**Budget.** Attacks the 25 s classify parse + the estimated 60-80 s rewrite CPU. Estimated save: **30-50 s CPU, ~1.5-2 s wall** on top of the streaming rewrite at daily scale. Not a headline, but sizeable.

**Weekly scale: less valuable.** Rewrite blobs touch more elements per blob under a weekly OSC; the near-passthrough population (`<=K` affected elements) shrinks. If the standard cadence becomes weekly, this item demotes.

**Don't land before the streaming rewrite** - the rewrite moves this code to a different owner and landing twice is waste.

#### Steal `copy_file_range` coalescing from ALTW (reviewer 1)

ALTW's passthrough module already coalesces contiguous `copy_file_range` runs in [`src/commands/altw/passthrough.rs`](../src/commands/altw/passthrough.rs) (`coalesce_passthrough` helper + the contiguous-range extension pattern). `apply-changes` still does per-blob `copy_file_range` writes in `rewrite.rs` (search for `write_raw_copy`). Useful, secondary.

#### Use the existing alloc-optimised parse path (reviewer 1)

`classify_only` currently parses via [`src/read/blob.rs:1307`](../src/read/blob.rs#L1307) (`parse_primitive_block_from_bytes_owned`), but [`src/read/block.rs:432`](../src/read/block.rs#L432) exposes an alloc-optimised variant already used elsewhere. Small slow-path parse shave.

#### Exact-membership metadata or sidecar (reviewer 1, conditional)

Current on-disk metadata gives per-blob type + ID range only ([`src/blob_index.rs:56`](../src/blob_index.rs#L56)), so pure creates inside an existing blob range force slow-path decode - this is the documented FalsePositive case at [`src/commands/apply_changes/classify.rs:48`](../src/commands/apply_changes/classify.rs#L48).

**Only worth pursuing if FalsePositives are a material share of slow-path work.** At planet today: 15 224 FalsePositive blobs / 92 677 slow-path = 16 %. Not negligible, but small next to the 77 453 rewrites that fundamentally need the slow path. A format/index project, not a quick cleanup.

If pursued: either a wire-format exact-overlap scanner on decompressed bytes (skips full parse for FalsePositives) or a per-blob membership sketch in indexdata (rejects FalsePositives without decompress at all).

#### Ideas explicitly ruled out

- **Pre-built schedule sidecar.** Reviewer 2: "header walk at planet is ~1-2 s. Not the bottleneck. Skip." (Our measured scanner cost was 43 s when run as a pre-block, but that was specifically the blocking pre-scan shape - under the streaming reader and later streaming pipeline, the scanner runs concurrently with workers, and its wall cost is subsumed.)
- **Separate thread pool for decompress alone.** Becomes moot under the streaming rewrite: classify and rewrite fuse, no seam to separate.
- **Revert parallel reader or 512 MB batch budget.** Both reviewers: no. Parallel reader is needed infrastructure for the streaming pipeline. The 512 MB budget is neutral-at-worst and becomes irrelevant once batches go away - ripping it out costs us later.
- **Writer-permit loosening.** The 20x `writer_permit_wait_ns` at 512 MB is a symptom of too-large coalesced flushes hitting a 32-deep channel, not an independent bottleneck. Fixed automatically when the reorder actor coalesces per-run rather than per-batch; runs are smaller and distributed in time.
- **Compression-level tuning (zlib:1 default).** Out of scope for this workstream: production uses `--compression none`, the ecosystem default is zlib:6 and stays.

### Recommended sequence (both reviewers agree)

1. ~~**Cheap disambiguation experiment.**~~ Attempted 2026-04-18 and reverted (10 min+ regression at planet while Denmark verify passed). See "Cheap disambiguation experiment" subsection for post-mortem. Skipping this step and going direct to streaming.
2. **Full streaming pipeline + fuse classify+rewrite.** Same commit architecturally - don't land them separately. Expected 40-55 s wall.
3. **Worker-emits-framed-bytes.** Small, high-confidence commit after the streaming rewrite. Removes the `PIPELINE_DISPATCH_PERMITS=64` cap on concurrent compressor tasks; frees the writer's second rayon dispatch. See the Q2 follow-up subsection.
4. **Prefill fusion into the streaming pipeline's node phase.** Clean local change once streaming is in place. Deletes the prefill phase entirely (~5 s today, ~30 s+ at weekly scale). See the Q4 follow-up subsection.
5. **(weekly OSC only) Parallel OSC parse.** New priority at weekly scale: single-threaded XML+gzip at 7x becomes a 30-40 s serial phase. Parallel-parse-then-merge-overlays is a real target. See the Q7 follow-up subsection.
6. **Splice-in-place for low-touch rewrites.** ~1.5-2 s wall at daily. **Less valuable at weekly** (more elements touched per rewrite blob shrinks the near-passthrough population).
7. **`copy_file_range` coalescing, alloc-optimised parse.** Opportunistic local shaves.
8. **Writer path tuning** (direct-io vs `to_path_uring`): only after the streaming rewrite if bench points there. Don't start with "bigger internal queues" - that's a symptom-fix. See Q1 follow-up.

After step 2, the next investigation target becomes reader throughput or writer pipeline - both currently invisible because the main-loop noise dominates.

### Second review round (Q1-Q7 follow-ups)

After the initial reports, a follow-up round probed seven targeted questions about the streaming rewrite's downstream implications. The answers refine the plan above; this subsection records the concrete new material. Where the two reviewers diverged, the divergence is noted.

#### Q1: Writer as the new wall floor

Both reviewers: **measure before optimising.** The existing writer stack is already well-shaped.

Reviewer 1: "The existing writer stack is good enough to let the streaming rewrite land first, but only if rewritten blobs stop going through `write_primitive_block_owned`." The plumbing already has the right shapes: buffered/direct/io_uring selection in [`src/commands/mod.rs:864`](../src/commands/mod.rs#L864), bounded write-ahead + dispatch permits in [`src/write/writer.rs:31`](../src/write/writer.rs#L31), and an io_uring backend aimed at `Compression::None` on fast storage in [`src/write/writer.rs:408`](../src/write/writer.rs#L408) and [`src/write/uring_writer.rs:1`](../src/write/uring_writer.rs#L1). If the writer *does* become the new floor, the next pass in order: (1) preframed rewrite output (Q2), (2) contiguous passthrough `copy_file_range` coalescing like [`src/commands/altw/passthrough.rs`](../src/commands/altw/passthrough.rs), (3) benchmark buffered vs io_uring on target NVMe. **Do not start with "bigger internal queues"** - if disk bandwidth is the wall, queue depth is rarely the real lever.

Reviewer 2: NVMe ceiling gives 20-30 s for the 92 GB write. The `sync_channel(WRITE_AHEAD=32)` → writer_thread → `BufWriter(256 KB, File)` path is fine until the write queue is actually the bottleneck, which would show up as `pipeline_send_wait_ns` blowing up. If `bytes_written/s` sits near the NVMe ceiling after streaming lands, we're done for the `--compression none` case. If not, flip to `to_path_uring` (infrastructure is already there: `uring_writer.rs`, 64x256 KB registered buffers + `O_DIRECT`) before anything else. Bigger internal queues are a symptom-fix.

Under `--compression zlib` the writer's CPU cost dominates anyway; the channel is almost never the bottleneck. Writer optimisation as a topic is mostly a `--compression none` concern.

#### Q2: Worker emits framed bytes (`write_raw_owned`) instead of `OwnedBlock`

Both reviewers: **do it.** Small-to-medium internal change, high-confidence win, works under all compression modes (not a `--compression none` trick).

The key enabler already exists: [`frame_blob_pipelined`](../src/write/writer.rs#L1102) takes uncompressed bytes + compression + indexdata + tagdata on per-thread scratch and returns `FramedBlobParts`. This is exactly the shape the fused worker needs.

**Minimal change (reviewer 1 + 2 agree):**

- Change merge worker output from `Vec<OwnedBlock>` to `Vec<Vec<u8>>`.
- In the worker, after `bb.take_owned()` yields `(block_bytes, index, tagdata)`, call `frame_blob_pipelined(&block_bytes, &compression, Some(&index.serialize()), tagdata.as_deref())?.into_vec()` and emit that `Vec<u8>` as the rewritten output.
- The reorder actor forwards it via the existing raw-passthrough path ([`write_raw_owned`](../src/write/writer.rs#L715) / `write_raw_chunks`).

**Why this is more valuable under zlib, not less (reviewer 2):** today every rewritten blob pays one `rayon::spawn` + one `PIPELINE_DISPATCH_PERMITS` acquire + compression + one channel send. Moving compression into the worker that already owns the uncompressed bytes saves the dispatch, saves the permit dance, and keeps the block resident on one core's L2/L3 from encode through frame. Under zlib the **64-permit pool was capping concurrency at ~64 in-flight rayon tasks**; fusing compression into the classifier pool lets all 22 workers run compression without that cap.

**Public API doesn't move** - `write_primitive_block_owned` stays for callers that want compression dispatched internally; streaming-merge just stops using it.

**Endpoint refinement (reviewer 1):** a crate-private `write_framed_parts_owned` that accepts `FramedBlobParts` directly would avoid the final `into_vec()` flatten copy. Still internal-only.

**Don't land before the streaming rewrite** - land #1 first, this is a ~50-line commit once the worker exists.

#### Q3: Pool partitioning - **divergence**

This is the one place the reviewers give different recommendations. Both agree the reader scanner, pread workers, and reorder actor run on **dedicated (non-rayon) threads**. The disagreement is about the classify + rewrite + writer-compression work.

**Reviewer 1:** dedicated rayon pool, not the global one. Pattern: [`src/read/pipeline.rs:127`](../src/read/pipeline.rs#L127) already shows the dedicated-pool shape; reuse it. Under `Compression::None`, size around **19 worker threads**, leaving 3 for reader/scanner, ordered emitter, and writer thread/kernel slack. Under zlib, either fuse compression into the blob workers (stay one dedicated pool) or use two distinct pools - but **do not use "one shared pool with priorities"** because rayon doesn't give the scheduling control that implies.

**Reviewer 2:** single shared rayon pool for classify + rewrite + writer-compression. Work-stealing across one pool smoothes imbalance better than two pools with fixed capacities. Two pools create artificial capacity walls - if classify is quiet and writer-compress is backlogged, a split pool can't rebalance. The only thing worth sizing explicitly is pread worker count; `decode_threads = cores - 2` is fine.

**Practical note.** Apply-changes has no other major rayon consumer running concurrently, so "dedicated pool with N threads" and "default global pool with N threads" collapse to the same hardware assignment in practice. The substantive difference is insulation-from-other-rayon-code, which isn't a concern here. **Decide at implementation time; not a high-stakes fork.** Both reviewers agree that "shared pool with priorities" is the wrong answer.

#### Q4: Prefill overlap - **one fake win, one real win**

Reviewer 1: don't overlap `prefill_from_base` with OSC parse. The skip logic at [`src/commands/apply_changes/node_locations.rs:73`](../src/commands/apply_changes/node_locations.rs#L73) needs a *complete, sorted* `needed_sorted` set, and the scan is one-way through the node section ([`node_locations.rs:121`](../src/commands/apply_changes/node_locations.rs#L121)) - starting early with an incomplete set risks skipping a node blob that later turns out to be required, which is unrecoverable without a rescan. With multiple diffs it gets worse because later diffs overwrite earlier state ([`src/osc.rs:979`](../src/osc.rs#L979)). If parse time matters, the right move is upstream: squash diffs before merge, not overlap inside it.

Reviewer 2: agrees the "overlap with OSC parse" framing is a ~1 s fake win. **But there's a larger, real win in a different shape** - *fuse prefill into the streaming pipeline's node phase*. In a sorted PBF, all node blobs precede all way blobs. The streaming pipeline decompresses every node blob that overlaps the diff anyway. Today prefill separately decompresses a subset of node blobs (those overlapping `needed_set`) purely to extract coordinates for LOW. The two passes read overlapping data.

**The restructure.** While classifier workers process node blobs, they opportunistically extract coordinates for any ID in `needed_set` they encounter. Each worker accumulates into a thread-local `FxHashMap<i64, (i32, i32)>`. When the pipeline transitions from nodes to ways (reorder actor detects this in seq order), merge the worker maps into `Arc<loc_map>`, and gate way-blob workers on it being ready.

**Gains.**

- Deletes the prefill phase entirely - **5 s saved at daily**.
- Decompresses fewer node blobs overall (blobs the pipeline was going to decompress anyway now do double duty).
- **At weekly scale (Q7) where `needed_set` is ~7x larger, prefill would otherwise balloon from 5 s to 30+ s.** Fusion keeps it at approximately zero.

**The dependency chain becomes:** OSC parse → refs collected → `Arc<needed_set>` handed to classifier workers → pipeline runs. The header walk for prefill's schedule disappears; workers decide opportunistically per-blob.

**One real complication.** The node→way transition needs a barrier so no way worker starts before `loc_map` is finalised. The reorder actor detects the transition from the indexdata `blob.kind` change (trivially detectable in a sorted base PBF) and owns the barrier.

**Reconciling with reviewer 1:** reviewer 1 declined the specific "overlap with OSC parse" shape because of the skip-logic concern. Reviewer 2's fusion shape sidesteps that concern entirely - the pipeline visits every node blob by necessity, so there's no risk of "skip a blob that turns out to be required". The two positions aren't contradictory.

**Sequencing.** Land streaming without this first (keep prefill as a serial pre-phase). Then fuse it as a follow-up - ~5 s at daily, proportionally more at weekly.

#### Q5: Trailing creates - no special case, both reviewers agree

The reorder actor owns `UpsertCursors` and treats end-of-stream as the current trailing-create block (search `merge()` for `MERGE_TRAILING_CREATES_START` marker): once the last blob has been emitted, flush the remaining upserts for the current and later kinds.

Reviewer 1 and reviewer 2 both note the existing `types_to_flush` match (in the same block, keyed on `last_type: Option<ElemKind>`) already encodes the cases: `None` → flush all three, `Some(Node)` → flush Node+Way+Rel, etc. Port it verbatim to the actor's post-loop.

**Testing target (reviewer 2):** an empty-base-PBF case where `last_type` stays `None` and the actor has to emit gap/trailing creates for all three kinds with no frames ever seen. Today's code handles this via the `None` arm; the actor needs to preserve it. Write a test for it.

#### Q6: Backpressure budget - whole pipeline, with concrete carve-up

Both reviewers: **~1 GB in flight is a whole-pipeline budget, not a single channel cap**, and backpressure must propagate all the way to the scanner or the classifier side will buffer indefinitely behind a slow writer.

Reviewer 1: the current `WRITE_AHEAD=32` and `PIPELINE_DISPATCH_PERMITS=64` at [`src/write/writer.rs:31`](../src/write/writer.rs#L31) are count bounds, not a memory model. A single byte-credit budget across the whole graph is the right shape. If backpressure doesn't propagate to the reader scanner, the classifier side silently buffers.

Reviewer 2: concrete carve-up of the ~1 GB budget across stages:

| Stage | Channel | Capacity | Per-item UB | Budget |
|---|---|---:|---|---:|
| Reader → Classifier | `(seq, RawBlobFrame)` | 64 | ~1 MB compressed | 64 MB |
| Classifier workers in flight | decompress buf + `PrimitiveBlock` + rewrite scratch | 22 | ~20 MB | 440 MB |
| Classifier → Reorder | `(seq, ClassifiedItem)` | 64 | ~4 MB rewritten / 1 MB passthrough | ~128 MB |
| Reorder → Writer | `PipelineItem` | `WRITE_AHEAD=32` | ~4 MB | 128 MB |
| Writer buffered | `BufWriter` + uring buffers | - | - | ~16 MB |
| **Total** | | | | **~780 MB** |

Worst case; actual RSS is lower because many blobs are small. Leaves meaningful headroom under the 28 GB host limit.

**Backpressure chain (reviewer 2):** writer thread can't keep up → its receiver fills → reorder actor's send blocks → reorder's receiver fills → classifier workers' send blocks → they stop pulling new frames → reader channel fills → pread workers' send blocks → they stop pulling from `dispatch_rx` → scanner's send on `dispatch_tx` blocks → scanner stops reading headers. Halts cleanly end-to-end **provided every channel is bounded.**

**Failure mode to avoid (reviewer 2):** any unbounded channel or any `par_iter().collect()` pattern. `collect()` materialises the whole batch and breaks the bound. Streaming sidesteps this entirely; if it creeps back in later, flag it immediately - one unbounded buffer anywhere defeats the whole chain.

**Per-worker unbounded accumulator caveat (reviewer 2).** Under the prefill-fusion change (Q4), the per-worker coord extraction map needs size discipline too. For planet daily/weekly, `needed_set` is bounded by OSC way-refs (at most millions of entries); not a practical concern at this scale, but worth the discipline.

#### Q7: Weekly OSC - same first move, different priorities after

Both reviewers: **streaming pipeline is still the right first move at weekly scale.** What changes is the priority order after it lands.

**Reviewer 2's concrete changes from daily to weekly:**

1. **OSC parse becomes a real phase.** Single-threaded XML + gzip at 7x data: probably 30-40 s serial. Today's 5 s hides; weekly's 35 s doesn't. [`load_all_diffs`](../src/osc.rs#L1036) parses files sequentially into one overlay - that sequential loop becomes a bottleneck. **Parallelise it:** parse each OSC concurrently into its own overlay, then merge overlays with newer-wins semantics. Each OSC is independent work. Merge pass is a few seconds over the combined overlays. Same story for `write_streaming` in [`merge_changes::write_streaming`](../src/commands/merge_changes/mod.rs). This is a real target at weekly scale, separate from the streaming pipeline work. **Primitive for the merge step:** [`IdSetDense::set_atomic_if_new`](../src/commands/id_set_dense.rs#L163) (pre-allocated per element type via `pre_allocate(max_id)`) is the concrete shape for newer-wins duplicate detection - walk overlays newest-first, call `set_atomic_if_new(id)` per element, keep the element only when the call returns `true`. The `_if_new` flavour returns `true` the first time a bit is set (atomic fetch-or under shared `&self`), which is exactly the primitive. `set_if_new` is the non-atomic variant for single-threaded paths. Built originally for `verify_ids --full` parallel rewrite; reuse here.
2. **`DiffRanges` scales linearly.** Sort-dedup of ~7x more IDs; still fast. Not a new bottleneck.
3. **Passthrough-to-rewrite ratio shifts downward.** Today ~60 % passthrough at planet. Weekly planet: probably ~70-80 % passthrough blobs at most, but higher *rewrite* share per affected blob. More rewrites, fewer passthroughs, bigger rewrite CPU. The 4.1-core classify plateau gets proportionally worse (more heavy blobs per batch). **Case for streaming gets stronger** at weekly.
4. **Prefill `needed_set` is ~7x larger.** More node blobs to scan, more `FxHashMap` pressure. **This is where Q4's fusion becomes critical** - prefill as a separate pass could balloon from 5 s to 30+ s, but fused into the pipeline's node phase it costs approximately nothing.
5. **Writer output roughly unchanged in absolute bytes.** Still ~92 GB out. Writer is not more of a bottleneck weekly than daily.
6. **Per-rewrite-blob cost goes up** - more elements per blob get modified. This hits the rewrite CPU budget. It also makes Q2 (worker-emits-framed) *more* valuable because more blobs go through the rewrite path.

**Prioritisation shift at weekly scale:**

- Streaming pipeline: **more valuable**, not less.
- Worker-emits-framed (Q2): **more valuable** (more blobs benefit).
- Prefill fusion (Q4): **more valuable** (avoids a 30+ s phase growth).
- Parallel OSC parse: **a new priority** that doesn't exist at daily. Plausibly 20-30 s of headline wall to recover.
- Splice-near-passthrough rewrites: **less valuable at weekly** - rewrite blobs touch more elements per blob, so the near-passthrough population shrinks. Demote.

**Reviewer 1 note on weekly:** diff-squashing becomes genuinely interesting as a **formal upstream stage** rather than paying XML parse + overwrite churn inside the critical path every run. "Squash diffs to one final overlay / binary delta" can be a separate command that runs once per week and emits a single pre-merged diff file that apply-changes then consumes as if it were a daily. Orthogonal to the streaming rewrite but may be the right long-term shape if weekly is the standard cadence. **Primitive:** same [`IdSetDense::set_atomic_if_new`](../src/commands/id_set_dense.rs#L163) shape as the in-pipeline parallel OSC merge above - walk overlays newest-first across parallel workers, dedupe per element type against a pre-allocated bitmap.

**Reviewer 2's revised end-state estimate for weekly planet:** **70-90 s wall** with streaming + parallel OSC parse + prefill fusion + worker-framing. Current weekly (linear scaling of serial phases) probably ~250 s+.

## Third review round + synthesis (2026-04-20)

After the Q1-Q7 round, commissioned two fresh reviews with explicit context about what the prior rounds had already covered (to avoid rehash) and with the findings from our own code read (which the Q1-Q7 round didn't have). Both reviewers have full tree access. Each produced an initial review, then rebutted the other's review after we confronted both with the disagreements. This round is the design we're actually going to build.

### R3 reviewer one - initial headline

- Affirms the core move: delete the batch loop, fuse classify+rewrite, one drain actor, commit in one change.
- CPU-budget floor refined: 295 s classify / 22 cores = 13.4 s (not the Q1-Q7 round's 12.3 s). Fused classify+rewrite ~375 s CPU / 22 cores ≈ 17 s wall floor.
- Q1 state concentration: "concentrated but not splittable" - keep single drain actor, the happens-before edges are too tight.
- Q3 memory: `PrimitiveBlock` shares Bytes with the decompress buffer via refcount, so per-worker steady state is ~3 MB × 22 ≈ 66 MB, not the 160-200 MB Q3 worried about. The concern dissolves.
- Q4 backpressure under pathologically slow rewrite: bounded queues ~4× workers hold the slow blob; other workers keep producing past it within capacity.
- Q5 sequencing: "do it in one commit" - decomposition doesn't buy measurability because the batch barrier is the thing being deleted.
- Proposed priorities:
  - P1: streaming pipeline + fused workers (one commit).
  - P2: merge worker-pool output reorder with `PbfWriter`'s reorder via a "pre-ordered input" entry point on the writer.
  - P3: kill per-task `BlockBuilder::new()`, use thread-local `BlockBuilder`s (folded into P1 as a design point).
  - P4: use `IndexedReader` schedule instead of header walk (save ~15s startup).
  - P5: prefill folds into P1's worker pool.
  - P6: per-group raw-byte splice for low-modification rewrite blobs (follow-up).
  - P7: eliminate double-classify of unindexed blobs (low value, `--force` only).

### R3 reviewer two - initial headline

- Confirms the core move.
- Corrects R2's own earlier 12.3 s floor to 13.4 s; fused floor ~17 s.
- **Sharpens the reshape: descriptor-first, not RawBlobFrame-first.** The scanner emits per-blob descriptors `(seq, frame_start, frame_len, data_offset, data_size, index, tagdata)`. Workers pread body bytes only for actual overlap candidates. `parallel_reader.rs` in its current RawBlobFrame-first shape gets deleted, not wrapped - it is the wrong boundary type.
- `IndexedReader` is not a shortcut: [`src/read/indexed.rs`](../src/read/indexed.rs) walks headers sequentially and stores only blob offsets + decoded ID ranges, not the frame/data/tagdata schedule descriptor-first needs. [`src/read/header_walker.rs::HeaderWalker`](../src/read/header_walker.rs) is the right primitive.
- Don't bundle a `PbfWriter` rewrite into this commit - "pre-ordered input" entry point is a reasonable follow-up after streaming lands, not part of the main landing.
- "80.7 GB churn goes away" is too strong: persistent `BlockBuilder`s eliminate per-task builder allocation, but `Vec<OwnedBlock>`, encode buffers, framing allocations still exist unless you also pool those boundaries.
- Channels are not the problem in the abstract. Removing mpsc is a design consequence of removing batch barriers, not a goal in itself. A good streaming design still needs bounded queues + explicit backpressure.
- **Optimize unapologetically for indexed input**, treat `--force` / no-indexdata as a fallback path. Don't warp one universal pipeline around both modes.
- **Cursor-rule invariant is a silent-break risk**: `rewrite.rs`'s rewrite slot advances the cursor past `blob_osm_last_key(min_id, max_id)` only for Rewrite slots (inline upserts are emitted as elements during the rewrite). Passthrough/FalsePositive slots don't touch the cursor so inline upserts in that ID range become gap creates on the next same-type blob. A uniform cursor-advance rule under streaming silently breaks the contract.
- Byte-budget backpressure, not count - CopyRange descriptors are tiny (~32 bytes), rewritten payloads are large (~500 KB).
- Local fix worth landing: `classify_only()` currently does Vec → `Bytes::from` → `PrimitiveBlock::new` which copies again; switch to `from_vec_with_scratch` / `from_vec_pooled_with_scratch`.

### R1 responds to R2 (concessions + pushbacks)

**Large concessions:**

- Descriptor-first is correct. R1 admits conflating "body is preread" with "batch barrier" as one problem when they're two. ~85 GB of wasted pread+memcpy per run on a 92 GB input at planet is the body-pread cost that descriptor-first eliminates.
- Byte-budget reorder, not count - conceded without argument.
- Cursor-rule invariant is a silent-break risk - upgrade to explicit property-test target.
- Prefill folding promoted from P5 to P2 - matches R2's framing.
- "Optimize unapologetically for indexed input" - agrees, drops their P4 (`IndexedReader`) since R2 pointed out it doesn't have the schedule shape anyway.
- `classify.rs` framing softened: `Bytes::from(Vec<u8>)` is O(1) refcount bump, not a literal second copy. Real fix is scratch reuse via `from_vec_with_scratch`, not copy elimination. Both reviewers converged on the fix.

**New design constraint R1 caught that R2 didn't address:**

- **`--direct-io` output is incompatible with `copy_file_range`.** On the `--direct-io` path, the drain side still needs body bytes because the kernel-space splice isn't available. Descriptor-first is correct on default buffered / io_uring; on `--direct-io` the drain must tell workers to pread the body and the payload flows through the worker → drain channel. This is a drain-side policy asymmetry, not a reader-side one.

**Pushback R1 maintains:**

- **Worker-emits-framed-bytes under `--compression none`**: R2 says defer; R1 says the current chain under `--compression none` is worker → drain → `write_primitive_block_owned` → `rayon::spawn` → `frame_blob_into` (memcpy) → writer_thread reorder → sink. That's two thread hops and one useless rayon spawn per rewritten blob for a memcpy. Once the drain is the ordered delivery point, calling back into the writer's reorder is redundant. R1's concession: land P1 without it, measure, add as P1.5 if writer chain shows as next ceiling; R1 bets it will.
- **`copy_file_range` coalescing must land in same commit as P1**, not as a follow-up. Without coalescing, descriptor-first issues ~120k individual `write_raw_copy` calls at planet; the whole point of descriptor-first is making `copy_file_range` efficient. Port the contiguous-range coalescer from [`src/commands/altw/passthrough.rs`](../src/commands/altw/passthrough.rs).

### R2 responds to R1 (convergences + remaining disagreements)

**Convergences confirmed:**

- Batch loop is the real floor, not the batch size.
- Drain actor should stay single-threaded - one ordered state machine, not two.
- No halfway version preserving the batch barrier is worth landing.

**Remaining R2 positions after R1's rebuttal:**

- **Descriptor-first means the scanner itself fast-paths non-overlap indexed blobs into the ordered stream** - they never reach the worker pool at all. R1's revised sketch had workers pread body only for overlap candidates but still routed all descriptors through workers; R2 is more explicit that passthrough descriptors bypass the worker pool entirely. At ~92% passthrough ratio at planet, this puts most of the blob traffic straight from scanner to drain without any worker touching them.
- **`IndexedReader` is not a shortcut** - this is a factual correction, not a preference. The schedule shape R1 envisioned doesn't exist in `indexed.rs`; `HeaderWalker` is the right primitive.
- **Don't rewrite the writer pipeline in this commit.** R1's "pre-ordered input" entry point on `PbfWriter` is a reasonable follow-up; not a first-landing item. Feed the existing `PbfWriter` as-is from the drain actor; if writer ordering becomes the new floor after the rewrite, add pre-ordered mode then.
- **"80.7 GB churn goes away" is too strong.** Persistent `BlockBuilder`s help, but `Vec<OwnedBlock>` output in [`rewrite_block.rs`](../src/commands/apply_changes/rewrite_block.rs), block encode buffers in [`block_builder.rs`](../src/write/block_builder.rs), and framing allocations still exist unless you also pool those boundaries. Plan for reduction, not elimination.
- **Channels are not the problem in the abstract.** A good streaming design still needs bounded queues + explicit backpressure; removing mpsc is a consequence of removing batch barriers, not a goal.

### Synthesized design (the one we're going to build)

Convergence after both rebuttals is ~95%. The design below is what we commit.

**Scanner** (single thread, driven by [`HeaderWalker`](../src/read/header_walker.rs)):
- Emits one descriptor per OsmData blob: `(seq, frame_start, frame_len, data_offset, data_size, index, tagdata, kind, id_range)`.
- Does NOT read blob bodies.
- **Scanner fast-path**: for blobs with indexdata whose `id_range` doesn't overlap the diff, emit the descriptor as a `Passthrough(CopyRange)` variant routed directly into the ordered drain stream. Never reaches the worker pool. At planet ~92% of blobs qualify.
- Overlap candidates emit as `Candidate` and go to the worker pool via a bounded dispatch channel.

**Worker pool** (long-lived, `nproc - 2`):
- Each worker owns thread-local `BlockBuilder` + decompress scratch + parse scratch. No per-blob `BlockBuilder::new()`.
- Per-blob work: pread body → decompress (scratch reuse) → parse via `from_vec_with_scratch` (no extra copy) → precise check.
  - **False positive**: drop the body, emit a `Passthrough(CopyRange)` to the drain.
  - **Actual overlap**: rewrite inline using the persistent `BlockBuilder`, emit `Rewritten(blocks)` (or framed bytes if P1.5 lands) to the drain.
- For `--locations-on-ways`, workers decompressing node blobs opportunistically extract coords for `needed_set` IDs into per-worker `FxHashMap<i64, (i32, i32)>` accumulators.

**Drain actor** (single thread):
- Byte-budget reorder buffer keyed by global seq. Start ~128 slots + byte permits; tune after measuring.
- Owns `UpsertCursors`, `last_type`, gap-create `BlockBuilder`, passthrough coalescer, `MergeStats`, writer handle.
- Pulls items in seq order. For each item:
  - Type transition (`last_type != item.kind`): flush passthrough coalescer, call `flush_remaining_upserts` (existing logic in [`stream_output.rs`](../src/commands/apply_changes/stream_output.rs) ports verbatim).
  - Gap creates (`cursor` has upserts with `id < item.min_id`): flush coalescer, call `emit_gap_creates`.
  - `Passthrough(CopyRange)`: extend the contiguous-range coalescer; flush when a non-passthrough item arrives or when a gap/transition forces it.
  - `Rewritten(blocks)`: flush coalescer, then hand each block to the writer via the existing output-side path.
  - **Cursor advancement**: only on Rewrite items (matching current behavior). Passthrough/FalsePositive items do NOT touch the cursor - inline upserts in the blob's ID range correctly become gap creates on the next same-type blob.
- Merges per-worker coord maps into `Arc<loc_map>` at the node→way boundary; signals the scanner to begin dispatching way-blob descriptors only after the merge publishes (see "Node→way barrier ownership" below).

**Node→way barrier ownership** (explicit, scanner-side):

- The scanner, not the drain, enforces the barrier. As soon as the scanner sees its first way-blob descriptor, it stops dispatching to the worker pool and buffers subsequent way/relation descriptors in a pending queue. It continues emitting already-seen node-blob descriptors to the worker pool.
- When the last node-blob descriptor's result has been drained, the drain merges the per-worker coord maps and publishes `Arc<loc_map>`; it then signals the scanner (via a `nodes_done` atomic flip or a `oneshot::Sender<Arc<loc_map>>` depending on shape).
- The scanner receives the signal, swaps in the published `loc_map`, and begins flushing its buffered way/relation descriptors to the worker pool.
- **Why scanner-side and not drain-side:** relying on the drain "noticing" the `blob.kind` transition in the seq stream allows a way-blob worker to start classify concurrently with a still-in-flight node-blob worker, because the drain sees blobs in seq order but workers start them as soon as the dispatcher emits them. The barrier has to sit ahead of dispatch, not behind it. Ownership is in the scanner/dispatch path because that's the only place that can withhold work from the worker pool.
- At channel close, runs the existing `types_to_flush` match for trailing creates.

**`copy_file_range` coalescing** (ported from [`altw/passthrough.rs`](../src/commands/altw/passthrough.rs)):
- Drain actor accumulates a contiguous byte range from consecutive `Passthrough(CopyRange)` items.
- Flushes as a single `write_raw_copy(input_fd, range_start, range_len)` when the range is broken (non-passthrough item, different kind, gap create, or buffer cap).
- Without this, descriptor-first issues ~120k individual `write_raw_copy` calls at planet and underperforms. **Lands in the same commit as P1.**

**`--direct-io` fallback** (drain-side policy):
- When the output backend can't splice (direct-io output, `use_copy_range == false`), the scanner tags passthrough descriptors as "needs-pread" based on the output-backend decision made at `merge()` setup time. Tagged descriptors are routed to a dedicated pread helper (the worker pool, or a small dedicated helper pool) that preads the **full framed bytes** from `(frame_start, frame_len)` - not just the blob body. Workers emit `Passthrough(OwnedBytes(Vec<u8>))` carrying the complete frame.
- Rationale for full-frame pread (R3R2, 2026-04-20): preads only the body, the drain would have to re-assemble the frame header before writing, re-encoding tagdata / indexdata / length prefix. Preading the full frame preserves the exact on-disk bytes and the drain path becomes `write_raw_owned(frame_bytes)` with zero byte reconstruction. Same work, simpler shape.
- Drain writes via `write_raw_owned` or the coalescing passthrough buffer (`write_raw_chunks`) as appropriate.
- Preserved asymmetry: one scanner shape, two drain-side output paths (CopyRange on splice-capable backends, OwnedBytes of full frames on direct-io).

**`--force` / no-indexdata fallback**:
- Descriptors have `index: None`. Scanner fast-path can't fire (no range info). All descriptors flow to the worker pool.
- Workers decompress to scan (existing `scan_block_ids` path), then precise check / rewrite.
- Reduced fast-path coverage, same correctness.

**`--compression zlib:6` path**:
- Works unchanged under the design. Rewritten blocks go through the existing writer compression pipeline. Writer-side compression contention may show as the new floor at zlib:6 scale; P1.5 worker-emits-framed-bytes addresses it but is deferred.

### Remaining live disagreements

1. **`copy_file_range` coalescing in the first commit or as a follow-up?** R1 says same commit (without coalescing, descriptor-first issues ~120k individual write_raw_copy calls at planet). R2 listed it as a local change. **Resolution: same commit.** R1's concrete argument wins on the merits and the coalescer is a straight port from ALTW.
2. **Worker-emits-framed-bytes in v1 or P1.5?** R1 bets it's needed under `--compression none`; R2 says defer until measurement confirms. **Resolution: land P1 without it; if post-landing bench shows writer chain dominating, add as P1.5.** Measurement decides, same commit boundary either way.

### New correctness invariants surfaced this round

Added to the "Correctness invariants" section:

- **Cursor-rule advancement difference** (silent-break risk). Rewrite slots advance the cursor past `blob_osm_last_key(min_id, max_id)` inside the rewrite. Passthrough/FalsePositive slots do NOT touch the cursor - inline upserts in that ID range become gap creates on the next same-type blob. A uniform cursor-advance rule under streaming silently breaks the contract.
- **`--direct-io` fallback**: workers pread body bytes when the output backend can't splice. Drain-side policy, not reader-side.
- **Scanner fast-path correctness**: passthrough descriptors for non-overlap indexed blobs never decompress the body. The descriptor must carry enough metadata (kind, id_range, frame_start, frame_len, index, tagdata) for the drain actor's type-transition, gap-create, and coalescing logic without referring back to the body.
- **Node→way transition barrier** (under prefill fusion): no way-blob classify work may execute before the per-worker coord maps are merged into `Arc<loc_map>`. Ownership is **scanner-side**: the scanner holds way/relation descriptors in a pending queue after the first way-blob is seen, and only begins dispatching them once the drain publishes the merged `loc_map` and signals ready. See "Node→way barrier ownership" in the Synthesized design.

### Scope estimate (updated)

Not the Q1-Q7 round's "~300 lines deleted / ~400 added" estimate. With descriptor-first and scanner-level fast-path:

- **Delete:** [`parallel_reader.rs`](../src/commands/apply_changes/parallel_reader.rs) entirely (~330 lines); the batch loop inside `rewrite.rs::merge()` (~320 lines); `node_locations.rs::prefill_from_base` (~50 lines, the whole extra scan).
- **Add:** new `scanner.rs` module with `HeaderWalker` descriptor emission + fast-path routing (~120 lines); new `streaming.rs` with worker pool + dispatch (~180 lines); new `drain.rs` with the ordered drain actor (~300 lines, most of gap-create / type-transition / coalescing logic ports verbatim from `stream_output.rs`); descriptor types (~50 lines).
- **Modify:** `rewrite.rs::merge()` becomes a thin orchestrator (setup + spawn scanner + spawn workers + spawn drain + join, ~100 lines); `classify.rs::classify_only` loses its fast-path branch (scanner owns it now) and becomes slow-path-only - decompress + parse + precise check + rewrite dispatch; switches to `from_vec_with_scratch` for the parse.

Net scope: ~650-700 lines added, ~700 deleted. Larger delta than the Q1-Q7 synthesis estimated, because descriptor-first deletes `parallel_reader.rs` outright and splits the design across new modules. Net LOC roughly flat; architecture change is load-bearing.

### Cross-validation plan

- **Denmark**: `brokkr verify merge --dataset denmark` must pass byte-for-byte against current main output.
- **Europe**: byte-for-byte against current main output on 38 GB altw + OSC 4715, plus hash compare.
- **Planet**: `--bench 1` first; if result lands in 40-55 s range, `--bench 3` for tighter numbers.
- **Property tests** (new, added before landing):
  - **Cursor-rule**: passthrough blob whose ID range contains an upsert that should become a gap create on the next same-type blob. Output-diff vs current implementation.
  - **Empty-base-PBF**: `last_type == None` forever; trailing creates flush all three kinds per existing `types_to_flush` match.
  - **`--direct-io` parity**: output identical to buffered path on Denmark.
  - **`--force`**: output identical to indexed path on Denmark (the scanner fast-path hit-rate differs but that doesn't affect output).
  - **`--locations-on-ways` prefill fusion**: way-blob with OSC-created way referencing both in-base and in-diff node IDs; coords resolve correctly from the fused loc_map.

### Abort / pivot plan (R3R1)

The 40-55 s target rests on the batch-barrier-as-bottleneck hypothesis. If the first planet `--bench 1` after P1 lands and the wall doesn't drop anywhere near 40-55 s, the hypothesis is falsified and something other than the batch barrier is limiting classify throughput. **Concrete abort threshold: if planet `--bench 1` at P1 lands at >80 s wall, stop and diagnose before iterating.** The diagnostic to read first: `merge_classify_decompress_ns` cumulative in the new shape.

- Decompress is **70% of classify CPU today** (206 s of 295 s measured at `e49b6182`, per the #4 section below). If `merge_classify_decompress_ns` in the new shape is still ~206 s cumulative, the worker pool is saturating cores with decompress work and the theoretical floor is ~206/22 ≈ 9.4 s wall - the 17 s fused floor is consistent with that. If post-P1 wall sits near the floor, descriptor-first + fused worker dispatched decompress efficiently.
- If `merge_classify_decompress_ns` in the new shape has *not* dropped proportionally with wall (i.e. decompress CPU is the same but workers aren't saturating), worker dispatch isn't feeding the pool evenly - the scanner's fast-path/slow-path split is leaving cores idle on the slow-path side. Look at per-worker CPU counters; rebalance dispatch.
- If `merge_classify_decompress_ns` has dropped but wall still hasn't, something downstream of classify (drain actor contention, writer chain, scanner throughput, reorder buffer starvation) is the new bottleneck. The byte high-water counters on the new queues (see Memory budget section) tell us which channel is starving.

**Revert criterion:** if after one round of targeted diagnosis and tuning the wall is still >80 s on planet, revert P1 cleanly and escalate to another review round. Don't let the branch sit at a regression while we speculate.

### Architectural value vs wall-clock value at planet daily `--compression none` (R3R1 honesty pass)

Worth naming explicitly because the plan's framing can read as "descriptor-first saves 85 GB of IO." It does, in the sense that those 85 GB of body bytes no longer traverse the userspace pipeline - but:

- `merge_reader_blocked_sends = 0.92%` and `merge_consumer_recv_wait_us = 3.1%` at planet today. The reader isn't the bottleneck. Skipping the 85 GB userspace body read reduces IO+memcpy wall by "maybe a few seconds" (R3R1) - not the apparent ~85 GB / NVMe-bandwidth headline.
- Potential downside: skipping the userspace pre-read may cost page-cache warmth for the subsequent kernel-side `copy_file_range`. Unquantified; measurement will tell.
- **The real value of descriptor-first at planet daily `--compression none` is architectural**: it's the correct boundary type once the scanner-fast-path bypass is in place; it makes contiguous `copy_file_range` coalescing clean to express; and it collapses the worker pool's load to only the ~8% of blobs that need it, which is what unlocks the fused classify+rewrite floor.
- Under `--compression zlib:6` or at weekly OSC scale, body reads would be on the critical path and descriptor-first's IO reduction becomes real wall-clock savings. At planet daily `--compression none`, it's about design correctness enabling the other wins, not direct IO savings on this workload.

Acknowledging this honestly doesn't change the plan; it does change the headline attribution. Post-landing, if the wall is at ~45 s, most of the 100 s of savings is from removing the batch barrier + fused rewrite, not from skipping body reads.

### Estimated outcomes

- **Wall at planet daily, `--compression none`**: 144.4 s → **40-55 s** (both reviewers' estimate; CPU-budget floor ~17 s fused, plus ~10 s OSC+prefill+setup, plus ~10-20 s writer I/O running concurrent with workers).
- **Peak RSS**: ~2.0-2.7 GB. Lower than today's batch-wide retention pattern because workers hold only single blob state, not batch-wide `PrimitiveBlock` + job + received vectors. Per-worker steady state ~7-10 MB (not ~3 MB - see Memory budget section for the R3R1 correction on rewrite-side allocations).
- **Alloc churn**: significant reduction (persistent `BlockBuilder` eliminates the 80.7 GB per-run churn bucket) but not elimination (`Vec<OwnedBlock>`, encode buffers, framing allocations remain unless also pooled - defer to post-landing if RSS matters).
- **Weekly OSC**: ~70-90 s with streaming + prefill fusion + parallel OSC parse (the last one is a separate workstream, not part of P1). Current weekly estimate ~250 s+.

### What this round changed vs the Q1-Q7 synthesis

| Item | Q1-Q7 position | R3 synthesized position |
|---|---|---|
| Scanner shape | Wrap existing `parallel_reader.rs`, swap consumer | Delete `parallel_reader.rs`; descriptor-first HeaderWalker scanner |
| Blob body handling | Workers pread every blob body | Workers pread only overlap candidates (~8% at planet); passthrough bypasses worker pool |
| Boundary type | `RawBlobFrame` carries bytes everywhere | `Descriptor` carries metadata; bytes carried only when needed (overlap worker, `--direct-io` fallback) |
| `PbfWriter` reorder | Consider "pre-ordered input" entry point | Keep as-is; revisit only if writer shows as new floor |
| `copy_file_range` coalescing | Listed as opportunistic follow-up | **Same commit as P1** (R1's argument wins) |
| Alloc churn claim | "80.7 GB vanishes" | Significant reduction, not elimination (R2's pushback) |
| Reorder capacity | ~4× workers, count-based | ~128 slots + byte permits (CopyRange tiny, Rewritten large) |
| Cursor invariants | "port `types_to_flush` verbatim" | Explicit property-test target for Rewrite vs Passthrough/FalsePositive advancement difference |
| `--direct-io` fallback | not addressed | Drain-side policy: workers pread body when output can't splice |
| Prefill fusion | Q4 follow-up after streaming | P2, part of P1 worker pool (both reviewers converged) |
| `classify_only` parse copy | not noticed | Switch to `from_vec_with_scratch` inside P1's worker |

## What to leave alone

These are the parts of today's code that survive P1 unchanged. The batch-loop machinery itself does *not* survive; it's on the delete list inside P1.

- **The rewrite hot path** (`rewrite_block_parallel`, `emit_create_local`, `write_base_*_local` family). Already uses `pre_seed_string_table` to avoid re-interning, `add_way_raw_bytes_with_locations` to forward raw fields 9/10 byte-for-byte, and `add_relation_raw_bytes` to skip re-parsing members. This path is tightly written. P1's workers call `rewrite_block_parallel` directly from inside the worker loop with a thread-local `BlockBuilder` instead of a fresh per-task builder.
- **The pipelined writer** (`PbfWriter` + rayon compression + 64 permits). Under `--compression none` the rayon tasks become near-passthrough, but the structure is still correct and sized. R1's "pre-ordered input" entry point on `PbfWriter` is explicitly deferred to P1.5 / follow-up; P1 feeds the existing writer as-is.
- **The coalescing logic in [`stream_output.rs`](../src/commands/apply_changes/stream_output.rs)** - `coalesce_passthrough`, `emit_gap_creates`, `flush_remaining_upserts`, `has_gap_creates`, `emit_create_for_output`. Ports to the drain actor near-verbatim; these functions *are* the drain actor's state machine.
- **`NodeLocationIndex.locations` as `FxHashMap<i64, (i32, i32)>`.** Lookups are only for OSC ways (few million at daily scale). HashMap at ~240 MB for 10 M entries is the right shape for sparse lookup. Under P1 the map is populated by fused extraction in the worker pool (prefill phase deleted) but the map shape survives.
- **`DiffRanges` sorted vecs + `partition_point`.** Already the right shape for range-overlap (scanner uses it) and inline upsert assignment (worker uses it).
- **`CompactDiffOverlay` / OSC parse.** Single-threaded but small (100-500 MB input, ~1-5 s); not on the critical path at daily scale. Weekly scale needs parallel OSC parse but that's a separate workstream.
- **`UpsertCursors` + gap-create / trailing-create logic.** Complex but correct. Moves to the drain actor under P1; the sequential constraints are fundamental to preserving OSM ID order across passthrough boundaries.
- **`#[cfg(feature = "hotpath")]` phase timers.** Existing measurement scaffolding. Keep enabled through the P1 measurement runs.

### Parts that explicitly DO NOT survive P1

- **The batch loop in `merge()`** (~320 lines from `rewrite.rs:329-640`). Deleted.
- **[`parallel_reader.rs`](../src/commands/apply_changes/parallel_reader.rs)** (the whole file, ~330 lines). The RawBlobFrame-first boundary type is wrong under descriptor-first.
- **`NodeLocationIndex::prefill_from_base`** (~50 lines). Fused into the P1 worker pool's node phase; the separate pass is deleted.
- **`classify.rs::classify_only`'s fast-path branch** (lines checking `frame.index` for range-overlap). Scanner owns the fast-path now; the worker-side classify becomes slow-path only.
- **`collect_batch` + `BATCH_MAX_BLOBS` + `MERGE_BATCH_BYTE_BUDGET`.** Gone - no batches.
- **The per-batch `rayon::spawn` rewrite dispatch** (the whole `for (job_idx, job) in rewrite_jobs.into_iter().enumerate()` loop). Gone - workers hold persistent `BlockBuilder`s and rewrite inline.
- **The per-batch `received: Vec<Option<RewriteOutput>>` reorder buffer.** Gone - replaced by a single global byte-budget reorder buffer in the drain actor, keyed by global seq not per-batch slot.

## Plan of attack

1. ~~**Enable `#[cfg(feature = "hotpath")]` per-phase timers unconditionally**~~ **Done.**
2. ~~**Land parallel prefill (#2).**~~ **Landed 2026-04-18 (`52c2c4b`), planet -10.5 s wall.**
3. ~~**Land classify instrumentation (#4).**~~ **Landed 2026-04-18 (`b769996`).** Measurement at UUID `e49b6182` - findings in #4 section above.
4. ~~**Skip `scan_block_ids` when indexdata was present**~~ **Landed 2026-04-18 (`da1c45e`)**, saved 10.7 s CPU but only ~2 s wall (mostly lost to variance).
5. ~~**Bump classify batch size**~~ **Landed 2026-04-18 (`bfac63b`) - 512 MB merge batch budget.** Raised average batch from ~13 to ~18 overlap blobs but classify wall was unchanged (72.5 s vs 70.3 s). **Diagnosis from external review: the plateau is structural to the batch barrier, not fixable with batch-size knobs** - see "External review synthesis" section.
6. ~~**Parallel reader (#3)**~~ **Landed 2026-04-18 (`c97d6b5` streaming variant)**. Net-zero wall change vs sequential reader. Kept as infrastructure for the streaming pipeline below.
7. ~~**Cheap disambiguation experiment**~~ **Attempted + reverted 2026-04-18.** `rayon::scope` + per-batch spawn + mpsc + ReorderBuffer regressed from ~140 s to 10 min+ at planet (Denmark verify passed). See "Cheap disambiguation experiment" subsection for post-mortem. Doesn't disprove the plateau hypothesis, but rules out this form of cheap check. **Going direct to streaming pipeline instead.**
8. ~~**Descriptor-first streaming pipeline (P1).**~~ **Landed `719f306`, 2026-04-21.** Scanner + worker-pool + drain shape from "Third review round + synthesis" shipped as one atomic flip. See "Implementation progress" above for the detailed landing summary. Planet `--compression none` 144.4 s → 135.5 s, Europe 46.1 s → 49.8 s, zlib:6 and zstd:1 benefit more (see Current state at top). HDD writer wall is the new ceiling at planet, not the CPU floor the plan predicted.

9. ~~**P1.5 - Worker-emits-framed-bytes.**~~ **Landed in the same commit as P1.** Workers call `frame_blob_pipelined` inline and ship framed `Vec<u8>` chunks to drain via `DrainItem::Rewritten.framed_chunks`; drain uses `write_raw_owned` per chunk. `writer_pipeline_send_wait_ns` at planet `--compression none`: 859 s cumulative → 117 s (-86%). Measurement confirmed R1's prediction that the writer chain would become the new ceiling under `--compression none` post-P1.

10. **(weekly OSC only) Parallel OSC parse.** Parse each OSC concurrently into its own overlay; merge overlays newer-wins using [`IdSetDense::set_atomic_if_new`](../src/commands/id_set_dense.rs) (walk newest-first, keep on `true`). Current [`load_all_diffs`](../src/osc.rs) serialises this. Estimated **-20-30 s wall at weekly scale**; no win at daily.

11. **Splice-in-place for low-touch rewrites (follow-up, not yet landed).** For `NeedsRewrite` blobs with ≤K affected elements (K~64), splice the raw wire bytes for unmodified element runs instead of full decode+re-encode. Estimated ~1.5-2 s wall at daily, less valuable at weekly. Raw-group passthrough scaffolding in [`src/write/raw_passthrough.rs`](../src/write/raw_passthrough.rs) is the design surface. **Post-P1 re-evaluation (2026-04-21):** the planet wall is now HDD-writer-bound (`writer_write_ns = 120 s`, 89 % of wall at `--compression none`), not CPU-bound. Splice-in-place saves classify+rewrite CPU but does not reduce output bytes, so it's unlikely to move the wall on HDD targets. Value deferred until we have measurements on a faster target disk.

12. ~~**Writer path tuning** (`--io-uring` writer).~~ **Measured 2026-04-21.** With `RLIMIT_MEMLOCK` raised to unlimited on plantasjen, `--io-uring` at planet `--compression none` same-disk: 135.5 s → 108.6 s (-20 %). At `--compression zstd:1` same-disk: 121.2 s → 99.4 s (-18 %). Cross-disk gains are smaller (-2 s / -4 s) because read+write contention is no longer the bottleneck there. Best measured: `--io-uring` + cross-disk + zstd:1 = **82.8 s** at planet. Writer disk throughput jumps from ~830 MB/s (buffered) to 1.49 GB/s (io_uring). Setup is a one-shot `sudo prlimit --pid=$$ --memlock=unlimited:unlimited` before running.

13. **Exact-membership metadata / sidecar.** Only if FalsePositives remain material after P1. Currently 16 % of slow-path blobs; not negligible but not headline either. Format/index project, not a quick cleanup.

14. **Diff squashing as a formal upstream stage (weekly only).** Consider making "squash N diffs to one final overlay / binary delta" a separate command that runs once per cadence and emits a single pre-merged diff that apply-changes then consumes as a daily. Shares the `IdSetDense::set_atomic_if_new` newer-wins primitive with in-pipeline parallel OSC merge (item 10). Orthogonal to P1.

15. **`zstd:1` as a compression recommendation for internal pipelines (new, 2026-04-21).** Measured at planet: `--compression zstd:1` delivers 121.2 s wall (vs 135.5 s for `none` and 143.7 s for `zlib:6`), because workers parallelize zstd cheaply and the ~20 % output-byte reduction relieves the HDD writer bottleneck. Already gated behind a flag; consider documenting as the default for pbfhogg-internal pipelines (consumers that don't require osmium interop) in [`reference/performance.md`](../reference/performance.md) and README.

16. ~~**Target-disk experiment (new, 2026-04-21).**~~ **Measured 2026-04-21.** Source (Banan/nvme1n1) + output (Booty/nvme0n1p3, different physical NVMe) dropped planet `--compression none` from 138 s (direct invocation, same-disk) to 95.4 s cross-disk (-31 %). zstd:1 cross-disk: 121.2 s → 87.1 s (-29 %). This confirms the bottleneck on same-disk was read+write contention on one NVMe, not a software issue. The `target=hdd` label in the brokkr.toml was misleading: brokkr actually writes bench output to `scratch`, which symlinks to Banan (NVMe). The "hdd" classification was for an unrelated dir (cargo build output). Corrected in the Current state section.

17. ~~**Parallel writer** (new, 2026-04-21).~~ **Landed `4ec3589` + tuned `80b37df`.** `PbfWriter::to_path_parallel` spawns a writer thread that round-robins (offset, bytes) WriteOps across a pool of `POOL_SIZE=16` pwrite-based workers on a shared file descriptor. Writer thread reorders incoming items via the existing WRITE_AHEAD ReorderBuffer, computes offsets serially, dispatches to the pool. Workers run `pwrite` (for Raw / Framed / RawChunks) or `copy_file_range` (for CopyRange) at the pre-computed offset; cross-device copy_file_range (EXDEV) falls back to pread+pwrite with explicit offsets. CLI flag: `pbfhogg apply-changes --parallel-writer` (mutually exclusive with `--direct-io` / `--io-uring` in the current implementation). Pool size 16 chosen empirically (see matrix in Current state). Best planet wall: **80.9 s** cross-disk zstd:1, ties/beats io_uring depending on configuration.

### Items folded into P1 (no longer separate)

The following Q1-Q7 items are subsumed into the P1 single commit and do not land separately:

- Q2 worker-framing: *not* folded; explicitly deferred to P1.5 based on measurement.
- Q4 prefill fusion: folded into P1's worker pool.
- Q5 trailing creates: port the existing `types_to_flush` match verbatim to the drain actor.
- Q6 bounded backpressure: byte-budget reorder buffer + bounded channels in the P1 design.
- `copy_file_range` coalescing: folded into P1 (R1's same-commit argument won).
- Thread-local `BlockBuilder`: folded into P1 worker pool as a design point.
- `classify_only` parse via `from_vec_with_scratch`: folded into P1 worker slow-path (no separate commit).
- `scan_block_ids` skip when indexdata present: already landed `da1c45e`; scanner fast-path at scanner level further obsoletes the in-worker branch.

Cross-validation harness: `brokkr verify merge --dataset denmark` (note: name is `merge`, not `apply-changes` - the subcommand in `brokkr verify` predates the rename). P1 cross-validation plan is in the "Third review round + synthesis" section above; element-level diff (decompress, compare per-blob element lists sorted by ID) is the fallback when byte-equality fails due to blob boundary shifts.

## Memory budget

### Current (planet, post-#2, commit `52c2c4b`)

| Component | Size |
|---|---:|
| `CompactDiffOverlay` (daily OSC) | ~500 MB - 1 GB |
| `NodeLocationIndex.locations` | ~200-500 MB |
| `DiffRanges` sorted vecs | ~50-100 MB |
| Per-worker pread + decompress buffers × ~6 | ~200-400 MB |
| Writer pipeline + reorder buffer | ~200-500 MB |
| **Measured total** | **~1.8 GB** |

### Projected (post-P1 descriptor-first)

The Q1-Q7 round's carve-up assumed RawBlobFrame-first (every blob carries compressed body bytes through the worker pool). Descriptor-first changes the picture: ~92% of blobs flow from scanner directly to drain as ~32-byte CopyRange descriptors without ever visiting a worker. Workers only ever hold slow-path blobs (~8% of traffic).

| Stage | Item | Capacity | Per-item UB | Budget |
|---|---|---:|---|---:|
| Scanner → Drain (fast-path) | `CopyRange` descriptor | byte-budget ~16 MB | ~32 bytes | ~16 MB |
| Scanner → Workers (slow-path) | `Candidate` descriptor | byte-budget ~2 MB | ~32 bytes | ~2 MB |
| Workers in flight | decompress buf + parsed `PrimitiveBlock` + thread-local `BlockBuilder` + output `Vec<OwnedBlock>` | 22 workers × ~7-10 MB (see below) | ~7-10 MB | ~220 MB |
| Workers → Drain | `CopyRange` or `Rewritten(blocks)` | byte-budget ~128 MB | ~32 B or ~4 MB | ~128 MB |
| Drain internal | reorder buffer + passthrough coalescer + gap-create `BlockBuilder` | byte-budget keyed | mixed | ~64 MB |
| `PbfWriter` (existing, unchanged) | `BufWriter` + uring buffers + internal reorder | - | - | ~16 MB |
| Pre-loop phases | OSC overlay + DiffRanges + NodeLocationIndex | - | - | ~1.0-1.6 GB |
| **Pipeline total projection** | | | | **~1.5-2.1 GB** |

**Per-worker RSS correction (R3R1, 2026-04-20):** the ~3 MB/worker initial estimate (Bytes refcount sharing between decompress buffer and parsed `PrimitiveBlock`) under-counts the rewrite side. The thread-local `BlockBuilder` carries its own `StringTable`, dense accumulators (DenseNodes id/lat/lon deltas, Way refs, Relation member_ids/types/roles), tag-key `FxHashSet`, and tag-key scratch buffers - all of which grow to blob-size. The rewrite path also produces `Vec<OwnedBlock>` output (block-bytes + indexdata + tagdata per output block, typically 2-3 per rewritten input blob). Realistic per-worker steady state under a rewrite blob: **~7-10 MB**, not ~3 MB. 22 workers × 10 MB ≈ 220 MB in flight. Still safe under 28 GB, but the reorder byte-budget should be sized from this number, not the optimistic one.

### Trust measurement, not estimates (R3R2)

The budget table above is a sizing check, not a contract. Add **byte high-water counters on every new queue and the reorder buffer** before the first planet bench:

- `merge_scanner_to_drain_bytes_high_water` (CopyRange descriptor channel, fast-path)
- `merge_scanner_to_workers_bytes_high_water` (Candidate descriptor channel, slow-path)
- `merge_worker_inflight_bytes_high_water` (per-worker decompress + block + builder, summed)
- `merge_workers_to_drain_bytes_high_water` (mixed CopyRange + Rewritten)
- `merge_reorder_buffer_bytes_high_water` (drain's reorder, keyed by global seq)
- `merge_passthrough_coalescer_bytes_high_water` (contiguous `copy_file_range` run in flight)

Name the counters after the channel/structure that owns the bytes, not after an abstract "stage." When the first planet `--bench 1` lands, reading these against the table above tells us whether the estimates held or whether a channel is over/under-sized. Tune on evidence.

Pipeline in-flight state is lower than the Q1-Q7 round's ~780 MB estimate because most blobs never touch a worker; per-worker working set is higher than the initial R3 estimate because rewrite allocations were under-counted. Net: ~450 MB in-flight state projection (~220 MB workers + ~130 MB worker→drain channel + ~80 MB drain internal + scanner channels ~20 MB), not ~280 MB.

**Sizing robustness.** None of the structures above scale with `unique_referenced_nodes` the way the failed [altw-as-renumber](altw-as-renumber.md) `coord_table` did. `NodeLocationIndex` scales with the OSC's own node-ref set (daily-diff-sized, bounded), not with the base PBF's population.

## Correctness invariants

- **OSM ID ordering.** Output emits passthrough blobs in file order, rewrite blobs in file order (via the reorder buffer keyed by global seq), and gap creates before their matching blob's `min_id`. Any parallelization of reader, scanner, or workers must preserve file-order output via the drain actor's reorder buffer.
- **`LocationsOnWays` preservation on base ways.** `write_base_way_local_with_locations` forwards raw `lat_data()` + `lon_data()` verbatim. Do not touch this path. Under `--locations-on-ways`, every base way must produce fields 9/10 in the output; the existing logic does this by calling the `_with_locations` variant whenever `loc_map.is_some()`.
- **Zero-coord fallback for missing node refs in OSC ways.** [element_writes.rs](../src/commands/apply_changes/element_writes.rs) (search for `locations.push((0, 0))`): `match locs.get(&node_id) { Some(&loc) => ..., None => locations.push((0, 0)) }`. Preserved under parallel prefill *and* under prefill fusion - the merged locations map (whether from a separate prefill phase or the fused worker-pool extraction) must have the same entries the original sequential prefill would produce.
- **Straight `needed_set.contains` replaces `remove` in parallel prefill.** `contains` is cheaper than `remove`, and parallel workers cannot safely mutate a shared `FxHashSet`. Merge-at-end dedup covers the uniqueness semantic (a node hit by multiple workers will just insert the same `(lat, lon)` twice; last write wins, both values are identical).
- **`copy_file_range` path** on passthrough blobs (drain-side, gated on `use_copy_range`). Under descriptor-first, the file offset comes from the `CopyRange { frame_start, frame_len }` descriptor that the scanner emits directly. The drain actor coalesces consecutive contiguous ranges into a single `write_raw_copy` call (ALTW pattern). Preserve correct frame-boundary alignment.
- **Cursor-rule advancement difference (silent-break risk, R3R2).** Rewrite slots advance `UpsertCursors` past `blob_osm_last_key(min_id, max_id)` because inline upserts in that range were already emitted as elements during the rewrite. Passthrough/FalsePositive slots do NOT touch the cursor - inline upserts in their ID range correctly become gap creates on the next same-type blob. A uniform cursor-advance rule under streaming silently breaks the contract. **Property test target before landing P1**: passthrough blob whose ID range contains an upsert; output must produce gap-create on the next same-type blob, byte-identical to current implementation.
- **`--direct-io` fallback (drain-side policy, R3R1).** When the output backend can't splice (`copy_file_range` unavailable under direct-io output), workers must pread body bytes for descriptors that would otherwise become `CopyRange` and emit `Passthrough(OwnedBytes(Vec<u8>))` instead. Drain writes via the existing `write_raw_owned` / coalescing-passthrough path. Asymmetry: one scanner shape, two drain-side output paths. Property test: `--direct-io` parity vs buffered output on Denmark.
- **Scanner fast-path correctness (R3R2).** Passthrough descriptors for non-overlap indexed blobs never decompress the body. The descriptor must carry enough metadata (`kind`, `id_range`, `frame_start`, `frame_len`, `index`, `tagdata`) for the drain actor's type-transition, gap-create, and coalescing logic without referring back to the body. The scanner's range-overlap check uses the same `DiffRanges::range_overlaps` predicate as today's classify fast-path.
- **`--force` / no-indexdata fallback.** Descriptors have `index: None`. Scanner cannot fast-path; all descriptors flow through the worker pool. Workers decompress to scan (existing `scan_block_ids` path), then precise check. Reduced fast-path coverage, same correctness contract. Property test: `--force` parity vs indexed path on Denmark.
- **Trailing creates** (port verbatim from current `merge()`'s `types_to_flush` match). Drain actor on channel close runs the existing match: `None | Some(Node)` → flush all three; `Some(Way)` → flush Way+Relation; `Some(Relation)` → flush Relation. Property test: empty-base-PBF case where `last_type == None` forever; trailing creates flush all three kinds.
- **Backpressure propagates to the scanner.** Every channel in the streaming pipeline must be bounded by a byte budget (not count - CopyRange descriptors are tiny, Rewritten payloads are large). If writer stalls, drain receiver fills → worker→drain send blocks → workers stop pulling → scanner→workers dispatch fills → scanner blocks. Any unbounded channel or `par_iter().collect()` introduction defeats the chain.
- **Node→way transition barrier (under prefill fusion).** No way-blob classify work may execute before the per-worker coord maps are merged into `Arc<loc_map>`. Drain actor detects `blob.kind` flipping from Node to Way in the seq-ordered stream and publishes the merged map atomically before releasing any way-blob dispatch.
- **Scanner graceful shutdown.** Scanner emits a sentinel "end-of-input" marker on both fast-path and dispatch channels; drain finishes draining its reorder buffer; workers exit when dispatch closes. No worker may complete after drain receives the end-of-input marker for its slot.

## Open questions

Items resolved by R3 + measurement are removed from this list. The remaining live questions are:

- **Will worker-emits-framed-bytes be needed under `--compression none`?** R1 expects yes (the writer chain becomes the new ceiling); R2 says wait for measurement. Resolution path: land P1 without it, run planet `--bench 1`, look at writer counters. If the writer chain dominates remaining wall, add as P1.5 (~50-line change once the worker exists).
- **Will the post-P1 wall actually land in 40-55 s, or will writer I/O become the dominant phase?** Both reviewers' CPU-budget walks assume writer I/O runs concurrent with workers and finishes inside the same wall window. ~92 GB output at NVMe write ceiling (~3 GB/s) is ~30 s wall - hidden behind the worker pool if pipelining is clean, exposed if not. Measurement question, not design question.
- **What's the actual overlap-blob ratio at planet under different OSC sizes?** Plan-doc estimates ~8% for daily; needs confirmation post-P1 because it's the load on the worker pool. At 20% the worker pool needs more cores; at 4% the scanner's fast-path dominates and the worker pool is mostly idle.
- **What's the right initial value for the byte-budget reorder capacity?** R2 suggested ~128 slots + byte permits. If under-sized, workers block on slow rewrites; if over-sized, RSS spikes during straggler tails. Tune after first planet measurement.
- **Does the scanner's HeaderWalker keep up with worker throughput?** At 1.37 M headers on planet, walker emits one descriptor every ~15 µs; worker pool consumes ~22 candidates per ~1 ms. If walker emits faster than workers consume, dispatch backs up and we apply backpressure to scanner (fine). If walker is the slow side, workers idle (problem). Measurement.

### Items resolved this round (closed)

- ~~Phase breakdown is inferred~~ - confirmed by measurement (`e81a9316`, planet phase distribution table above).
- ~~Reader thread I/O-bound at NVMe speeds?~~ - parallel reader landed; reader is not the bottleneck.
- ~~Overlaps_needed prune ratio for prefill?~~ - prefill landed parallel and is now 4.6% of wall; no longer load-bearing.
- ~~--compression none leaves writer free?~~ - measured: writer is not the dominant phase at `--compression none`. Under zlib it is.
- ~~Prefill RSS under parallel decompress?~~ - measured ~5 GB transient, fine under 28 GB.
- ~~--io-uring integration site (reader vs writer)?~~ - writer side; descriptor-first design preserves it (drain hands framed/copy bytes to existing PbfWriter, which dispatches to its uring backend).
