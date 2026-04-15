# ALTW external optimization plan

This note is the working plan for `add-locations-to-ways --index-type external`
on current `main`, separate from `TODO.md`.

It is intentionally narrower than `TODO.md`:

- it only covers the external-join ALTW path
- it reflects the code that exists now, not stale hypotheses
- it says what to test next, in what order
- it assigns the smallest dataset that can answer each question
- it defines the keep/discard gate for each hypothesis

Use this as the source of truth for sequencing. `TODO.md` remains the backlog.

## Current architecture

1. `stage1.rs` — pass A parallel way scan builds a shared `IdSetDense` of
   referenced nodes; pass B rescans ways and emits rank-bucketed
   `(local_rank, slot_pos)` records; `build_node_blob_mapping()` records
   `[ref_rank_start, ref_rank_end)` per node blob via a header-only walk.
2. `stage2.rs` — per-rank-bucket counting-sort with inline coordinate
   resolution from covering node blobs; resolved `(slot_pos, lat, lon)`
   entries flushed into disk-backed slot buckets.
3. `stage3.rs` + `coord_payloads.rs` — scatter slot buckets directly from
   raw bytes into per-bucket `scatter_buf`, classify blob/bucket
   intersections, emit per-blob payloads to worker temps, stage straddlers,
   parallel-finalize into `coord_payloads`.
4. `stage4.rs` — way blobs use wire-format reframe with per-blob
   `coord_payloads`; relations are raw-passthrough; non-way node blobs go
   through full decode + `BlockBuilder`.

The disk-backed slot-bucket path, shared-set pass A, and current
`coord_payloads` representation all have alternatives that were measured
and shelved; see §Measured and shelved below.

## Current measured profile

Recent clean normal baselines:

| Dataset | UUID | Wall | Stage 1 | Stage 2 | Stage 3 | Finalize | Stage 4 |
|---|---:|---:|---:|---:|---:|---:|---:|
| Europe | `ffdf5f69` | 375.9 s | 71.0 s | 97.0 s | 37.2 s | 17.8 s | 121.1 s |
| Planet | `4f059b67` | 867.7 s | 148.5 s | 266.6 s | 100.2 s | 46.4 s | 231.6 s |

What those runs say:

- Europe is still stage-4-led.
- Planet is now stage-2-led, with stage 4 second.
- Stage 3 + finalize are no longer the obvious next wall bite; they are large,
  but not first.
- The pipeline is no longer in the "one giant structural mystery" state.
  Remaining work should mostly be current-architecture wins, not another major
  redesign.

## Dataset ladder and benchmark policy

The old "2× Europe + 2× planet between every coding sprint" cadence does not
scale. Use this ladder. Batch related work before paying for Europe again.

**Denmark** — correctness, invariant checks, output parity, RSS sanity.
Denmark wall is noise for almost every external-join item; do not accept or
reject on it unless the change is extremely local and CPU-only.

**Japan** — first performance gate for stage-1/2/3 CPU-local work and
bucket-count sweeps. Gate: `< 1%` total-wall and `< 3%` targeted-phase movement
are noise; only escalate to Europe when the target phase moves clearly in the
expected direction.

**Europe** — first real gate for anything syscall / I/O / cache / page-cache
sensitive, anything touching stage-4 consumer/writer balance, and any
shippable candidate. Gate: low/medium-effort items keep only if total wall
improves `>= 1.5%` or the targeted phase improves `>= 3%` with the rest of
the pipeline flat; architectural items need a clear Europe phase win or a
compelling planet-only reason.

**Planet** — ship/no-ship, memory floor, or cases where slot count
fundamentally changes the trade. Do not pay for a planet A/B unless
Denmark / Japan / Europe have cleared, or Europe is known to be
non-representative.

### Noise-floor calibration

Gate thresholds are policy values. The real floor is within-commit variance
on the current host. Before trusting them, run the baseline `--bench 3` on
Japan and Europe once; if natural spread is `> 1%` on Japan or `> 1.5%` on
Europe, raise the gates accordingly. Rerun after host changes or a major
multi-stage-file churn.

## Already shipped on current main

Do not re-plan these as hypotheticals — they already ship:

- `coords_by_rank` removal: stage 2 decodes node blobs directly via
  `NodeBlobInfo`.
- Stage-3 direct scatter from raw `ResolvedEntry` bytes — no
  `Vec<ResolvedEntry>` materialization between read and scatter.
- Parallel finalize tail in `coord_payloads.rs` — per-blob pread+pwrite
  work-stealing across `available_parallelism` threads.
- Stage-4 per-way refcount sidecar consumption in the way reframe path
  — no field-8 ref re-counting in the hot loop.
