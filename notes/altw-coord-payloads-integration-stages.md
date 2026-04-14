# coord_payloads integration — staged implementation plan

Target: replace `coord_slots` (99 GB at planet, flat 8B-per-slot) with
`coord_payloads` (~55 GB at planet, per-way delta-varint, per-blob
offset-indexed) in the `external_join` ALTW pipeline.

Measured savings (see `notes/altw-optimization-history.md`): planet
982 s → ~900 s (−8%), scratch ~300 GB → ~256 GB.

Prototype in-tree at commits `a13a6a8` / `e9e1d77` / `7738642` defines
the format, the stage-4 reader (`CoordPayloadsReader`), and a
throwaway transform pass (`transform_coord_slots_to_payloads`). This
plan integrates the coord_payloads emission into stage 3 and retires
the prototype transform.

Each stage below is a self-contained task ending in one commit. Main
conversation reviews between stages. **Subagent must NOT build, test,
commit, bench, or run any shell commands — code changes only.**

---

## Stage 1 — Extract per-blob delta-encode helper

**Goal.** Factor the per-blob delta-varint encoder out of the
prototype's `transform_coord_slots_to_payloads` into a pure helper
usable by both the existing transform and the future integrated
stage 3. No behavior change.

**Files.** `src/commands/altw/coord_payloads.rs`.

**What to do.**

1. Introduce a public-to-module function with this exact contract:

   ```rust
   /// Delta-encode one blob's coord slice into `output`.
   ///
   /// `coord_bytes.len() == 8 * sum(per_way_rcs)`. Within `coord_bytes`
   /// each 8-byte slot is `[i32 LE lat][i32 LE lon]`. For each way
   /// (refcount `rc` from `per_way_rcs`), consume `rc` consecutive slots
   /// and emit `2*rc` zigzag-varints into `output`: `lat_delta_0`,
   /// `lon_delta_0`, `lat_delta_1`, `lon_delta_1`, ... where
   /// `delta_0` is absolute (delta from 0), deltas reset per way.
   /// Bytes already in `output` are preserved; encoder appends.
   pub(super) fn encode_blob_payload(
       coord_bytes: &[u8],
       per_way_rcs: &[u32],
       output: &mut Vec<u8>,
   ) -> std::result::Result<(), String>;
   ```

2. Refactor `transform_coord_slots_to_payloads` to call this helper
   instead of inlining the encode loop. The existing `stats.encode_ms`
   must still time the call (wrap with the same `Instant::now()`).

3. Add unit tests in the same file (`#[cfg(test)] mod tests`):

   - `encode_blob_payload_single_way_single_ref` — one way, one ref at
     (lat=12345, lon=67890); output is exactly 2 zigzag-varints.
   - `encode_blob_payload_single_way_multi_ref` — one way, three refs;
     first pair absolute, next two are deltas from prior ref.
   - `encode_blob_payload_multiple_ways` — three ways of 2/3/1 refs;
     deltas reset at way boundaries (second/third ways' first pairs
     are absolute again).
   - `encode_blob_payload_empty_blob` — zero ways; output unchanged.
   - `encode_blob_payload_zero_coords` — two refs at (0,0); deltas are
     zero-valued zigzag-varints.
   - `encode_blob_payload_negative_coords` — ref at (-1_000_000,
     -1_000_000) followed by (0,0); exercises zigzag negative values.
   - `encode_blob_payload_length_mismatch` — `coord_bytes.len()` doesn't
     match `sum(per_way_rcs) * 8`; must return `Err`.

   For each success case, decode the output with `protohoggr::Cursor`
   + `zigzag_decode_64`, accumulate deltas, and assert the reconstructed
   (lat, lon) sequence equals the input.

**Out of scope.** No changes to stage 3, stage 4, or any other file.
No new CLI flag. No env var. No file format change.

**Exit criteria (main conversation verifies).**

- `brokkr check` passes (clippy + tests).
- Unit tests above all pass.
- `git diff` shows edits only to `src/commands/altw/coord_payloads.rs`.

**Commit message.** `external_join: extract per-blob delta-encode
helper for coord_payloads`

---

## Stage 2 — Blob↔bucket classification helper

