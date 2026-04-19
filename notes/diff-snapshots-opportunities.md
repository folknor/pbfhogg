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

### Sidecar profile (`42aedca1`)

```
Phase             Duration   Peak RSS  Peak Anon  Disk Read  Avg Cores
DIFF_SCAN_START  2107.260s   42.5 MB    38.0 MB    227.2 GB       1.0
                 user=0.9  kern=0.0  peak_threads=1
                 minflt=328240  vol_cs=710199
```

Element counters:
- common:  11,587,352,039 (98.8%)
- created:    109,478,210 (0.9%)
- modified:    30,961,245 (0.3%)
- deleted:      9,139,976 (<0.1%)

### Key takeaways

- **Single-threaded.** `Avg Cores=1.0`, `peak_threads=1`. The whole
  2107 s of real work runs on one main thread reading old, reading new,
  comparing, and emitting.
- **CPU-bound, not I/O-bound.** `user=0.9` (interpretation: ~90 % of
  wall time in userspace CPU). Disk read is 227 GB over 2107 s =
  ~108 MB/s effective, well under the ~2 GB/s NVMe ceiling. The
  bottleneck is decompression and protobuf decode, not disk.
- **Memory is comfortable.** 42 MB peak RSS. Plenty of headroom for
  parallelism on a 30 GB host.
- **98.8 % of element work is "common"** - bytes that pass through
  unchanged. The v1 byte-equal fast path already skips decompress on
  a fraction of that, but only on overlapping blob pairs.

## Target: ~8 min (~4.4× speedup)

Four-plus cores on a single-socket box with comfortable RAM means
parallel decode across two readers is the largest available win.
Additional items compound on top of that.

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

Rerun before committing to any item below:

```
brokkr diff-snapshots --dataset planet --from base --to 20260411 --bench 1
brokkr sidecar <UUID> --human
brokkr sidecar <UUID> --durations
brokkr sidecar <UUID> --counters --human
```

## Ranked opportunities

### 1. Parallel two-reader decode (largest expected win)

**Problem.** Today `block_pair_merge_phase` drives two sequential
`BlobReader` iterators from one thread. Each loop iteration:
reads old blob (waits I/O + decompress) → reads new blob (waits I/O +
decompress) → compares → emits. No pipelining.

**Shape.** Split decode into two worker pipelines (old + new), each
producing a stream of decoded `BlockState`s. A merge consumer runs on
its own thread, pulling one decoded block from each side and running
the existing overlap/non-overlap/byte-equal logic.

Decompression parallelizes across cores. At planet, ~300 K blob pairs
need decoding; on 4-6 cores we should see close to linear scaling on
the decompress phase. Peak RSS cost is the in-flight block buffer (~2
MB per decoded block × pipeline depth × 2 sides). 128 MB at depth=32
each side is fine on 30 GB hosts.

**Risk.** Reorder and residual-block handling in the merge phase is
stateful. Workers must deliver blocks in source order. Standard
`ReorderBuffer` pattern (used in `write/pipeline.rs`, multi-extract
consumer) applies.

**Expected impact.** If CPU is ~60-80 % of the wall (disk accounts for
the rest), parallel decode on 4 cores could bring 2107 s to ~700 s
(3× speedup). On 6-8 cores, ~500-600 s. Gets us most of the way to
the 8-minute target.

### 2. v3 - skip decode for entirely single-sided blobs

**Problem.** When a blob's ID range falls entirely before or after the
other side's range (e.g., trailing new blobs in the newer snapshot),
`block_pair_merge_phase` today decodes the blob purely to iterate its
elements for the per-element output line. See `merge_join.rs:766` and
`:773` (fast path) plus the slow-path residuals at `:801-817`.

**Measurement gate.** Land the shadow counters (done), run
`diff-snapshots` at planet, compute:

```
v3_opportunity_fraction =
  (blobs_old_only + blobs_new_only) / (blobs_old_only + blobs_new_only
                                        + pairs_byte_equal
                                        + pairs_overlapping_decoded * 2)
```

Analytical prediction: OSM grew by ~10 M nodes in the 47-day window.
At ~8000 nodes/blob that's ~1250 trailing new-only blobs in a ~300 K
node-blob population ≈ 0.4 %. OldOnly is rarer (complete-blob
deletes are uncommon). **Expected: < 1 % of blobs, < 1 % of wall.**
If the counters confirm this, mark v3 a load-bearing pin and move on.

**If the fraction is larger than expected**, the fix shape is:
- Add a no-element variant (`BlobOldOnlyByIndex { min_id, max_id,
  count, kind }`, symmetric for new) that callers can opt into.
- For `diff` human-readable: emit a blob-level summary line instead
  of per-element lines. User-visible change - gate behind a flag or
  make it the default only under `--suppress-common`.
- For `derive_changes`: BlobNewOnly cannot use this (needs full
  element content for `<create>` XML). BlobOldOnly could, but only
  needs id+version per element, and a lightweight scanner that reads
  id+version without full decode is a separate primitive.

### 3. Overlap BlobEqual checks could short-circuit earlier

**Observation.** `blobs_byte_equal` (line 829) requires that
`index.min_id`, `index.max_id`, `index.count` AND full compressed-byte
equality all match. The equality check reads both blobs' compressed
data through the underlying reader, which for a 64 KB blob is already
a decompression-worthy amount of I/O. If the bytes differ by even one,
we've paid that I/O and then still go to decode.

**Opportunity.** Add a lightweight hash (XXH128 over compressed
bytes) stored in a sidecar or computed lazily, compare hashes first,
fall through to byte compare only on hash match. Saves the
byte-compare memcmp work on the (small) no-match cases but doesn't
skip the I/O. Probably not worth it unless counters show byte-compare
itself is meaningful CPU.

Skip until measurement justifies.

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
2. **Run a measured `diff-snapshots` at planet.** 35 minutes. Pass
   through the counters, durations, and hotpath views. Partitions the
   remaining items from "guess" to "data".
3. **Parallel two-reader decode.** Expected headline win. Design
   around `ReorderBuffer` and `thread::scope` - same shape as
   multi-extract and external-join stage 4.
4. **Re-measure.** Decide if the target is met or we need more.
5. **v3 or skip.** Based on counter data from step 2. Plan shape
   documented above - build if the fraction justifies, pin if not.
6. **I/O primitive upgrade.** Only if step 4 shows I/O has become the
   ceiling.

## Constraints

- `derive_changes` BlobNewOnly emits full `<create>` XML per element -
  cannot skip decode. Any v3 work only helps BlobOldOnly for this
  command. `diff` human-readable path has more flexibility.
- Both callers require indexed inputs for the block-pair fast path.
  The element-stream fallback (`diff_element_stream`,
  `derive_changes_element_stream`) is already owned-allocation-heavy
  and not the planet baseline. Parallel decode applies there too
  via the pipelined reader, but priorities are the indexed path.
