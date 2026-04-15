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

The current external path is:

1. `stage1.rs`
   - Pass A: parallel way scan builds a shared `IdSetDense` of referenced nodes.
   - Pass B: rescans ways and emits rank-bucketed `(local_rank, slot_pos)` records.
   - `build_node_blob_mapping()` does a header-only node-blob walk and records
     `[ref_rank_start, ref_rank_end)` for each node blob.

2. `stage2.rs`
   - per-rank-bucket counting-sort of rank records
   - inline coordinate resolution by decoding the node blobs that intersect the
     bucket's rank range
   - resolved `(slot_pos, lat, lon)` entries flushed into disk-backed slot buckets

3. `stage3.rs` + `coord_payloads.rs`
   - read each slot bucket
   - scatter directly from raw `ResolvedEntry` bytes into a dense per-bucket
     `scatter_buf`
   - classify blob/bucket intersections
   - emit fully-contained payloads into per-worker temp files
   - stage straddlers
   - parallel finalize into `coord_payloads`

4. `stage4.rs`
   - pre-scan blob schedule
   - way blobs: wire-format reframe using per-blob `coord_payloads`
   - non-way blobs: decode + `BlockBuilder`
   - relation passthrough is already shipped

Important current decisions:

- Keep the inline stage-2 node-blob path. `coords_by_rank` is gone.
- Keep the disk-backed slot-bucket path. The epoch-spill prototype was measured
  and shelved.
- Keep the shared-set stage-1 pass A. The per-worker local `IdSetDense` branch
  was measured and shelved.
- Keep the current `coord_payloads` representation. The old `coord_slots`
  family is closed.

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

The old "2x Europe + 2x planet between every coding sprint" cadence does not
scale. Use this ladder instead.

### Denmark

Use Denmark for:

- correctness
- invariant checks
- output parity
- scratch/output byte-shape sanity
- verifying that a new path does not explode RSS immediately

Do not use Denmark to accept or reject ALTW performance work unless the change is
extremely local and CPU-only. For almost every external-join item, Denmark wall
is noise.

### Japan

Use Japan as the first performance gate for:

- stage-1 CPU-local work
- stage-2 CPU-local work
- stage-3 CPU-local work
- bucket-count sweeps

Japan is the smallest dataset that is usually big enough to exercise the real
hot loops without paying Europe time.

Default Japan gate:

- treat `< 1%` total-wall movement as noise
- treat `< 3%` targeted-phase movement as noise
- only escalate to Europe if the target phase moves clearly in the expected
  direction

### Europe

Use Europe as the first real gate for:

- anything syscall / I/O / cache / page-cache sensitive
- anything touching stage-4 consumer / writer balance
- any candidate you might actually ship

Default Europe gate:

- low/medium-effort item: keep only if total wall improves by `>= 1.5%`, or the
  targeted phase improves by `>= 3%` and the rest of the pipeline stays flat
- high-effort / architectural item: require either a clear phase win on Europe
  or a compelling reason that only planet can decide it

### Planet

Use planet only for:

- ship / no-ship decisions
- memory-floor decisions
- cases where slot count fundamentally changes the trade

Do not pay for a planet A/B unless a candidate has already cleared its Denmark /
Japan / Europe gate, or Europe is known to be non-representative for that
question.

### Noise-floor calibration

The gate thresholds below are policy values. The real floor is whatever the
current host's within-commit variance produces. Before trusting them:

- run the current baseline `--bench 3` on Japan, record the spread
- run the current baseline `--bench 3` on Europe, record the spread
- if natural spread is `> 1%` on Japan or `> 1.5%` on Europe, raise the gates
  accordingly

Rerun calibration when the host changes or after a major code churn across
multiple stage files.

### General rule

Batch related work before paying for Europe again.

Examples:

- do not Europe-bench two stage-2 hot-loop edits separately
- do not planet-bench an architecture branch until the clean normal Europe gate
  says it is worth carrying

## Reconciliation with TODO.md

`TODO.md` is now stale in several places for ALTW external. Before scheduling
new work, normalize the state mentally:

### Already shipped, even if TODO still says "open"

- `coords_by_rank` removal: stage 2 now decodes node blobs directly using
  `NodeBlobInfo`
- stage-3 direct scatter from raw `ResolvedEntry` bytes
- parallel finalize tail in `coord_payloads.rs`
- stage-4 per-way refcount sidecar consumption in the way reframe path

These should not be re-planned as hypotheticals.

### Measured and shelved

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

### Obsolete TODO wording

- The remaining "coord pass ranked_coords fusion" note is obsolete on current
  `main` because the old `coord_pass` path no longer exists in this pipeline.

