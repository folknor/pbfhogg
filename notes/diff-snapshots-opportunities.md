# `diff-snapshots` optimization plan

## Scope

`brokkr diff-snapshots` compares two independent PBF snapshots (no
byte-level overlap, full decode both sides). Drives both `pbfhogg diff`
(human-readable output) and `pbfhogg diff --format osc`
(OSC XML via `derive_changes`). The code path shared by both is
`block_pair_merge_phase` in `src/osc/merge_join.rs`.

This document supersedes the "diff v3: non-overlapping block skip" TODO
entry - v3 is one of several items here, not the whole plan.

## Baseline (2026-04-17)

Planet 47-day snapshot pair (`base` vs `snapshot.20260411`), commit
`7e9c2e9`, `plantasjen`:

| Command                               | UUID       | Wall      |
|---------------------------------------|------------|-----------|
| `diff-snapshots` (human-readable)     | `42aedca1` | 2150.9 s (35m50s) |
| `diff-snapshots --format osc`         | `53900d5f` | 2225.6 s (37m06s) |

## Measured phase breakdown (2026-04-19, `a9d430f2`)

First run with the shadow counters + phase markers + hotpath
annotations landed. Commit `052da8b`, `plantasjen`, `--bench 3`:

**Wall: 2134.3 s (35m34s).** Within noise of the `42aedca1` baseline.

| Phase                        | Duration  | % of wall | Avg Cores | Peak Anon | Disk Read |
|------------------------------|-----------|-----------|-----------|-----------|-----------|
| `DIFF_PHASE_NODE_START/END`  | 1572.6 s  | 73.7 %    | 1.0       | 41.4 MB   | 121.0 GB  |
| `DIFF_PHASE_WAY_START/END`   |  547.5 s  | 25.7 %    | 1.0       | 37.8 MB   |  57.8 GB  |
| `DIFF_PHASE_REL_START/END`   |   14.1 s  |  0.7 %    | 1.0       | 28.2 MB   |   1.7 GB  |

All three phases: `user=0.9 kern=0.0`, `peak_threads=1`,
`majflt=0`. Single core, userspace, nothing waiting on disk or
page-faulting. Aggregate disk read 180.5 GB over 2134 s = ~85 MB/s
effective, an order of magnitude below the NVMe ceiling.

### Shadow counters

```
pairs_byte_equal               = 0
elements_byte_equal            = 0
pairs_overlapping_decoded      = 3
elements_overlapping_decoded   = 546 473
blobs_old_only                 = 0
elements_old_only              = 0
blobs_new_only                 = 440
elements_new_only              = 109 463 424

diff_common   = 11 587 352 039  (98.8 %)
diff_created  =    109 478 210  (0.9 %)
diff_deleted  =      9 139 976  (<0.1 %)
diff_modified =     30 961 245  (0.3 %)
```

### What the measurement resolves

- **The entire wall is single-core user CPU across two reader streams.**
  Disk is not the ceiling, memory is not the ceiling, context switches
  are only vol_cs from channel waits. Decompress + protobuf decode +
  the sequential merge loop saturate one core.
- **v1 byte-equal fast path never fires.** `pairs_byte_equal = 0` across
  the full run. Two independent PBFs with different compression metadata
  mean no blob is literally byte-identical, even when its elements are.
  The optimization does not apply to the `diff-snapshots` workload - it
  was designed for the `diff` workload (apply-changes internally first,
  then diff against the same input).
- **v3 non-overlapping block skip is not a planet-relevant item.**
  `blobs_old_only = 0` across all three phases. `blobs_new_only = 440`
  (all concentrated in the trailing end of new-side node and way files).
  440 out of ~300 K blob pairs ≈ 0.15 % of the population - skipping
  these saves at most 0.15 % of the wall. Close the item.
- **Type-phase split matches priority:** the node phase is 74 % of the
  wall on its own. Any parallel-decode work should land there first;
  way phase second; rel phase is a rounding error.

## Target: ~8 min (aspirational, not a costed plan)

8 min is extrapolated from the `renumber` 58 min → 3m14s result (18×),
not derived from a known pipeline. The measured number below is the
actual ceiling analysis.

### Parallelism ceiling

Pure Amdahl on the node phase (the 74 % dominator):