**Goal.** Add a pure function that, given a slot bucket's slot range
and the full `way_slot_starts` array, classifies way blobs by how
they intersect the bucket. No behavior change.

**Files.** `src/commands/altw/coord_payloads.rs` (new helper) or a
new file `src/commands/altw/blob_bucket_index.rs` — pick whichever
keeps the module tree cleaner. If new file, add `mod blob_bucket_index;`
in `src/commands/altw/mod.rs`.

### Structural assumption (asserted at runtime)

**Every way blob is smaller than every slot bucket.** This is a hard
structural property of the pipeline at all measured scales:

| Dataset | Bucket size | Blob size avg | Blob size max (PBF spec) |
|---|---|---|---|
| Denmark | ~2 MB | ~240 KB | ≤ 16 MB (PBF blob-data limit) |
| Europe | ~147 MB | ~820 KB | ≤ 16 MB |
| Planet | ~388 MB | ~5.7 MB | ≤ 16 MB |

Since PBF caps blob-data at 16 MiB raw, and the smallest bucket we
ever produce (Denmark) is ~2 MB, a blob > bucket size is only
possible at datasets so small that `total_slots < 256 × blob_size`
(roughly sub-Denmark). Add a startup assertion in the integrated
path: `assert!(total_slots / NUM_BUCKETS >= max_expected_blob_slots)`
with a tunable safety margin. This lets Stage 3 assume **a blob
spans at most two buckets** and eliminates the "FullyContaining"
case from the design.

### What to do

1. Introduce the helper with this exact contract:

   ```rust
   /// Classification of how a way blob intersects a slot bucket.
   ///
   /// A blob's slot range is assumed to be smaller than a bucket's
   /// slot range (enforced by the pipeline's startup assertion). So a
   /// blob either fits fully within one bucket or straddles exactly
   /// two adjacent buckets (contributing a left half to the earlier
   /// bucket and a right half to the later bucket).
   pub(super) enum BlobBucketIntersection {
       /// Blob's slot range is entirely within the bucket.
       FullyContained { blob_idx: usize },
       /// Blob extends before the bucket start; this bucket contains
       /// the right-hand slot range `[bucket_start_slot, blob_end_slot)`.
       RightHalf { blob_idx: usize },
       /// Blob extends past the bucket end; this bucket contains the
       /// left-hand slot range `[blob_start_slot, bucket_end_slot)`.
       LeftHalf { blob_idx: usize },
   }

   /// Classify all way blobs intersecting slot range
   /// [bucket_start_slot, bucket_end_slot). Returns intersections in
   /// blob-index order.
   ///
   /// `way_slot_starts[i]` is the starting slot of blob `i`; blob `i`
   /// spans [way_slot_starts[i], way_slot_starts[i+1]) for i < N-1
   /// and [way_slot_starts[N-1], total_slots) for the last.
   ///
   /// Empty blobs (slot range of length 0) are omitted.
   ///
   /// Returns `Err` if any intersecting blob's slot range is wider
   /// than the bucket's (structural assumption violated — indicates
   /// a bug upstream).
   pub(super) fn classify_blobs_in_bucket(
       bucket_start_slot: u64,
       bucket_end_slot: u64,
       way_slot_starts: &[u64],
       total_slots: u64,
   ) -> std::result::Result<Vec<BlobBucketIntersection>, String>;
   ```

   Naming note: `RightHalf` = the blob's right-hand slot range is in
   this bucket (and its left half was/will be in the previous
   bucket). Symmetric for `LeftHalf`. This is clearer than "straddle"
   because each bucket sees at most one half per straddling blob.

2. Implementation approach: binary-search `way_slot_starts` for the
   first blob with `start < bucket_end_slot`, then walk forward until
   blob-start ≥ bucket_end_slot. For each visited blob, classify by
   comparing its `[blob_start, blob_end)` range to the bucket range.
   Return `Err` if `blob_end - blob_start > bucket_end_slot - bucket_start_slot`.