- Stage-4 raw passthrough for relation blobs (always) and node blobs
  when `keep_untagged_nodes` is set; consumer-owned, mirroring
  `extract.rs:pread_execute`.
- `PerWayRcs` lazy per-blob decode via blob-offset sidecar index — no
  flat planet-scale `Vec<u32>` residency through stage 3 / finalize.

## Measured and shelved

- Slot-space epochs / epoch spill:
  - Denmark correctness: passed
  - Europe `E=4`: won locally and on wall
  - Planet `E=4`: OOM
  - Planet `E=8`: fit, but lost to same-commit normal
  - conclusion: shelve the path; do not keep it on `main`

- Per-worker local `IdSetDense` in pass A:
  - Europe showed `s1a_idset_local_chunks=8932` vs
    `s1a_idset_final_chunks=406`
  - conclusion: shelve the current design; do not use it as a baseline

- Stage-1 vector-fusion experiments already tried and reverted:
  - pass-A direct-set fusion
  - pass-B ranked-vector fusion

- Stage-1B per-blob bucket staging (batch `write_all` per bucket per blob):
  - commits: `e16674b` (land), `950c22d` (revert), 2026-04-14
  - Europe stage 1 regressed +30% (77.0 s → 99.9 s); every CPU-bound
    counter (`s1b_scan_ms`, `s1b_rank_ms`, `s1b_encode_write_ms`)
    regressed together
  - `write_all` call count dropped 4.69 B → 14.16 M (−331×) as designed,
    but `BufWriter` was already amortizing the syscall cost; the staging
    layer added an extra memcpy and scattered writes across 256
    `Vec<u8>` tails, thrashing L1/TLB
  - lesson: reviewer consensus + cumulative-ms numbers are not evidence
    of a bottleneck; measurement is. `s1b_encode_write_ms` cumulative
    looks like an attack surface but is a `BufWriter`-amortized memcpy,
    not a syscall pile-up.
  - conclusion: shelve the direct batching approach. The
    grouped-by-local-rank redesign (item 5) is a different mechanism and
    survives.

## Proposed order

This is the order to work in unless a new measurement disproves it.

### 0. Refresh stage-2 instrumentation

Hypothesis:

- We need better stage-2 visibility before deciding whether the local
  follow-ups are worth another Europe gate.

Code surface:

- `src/commands/altw/stage2.rs`
- optionally `src/commands/altw/stage1.rs` / `stage4.rs` for explicit schedule
  scan markers

Change:

- convert the stage-2 node subcounters from per-blob milliseconds to nanosecond
  accumulators
- follow the `WayReframeCounters` shape in `stage4.rs` (AtomicU64 ns
  accumulators, `ns_to_ms` helper at emit time) — do not reinvent
- split `coord_slice` zeroing from the rest of `s2_coord_fill_ms`
- add an explicit marker/timer for `build_way_schedule()`
- add an explicit marker/timer for the stage-4 schedule pre-scan
- add `EXTJOIN_RELATION_SCAN_START` / `_END` markers around
  `collect_relation_member_node_ids()` in `mod.rs`. Currently the cost
  sits in the gap between `EXTJOIN_STAGE3_END` and `EXTJOIN_STAGE4_START`
  with no attribution; item 4 (stage-4 node filter) and the low-priority
  relation wire scanner both need this baseline.

Smallest meaningful dataset:

- Denmark for correctness
- Japan for "did the instrumentation itself distort the run?"

Keep gate:

- keep the instrumentation if Japan total wall changes by `< 1%`
- if the overhead is higher, simplify until it is cheap enough to leave on

Discard gate:

- none; this is enabling work, not a product path

Why first:

- The ns counters make the stage-2 hot-loop judgement (item 3) cheaper.
- The relation-scan marker is a precondition for item 4 and the
  low-priority relation scanner.
- The schedule-scan timers feed item 8.

### 1. Make bucket count dev-tunable, then sweep `>256`

Hypothesis:

- Smaller rank/slot buckets may improve cache fit in stage 2 and stage 3 enough
  to outweigh the extra straddlers and file-management overhead.

Code surface:

- `src/commands/external_radix.rs`
- `src/commands/altw/mod.rs`
- `src/commands/altw/stage1.rs`
- `src/commands/altw/stage2.rs`
- `src/commands/altw/stage3.rs`

Important caveat:

- This is not just a scatter-buffer question.
- `NUM_BUCKETS = 256` is a `const` in `external_radix.rs`. The dev knob
  becomes a runtime parameter threaded through `external_join` → stage 1 →
  stage 2 → stage 3. Do not replace the const with another const.
