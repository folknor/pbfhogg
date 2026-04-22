# ALTW external-join: live leads

Every actually-still-open lever for `add-locations-to-ways --index-type external`.
Originally consolidated from four source docs; three of those (structural
reports, in-RAM-coord-table thesis, historical probe record) were folded
into `altw-optimization-history.md` and deleted once their content lived
fully in the history doc + this one.

Current baseline: planet 661.2 s (`aee7727`, `--bench 3`), europe 291.6 s.

Companion doc:

- [`altw-optimization-history.md`](altw-optimization-history.md) - journey
  from 96 min planet to 661 s; failed attempts with UUIDs + numbers;
  measured physical floors; meta-lessons.

Leads below are grouped by what is blocking them. Entries tagged `[hist]`
have a matching failed-attempt record in the history doc; `[tr]` marks
leads imported from apply-changes TODO transfer notes. Untagged leads are
self-contained here.

Meta-lessons worth pinning before picking an item:

- **Desk estimates on this code path have been systematically optimistic**
  (`altw-optimization-history.md`: 1B batching predicted -6 s, measured +22.9 s;
  stage-2+3 fuse sketch contradicted by desk analysis by an order of magnitude).
  Bound estimates with micro-benchmarks or skip to a small-dataset measurement.
- **Writer ceiling diagnostic.** Real stage-4 CPU wins are invisible on wall
  under zlib:6 output because freed decoder CPU refills the writer queue.
  Always measure keep/revert under both `zlib:6` and `zstd:1` (or
  `compression:none`) for any stage-4-side item.
- **Physical NVMe floor.** Stage 4 is 720 MB/s * 37 GB = ~51 s of coord read
  on this hardware; total stage-4 floor is ~141 s. Designs that do not reduce
  bytes-read cannot beat that floor.

---

## Tier 1: actionable now (platform decisions + design work)

### L1. #10 conservative: BlobHeader refcount extension `[hist]`

Embed per-way refcount + per-blob total refs in `BlobHeader` unknown-field
extensions during `pbfhogg cat`. At ~8000 ways/blob * ~2 bytes/varint refcount
= ~16 KB/blob, fits comfortably inside the 64 KiB header cap. ALTW stage 1
reads refcounts from headers instead of generating `ref_count_sidecar` +
per-way-refcounts scratch.

- Endorsed by R5 and R6.
- Blocker: decision on whether production pipeline always feeds ALTW from
  `pbfhogg cat`. The PBF spec invites unknown-field extensions, so non-cat
  consumers treat them as opaque by design.
- Scope: extend `BlobHeader` encoder (`src/write/writer.rs:1247`), extend
  decoder (`src/read/blob.rs:346` region), teach ALTW stage 1 to prefer
  header-provided refcounts when present. Small-to-moderate.
- Payoff: deletes a stage-1A scratch-writing pass. Small fraction of stage 1
  wall. Low conviction on wall, real structural cleanup.

### L2. #10 aggressive: BlobHeader per-way node-ID lists `[hist]`

Embed delta-varint per-way node-ID lists in header extensions so ALTW stage 1
reads only blob headers, no way-blob payload decompression at all. Naive form
is ~240 KB/blob at ~8000 ways * ~10 refs * 2-3 bytes, which exceeds the 64 KiB
header cap. Two design paths documented but not explored:

- (a) **Smaller blob groups.** More blobs, less data per header. Increases
  blob count and header-walk cost but keeps per-blob node lists inside the cap.
- (b) **Side-table addressed by blob position.** Move the data out of the
  header itself into a sibling file emitted by `cat`, still opaque to
  non-ALTW consumers.

- Biggest uncontracted ALTW win in this doc: would eliminate roughly 50% of
  stage 1 wall (CPU-bound zlib decompression of way blobs).
- Blockers: same cat-contract decision as L1, plus a concrete size-cap design.
- Scope: moderate-to-large. Needs the header/side-table framing decision first.

### L3. #3 R4 A1: single-pass ID-bucketed stage 1 `[hist]`