| Effective decode-side cores | Node phase | Way phase | Rel | Total | vs baseline |
|-----------------------------|------------|-----------|-----|-------|-------------|
| 1 (today)                   | 1572 s     |  547 s    | 14 s | 2134 s | 1.0×        |
| 4                           |  393 s     |  137 s    | 14 s |  544 s | 3.9×        |
| 6                           |  262 s     |   91 s    | 14 s |  367 s | 5.8×        |
| 8                           |  197 s     |   68 s    | 14 s |  279 s | 7.6×        |

Assumes the merge consumer does not become the new bottleneck. In
practice it will: the consumer still runs the sequential element-merge
loop over decoded blocks. Consumer work per element is small (a borrowed
`Equal` compare for 98.8 % of elements) but non-zero. Realistic 6-8 core
ceiling is probably ~400-500 s (7-8 min), matching the aspirational target
only if merge-consumer work stays ≤ 20 % of decode cost.

**If the consumer becomes the ceiling**, the next move is parallelising
the merge itself across disjoint ID ranges per type-phase - decoded blocks
are already ID-sorted, so a range partitioner can hand each merge worker
its own shard. That is a separate, harder plan; do not commit to it until
the parallel-decode result forces the conversation.

## Measurement prerequisites

**Shadow counters (shipped 2026-04-19)** in
`src/osc/merge_join.rs::BlockPairMergeStats`, emitted from `diff` and
`derive_changes` after all three kind-phases complete. Counter names
all prefixed `mergejoin_shadow_`:

| Counter                          | Meaning |
|----------------------------------|---------|
| `pairs_byte_equal`               | v1 fast-path hits (no decode). |
| `elements_byte_equal`            | Sum of element counts for v1 hits. |
| `pairs_overlapping_decoded`      | Overlapping pairs that decoded both sides. |
| `elements_overlapping_decoded`   | Both-sides element count sum for overlapping decode. |
| `blobs_old_only` / `elements_old_only`   | BlobOldOnly emits (entirely single-sided) - v3 skip candidates. |
| `blobs_new_only` / `elements_new_only`   | BlobNewOnly emits - v3 skip candidates. |

**Phase markers:** `DIFF_PHASE_<NODE|WAY|REL>_START/END` and
`DERIVECHANGES_PHASE_<NODE|WAY|REL>_START/END`. Plus the existing
`DIFF_SCAN_START/END` and `DERIVECHANGES_SCAN_START/END` wrapping
the whole run. Lets `brokkr sidecar --durations` attribute the 2107 s
wall across the three type phases.

**Hotpath annotations:** `#[cfg_attr(feature = "hotpath",
hotpath::measure)]` on `diff_block_pair`, `diff_element_stream`,
`derive_changes_block_pair`, `derive_changes_element_stream`,
`collect_phase_block_pair`, `block_pair_merge_phase`. `brokkr <cmd>
--hotpath` attributes CPU time to these functions.

First measured run: `a9d430f2` (commit `052da8b`, 2026-04-19, --bench 3).
Rerun after any landed item:

```
brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 3
brokkr sidecar <UUID> --human
brokkr sidecar <UUID> --durations
brokkr sidecar <UUID> --counters --human
```

## Ranked opportunities

### 1. Parallel two-reader decode (largest expected win)

**Problem.** Today `block_pair_merge_phase` (`src/osc/merge_join.rs:773`)
drives two sequential `BlobReader` iterators from one thread. Each loop
iteration: reads old blob (no decompress) → reads new blob (no
decompress) → range/byte-equal classify → if overlapping, calls
`decode_pending` on both sides synchronously → `element_merge_pair` →
emits. Only `decode_pending` is expensive; everything else is cheap.

**Hot path cost.** Two `decode_pending` calls per overlapping blob
pair. `decode_pending` = zlib decompress (~60 % of its cost) + protobuf
parse + StringTable inline (~40 %). Measured: `pairs_overlapping_decoded`
counter times 2 dominates the single-core 2134 s wall. The non-overlap
fast path and the byte-equal gate are never the bottleneck:
`pairs_byte_equal = 0`, `blobs_old_only + blobs_new_only ≈ 0.15 %` at
planet scale for this workload.

**Chosen shape: pipelined prefetch with shared decode pool.**