- Pass B keeps `workers × buckets` shard writers open. Today `6 × 256 = 1536`
  FDs; at `512` it is `3072`; at `768` it is `4608`. Default `ulimit -n` is
  `1024` on most Linux distros — the sweep raises it via `setrlimit` at
  startup or documents the `ulimit` prerequisite.
- The `slot_bucket_count` floor (`total_slots / max_blob_slots`) enforces
  the 2-piece straddler invariant. Sweeps only widen where the invariant
  still holds; see the invariant list in the appendix.

Implementation shape:

- do not hardcode a new constant first
- add a dev/runtime knob so `256`, `384`, `512`, and maybe `768` can be swept
  without a code fork
- emit the live rank-bucket count as a counter (`extjoin_rank_bucket_count`)
  in the same `emit_counter` block that already emits
  `extjoin_slot_bucket_count`. Without this the sweep points are
  indistinguishable in `brokkr results`.
- env-var-gated prototype idiom — see appendix

Smallest meaningful dataset:

- Japan sweep first
- Europe only for the best Japan candidate

Metrics:

- `EXTJOIN_STAGE1`
- `EXTJOIN_STAGE2`
- `EXTJOIN_STAGE3`
- `COORD_PAYLOADS_FINALIZE`
- `s2_prepare_scatter_ms`
- `s2_slot_flush_lock_wait_ms`
- `s3_scatter_ms`
- `s3_integrated_straddler_count`
- `s3_worker_tmp_bytes`
- FD count / operational friction

Keep gate:

- keep exactly one non-256 candidate only if Japan improves
  `EXTJOIN_STAGE2 + EXTJOIN_STAGE3 + COORD_PAYLOADS_FINALIZE` by `>= 3%`
- and `EXTJOIN_STAGE1` gives back less than half of that gain
- then run Europe on the winner and only bank it if Europe also clears the
  normal gate

Discard gate:

- if every `>256` sweep point is flat or worse on Japan
- or if the only wins require operationally ugly FD settings / worker caps

Why this is first:

- it is the cheapest current-architecture knob with plausible upside
- it answers a long-open question without another large design fork

### 2. Shrink slot-bucket records from 16 to 12 bytes

Hypothesis:

- Stage 2 writes `~199 GB` of slot-bucket records at planet
  (`s2_slot_bytes_written = 198967358576`). Each `ResolvedEntry` today
  is 16 bytes (`u64 slot_pos + i32 lat + i32 lon`). Within a slot bucket
  the range `[bucket_start, bucket_end)` fits in `u32`, so a
  bucket-local `u32 local_slot_pos` shrinks the record to 12 bytes
  (−25 % scratch; `~199 GB → ~150 GB` on planet). Stage 3 already
  computes `local_pos = slot_pos - bucket_start`, so the subtraction
  comes for free on the reader side too.

Prior art:

- `cfa916f external_join: shrink rank record from 16 to 12 bytes`
  applied exactly this technique to the rank record (stage 1 → stage 2)
  via `(u32 local_rank, u64 slot_pos)` for a 25 % I/O reduction. The
  commit is proof that the pattern works; the same idea applied to the
  slot record is the new surface.

Code surface:

- `ResolvedEntry` in `src/commands/altw/mod.rs` (write + read helpers,
  `slot_bucket()` routing)
- `src/commands/altw/stage2.rs` (write path, slot-buffer append)
- `src/commands/altw/stage3.rs` (`scatter_bucket_entries`, the
  raw-bytes-to-`scatter_buf` loop)

Implementation notes:

- Bucket width = `total_slots / slot_bucket_count`. At planet with 256
  buckets: `12.4 B / 256 ≈ 48 M` — fits in `u32` with room to spare.
  Widening buckets (item 1 sweep to 512 or 768) narrows the range;
  narrowing buckets (small-input floor) is bounded by `max_blob_slots`.
  In every case the bucket-local `slot_pos` fits in `u32`.
- Add a debug-assert at routing time:
  `debug_assert!(bucket_end - bucket_start <= u32::MAX as u64)`. The
  assertion is cheap and documents the invariant in code.
- Touches the same three stage files as item 1. Ship them in separate
  commits so either can be reverted independently — do not batch.

Smallest meaningful dataset:

- Denmark for correctness (MD5 parity vs current path)
- Japan for the first CPU signal
- Europe for scratch / page-cache impact

Metrics:

- `EXTJOIN_STAGE2`
- `EXTJOIN_STAGE3`
- `s2_slot_bytes_written` (expected −25 %)
- `s3_bytes_read` (expected −25 %)
- `s2_bucket_load_ms` (expected smaller; less data to load per bucket)
- `s2_max_worker_buf_bytes` (per-bucket slot buffers shrink)