3. Unit tests:

   - `classify_empty_inputs` — empty way_slot_starts → empty result.
   - `classify_single_blob_fully_contained`
   - `classify_single_blob_left_half` — blob ends past bucket end.
   - `classify_single_blob_right_half` — blob starts before bucket start.
   - `classify_multiple_blobs_in_bucket` — 5 blobs, first is RightHalf
     (came from prior bucket), middle 3 FullyContained, last is LeftHalf.
   - `classify_boundary_exact_match` — blob end exactly equals
     bucket end (FullyContained, not half).
   - `classify_empty_blob_omitted` — zero-ref blob skipped.
   - `classify_last_blob_uses_total_slots` — last blob's end comes
     from `total_slots` parameter, not `way_slot_starts[N]`.
   - `classify_blob_wider_than_bucket_errors` — assumption violation
     returns `Err`.

**Out of scope.** No wiring into stage 3 yet. No format changes.
No CLI / env var.

**Exit criteria.** `brokkr check` passes; tests green; diff is small
and localized.

**Commit message.** `external_join: add blob↔slot-bucket
classification helper`

---

## Stage 3 — Integrated stage 3 (dual-output pipeline behind env var)

**Goal.** When `PBFHOGG_COORD_PAYLOADS_INTEGRATED=1`, stage 3 runs a
**second output pipeline in parallel with the existing coord_slots
pipeline**: the bucket scatter feeds both the existing
`pwrite(coord_slots)` path AND a new per-bucket emission into
per-worker temp files (plus straddler staging), followed by a
sequential finalization pass that produces `coord_payloads`. Stage 4
reads `coord_payloads` via the existing `CoordPayloadsReader`. When
the env var is unset: current behavior unchanged.

This is not a small dual-emit; it is a whole second output pipeline
including manifest coordination and sequential assembly. The
correctness-critical part is the contract below, not the env-var
wiring.

### Invariants (must hold after each stage-3 run)

1. **Exactly one payload per way blob.** Every `blob_idx` in
   `[0, num_way_blobs)` appears in the final `coord_payloads` file
   exactly once — either via a worker manifest entry (fully-contained
   case) or via the straddler finalizer (half-split case). Never
   both, never zero, never twice.
2. **Blob-index order preserved.** The coord_payloads offset table's
   entry `i` points to the payload bytes for `blob_idx == i`.
3. **Manifest uniqueness.** Across all worker manifest vectors, each
   `blob_idx` appears at most once. (Straddler blobs appear zero
   times in worker manifests.)
4. **Straddler piece completeness.** Every blob classified as
   `LeftHalf` in some bucket is also classified as `RightHalf` in
   the adjacent bucket (and vice versa). Before finalization, each
   straddler's two halves must both be present. Missing-half is a
   runtime error, not a silent zero-fill.
5. **Delta encoding applied exactly once per blob.** Fully-contained
   blobs are delta-encoded by the worker; straddler blobs are
   delta-encoded by the finalizer. Workers NEVER delta-encode a
   straddler piece.
6. **Encode-context identity.** For a straddler blob, the finalizer
   concatenates the left and right raw slot bytes (in slot order)
   and then `encode_blob_payload`s the full `coord_bytes` with the
   blob's full `per_way_rcs` — byte-identical to what the prototype
   transform would produce from the same `coord_slots`.

A subagent-facing assertion: for the integrated path, every
`coord_payloads` byte must match the bytes the prototype transform
would have produced from the same `coord_slots`. Main conversation
verifies by running both paths on Denmark and SHA256-comparing the
two `coord_payloads` files directly, not just the final PBFs.

### Memory bound for straddler staging

Straddler memory is bounded by the **size of straddling blobs**, not
the straddler count. Planning numbers:

| Dataset | Num blobs | Straddler count (max) | Avg blob size | Max staging |
|---|---|---|---|---|
| Denmark | ~825 | ≤ 255 | 240 KB | 61 MB |
| Europe | 56,692 | ≤ 255 | 820 KB | 210 MB |
| Planet | 17,529 | ≤ 255 | 5.7 MB | 1.5 GB |

Upper bound: `straddler_count × max_blob_coord_bytes ≤ 255 × 16 MB =
4 GB` (hard PBF-spec ceiling). In practice the dataset avg dominates
and is well under 2 GB. This is well within the 27 GB RAM budget.