Instead of the reverted scratch-spool retry, fuse stage 1A + 1B by changing
the stage-1B partition key from rank to node-ID high bits. Each pass-A worker
simultaneously (a) calls `set_atomic` on `IdSetDense` and (b) emits
`(node_id: u64, slot_pos: u64)` 16-byte records into 256 ID-bucketed shard
files. Pass B disappears entirely. Stage 2 radix-sorts by
`(node_id - bucket_id_low) as u32` instead of counting-sorting by rank.

- Plan explicitly calls this "the only live variant" for #3 after the
  scratch-spool retries failed. The main objection (extra downstream
  rank work) was dissolved by #4 landing.
- Scratch grows planet 175 GB -> 234 GB (+33%). Extra 60 GB at multi-GB/s NVMe
  is sub-minute; saved decompression is CPU-minutes at planet.
- Blocker: needs a contained branch + Europe `--bench 3` keep-gate. Head-to-head
  against the current shape is cheaper than more speculation.
- Scope: larger than scratch-spool (stage 2's `prepare_bucket` needs a
  sort-by-ID path instead of counting-sort-by-rank).

### L4. Stage 1B grouped-by-local-rank, segmented variant `[hist]`

Design drafted 2026-04-14 but not implemented. Naive grouped emission needs
55 GB of per-worker per-bucket buffering (doesn't fit); the segmented form
buffers ~10 blobs per worker, does a local counting-sort, and k-way merges
in stage 2. Feasible memory ~920 MB/worker; estimated ~9 s wall savings from
eliminating `s2_prepare_scatter_ms`.

- Estimate is theoretical (the 1B batching "improvement" predicted -6 s and
  measured +22.9 s; mental model for this code path is unreliable).
- Distinct from the rejected flat grouped-by-local-rank shape (`856a7bb9`):
  segmentation bounds memory to worker-scale, k-way merge does the rest.
- Complexity: moderate-high. Measure on Japan or Denmark before planet.

### L5. Boundary-blob cache / contiguous bucket assignment in stage 2 `[hist]`

R5 flagged this as "a real, contained win": atomic bucket stealing at
`stage2.rs:356` throws away locality because workers end up processing
non-contiguous buckets. A contiguous-block bucket assignment, or a tiny
per-worker cache of the boundary-blob decode output between adjacent
buckets, would avoid re-decoding the same straddler blob on the next bucket.

- R5 originally said "defer until after #1/#8 land" because the slot-bucket
  layer might go away. #8 landed; #1 is dead; the slot-bucket layer is
  staying. Defer condition resolved.
- ~255 straddler re-decompressions at planet (100 MB scale extra decompress)
  is the upper bound; real saving probably smaller. Small item, small scope.

### L6. Stage 1B consolidated per-bucket writers (R4 B2) `[hist]`

The current stage 1 fanout is `num_workers * NUM_BUCKETS` = ~1500 files at
planet (~400 MB of `BufWriter` buffer memory resident). Consolidate to 256
shared per-bucket writers with batched per-worker flush (e.g. 64 KB chunks
under per-bucket lock). Distinct from the two previously-regressed 1B shard
experiments: fewer files and less buffer memory, rather than reshaping
emission.

- R4 itself said "the contention concern goes away if A1 + A3 are done
  (records flow through memory, not files)" - so this is fallback territory
  relative to L3. But if L3 is not pursued, this is a cheaper contained probe.
- Scope: small-to-moderate. Keep-gate: flat-or-better Europe wall plus a
  drop in resident `BufWriter` allocation.

---

## Tier 2: speculative, worth a measurement

### L7. Worker-emits-framed-bytes in stage 4 `[tr]`

Pattern transfer from apply-changes P1.5 (`719f306`, 2026-04-21). If ALTW
stage 4 still dispatches framing via `rayon::spawn` per output block and
funnels through `write_primitive_block_owned`, move framing inline into the
decoder worker: call `frame_blob_pipelined` directly, ship the framed
`Vec<u8>` to the writer thread via `write_raw_owned`. Apply-changes saw
`writer_pipeline_send_wait_ns` drop 86% at planet `--compression none`.

- Trigger condition: check `s4_send_ms` / `writer_pipeline_send_wait_ns` on
  the current 661 s baseline. If large, the pattern transfers; if tiny,
  it doesn't.
- Applies against the writer ceiling diagnostic: this is the way to
  *raise* the ceiling, not evade it.
- Zero-cost probe: just read counters from the existing bench database.

### L8. zstd:1 for internal ALTW pipeline `[tr]` `[hist]`

Already measured at Europe (`--compression zstd:1`): 419 s -> 379 s (-9.5%),
stage 4 wall -28%, `s4_send_ms` cumulative -81%, `s4_channel_high_water` far
below capacity. zstd:1 is not safe as the library default (osmium and the
wider ecosystem expect zlib-compressed blobs) but is the right choice for
users running an internal pipeline that controls both ends.

- Landed context, not landed knob: document this as a first-class flag
  recommendation in the ALTW CLI help rather than leaving it buried in
  benchmark notes.
- Composes with L7 and with any other stage-4-side item that would otherwise
  be masked by the zlib writer ceiling.

### L9. Stage 4 wire-format DenseNodes filter (re-measured non-zlib) `[hist]`

Shelved at `4910fd9` because the wall win was consumed by the zlib writer
ceiling. Europe `s4_nonway_assemble_ms` dropped 53% (78.5 s -> 36.9 s) under
zlib:6, but `EXTJOIN_STAGE4` went 122.7 -> 127.6 s as freed decoder CPU
refilled the writer queue. Not re-measured after #2 landed (which uncapped
stage-4 decode threads to 22).