Keep gate:

- Europe scratch write+read drops `≥ 20 %` AND stage 2 + stage 3 wall is
  flat or better
- do not keep if stage 2 + stage 3 CPU regresses by `> 1 %`

Discard gate:

- counting-sort or scatter regresses materially on Japan
- Denmark MD5 parity fails

Why this is second:

- Narrow blast radius, clear signal, prior art in-repo.
- Natural pairing with item 1: both touch stage-2/3 scratch layout.
  Running the record shrink at the baseline bucket count gives a clean
  delta; running it again at the bucket-sweep winner confirms the
  scratch numbers under the chosen layout.

Why not earlier:

- Item 1's knob lands before item 2 so that the bucket-width assertion
  holds across every sweep point we might keep. If item 1 is flat and
  the knob stays at 256, item 2 still runs.

### 3. Batch the stage-2 hot-loop follow-ups

This is one branch, not two separate Europe gates.

Hypothesis:

- The inline node-blob path is now the right architecture, but its inner loop
  still does more work than necessary:
  - `rank_if_set()` per referenced node
  - `Vec<NodeTuple>` materialization before the rank/write loop

Code surface:

- `src/commands/altw/stage2.rs`
- `src/commands/node_scanner.rs`

Sub-items to batch:

1. replace `rank_if_set()` with `get()` + monotonic `next_ref_rank`
2. add a callback-based node scanner that writes directly into `coord_slice`
   instead of building `Vec<NodeTuple>`

Implementation notes:

- the monotonic-rank idea works because node tuples within a blob are
  ID-sorted and `IdSetDense` rank is monotonic in ID. Seed `next_ref_rank`
  per blob from `NodeBlobInfo.ref_rank_start`, which stage 1 already
  computes — no new seed work needed.
- read the stage-2 inline node-blob sections of
  `notes/altw-optimization-history.md` before starting; it covers the
  reasoning trail for the current shape.

Smallest meaningful dataset:

- Japan

Metrics:

- `EXTJOIN_STAGE2`
- nanosecond-correct `s2_node_extract_*`
- nanosecond-correct `s2_node_rank_*`
- `s2_coord_fill_ms`
- `s2_max_worker_buf_bytes`
- `s2_node_blobs_read`
- `s2_node_straddler_blobs`

Keep gate:

- keep the batch if Japan improves `EXTJOIN_STAGE2` by `>= 3%`
- or if the new stage-2 subcounters improve by `>= 10%` and total stage 2
  still moves in the right direction
- then confirm on Europe once, as a batch

Discard gate:

- if the batched branch is flat on Japan
- or if it only moves micro-counters while `EXTJOIN_STAGE2` stays flat
- or if it increases node rereads / correctness complexity

Why this is third:

- stage 2 is the largest remaining phase on planet
- this is still current-architecture work, not another redesign
- Japan is big enough to answer it

### 4. Wire-format DenseNodes filter for stage-4 non-way blobs

Hypothesis:

- Stage 4's non-way blob path does full `PrimitiveBlock` decode +
  element iteration + `BlockBuilder::add_node` + `flush_local` per kept
  node. Planet `s4_nonway_assemble_ms = 868410` cumulative (~145 s /
  worker) is one of the largest wall-attributable signals in stage 4.
- A wire-format DenseNodes filter — analogous to
  `reframe_way_blob_with_locations` and `reframe_dense_with_new_ids` —
  could splice out untagged non-member dense-nodes while copying
  stringtable entries, lat/lon deltas, and keys/vals verbatim for kept
  elements, skipping the `PrimitiveBlock` materialization entirely.

Prior art:

- `reframe_way_blob_with_locations` in `src/commands/altw/stage4.rs`
  (commit `a705fde`) — the analogous way-side rewrite that ships today.
  The commit note explicitly defers the non-way case: "Non-way blobs
  (nodes, relations) stay on the full decode+re-encode path since nodes
  need per-element filtering (tag/member check)." That deferred case is
  this item.
- `reframe_dense_with_new_ids` in `src/commands/renumber_external.rs`
  (commit `dc13a7b`) — a DenseNodes wire-format rewriter that patches
  ID deltas and copies all other fields verbatim. Per-node cost 113 ns
  → 16 ns (7×). Demonstrates the technique on DenseNodes end-to-end in
  the repo.

Why this is harder than either precedent:

- Renumber rewrites every element; ALTW has to drop some. Dropping
  dense-nodes breaks the packed delta chains in `ids`, `lat`, `lon`,
  and `keys_vals` simultaneously — each kept element's first delta has
  to be rebased to the last kept predecessor.