**Design requirement:** straddler staging must free each blob's
pieces as soon as the finalizer has encoded and written the blob.
Do not accumulate all straddler bytes until after all straddlers
are finalized — walk blobs in order, encode, write, drop.

### Straddler staging data model

Raw bucket-local slot bytes (not pre-decoded, not delta-encoded).
Rationale: simpler to reason about, defers all encoding to a single
location (the finalizer), preserves invariant #5 above.

```rust
/// One straddler's two-piece state. Only one blob_idx in
/// `num_way_blobs` will have `Some(StraddlerSlot)` — the rest are
/// `None` (allocated as `Vec<Mutex<Option<StraddlerSlot>>>` of size
/// num_way_blobs, initialized to None).
struct StraddlerSlot {
    /// Raw coord bytes (8 per slot) for the left half, if received.
    /// slot range [blob_start, bucket_boundary).
    left: Option<Vec<u8>>,
    /// Raw coord bytes (8 per slot) for the right half, if received.
    /// slot range [bucket_boundary, blob_end).
    right: Option<Vec<u8>>,
}
```

Locking: `Vec<Mutex<Option<StraddlerSlot>>>` of length `num_way_blobs`.
A worker that encounters a `LeftHalf` or `RightHalf` for blob B
acquires `mutex[B]`, initializes the `Option` to `Some` on first
touch, and writes the appropriate half. Contention is minimal (only
2 writers per straddler blob, during non-overlapping bucket
processing).

Allocation alternative to consider at implementation time:
`HashMap<usize, Mutex<StraddlerSlot>>` populated lazily. Saves
memory for non-straddler entries, adds one hash lookup per
straddler encounter. Probably not worth the complexity given the
small absolute number of slots — prefer the simple Vec.

### Worker temp files & manifests

Each of the `N` stage 3 workers (typically 6) maintains:
- A `BufWriter<File>` on `scratch_dir/payloads-W{worker_id}`.
- An in-memory `Vec<ManifestEntry>` where
  `ManifestEntry { blob_idx: u32, byte_offset: u64, byte_length: u64 }`.
  `byte_offset` is into the worker's temp file.

At end of stage 3 (before finalization): flush BufWriters, hand
manifests to the finalizer.

**Manifest invariant (worker-side):** `manifest[k].byte_offset ==
sum(manifest[j].byte_length for j < k)` — entries append sequentially.

### Finalization

After the stage-3 bucket barrier, sequentially:

1. For each blob_idx in `0..num_way_blobs`:
   a. If this blob has a straddler entry: concatenate
      `left.unwrap() + right.unwrap()` (error if either is None);
      call `encode_blob_payload(coord_bytes, per_way_rcs[blob_idx],
      &mut encode_scratch)`. Drop the straddler pieces.
   b. Else: look up the blob in worker manifests (via a pre-built
      `Vec<(worker_id, manifest_idx)>` index keyed by blob_idx), pread
      `(byte_offset, byte_length)` from that worker's temp file into
      `encode_scratch`.
   c. Write `encode_scratch` to the output `BufWriter`, recording the
      cumulative byte position for the offset table.
2. Header + offset table written via the same strategy as the
   prototype: seek past reserved header bytes on the output file at
   the start, sequential-write the payload section, then `pwrite` the
   header + offsets at offset 0 at the end. (Sequential write model;
   header backfill via `pwrite`. No two-pass scan of payloads.)

```rust
pub(super) fn finalize_coord_payloads(
    output_path: &Path,
    num_way_blobs: usize,
    per_way_rcs: &[Vec<u32>],        // indexed by blob_idx
    worker_manifests: Vec<Vec<ManifestEntry>>,
    worker_tmp_paths: &[PathBuf],
    straddler_slots: Vec<Option<StraddlerSlot>>,  // indexed by blob_idx
) -> Result<FinalizeStats>;
```