- If zstd:1 (L8) or `compression:none` is adopted as the internal default,
  retrying wire-format DenseNodes filter under that output mode could cash
  in the real CPU win that was invisible before.
- Blocker: L8 decision first, then a small re-measurement branch.

### L10. Coord payload redesign: beyond 1.81x and/or wire-format-ready `[hist]`

The shipped `coord_payloads` delta-varint format achieves 1.81x (not the
3-4x originally estimated) because absolute lat/lon values in the first ref
per way remain 5-byte varints. The history doc closes the "change coord
access mechanism" family but does not close "change coord representation."
A format that shares a base lat/lon across all refs in a way (rather than
only between consecutive refs) would compress the absolute-value tax.

- Physical ceiling from `altw-optimization-history.md`: stage 4 is
  720 MB/s * 37 GB = ~51 s at Europe; coord_payloads is 20.8 GB, so a 2.5x
  compression ratio would put coord read under 30 s.
- Separate measured lever from the same history doc: a wire-format-ready
  payload variant eliminated `s4_way_delta_encode_ms` entirely and cut
  Europe stage 4 by ~11 s, but only inside a prototype that paid a separate
  65 s transform pass. If stage 3 emitted that payload form directly, the
  measured projection was about ~8% upside at planet.
- These are orthogonal subpaths: denser coord representation reduces bytes
  read; wire-format-ready payloads reduce stage-4 encode work. They can be
  combined or tried independently.
- Scope: stage 3 encode rewrite + stage 4 decode path + new escape hatch
  during rollout. Larger than incremental.

### L11. Compact 7-byte-per-coord encoding `[hist]`

From the failed altw_v2 in-RAM-coord-table experiment (see history doc):
at ~12% size saving versus 8 bytes (sign-bit-carrying the lat into 3
bytes when ranges permit), the in-RAM plan still didn't fit. But the
same encoding applied to on-disk `coord_payloads` is a straightforward
size win disjoint from L10.

- Small payoff, small scope.
- Subsumed by L10 if L10 is pursued (bigger format change covers it).

### L12. #11 presence bitmap `[hist]`

Reverted 2026-04-17 as a standalone change (+6 s stage 2). Parked explicitly
as a carry-along: "If a future seam reshapes the stage-2 inner loop, it can
carry a presence bit for free." Any other stage-2 item on this list (L3, L5)
should land it in the same diff.

### L13. Lift hard `.min(6)` worker caps `[hist]`