```
┌──────────────────────┐     ┌──────────────────────┐
│  Old reader thread   │     │  New reader thread   │
│  → sync_channel<Pending>    │  → sync_channel<Pending>
└─────────┬────────────┘     └─────────┬────────────┘
          │                            │
          ▼                            ▼
    ┌─────────────────────────────────────────────────┐
    │  Consumer thread (existing merge control flow)  │
    │  - pull next pending from both sides            │
    │  - cheap classify: disjoint / byte-equal /      │
    │    overlapping                                  │
    │  - disjoint / byte-equal: fast path inline,     │
    │    decode the single emitted side on-thread     │
    │    (only fires on ~0.15 % of pairs at planet)   │
    │  - overlapping: push decode task onto rayon     │
    │    pool, keep a VecDeque<oneshot<BlockState>>   │
    │    of in-flight tasks (depth D, e.g. 16)        │
    │  - when the front of the deque resolves, run    │
    │    element_merge_pair on that pair              │
    └─────────────────────────────────────────────────┘
```

Key properties:
- **Preserves `PendingBlob` layer.** Reader threads emit undecoded
  blobs with their `BlobIndex`, so the classify step still gets
  range/count/kind without paying decode cost.
- **Preserves the v1 byte-equal gate.** Still fires for the `diff`
  workload (apply-changes-then-diff, where compressed bytes overlap).
  `diff-snapshots` counters show it doesn't fire there, but cost is
  zero if we're past the byte-compare - we don't lose the capability.
- **Residual handling is unchanged.** Consumer still holds
  `old_decoded` / `new_decoded` residual blocks in local scope; only
  the *initial* decode of each pair is off-thread.
- **Backpressure is natural.** Pending channel depth (8 per side) caps
  reader run-ahead; decode deque depth (D ≈ 16) caps decode run-ahead.
  Peak in-flight memory: `D × 2 × avg_decoded_block_size` ≈ 16 × 2 ×
  2 MB = 64 MB. Comfortable vs the 27 GB available.
- **Type-phase loop is untouched.** Each of the three phase calls
  (NODE, WAY, REL) runs an independent pipeline with its own scoped
  threads; shared decode pool persists across phases.

**What changes in code:**
1. New `src/osc/merge_join/parallel.rs` or in-module helpers that:
   - Spawn two reader threads via `std::thread::scope` around
     `block_pair_merge_phase`.
   - Replace the synchronous `decode_pending` calls inside the loop
     with a "submit + wait on front of deque" pattern backed by a
     rayon pool.
2. `decode_pending` itself is already pure (`blob → BlockState`) once
   you hand it scratch buffers - it needs thread-local scratch like
   `run_pipeline` uses (`thread_local! ST_SCRATCH/GR_SCRATCH`).
3. `BlockPairMergeState` loses `old_buf`/`new_buf`/`old_st`/`new_st`/
   `old_gr`/`new_gr` (they move into thread-local decode scratch); the
   stash/readers/stats fields stay.

**Decode pool sizing.** Follow `run_pipeline`'s pattern:
`available_parallelism().saturating_sub(3).max(1)` (subtract main
consumer + 2 reader threads). On an 8-core host: 5 decode workers.
Amdahl with the 74 % node phase dominator says ≥ 6 effective cores
hits the aspirational 8 min target; 5 decode workers + 1 consumer +
2 readers = 8 live threads is the right oversubscription.

**Where the merge-consumer ceiling shows up.** The consumer thread
still runs `element_merge_pair` serially per pair. At ~6-8 decode
workers it may start to starve: the deque front needs to process at
consumer speed, and if consumer < decode aggregate, the deque fills
up and decode workers stall on backpressure. First measurement
afterward looks at `vol_cs` on the consumer thread to see if that's
happening. If it is, item 5 (below) becomes real.

**Risk.** Reorder is automatic because we submit tasks to rayon in
pair order and use a `VecDeque<oneshot<BlockState>>` per side -
consumer waits on the *front* oneshot, not arbitrary completions.
No reorder buffer needed at the pair level.

Stateful residual logic in `merge_decoded_pair` does not interact
with the pipeline because it operates on already-decoded blocks
owned by the consumer. We continue to carry residual blocks across
loop iterations in `old_decoded`/`new_decoded` exactly as today.