- `keys_vals` is a flat stream keyed by per-element counts terminated
  by `0`; the filter has to walk it in lockstep with the id/lat/lon
  streams and emit only the runs belonging to kept elements.
- `DenseInfo` (if present) carries parallel arrays for version /
  timestamp / changeset / uid / user_sid that all need the same
  filtering.

Code surface:

- `src/commands/altw/stage4.rs` — the `assemble_block` non-way branch
  and a new `reframe_node_blob_filtered` function modelled on
  `reframe_way_blob_with_locations`
- `src/read/dense.rs` (DenseNodes wire layout reference)
- the `!keep_untagged_nodes` case is the primary target; the
  `keep_untagged_nodes` case is already raw-passthrough (item does not
  affect it)
- relation-member predicate via `relation_member_node_ids.any_in_range`
  — same check as today, just applied inside the wire-format walk

Implementation notes:

- Start behind an env var (`stage23_epoch.rs` idiom), default off. Gate
  removal waits on clean Denmark MD5 parity + Europe wall gate.
- The filter can fall back to the existing `assemble_block` path for
  blobs with unusual layouts (non-dense Nodes, unexpected fields). Keep
  the fallback lit during prototype.
- Instrumentation: per-blob `s4_nonway_filter_ms` ns counter; kept /
  dropped element counts; emitted bytes. Counter names mirror the way
  reframe set.

Smallest meaningful dataset:

- Denmark for correctness (MD5 parity vs current non-way path)
- Japan for per-node CPU signal
- Europe for wall
- Planet for ship decision

Metrics:

- `s4_nonway_assemble_ms` (expected material drop)
- `s4_nonway_filter_ms` (new, ns)
- `EXTJOIN_STAGE4`
- `s4_send_ms` (may drop — workers produce faster; may reveal writer
  ceiling underneath)
- `s4_bytes_written` (parity check: kept element bytes should match the
  decode+re-encode path within tolerance)
- total wall

Keep gate (first pass — CPU):

- Denmark MD5 parity intact
- Japan or Europe `s4_nonway_assemble_ms` drops by `≥ 10 %`

If the first-pass gate clears, evaluate the second:

Keep gate (second pass — wall):

- Europe `EXTJOIN_STAGE4` improves by `≥ 5 %`, or
- Europe total wall improves by `≥ 2 %` with no RSS regression

If the second gate is flat under default `zlib:6`, the CPU win may be
masked by the writer ceiling. Re-evaluate under `zstd:1` (item 6a)
before discarding — the stage-4 raw passthrough for relations showed
exactly this pattern at `4910fd9`: `s4_nonway_assemble_ms` cumulative
dropped 5 % while wall moved +0.4 %, because the freed worker time
refilled the writer queue. A real stage-4-local CPU win can be invisible
on wall under a writer-bound workload and still be worth shipping for
the workloads where the writer is not the ceiling.

Discard gate:

- Denmark MD5 parity fails (correctness; no perf argument overrides)
- both first-pass CPU and second-pass wall gates are flat AND `zstd:1`
  is also flat

Why this is fourth:

- Attacks one of the larger cumulative planet signals
  (`s4_nonway_assemble_ms`) with stage-4-local scope.
- Prior art at two levels (way reframe + renumber dense rewriter) makes
  the path credible.
- Harder than item 3, so sits after the stage-2 hot-loop batch.

Why not earlier:

- filter-during-rewrite is a strictly harder edit than either shipped
  precedent; the stage-2 hot-loop batch is a smaller-effort win on a
  larger phase and goes first.
- item 0's relation-scan marker gives this item a clean baseline to
  measure against.

### 5. Prototype stage-1B grouped-by-local-rank emission

Hypothesis:

- Changing the stage-1B output shape may remove a meaningful chunk of
  `s2_prepare_scatter_ms` and some stage-2 grouping work.

Code surface:

- `src/commands/altw/stage1.rs`
- `src/commands/altw/stage2.rs`

Why this is not earlier:

- the stage-1B desk-estimate track record is poor
- previous "obvious" pass-A / pass-B write-path changes regressed
- this needs a prototype discipline, not a straight replacement

Implementation shape:

- follow the `stage23_epoch.rs` pattern: a parallel stage file, gated by
  env var in `mod.rs`, deletable in one commit when shelved
- Denmark first for correctness and file-shape sanity
- then Japan for the first real performance answer
- read the stage-1B desk-estimate failure notes in
  `notes/altw-optimization-history.md` before starting — two previous
  reshapes regressed despite positive micro-counter movement

Smallest meaningful dataset:

- Denmark for correctness only
- Japan for performance

Metrics:

- `EXTJOIN_STAGE1`
- `EXTJOIN_STAGE2`
- `s1b_encode_write_ms`
- `s1b_rank_ms`
- `s2_prepare_scatter_ms`
- `s2_bucket_load_ms`
- temp-file bytes / scratch footprint

Keep gate:

- keep only if Japan cuts `s2_prepare_scatter_ms` materially
  (`>= 20%`) and `EXTJOIN_STAGE1 + EXTJOIN_STAGE2` is at least flat,
  preferably better
- only take it to Europe if Japan clears that bar

Discard gate:

- discard immediately if Japan repeats the earlier pattern:
  lower write-call count but slower stage 1 or slower total stage 1+2

Why this is fifth:

- it still attacks the biggest open planet phase
- but it is riskier than items 1–4
- the batching variant (per-blob per-bucket staging) was tried and
  regressed +30 % on Europe stage 1 — see the measured-and-shelved
  entry for that failure; this item is the surviving grouped-emission
  direction, not the batching direction

### 6. Compressed-output rail: writer/compression first

This is a conditional branch, not the default-path mainline.

Why it moves up:

- On the current planet baseline, `s4_send_ms = 1461401` cumulative is the
  single largest cumulative backpressure signal in the pipeline.
- Europe already proved that `zstd:1` can move ALTW wall materially by relieving
  writer/compression saturation.
- That is a stronger measured bottleneck than the current `io_uring` hypothesis.

Important scope rule:

- Only take this branch if compressed output matters for the workload being
  optimized.
- If the real workload is `compression:none`, skip this section entirely and
  stay on the default-path rail.

#### 6a. One planet `zstd:1` characterization run

Hypothesis:

- The Europe `zstd:1` result was not a Europe-only artifact; the same writer
  relief should matter on planet.

Smallest meaningful dataset:

- Planet

Metrics:

- total wall
- `EXTJOIN_STAGE4`
- `s4_send_ms`
- `s4_consumer_write_ms`
- `s4_flush_ms`
- output size delta

Keep gate:

- if `zstd:1` gives a clear planet win for the actual internal pipeline use
  case, record it as an operational recommendation and then consider 6b

Discard gate:

- if planet `zstd:1` is flat, stop the compression rail there

#### 6b. Stage-4 decode-worker balance under compressed output

Hypothesis:

- If compressed output is still writer-bound, giving one core back from stage-4
  decode may help compression more than it hurts decode.

Code surface:

- `src/commands/altw/stage4.rs`
- optionally `src/write/writer.rs` only for measurement
- `decode_threads` in `stage4.rs` is `available_parallelism() - 2`; the
  `PbfWriter` rayon pool is separate. Total CPU = decode_threads +
  writer_pool_size. Any balance change accounts for both.

Smallest meaningful dataset:

- Europe with the actual compressed-output mode that matters

Metrics:

- `EXTJOIN_STAGE4`
- `s4_send_ms`
- `s4_consumer_write_ms`
- `s4_flush_ms`
- total wall

Keep gate:

- keep only if Europe total wall improves by `>= 1%`

Discard gate:

- if flat on the first Europe A/B, stop

Why this stays conditional:

- it tunes a compression-bound path, not the universal default path

### 7. `io_uring` preads, staged and gated

Hypothesis:

- The current stage-2 and stage-4 pread paths still pay enough syscall /
  submission overhead that `SQPOLL` may help.

Code surface:

- stage 4 first: `src/commands/altw/stage4.rs`,
  `coord_payloads::CoordPayloadsReader`
- if positive, extend to stage 2 preads in `src/commands/altw/stage2.rs`
- the project already has a `linux-io-uring` feature for the writer
  (`src/write/uring_writer.rs`). Reuse the same crate + feature flag for
  the reader side; do not introduce a second io_uring dependency.

Why stage 4 first:

- mechanically narrower
- the pread call site is isolated
- easier to discard quickly if it is flat

Smallest meaningful dataset:

- Europe

Metrics for stage-4-first prototype:

- `EXTJOIN_STAGE4`
- `s4_coord_payload_pread_ms`
- `s4_pread_ms`
- `s4_send_ms`
- `s4_consumer_write_ms`
- total wall

Metrics if extended to stage 2:

- `EXTJOIN_STAGE2`
- `s2_node_pread_ms`
- total wall

Keep gate:

- keep only if Europe total wall improves by `>= 2%`
- or the targeted phase improves by `>= 5%` with no obvious new complexity or
  RSS regression

Discard gate:

- if stage-4-only is flat on Europe, stop there
- do not extend a flat stage-4 prototype into stage 2