`src/commands/altw/mod.rs:328`, `stage2.rs:234`, `stage3.rs:125`. R5 flagged
as "obvious anti-saturation choices on wide hosts" but said "I would not
treat it as a first-order optimization on the current architecture" because
the structural rewrites might change the parallelism model. Those rewrites
(#1, #6) are shelved, so the architecture is not moving. Worth a bench sweep
(6, 8, 12, `available_parallelism - 2`) on whatever wide host is available.

- Risk: wider `thread::scope` may inflate resident buffer totals. Measure
  peak anon RSS alongside wall.

---

## Tier 3: hardware-gated

### L14. #6 blob-group downstream rewrite + faster second NVMe `[hist]`

Reverted 2026-04-22 on plantasjen with a measured design tax (+3.6% zlib:6,
+9.4% `--compression none`). The cross-disk probe that would split the
stage-2-write / stage-3-read contention was aborted because the second NVMe
(Samsung 970 EVO Plus 1 TB) regressed wall +30% versus the primary (Samsung
990 PRO 4 TB). Design, code, and measurements are all captured in the
history doc (`#6 blob-group downstream rewrite`).

- Unblocker: a second drive matching the primary's throughput, or a
  different host with symmetric NVMe. Then the pre-revert branch can be
  revived and re-benched cross-disk.
- As-is on single-fast-disk hosts: dead.

### L15. Cross-disk scratch as pure config `[tr]`

Separate from L14. Apply-changes saw a 31% planet drop just by moving bench
output to a different physical NVMe. The same `brokkr.toml` edit against
ALTW would test the hypothesis without any code changes. Blocked on the
same slower-second-NVMe constraint as L14, so coupled to L14's unblocker.

### L16. #1 variant (c): per-epoch u32 `local_slot_pos` `[hist]`

Last-resort per the plan doc, kept only because a future revisit will want
the documented retry shape. Per-epoch-scoped `local_slot_pos: u32` in a
single 12-byte stream; drain recomputes bucket from
`epoch_slot_start + local_slot_pos`. Costs one extra arithmetic op per drain
record. Auto-tune `num_epochs` from `/proc/meminfo` so Europe picks E=2-3
and planet picks E=4-6 rather than hardcoding E=4 (loses at europe) or E=8
(loses at planet).

- Plan's own math: ~14 s net at planet E=4 against the current 12-byte
  slot-bucket path. Inside bench noise.
- Revisit only if L3 / L4 / L5 / L7 / L10 are all tried and the stage
  2 -> stage 3 seam is still dominant.

### L17. #5 direct-to-`coord_payloads` via per-blob accumulators `[hist]`

`#5` is dead on plantasjen-class RAM, not dead in principle. The structural
reports' own math is ~100 GB of accumulator payload at planet
(`Vec<(u16 local_offset, i32 lat, i32 lon)>`), so this only reopens on a
host that can either carry a much larger in-memory working set or revive an
epoch-based downstream path without spilling away the gain.

- Value if unblocked: deletes `scatter_buf` zero-fill + reread, and drops
  `classify_blobs_in_bucket` / straddler machinery by routing resolved
  entries straight to blob-local accumulators.
- Unblocker: materially more RAM than the current 25 GB-available host, or
  a future epoch design that makes the working set truly bounded.
- Treat as paired with L16 or a larger-memory machine; on the current host
  it remains in the negative-results bucket.

---

## Tier 4: deep stretch

### L18. #7 single-decode node path `[hist]`

Stage 2 decodes kept node blobs; stage 4 decodes them again on the non-way
passthrough path. Planet cumulative `s2_node_decompress_ms = 192356`; stage
4 processes all 32835/32835 node blobs again. Fusing is "architecturally
awkward" (stage 2 is rank-bucket-ordered, stage 4 is file-ordered), and
the plan explicitly flags writer-ceiling risk.

- Needs a scheduler rewrite, not a patch.
- Medium-low conviction, very large scope.
- Worth revisiting only after L8 neutralizes the writer ceiling.

### L19. Overlapping stages 1 and 4 `[hist]`

Pipe decompressed way blobs from stage 1 through to stage 4 so the
way-pass decompression is reused across the two stages. Requires running
stages 2/3 concurrently with way-blob transit. "Win: one fewer PBF read
of way blobs. Complexity: enormous. Not justified pre-1.0."

- Listed because the user said "any nugget."
- Do not pursue until everything else is exhausted.

### L20. io_uring SQPOLL + registered buffers + IOPOLL `[hist]`

Not applicable to the current shape (stage 3 writes are large sequential
pwrites). Filed in the history doc "only if a future structural change
(e.g., way-ordered payloads) creates many small concurrent writes." If
L14 unblocks and a blob-group variant with small discontinuous writes
becomes the new shape, re-open this.

---

## Do not re-try on this hardware

Preserved as negative results so these do not get re-proposed:

- #1 epoch-spill with the 16-byte spill record format (2026-04-21 port
  regressed +10% at planet). Any retry must use L16 (12-byte single stream).