`FinalizeStats` new struct (not prototype's `TransformStats`):
`{ output_bytes, num_way_blobs, num_straddlers, finalize_ms, read_ms,
encode_ms, write_ms }`.

### mod.rs wiring

1. Env var logic:
   - Neither `PBFHOGG_COORD_PAYLOADS_PROTOTYPE` nor
     `PBFHOGG_COORD_PAYLOADS_INTEGRATED` set → current baseline path.
   - `INTEGRATED=1`: dual-output stage 3, stage 4 reads integrated
     coord_payloads. Skip the prototype transform.
   - `PROTOTYPE=1`: unchanged prototype path.
   - Both set: startup error.
2. Startup assertion: enforce the "no blob wider than a bucket"
   assumption (integrated path only). Compute max blob slot span
   from `way_slot_starts` and compare to `total_slots / NUM_BUCKETS`.

### Counters

- `s3_integrated_encode_ms` (cumulative encoder wall across workers,
  fully-contained blobs only)
- `s3_integrated_straddler_copy_ms` (cumulative raw-byte copy into
  straddler slots)
- `s3_integrated_straddler_count` (number of straddler blobs observed)
- `s3_integrated_worker_tmp_bytes` (total bytes written to worker
  temp files)
- `s3_integrated_finalize_encode_ms` (straddler delta-encode during
  finalization)
- `s3_integrated_finalize_read_ms` (pread from worker temp files
  during finalization)
- `s3_integrated_finalize_write_ms` (sequential write of
  coord_payloads)
- `s3_integrated_output_bytes` (final coord_payloads file size)

### Correctness strategy (main conversation verifies)

Two independent checks:

1. **Bit-equality of `coord_payloads` files** (integrated vs
   prototype transform) on Denmark. This is the strongest semantic
   check — if these are byte-identical, the integrated path produces
   the exact payload the prototype defines.

2. **Bit-equality of output PBF** (integrated vs baseline) on
   Denmark, then Europe. Confirms the consumer side works correctly.

### Out of scope for Stage 3

- Deleting the prototype transform or `CoordSlots`.
- CLI flag (env vars only).
- Any changes to stage 4's code paths (the integrated path reuses
  `CoordPayloadsReader` and the existing stage-4 payload consumer
  from commit `7738642`).
- Flipping defaults.

### Exit criteria

`brokkr check` passes. Four code paths:
- Neither env var: byte-identical to pre-integration baseline.
- `PROTOTYPE=1`: byte-identical to prototype behavior.
- `INTEGRATED=1`: new path; produces coord_payloads used by stage 4.
- Both env vars: startup error.

### Commit message

`external_join: integrated stage 3 coord_payloads pipeline behind
PBFHOGG_COORD_PAYLOADS_INTEGRATED`

---

## Stage 4 — Main conversation: validate integrated path

Not a subagent task. Main conversation:

### Denmark (fast, correctness-first)

1. **Baseline output PBF**:
   `brokkr add-locations-to-ways --dataset denmark --index-type external`
   → save as `baseline.osm.pbf`.
2. **Prototype output PBF + coord_payloads**:
   `PBFHOGG_COORD_PAYLOADS_PROTOTYPE=1 brokkr ... --keep-scratch`
   → save output PBF as `prototype.osm.pbf`; note the scratch dir.
3. **Integrated output PBF + coord_payloads**:
   `PBFHOGG_COORD_PAYLOADS_INTEGRATED=1 brokkr ... --keep-scratch`
   → save output PBF as `integrated.osm.pbf`; note the scratch dir.

4. **Assertions** (these are primary — Stage 5 is blocked on all 4):
   - `sha256sum baseline.osm.pbf integrated.osm.pbf` match.
   - `sha256sum prototype/coord_payloads integrated/coord_payloads`
     **match** (the prototype-equality check called out in Stage 3).
   - `brokkr verify add-locations-to-ways --dataset denmark` passes
     with `PBFHOGG_COORD_PAYLOADS_INTEGRATED=1` in env (cross-validation
     against osmium).
   - `s3_integrated_straddler_count > 0` (sanity: Denmark must
     produce at least some straddlers to exercise the path).

### Europe (performance + correctness)

5. `brokkr add-locations-to-ways --dataset europe --index-type external
   --bench 1` (baseline, if not already on record).