Why this is seventh:

- it is system-specific and higher effort
- it needs Europe to answer it
- there is no reason to pay this cost before current-architecture CPU work

### 8. Shared header-scan / blob-metadata sidecar

Hypothesis:

- Current normal ALTW still pays for multiple full-file header-only scans that
  might be collapsed into one reusable metadata pass.

Code surface:

- `src/commands/altw/stage1.rs` (`build_way_schedule`, `build_node_blob_mapping`)
- `src/commands/altw/stage4.rs` (schedule pre-scan)

Smallest meaningful dataset:

- Europe

Why this is already likely material:

- planet already shows `s1_node_map_build_ms = 17436`
- and that is only one of the current whole-file header-oriented scans
- `build_way_schedule()` and the stage-4 schedule pre-scan are additional scans
  over the same input

So the right framing is not "maybe material." It is "likely material; confirm
explicitly before redesigning it."

Metrics:

- `build_way_schedule` time
- `s1_node_map_build_ms`
- stage-4 schedule pre-scan time
- total wall

Keep gate:

- only keep if the explicit timings confirm combined scan time is clearly
  material (`> 10s` on Europe or obviously larger on planet)
- and the prototype saves at least half of that on Europe without
  making the code materially uglier

Discard gate:

- if the explicit timings show the scans are small
- or if stage 4 still needs bespoke tagdata parsing that erases most of the win

Why this stays late despite being likely material:

- the win may be real, but it is less direct than the stage-2 / bucket items
- confirm it explicitly first, then decide whether it is worth a redesign

## Low-priority extras

These are real but should not get ahead of the main queue.

### Relation-member node wire scanner

Current code:

- `collect_relation_member_node_ids()` fully decompresses relation blobs and
  parses `PrimitiveBlock`s

This is only worth revisiting if explicit measurement shows it above the noise
floor. It is probably too small to matter before the eight items above, and
item 0's relation-scan marker gives it a baseline when revisited.

Smallest meaningful dataset:

- Europe

Keep gate:

- only if explicit timing shows `> 5s` wall

### Node-blob double-decode across stage 2 and stage 4

Stage 2 decodes the kept node-blob set to populate bucket-local `coord_slice`;
stage 4 decodes the same kept node blobs again on the non-way path. Real
cumulative planet work: `s2_node_decompress_ms = 192356`, plus stage 4
processing all `32835 / 32835` node blobs again. Fusing is architecturally
awkward — stage 2 is rank-bucket ordered while stage 4 is file-ordered and
consumer/writer-bound. Kept visible as a deferred structural item; not a
next-sprint candidate. Item 4 (wire-format DenseNodes filter) reduces stage
4's side of this cost without cross-stage fusion.

### Generic writer/compression throughput work

This is relevant to ALTW external mode, but it is not ALTW-local. If default
compressed-output wall matters more than the ALTW-local items above, the right
follow-up is a generic write-path plan, not more ALTW surgery.

Operational note:

- `zstd:1` is already a proven lever when interop allows it

## What not to revisit right now

Do not spend more time on these until a precondition changes:

- epoch spill / slot-space epochs
- per-worker local `IdSetDense`
- stage-1 pass-A direct-set fusion
- stage-1 pass-B ranked-vector fusion
- relation-only passthrough tweaks beyond the shipped path

## Recommended sprint shape

The next sensible batching is:

1. instrumentation refresh (the relation-scan marker is the only
   outstanding piece; the rest is already in tree)
2. bucket-count tunable + Japan sweep (item 1)
3. slot-bucket record shrink (item 2) — independent scratch win, runs on
   the bucket-sweep winner (or at 256 if item 1 is flat)
4. if that cluster is flat, move to the stage-2 hot-loop batch (item 3)
5. only then pay for Europe again on the default path
6. stage-4 wire-format DenseNodes filter (item 4) is the next
   stage-local opportunity; schedule it once the Europe baseline from
   step 5 exists
7. only branch into the compressed-output rail (item 6) if
   compressed-output wall actually matters for the target workload

After that:

- if any of items 1–4 win on Europe, planet becomes worth paying for
- if all four are flat, move to the grouped-by-local-rank prototype
  (item 5)
- item 7 (`io_uring`) and item 8 (header-scan sidecar) stay deferred
  behind the CPU-local work

That gives one Europe gate per cluster, not one Europe gate per commit.

## Appendix

### A. Bench, verify, lint commands

Bench (single run):

- `brokkr add-locations-to-ways --dataset japan` (or `europe` / `planet`)

Bench (store best-of-3):

