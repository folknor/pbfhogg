# Pool-heavy RSS growth at planet (research note, transient)

Status: transient research note, 2026-10-30. Research only: no code changed,
no new cells run. It covers the `notes/pre-release.md` bullet "Moderate RSS
increases, all pool-heavy commands" (planet, bygg 16c/32t, `627883d7`):
`cat --clean version` +65 %, `time-filter` +43 %, `getparents` +39 %,
`apply-changes` +30 %, `check --ids` +28 %, `extract --simple` +17 %,
`extract --complete` +12 %, `check --ids --full` +11 %,
`multi-extract --simple` +10 %, `tags-filter` two-pass +9 %.

Inputs: the scout inventory `target/spar/scout-pool-rss.md` (I checked its
load-bearing claims against the source), stored sidecars, and two exchanges
with a codex `read` spar session (`01a18d8c-09f5-7b30-b971-95a21eb48e9b`). It
builds on `notes/cat-dedupe-memory.md`: writer permits are released at the end
of framing rather than at ordered delivery. It also builds on
`notes/merge-changes-memory.md`: credits should be held until the ordered
write. Both are refined below.

---

## 1. Root causes

### 1.1 Shared: transient decode storage scales with worker count, and nothing caps it in bytes

Every listed command decodes on a pool sized `parallelism - 2`. That is 30
workers on bygg and 22 on the README host, a ratio of 1.364. No byte budget
caps how many blobs are in flight at once. While a worker processes one blob,
it holds the following:

- `read_buf`, which holds the compressed blob (p50 about 1.8 MB, p99 about
  2.7 MB). Its capacity is kept between blobs.
- A decompress buffer from `pool_get(pool, data_size * DECOMPRESS_EXPANSION_GUESS)`
  (src/scan/classify.rs). `decompress_blob_raw` then reserves `raw_size` on
  top of that.
- String and group offset tables. These are appended to the same payload
  buffer (src/read/wire.rs, `parse_and_inline_with_scratch` in block.rs). The
  append can trigger geometric growth and relocate the whole payload. The
  scratch tables stay allocated alongside.
- For rewriting commands, a per-blob `BlockBuilder` and the uncompressed
  `Vec<OwnedBlock>` output.
- For cat, time-filter and apply-changes, a thread-local `FrameScratch`
  inside the worker (retention ceiling 12 MiB), plus the copy made by
  `FramedBlobParts::into_vec`. At that moment the parts and the flattened
  frame both exist.

**The decompress pool is effectively disabled at planet.** `DecompressPool::put`
drops any buffer with capacity over 4 MiB. Because the reservation is 4x the
compressed size and then grows, almost every buffer exceeds that.
`pipeline_decompress_pool_oversize_drops` equals the scheduled blob count in
every pool-based cell (50,815 of 50,816 for cat, time-filter and check
`--ids`). The mean dropped capacity is 7.25 MB. That figure is capacity, not
resident bytes. What it proves is that the pool achieves zero reuse, so every
blob does a fresh multi-MB allocate-and-free cycle through glibc.

**Allocator retention multiplies the effect.** At the end of time-filter
(`c33e5ddd`), mallinfo2 shows 661 MB of arena and 0.96 MB in use, even with
the `M_ARENA_MAX=2` cap applied. Cat (`a8a4384e`) still holds 487 MB of anon
after its workers drain, also with the cap applied. getparents, extract,
tags-filter and apply-changes run without the cap.

**Fit to the data.** Whole-process peak divided by worker count is the same on
both hosts: getparents 23.5 MB, check `--ids` 21-23 MB, time-filter 37-39 MB.
That is arithmetic on peak RSS, not a measured per-worker figure, but it is
the best fit available.