### Still genuinely live

- bucket-count tuning (`>256`)
- stage-2 hot-loop cleanup under the inline node-blob design
- stage-1B grouped-by-local-rank redesign
- `io_uring` for preads
- compression-conditional stage-4 worker balance

## New opportunities from code inspection

These are not top-level TODO items today, but they are real enough to track:

1. **Stage-2 callback node scanner / direct coord-slice fill**
   - `stage2.rs` currently calls `extract_node_tuples()` into a `Vec<NodeTuple>`
     and then loops that vector to do `rank_if_set()` + `coord_slice` writes.
   - A callback-style scanner can parse DenseNodes and write directly into the
     target slice, removing the `Vec<NodeTuple>` materialization and the second
     loop.
   - This is a natural paired follow-up to the existing
     `rank_if_set()` -> monotonic-rank idea.

2. **Stage-2 instrumentation is currently too lossy for micro-opts**
   - `s2_node_pread_ms`, `s2_node_decompress_ms`, `s2_node_extract_ms`, and
     `s2_node_rank_ms` are accumulated via per-blob `as_millis()`.
   - That has the same truncation problem the stage-4 way counters used to
     have: sub-ms work disappears.
   - Before doing stage-2 hot-loop work, fix the instrumentation.

3. **Shared header-scan / blob-metadata sidecar**
   - Current normal path still performs multiple whole-file header-only scans:
     `build_way_schedule()`, `build_node_blob_mapping()`, and the stage-4
     schedule pre-scan.
   - This may be worth collapsing into one reusable metadata pass or sidecar,
     but only if explicit timing proves the scans are material.

4. **Relation-member node collection is still full relation decode**
   - `collect_relation_member_node_ids()` fully decompresses and parses relation
     blobs.
   - Relation blobs are few, so this is low priority, but if later profiling
     shows it above the noise floor, a wire-format member scanner is plausible.

5. **Known duplicated node-blob decode work**
   - Current normal external mode decodes the kept node-blob set twice:
     - stage 2 decodes node blobs to populate bucket-local `coord_slice`
     - stage 4 decodes the same kept node blobs again to emit nodes
   - On the current planet baseline this is real cumulative work:
     - stage 2 `s2_node_decompress_ms = 192356`
     - stage 4 processes all `32835 / 32835` node blobs again on the non-way
       path
   - This is architecturally awkward to fuse because stage 2 is rank-bucket
     ordered while stage 4 is file-ordered and consumer/writer-bound.
   - Keep it visible as a deferred structural item so it is not lost, but do
     not make it part of the next sprint.

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

Smallest meaningful dataset:

- Denmark for correctness
- Japan for "did the instrumentation itself distort the run?"

Keep gate:

- keep the instrumentation if Japan total wall changes by `< 1%`
- if the overhead is higher, simplify until it is cheap enough to leave on

Discard gate:

- none; this is enabling work, not a product path

Why first:

- It makes items 2 and 6 much cheaper to judge.

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

### 2. Batch the stage-2 hot-loop follow-ups

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

Why this is second:

- stage 2 is the largest remaining phase on planet
- this is still current-architecture work, not another redesign
- Japan is big enough to answer it

### 3. Prototype stage-1B grouped-by-local-rank emission

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

Why this is third:

- it still attacks the biggest open planet phase
- but it is riskier than items 1 and 2

### 4. Compressed-output rail: writer/compression first

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

#### 4a. One planet `zstd:1` characterization run

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
  case, record it as an operational recommendation and then consider 4b

Discard gate:

- if planet `zstd:1` is flat, stop the compression rail there

#### 4b. Stage-4 decode-worker balance under compressed output

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

### 5. `io_uring` preads, staged and gated

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

Why this is fifth:

- it is system-specific and higher effort
- it needs Europe to answer it
- there is no reason to pay this cost before current-architecture CPU work

### 6. Shared header-scan / blob-metadata sidecar

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
floor. It is probably too small to matter before the six items above.

Smallest meaningful dataset:

- Europe

Keep gate:

- only if explicit timing shows `> 5s` wall

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

1. instrumentation refresh
2. bucket-count tunable + Japan sweep
3. if that is flat, move immediately to the stage-2 hot-loop batch
4. only then pay for Europe again on the default path
5. only branch into the compressed-output rail if compressed-output wall
   actually matters for the target workload

After that:

- if one of those two branches wins on Europe, planet becomes worth paying for
- if both are flat, move to the grouped-by-local-rank prototype

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
  shelving. Use for any item behind a dev flag (items 1, 3, 4b, 5).
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