- #3 scratch-spool in any buffered/varint form (2026-04-17 flat-i64 attempt
  and 2026-04-21 BufWriter + delta-varint + Cursor fast-path attempt both
  regressed). zlib-rs decompresses way blobs faster than we can reread the
  scratch. Only L3 (R4 A1) is live.
- #5 per-blob accumulators on plantasjen-class RAM (~100 GB working set;
  25 GB RAM host; see L17 for the reopen condition).
- In-RAM coord-table form (altw_v2 experiment, OOM-killed on europe at
  29 GB, planet projected ~80 GB; see history doc).
- Further rank-bucket-count experiments (measured flat-to-regressive at 384
  and 512 on Japan).
- More micro-opts on the current `rank_if_set` stage-2 hot loop (attribution
  shuffle without wall movement).
- pwritev for stage 3's current shape (one contiguous 150 MB buffer per
  bucket degenerates to pwrite).
- Non-way-blob wire-format filter under zlib:6 output (writer ceiling eats
  the win; see L9 for non-zlib re-measurement).

---

## Suggested ordering

If someone picks this doc up cold and wants a sequence:

1. **L7** counter inspection (zero-cost, disqualifies half the remaining
   ideas if writer queue is not the bottleneck).
2. **L8** as a recommendation / CLI hint (documentation, not code).
3. **L1** BlobHeader refcount extension (platform decision; if production
   uses cat-in, this is uncomplicated). Then L2 design exploration on top.
4. **L5** boundary-blob cache or **L3** R4 A1 as the next contained bench.
   L5 is smaller; L3 is bigger but attacks the larger seam.
5. **L10** coord format compression if stage 4 is still the governing
   phase after the above land.
6. **L17** only if a materially larger-RAM host becomes available or L16's
   epoch-style downstream path is revived.
7. Everything else on demand.

"No obvious next-live structural item" was true for the ranked-seam items in
the structural reports. It is not true across the union of all four docs.

---

## Correctness invariants

Any stage 1-4 edit must preserve these or explicitly replace them. Guardrails
for the live leads above, not optional.

- **Sorted + indexed PBF precondition.** `external_join` requires
  `Sort.Type_then_ID` headers and indexdata. Enforced at entry; do not relax.
- **2-piece straddler invariant.** A blob's slot range spans at most two
  adjacent slot buckets. `slot_bucket_count` is chosen so every bucket width
  is at least `max_blob_slots`. Constrains L14 (blob-group rewrite) and any
  layout change to slot buckets.
- **Zero-coord sentinel.** Stage 2's `coord_slice` uses `(lat==0, lon==0)`
  as the unresolved sentinel; the slice is fully zeroed at the start of each
  rank bucket. Any redesign that removes zeroing (L17 per-blob accumulators
  skipping empty slots, L12 explicit presence bitmap) must replace the
  sentinel with an explicit presence signal.
- **Per-way refcount ordering.** The stage-1 per-way refcount sidecar is
  written in PBF blob order and consumed in PBF blob order by stage-4
  reframe. Any stage-1 reshape (L3, L4, L6) preserves this ordering.
- **Straddler state machine.** Stage 3's merge is exhaustive
  `None -> Left|Right -> Both`; duplicate or third halves error. Do not
  weaken to `Option<(Vec<u8>, Vec<u8>)>`. The streaming coordinator that
  landed in #2 (`beb7838` + `f93d896` + `eecb46c`) preserves this; any
  future rewrite of the stage 3 -> stage 4 handoff must too.
- **Blob-local rank monotonicity.** For sorted PBFs, `extract_node_tuples()`
  yields node tuples in ascending ID order, and referenced nodes inside a
  blob occupy the contiguous rank interval `[ref_rank_start, ref_rank_end)`.
  This invariant is what made the landed #4 (`f1a4ada`) blob-local rank
  counter correct; any future stage-2 reshape (L3, L17) that changes how
  ranks are assigned inherits the same precondition.