- `brokkr add-locations-to-ways --dataset japan --bench` (default `--bench 3`)
- `--bench 1` for characterization runs where only the first sample matters
- requires clean git tree unless `--force`; `--force` runs but does not store

Stop early at a marker (bench-mode only):

- `brokkr ... --bench 1 --stop EXTJOIN_STAGE2_END`

Read results:

- `brokkr results` — most recent
- `brokkr results <UUID>` — specific run
- `brokkr results <UUID> --phases` — durations + peak RSS/anon + counters
  grouped by marker
- `brokkr results <UUID> --markers` — raw markers (`--durations` for pairs)
- `brokkr results <UUID> --timeline --summary` — disk I/O by phase
- `brokkr results --compare-timeline A B` — phase-aligned delta
- `brokkr results <A> --timeline --fields counter_name` — single counter

Cross-commit bench (avoid git-checkout corrupting `.brokkr/results.db`):

- `brokkr --commit <HASH> add-locations-to-ways --dataset europe --bench`
- brokkr spawns an internal worktree; the repo tree you are editing is
  untouched

Correctness:

- `brokkr verify add-locations-to-ways --dataset denmark` — cross-validate vs
  osmium
- Denmark output MD5 vs a known-good run — used by the epoch prototype for
  bit-identical parity
- `brokkr check` — clippy + tests, run after every code change
- `brokkr check -- --ignored` before a release — runs `roundtrip_denmark`

### B. Correctness invariants

Any ALTW external prototype preserves these or explicitly replaces them:

- **Sorted + indexed PBF precondition.** `external_join` requires
  `Sort.Type_then_ID` headers and indexdata. Enforced at entry; do not relax.
- **2-piece straddler invariant.** A blob's slot range spans at most two
  adjacent slot buckets. `slot_bucket_count` is chosen so every bucket width
  `>= max_blob_slots`. Bucket-count sweeps preserve this.
- **Zero-coord sentinel.** Stage 2's `coord_slice` uses `(lat==0, lon==0)` as
  the unresolved sentinel. The slice is fully zeroed at the start of each
  rank bucket. Any redesign that removes zeroing must replace the sentinel.
- **Per-way refcount ordering.** The stage-1 per-way refcount sidecar is
  written in PBF blob order and consumed in PBF blob order by stage 4
  reframe. Prototypes that reshape stage 1 output keep this ordering.
- **Straddler `None → Left|Right → Both` transitions.** Stage 3's straddler
  merge is an exhaustive state machine; duplicate halves or third halves
  error. Do not weaken to `Option<(Vec<u8>, Vec<u8>)>`.
- **`build_rank_index()` before any `rank_if_set` / `rank`.** `IdSetDense`
  requires the rank index built after all `set_atomic` calls. Stage 1 pass A
  calls it once; later stages read only.

### C. Codebase patterns to follow

- **Ns accumulators for per-item timing.** `AtomicU64` holding nanoseconds,
  `ns_to_ms` helper at emit time. Reference: `WayReframeCounters` in
  `stage4.rs`. Do not accumulate `as_millis()` per item — sub-ms work
  truncates.
- **Reorder-buffer for parallel producer → serialized consumer.**
  `crate::reorder_buffer::ReorderBuffer::with_capacity(N)`; push with
  `(seq, value)`, `pop_ready()` drains in order. Used by stage 1 pass A,
  stage 3, stage 4. Reuse — do not reinvent.
- **ScratchDir for all temp files.** `scratch.file_path(name)` or
  `scratch.bucket_path(kind, idx)`. Lifetime-tied cleanup on drop.
- **`#[hotpath::measure]` on functions > 1 ms wall** so they show in
  `--hotpath` profiles. New stage functions carry it.
- **Env-var-gated prototype.** `stage23_epoch.rs` was the reference shape:
  parallel file, gated in `mod.rs` by env var check, one-commit delete on
  shelving. Use for any item behind a dev flag (items 1, 4, 5, 6b).
- **Worker count convention.** `available_parallelism() - 2 max 1 min 4`,
  often `.min(6)`. The `-2` reserves cores for the consumer + writer threads.
  Any tuning that changes this justifies why.
- **Counter naming.** `s<stage><phase>_<thing>_ms` / `_bytes` / `_calls`.
  Stage-scoped prefix keeps grep/history readable.

### D. Where to look for prior reasoning

- `notes/altw-optimization-history.md` — complete history of what was tried,
  what won, what regressed, and why. Read the section matching your item
  before starting.
- `TODO.md` — current backlog outside ALTW external; reconciled above.
- `git log --oneline src/commands/altw/` — commit message text is
  deliberately explicit; use as a secondary index into the history file.