6. `PBFHOGG_COORD_PAYLOADS_INTEGRATED=1 brokkr ... --bench 1 --force`
   (integrated Europe; `--force` because env var doesn't show in git
   state and results won't store without an associated commit).
7. Compare stage timings and confirm:
   - Stage 3 wall rises modestly (new encode work + temp file writes).
     Expected delta: ≤ 10 s cumulative (encode + straddler copy +
     finalize).
   - Stage 4 wall matches prototype (~130 s), not baseline (141 s).
   - Total ≈ 373 s. Transform tax (65 s) is absent.
8. If feasible (Europe output PBF is ~60 GB — disk budget
   permitting): save integrated + prototype Europe outputs, SHA256-
   compare them as a second cross-check on the larger dataset.

### Planet (production target)

9. `brokkr add-locations-to-ways --dataset planet --index-type external
   --bench 1 --force` with `PBFHOGG_COORD_PAYLOADS_INTEGRATED=1`.
10. Target: ≤ 910 s (baseline 982 s − ~80 s). Confirm output via
    `brokkr verify`.

### Pass / fail

- All Denmark assertions pass → Stage 5 is unblocked.
- Europe / planet numbers are directional (report and compare) but
  not gating for Stage 5.
- If any Denmark assertion fails, return to subagent with a concrete
  bug report (diverging byte range + the `blob_idx` it belongs to).

---

## Stage 5 — Flip default + retire prototype transform

**Goal.** Default the external path to the integrated
coord_payloads pipeline. Keep `PBFHOGG_COORD_SLOTS=1` as a pure
**pre-integration regression path**: old stage 3 producing
coord_slots, old stage 4 reading coord_slots via mmap, no
coord_payloads produced or consumed. Delete the prototype transform
pass entirely (it was throwaway throughout).

**Files.**
- `src/commands/altw/mod.rs` — invert the env var logic.
- `src/commands/altw/coord_payloads.rs` — delete
  `transform_coord_slots_to_payloads` and any helper used only by it.
  Keep: `encode_blob_payload`, `classify_blobs_in_bucket`,
  `finalize_coord_payloads`, `CoordPayloadsReader`,
  `load_per_way_refcount_sidecar`.
- `src/commands/altw/stage3.rs` — remove the env-var branch; always
  run the integrated pipeline AND write coord_slots (the latter is
  needed only when the escape hatch is active, but writing it
  unconditionally is simpler than branching; see below).
- `src/commands/altw/stage4.rs` — when `PBFHOGG_COORD_SLOTS=1`, use
  the original mmap `CoordSlots::get` path. Otherwise, use
  `CoordPayloadsReader`.

### Escape hatch semantics (`PBFHOGG_COORD_SLOTS=1`)

This is the **pre-integration path**, not a hybrid:

- Stage 3: writes `coord_slots` (unchanged). Does NOT run the
  integrated payload pipeline (no worker temp files, no straddler
  staging, no finalizer).
- Stage 4: opens `coord_slots` via `CoordSlots::open`, reads via
  `coord_slots.get(slot_pos)` in the way-reframe inner loop
  (pre-prototype, pre-integration behavior).
- No coord_payloads file is produced or consumed in this path.

So the branch in `mod.rs` dispatches:
- `PBFHOGG_COORD_SLOTS=1` → old path, both coord_slots-producing
  stage 3 and coord_slots-reading stage 4.
- default → new path, integrated stage 3 + stage 4 reads
  coord_payloads.

Note: Stage 5 removes both env vars from the prototype era. The
prototype transform pass is gone. `PBFHOGG_COORD_PAYLOADS_PROTOTYPE`
and `PBFHOGG_COORD_PAYLOADS_INTEGRATED` are no longer honored. Only
`PBFHOGG_COORD_SLOTS=1` (the regression escape hatch) remains.

### Simpler branching option (recommended)

An alternative is to branch stage 3 entirely: integrated pipeline
for default, old coord_slots-only emission for escape hatch. That
avoids writing coord_slots unnecessarily when the escape hatch is
off. Downside: two copies of stage 3 driver code during the
escape-hatch lifetime. The subagent should choose whichever is
cleaner; main conversation can review the tradeoff at code review.

### Correctness strategy

Same SHA256 + `brokkr verify` checks as Stage 4. Explicitly:
- Output PBF in default mode bit-identical to Stage-4-validated
  integrated output.
- Output PBF with `PBFHOGG_COORD_SLOTS=1` bit-identical to the
  pre-prototype baseline.

### Out of scope

Dropping coord_slots production entirely. Removing `CoordSlots`
type. Any CLI surface changes.

### Exit criteria

`brokkr check` passes. Default external path produces
coord_payloads and uses it in stage 4. `PBFHOGG_COORD_SLOTS=1`
reverts to the pre-integration path byte-for-byte.

### Commit message

`external_join: default external path to coord_payloads;
PBFHOGG_COORD_SLOTS=1 pre-integration escape hatch`

---

## Stage 6 — Cleanup (conditional; only after ≥ 1 week stable)

**Goal.** Remove coord_slots production from stage 3. Remove the
`PBFHOGG_COORD_SLOTS` escape hatch. Keep `CoordSlots` only for
dense/sparse/`--force` fallback (which opens a different on-disk
artifact in those code paths anyway — verify this assumption before
deleting).

**Criteria to schedule Stage 6.**

- Planet bench passes `brokkr verify` in the integrated path.
- At least one public/published user has run the integrated path on
  their data without incident (or several days of project-internal
  runs, whichever the user prefers).
- No open bug report related to coord_payloads integration.

**Out of scope.** Further coord format changes.

---

## Invariants preserved across all stages

- `brokkr check` passes after every stage commit.
- Baseline path (no env vars set) is byte-identical for all stages
  1-4.
- Prototype path (`PBFHOGG_COORD_PAYLOADS_PROTOTYPE=1`) is byte-identical
  for all stages 1-4. (Stage 5 removes the prototype; this invariant
  ends at Stage 5.)
- No subagent runs shell commands, benches, or commits. Subagent
  only writes code. Main conversation handles all integration,
  building, testing, and benching.
- No worktrees. All subagent work is on the main tree.

## Open design questions — resolved

**Q: Straddler staging — raw slot bytes or decoded per-way slices?**
**A: Raw slot bytes.** Rationale: (a) simpler contract — workers
push raw `[i32 LE lat][i32 LE lon]` byte slices, finalizer does all
delta work. (b) preserves the "delta encoding applied exactly once"
invariant (#5 above). (c) no ambiguity about where per-way
boundaries fall inside a half — delta encoding needs the full blob's
coord sequence anyway, and the finalizer has the full
`per_way_rcs[blob_idx]` available.

**Q: Is the per-way refcount sidecar indexed strictly by way blob
index, and is that index stable across paths?** **A: Yes,
unconditionally.** The sidecar is emitted in stage 1A in schedule
order (the same order that populates `way_slot_starts`), consumed by
every downstream path via `load_per_way_refcount_sidecar` which
returns `Vec<Vec<u32>>` indexed by blob_idx. Both the prototype
transform and the integrated stage 3 use the same sidecar with
identical indexing. Stage 1's schedule is deterministic, so
blob_idx is stable across runs on the same input PBF.

**Q: Header-writing strategy for `finalize_coord_payloads` — one pass
gather + second pass write, or buffer and backfill?** **A: Buffer
and backfill via `pwrite`.** Same technique as
`transform_coord_slots_to_payloads`: `set_len(header_size)` on the
output file, seek the BufWriter to `header_size`, write payload
bytes sequentially in blob-index order (accumulating offsets into a
`Vec<u64>` in memory), then after all payload bytes are flushed,
`pwrite` the header + offset table at file offset 0. Single
sequential write pass through the payload section; no pre-scan.

## Dataset facts (context for the subagent)

- Europe: ~453M ways, 4.69B refs, 56,692 way blobs, coord_slots 37 GB.
- Planet: ~1.17B ways, 12.4B refs, 17,529 way blobs, coord_slots 99 GB.
- Per-way refcount sidecar: 455 MB Europe, ~1.2 GB planet (varint stream
  per blob: `[varint num_ways][varint rc0]...[varint rcN-1]`).
- Bucket count: 256. Bucket size: ~147 MB Europe / 388 MB planet.
- Workers in stage 3: `min(available_parallelism - 2, 6)` — typically 6.