- **`build_rank_index()` discipline.** `IdSetDense` requires the rank index
  built after all `set_atomic` calls and kept until the last rank consumer
  is gone. L3's R4 A1 must finish populating `IdSetDense` during pass A
  before `build_rank_index()`; if pass B disappears, the stage-1 boundary
  becomes the rank-metadata drop point.

---

## Implementation conventions

Load-bearing patterns learned across landed and reverted items. Apply when
implementing any lead above.

- **Ns accumulators for per-item timing.** `AtomicU64` holding nanoseconds,
  `ns_to_ms` helper at emit time. Reference: `WayReframeCounters` in
  `stage4.rs`. Do not accumulate `as_millis()` per item; sub-ms work
  truncates.
- **ReorderBuffer for parallel producer -> serialized consumer.**
  `crate::reorder_buffer::ReorderBuffer::with_capacity(N)`; push with
  `(seq, value)`, `pop_ready()` drains in order. Already used by stage 1
  pass A, stage 3, stage 4, and the streaming coordinator in #2. Reuse;
  do not reinvent.
- **ScratchDir for all temp files.** `scratch.file_path(name)` or
  `scratch.bucket_path(kind, idx)`. Lifetime-tied cleanup on drop. Applies
  to L3's node-ID shards, L4's segmented per-worker buffers, L17's
  per-blob spill.
- **`#[hotpath::measure]` on functions > 1 ms wall** so they show in
  `--hotpath` profiles. Annotate *inner* hot-loop helpers, not just the
  outer phase wrappers - the outer wrapper alone just says "the phase took
  Xs", which you already know from the phase marker. If a `--hotpath` run
  produces zero function rows and brokkr logs `failed to read hotpath
  report`, check whether the CLI path went through `process::exit(1)`;
  `process::exit` skips destructors, which prevents the
  `HotpathGuardBuilder` from flushing its JSON. Fixed globally at
  `a3795c2` (2026-04-20) by returning `process::ExitCode` from `main`;
  re-break with caution.
- **Pread-only header walker.** `src/read/header_walker.rs::HeaderWalker`
  is the shared primitive for `pread`-only header walks with
  `posix_fadvise(POSIX_FADV_RANDOM)`. Each blob costs two small preads
  (4-byte length prefix + header bytes) and skips the data payload by
  offset advance. Used by getid include mode (6.2x planet) and the diff
  shard planner. If a future ALTW seam needs header-only walking (e.g. an
  extension of #9 beyond relation indexdata), reuse this primitive instead
  of hand-rolling another walker; it already handles the kernel-readahead
  edge that a naive `BufReader` walk hits.
- **Worker count convention.** `available_parallelism() - 2 max 1 min 4`,
  often `.min(6)`. The `-2` reserves cores for the consumer + writer
  threads. L13 proposes sweeping this; any tuning that changes it must
  justify why.
- **Counter naming.** `s<stage><phase>_<thing>_ms` / `_bytes` / `_calls`.
  Stage-scoped prefix keeps grep/history readable. For partitioned work
  (rank buckets, slot buckets, shards), emit min/max/count-per-phase
  counters as a balance diagnostic - max/min ratio near 1 means balanced,
  big spread means the partitioner collapsed. Pattern landed in
  `src/commands/diff/derive_parallel.rs` as
  `derivepar_{node,way,rel}_shards` / `_shard_max_blobs` /
  `_shard_min_blobs`; catches partitioner regressions in one
  `brokkr sidecar --counters` look.
- **Prototype discipline.** Prefer full coherent branch rewrites with
  keep/revert benchmarking over env-var-gated probes. If a temporary
  fallback is unavoidable during rollout, keep it short-lived and delete
  it as soon as the decision is made. Narrow env-var probes created
  codebase pollution and often failed to answer the real structural
  question.
- **Assert rank invariants when deleting rank queries.** When removing
  `rank_if_set()` / `rank()` / `count_below()` calls from a stage (as #4
  did, landed `f1a4ada`), add debug/validation checks for monotonic node
  IDs and final `next_rank == ref_rank_end`; do not rely on comments alone
  for blob-local rank correctness.