**Expected impact.** Measured wall is 100 % user CPU on 1 core across
all three phases, so every effective decode-side core translates to
near-linear speedup on decompress + protobuf decode. Amdahl table
above: 4 cores → ~544 s (3.9×), 6 cores → ~367 s (5.8×), 8 cores →
~279 s (7.6×) assuming the merge consumer stays below the decode-side
cost. Realistic ceiling is probably 7-8 min on a 6-8 core host
before consumer-side sequential work caps the win.

The node phase (74 % of the wall) is the right first target; way
phase (26 %) uses the same primitive; rel phase (0.7 %) is not worth
touching.

### 2. ~~v3 - skip decode for entirely single-sided blobs~~ CLOSED 2026-04-19

Shadow counters on `a9d430f2`:
- `blobs_old_only = 0`
- `blobs_new_only = 440` (out of ~300 K blob pairs ≈ 0.15 %)

Less than the 0.4 % analytical estimate. Skipping these saves at most
0.15 % of the wall - not worth the new `BlobOldOnlyByIndex` /
`BlobNewOnlyByIndex` variants, the opt-in callsite changes, or the
user-visible behavior split on `diff` vs `derive_changes`. Kept here
as a dead item so future reviewers don't re-propose it.

### 3. ~~Overlap BlobEqual checks could short-circuit earlier~~ CLOSED 2026-04-19

Shadow counters on `a9d430f2`:
- `pairs_byte_equal = 0` across the full run.

Two independent PBFs produced by different toolchains never share
byte-identical blobs even when their element content is identical.
The v1 byte-equal fast path is a `diff`-on-apply-changes optimization
and has no signal for `diff-snapshots`. No work to sharpen here.

### 4. Element-merge allocation audit

**Observation.** `diff_block_pair` uses borrowed elements and is
documented as zero-String-alloc for the `Equal` path (98.8 % of
elements). But the profile says 328 K minor faults over 2107 s -
mostly page dirtying during decoded block construction. At 300 K
blob pairs, that's ~1 fault per blob - about what you'd expect for
`PrimitiveBlock::from_vec_with_scratch`. Probably not reducible.

**Check with `--hotpath` run.** If `decode_pending` dominates per-block
time, this is where the parallel-decode win comes from. If something
else dominates (StringTable UTF-8 validation, varint loops), different
axis to attack.

### 5. Direct-IO / io_uring read path

**Observation.** Reader uses buffered `FileReader` (256 KB `BufReader`
with `fadvise(SEQUENTIAL)`). Disk bandwidth is not the bottleneck at
108 MB/s observed throughput vs 2+ GB/s available. So switching I/O
primitives gives nothing on its own.

**Composition with item #1.** If parallel decode lifts throughput
enough that I/O becomes the ceiling, then O_DIRECT or io_uring matters.
Not before.

Closed for now; revisit only if post-parallel profile shows I/O wait
dominating.

## Ordering

1. ~~**Shadow counters + markers + hotpath annotations**~~ - landed
   2026-04-19.
2. ~~**Run a measured `diff-snapshots` at planet.**~~ - landed
   2026-04-19 as `a9d430f2`. Results partition the remaining items:
   parallel decode is the whole plan; v1 and v3 optimizations are
   closed out.
3. **Parallel two-reader decode.** The entire path to the target.
   Design around `ReorderBuffer` and `thread::scope` - same shape as
   multi-extract and external-join stage 4. Land node phase first
   (74 % of wall), way phase second.
4. **Re-measure on a 6-8 core host.** Decide if the consumer-side
   merge loop has become the new ceiling.
5. **Parallel merge across disjoint ID shards.** Only if step 4 shows
   consumer saturation. Decoded blocks are already ID-sorted; a range
   partitioner can fan them out to N merge workers per type phase.
6. **I/O primitive upgrade.** Only if step 4 or 5 shows I/O wait
   dominating (currently ~85 MB/s on a 2+ GB/s NVMe, nowhere close).

## Constraints

- `derive_changes` BlobNewOnly emits full `<create>` XML per element -
  cannot skip decode. Any v3 work only helps BlobOldOnly for this
  command. `diff` human-readable path has more flexibility.
- Both callers require indexed inputs for the block-pair fast path.
  The element-stream fallback (`diff_element_stream`,
  `derive_changes_element_stream`) is already owned-allocation-heavy
  and not the planet baseline. Parallel decode applies there too
  via the pipelined reader, but priorities are the indexed path.