**Ceiling (codex's point, conceded).** Under the model
`fixed + workers * constant`, going from 22 to 30 workers can add at most
36.4 %. So worker count alone explains getparents (+36 % in kB) and roughly
check `--ids`. It cannot fully explain cat (+62 % in kB, 767,420 to 1,241,980)
or time-filter (+43 %). Both of those compare an older-commit plantasjen cell
against bygg HEAD. On bygg alone, time-filter is flat at 1.13-1.21 GB across
three commits. Neither of us found a source term that grows faster than
linearly with workers. Cat and time-filter frame inside the worker, so
rayon's 32-versus-24 global pool does not touch them. Cat's window is fixed,
and time-filter's window never binds (`window_wait` is 0). The remainder is
attributed to a per-task shape change between commits plus allocator
retention. That attribution is plausible but not settled.

### 1.2 Per command

| Command | Worker term dominant? | Other material holders |
|---|---|---|
| getparents | Yes, the cleanest case. Output is 684 bytes. | none |
| check --ids | Yes | per-blob result vector (small) |
| time-filter | Worker term plus retention | 661 MB free arena at end |
| cat --clean | Major part, but not the whole increase | 64-task window, where each task is a *vector* of framed outputs (one dense input blob can split into several output blocks); 487 MB residual |
| apply-changes | Not established | `CompactDiffOverlay` 575 MB; drain `ReorderBuffer` with no count bound, 415 MB high water (src/commands/apply_changes/drain.rs, a real byte measurement); pwrite queue about 100 MB; and an ownership hole: `std::mem::take(decompress_buf)` at streaming.rs moves the worker's buffer into `PrimitiveBlock`, so it is never reused. These maxima need not be simultaneous. |
| check --ids --full | No for the total; plausibly yes for the delta | dense `IdSet`s pre-allocated per kind |
| extract --simple / --complete | No for the total | persistent IdSets. **The rows compare different bboxes** (`-10,35,32,72` on the README host vs `-25,34,45,72` on bygg), so the percentages are not causal. |
| multi-extract --simple | No | n-region IdSets. The real delta is about **4 %** (9,876,364 to 10,283,956 kB); the +10 % mixes GiB and GB. Write phases run at 2.1-3.1 cores because of serial compression in 5 sync writers. |
| tags-filter two-pass | No for the total | most of the peak exists after pass 1 (2.59 GB) as dependency sets |

### 1.3 Smaller corrections to the inventory

- `writer_reorder_high_water` (310 for extract, 208 for apply-changes)
  records `pending_len()`. That is the span of sequence numbers outstanding,
  not the number of filled slots. It shows the window runs beyond 64, not
  that 310 frames are resident. Early permit release is still a real defect.
- `writer_frame_scratch_capped_bytes` (500 MB for apply-changes) is
  cumulative. It says nothing about how much was resident at once.

---

## 2. Hypotheses and confidence

| # | Hypothesis | Confidence |
|---|---|---|
| H1 | Transient decode storage per in-flight blob, times worker count with no byte cap, is the shared mechanism | High for getparents, check --ids, time-filter; medium for cat |
| H2 | Zero decompress-pool reuse at planet drives glibc churn and retention, which inflates peak anon beyond the live set | High that reuse is zero; medium on its share of the peak |
| H3 | The cat/time-filter excess over 36 % is a cross-commit per-task shape change plus retention | Medium-low; no bygg baseline at the old commit |
| H4 | apply-changes growth comes from worker storage plus the uncapped drain reorder plus the take() hole | Medium |
| H5 | The extract and multi-extract rows are mostly measurement artefacts (bbox, units), sitting on a real but small worker term | High |
| H6 | tags-filter and check --full deltas are the worker term on top of large persistent sets | Medium |

---

## 3. High-conviction architectural rewrite

**R1. A byte-budgeted task engine that fuses decode, transform, encode and
framing.** This replaces the unordered classify pool, `run_ordered`, the
writer's separate rayon framing dispatch, and apply-changes' private
scanner/worker/drain pipeline with one execution model offering three task
shapes:

```text
scan:        compressed -> borrowed validation/projection -> compact result
rewrite:     compressed -> borrowed decode -> transform -> encode -> compress -> framed result
passthrough: verified source range -> ordered output descriptor
```

- **Bottleneck.** Transient storage is set by worker count times the
  per-blob envelope. Large intermediate copies coexist: the whole rebuilt
  uncompressed output, then framed parts, then a flattened frame. Ownership
  handoffs between stages have no accounting. These are the drain reorder,
  the writer reorder after early permit release, and the pwrite queue.
- **Why the structure causes it.** Each stage owns its own buffers and its own
  count bound. No stage sees bytes. The writer is a second pool that sits
  outside the decode pool's window.
- **Redesign.**
  1. Admit tasks in sequence order against a byte budget.
  2. Before a task runs, reserve a *completion envelope* large enough that the
     task is guaranteed to finish. Acquiring storage piecemeal can deadlock:
     every worker holds input credit while waiting on output credit, and the
     earliest task starves.
  3. Recycle sub-buffers within that reservation.
  4. Hand the output allocation lease to the drain and then to the backend.
     Release credit when storage *becomes reusable*: the ordering reservation
     at ordered delivery, the output lease at I/O completion. This refines
     the earlier "hold until ordered write" rule, because the pwrite backend
     keeps the vector past the handoff.
  5. Encode each block and compress it immediately. Never materialize a
     task's whole uncompressed output.
  6. Have compression write straight into final handoff storage with framing
     space reserved in front, so `into_vec` and its copy go away.
  7. Keep offset tables in a separate workspace store that the parsed view
     borrows, so nothing is appended to the payload.
  8. Use size-class or segmented storage, sharded rather than behind one
     mutex, with an explicit path for oversized tasks: dedicated progression
     or spill, so the earliest task can always finish.
  9. Treat `raw_size` as an untrusted hint.
- **Payoff.** Transient memory becomes a chosen constant that does not depend
  on core count. That makes the regressions irrelevant for H1, H2 and H4.
  Removing materialization and copies shrinks the envelope, so the same
  budget sustains more concurrency.
- **The honest claim, agreed with codex.** Moving storage from workers to
  tasks does not lower peak by itself at saturation. These cells run 26-28
  busy cores out of 30, so workers are rarely idle. The budget value is the
  real memory control. If it is set below workers x envelope, throughput
  pays. Fusion is what shrinks the envelope, and with it that throughput
  cost.
- **Risks.** Deadlock in the admission protocol, which must be proven around
  the earliest task. Handling oversized and malicious blobs. Ordering per
  output once multi-extract joins. Cancellation and error paths that release
  leases. A byte ledger also does not bound pages glibc retains, so storage
  must actually be recycled (owned slabs or mappings), not just counted.

**R2. Projection scanners instead of a full `PrimitiveBlock`** (an
alternative I had passed over; codex elevated it and I agree). check `--ids`
needs IDs plus boundary and violation summaries. getparents needs refs and
members matched against the requested ID set. A borrowed scanner skips the
offset tables and the general element interface. It must keep every
structural and corruption check the command performs today. This shrinks the
envelope that R1 rations, and it cuts CPU. Priority is equal to R1 for these
scan-heavy commands.

**R3. Membership and overlay track** (the set-dominated rows: extract,
multi-extract, tags-filter, check --full, the apply-changes overlay). The
direction is fewer coexisting sets, releasing each set after its last
dependent phase, a shared region-membership representation for overlapping
multi-extract regions, and partitioned or spillable dependency state when a
hard ceiling matters. "Swap `IdSet` for a compressed bitmap" is too shallow,
because `IdSet` already has sparse and dense chunks. A bolder idea for check
`--ids --full`: on input proven strictly sorted, strict monotonicity already
rules out duplicates, so the dense sets can go, with a fallback for unsorted
input. Whether that pays depends on what the report must say.

**R4. Parallel compression for multi-extract inside the R1 tasks**, with
per-region ordering under one shared budget. Five separately async writers
would each get their own buffering allowance, so that alone is wrong. This is
the clearest throughput win in the stored data: about 600 s of write phases
at 2-3 cores.

## 4. Medium-value local changes

| # | Change | Note |
|---|---|---|
| L1 | Size the decompress reservation from `raw_size` (clamped) plus a metadata allowance, not `4 * compressed` | Fixing only the raw_size sizing can backfire: filling to exactly raw_size and then appending metadata can double capacity. Fix the whole construction path. |
| L2 | Replace the 4 MiB retention cap with byte-bounded reuse, or a worker-owned buffer in the synchronous scan pools | Removing the cap without a bound raises retention |
| L3 | Return the apply-changes decode buffer: remove the `mem::take` ownership hole | A real defect; cheap |
| L4 | Bound apply-changes drain admission *before* execution, not at insertion behind a gap | High priority within apply-changes |
| L5 | Hold writer permits through ordered delivery, and keep counting what the backend holds | Repair from cat-dedupe-memory, refined |
| L6 | Apply the arena cap uniformly | Supporting policy only. Cat and time-filter already cap and still retain; more contention is the cost. |
| L7 | Fix the counters: filled slots and bytes for the writer reorder; live versus cumulative scratch | Without these, the attribution in section 1 stays partly inferred |

## 5. Prioritized list

1. R1 fused, budgeted task engine (with L5 semantics built in).
2. R2 projection scanners for check `--ids` and getparents.
3. L3 + L4 now for apply-changes. They are independent of R1 and cheap.
4. L1 + L2 as an interim fix for zero decompress reuse, if R1 is far off.
5. R4 multi-extract parallel compression.
6. R3 membership and overlay redesign. This is the only item that lowers the
   largest absolute peaks.
7. L6 allocator policy, reassessed after the churn is gone.
8. Correct the item table itself: the extract rows compare different bboxes,
   and multi-extract is +4 % with consistent units.

## 6. Plan of action (each step can be deep-dived on its own)

- **H1/H2.** Do not probe. Build L1 + L2 completely, then run matched
  getparents and check `--ids` planet cells on bygg, interleaved and one at a
  time, with mallinfo2 samples at the phase boundaries. If peak anon per
  worker falls sharply, H2 has a large share. Then build R1 and R2 for
  getparents first, the smallest surface with the cleanest signal, and
  compare at budget = workers x envelope and at half that.
- **H3.** Run `--commit 4fc8e35` (cat) and `--commit 83183fb` (time-filter)
  on bygg. This is the one cell this item genuinely needs, and it separates
  cross-commit change from worker count.
- **H4.** Land L3 + L4, then rerun the apply-changes zstd:1 cell and read the
  drain high water against the overlay timeline in `--samples`.
- **H5.** Rerun extract on the README bbox on bygg before treating the rows as
  regressions at all.
- **R1 validation.** Test with a delayed earliest task, expanding output,
  absent or false `raw_size`, huge string tables, a slow backend, multi-region
  amplification, and cancellation and failure mid-task. Assert accounting and
  progress, never a specific host's RSS.

## 7. Spar record

- **Agreed.** The shared mechanism (H1). The decompress pool is dead at
  planet. A budget, not a change of storage owner, is the memory control. Set
  rows need their own track.
- **Codex moved me on:**
  - the 36.4 % ceiling, which ends "worker count explains everything";
  - parse does not copy strings, it appends offset tables (so the claim
    became "no relocation, separate metadata", not "zero-copy");
  - deadlock in piecemeal credit acquisition, so a completion envelope is
    required;
  - release on storage reuse, not uniformly at ordered delivery;
  - the apply-changes take() hole;
  - reorder high water is span, and the scratch counters are cumulative;
  - multi-extract is 4 %;
  - projection scanners deserve equal rank;
  - the centre of the rewrite is fused task ownership, not slabs.
- **Still open, not a disagreement.** The attribution of the cat and
  time-filter excess over 36 %. Neither side found a superlinear source term;
  the cross-commit `--commit` cells in section 6 settle it.

## 8. Lateral findings

1. apply-changes `std::mem::take(decompress_buf)` (src/commands/apply_changes/streaming.rs)
   defeats the worker's persistent buffer on every rewritten blob. Its zero
   pool-drop counters hide this.
2. `writer_reorder_high_water` measures the sequence span (`pending_len`),
   not filled slots. Docs and notes that read it as resident frames are
   overstating.
3. The tunables doc promises a 256 MiB retained bound for the decompress
   pool. At planet that bound is never exercised, because nothing is
   retained.
4. The extract rows in the pre-release table compare different bboxes, and
   the multi-extract row mixes GiB with GB.
5. getparents shows 65 peak threads for 684 bytes of output: rayon's global
   pool is spun up for two framed items.
6. multi-extract workers spend about 17,000 thread-seconds blocked on result
   send behind serial compression on the consumer thread.
