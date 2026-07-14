# ALTW external-join: live leads

Every actually-still-open lever for `add-locations-to-ways --index-type external`.

Rewritten 2026-07-13 from a full code re-read of `src/commands/altw/external/`
(post-A1, post-`--inject-prepass`), the planet counter set from run
`abe2ebf2`, and an external design critique (codex gpt-5.6-sol at xhigh;
transcript `~/.codex/sessions/2026/07/13/rollout-2026-07-13T10-30-40-*.jsonl`).
The previous edition's L1-L20 items are dispositioned in the ledger near the
bottom; nothing was silently dropped.

Companion doc:

- [`altw-optimization-history.md`](altw-optimization-history.md) - journey
  from 96 min planet to the current baseline; failed attempts with UUIDs +
  numbers; measured physical floors; meta-lessons.

## Baselines

All plantasjen (Ryzen 5900X 12c/24t, ~26 GB available RAM, Samsung 990 PRO
4 TB, single data/scratch drive).

| Scale | Wall | UUID | Commit | Date | Status |
|---|---:|---|---|---|---|
| planet | **592.2 s** (bench 3) | `b65fcad0` | `dcc445e` | 2026-07-14 | **current reference baseline** |
| planet | 589.9 s (bench 3) | `dc62a437` | `16e3694` | 2026-07-14 | April commit re-run today; see drift verdict |
| planet | 628.2 s (bench 1) | `ed5dd6c5` | `dcc445e` | 2026-07-14 | tired-drive control, run late in the suite |
| planet `--inject-prepass` | 624.6 s (bench 3) | `b1d9ba16` | `dcc445e` | 2026-07-14 | run-order confounded; see A/B section |
| planet | 546.0 s | `7fd04130` | `16e3694` | 2026-04-27 | **retired** - same commit measures 589.9 s today |
| europe | **285.7 s** (bench 3) | `cdfa9453` | `dcc445e` | 2026-07-14 | current reference; N1 keep/revert comparand |
| europe | 270.8 s | `0b89f986` | `0dc8ae1` | 2026-04-25 | superseded |

Pre-N1 europe reference profiles at `dcc445e` (2026-07-14), captured so
the packed-u64-IdRecord work has a matched-commit comparand: hotpath
`2db37ded` (257.5 s), alloc `a6806e34` (246.2 s). No function-level or
allocation profile existed for external at europe before these.

Compression axis at europe is **measured and settled** - see
`reference/performance.md` "Compression axis": `none` 246.8 s, `zstd:1`
233.3 s (-13.9 %) vs the zlib:6 270.8 s reference. zstd:1 for
closed pipelines is already documented in README.md and
`reference/performance.md`. Do not re-propose output-compression knob
sweeps.

**The europe compression result does not transfer to planet.** See "The
planet streaming phase is not compression-bound" below - the reopen
condition is now spent, and the answer changed the S2/A2 gating story.

### The planet streaming phase is not compression-bound (NEW 2026-07-14)

The single planet compression cell that the do-not-retry reopen condition
allowed has been spent. Matched HEAD `--bench 1` pair, zstd:1
(`0e9d93cc`) vs zlib:6 (`ed5dd6c5`):

| | zlib:6 | zstd:1 | Delta |
|---|---:|---:|---:|
| `writer_compress_ns` | 3855.2 s cum | 767.8 s cum | **-80 %** (-3087 core-s) |
| streaming wall (s3+s4) | 252.8 s | 240.0 s | -5.1 % |
| `s4_send_ms` | 705.2 s cum | 59.3 s cum | -92 % |
| `s4_readiness_wait_ms` | 667.0 s cum | 1395.2 s cum | **+109 %** |
| `writer_pipeline_send_wait_ns` | 20.5 s cum | 399.9 s cum | +20x |
| output bytes | 91.17 GB | 95.20 GB | +4.4 % |

**Deleting 3087 core-seconds of compression bought at most 12.8 s of
wall.** The phase is not compression-bound at planet. This is the
"regime transfer is a trap" meta-lesson landing on the doc that wrote
it: the compression-saturated reading came from europe plus the zlib:6
planet counters (4306 core-s over a 248 s phase looks like saturation),
but core-seconds of a resource are not evidence that the resource binds.

Under zstd:1 the constraint moves to **stage-3 -> stage-4 payload
readiness**: `s4_readiness_wait_ms` doubles to 1395 s cum, ~63 s per
decode thread of a 240 s phase. That is N3's exact target.

Read the direction, not the magnitude. Total wall says zstd:1 is *worse*
(648.4 s vs 628.2 s) because +31.7 s of drive drift in the
codec-independent stages 1+2 swamped the -12.8 s streaming win. The
robust claim rests on the counter ratio (-80 % CPU for <=-5 % wall), not
on either wall number. This cell is also the cleanest illustration on
file of why planet single-sample total-wall comparisons cannot be
trusted here.

Consequences, both of which change plans:

- **S2's gating premise is false.** It was shelved as "invisible under
  zlib:6; becomes a wall item if the compression ceiling ever moves."
  The ceiling is not compression, so freed stage-4 assemble CPU has
  nowhere to go under *either* codec. S2 is dead at planet today, not
  merely parked. Re-evaluate only after N3 moves the payload-readiness
  constraint - and re-derive the ceiling then rather than assuming.
- **The A2 / output-executor family loses the same story.** It was
  already shelved on measurement (pool consolidation lost 30 s planet);
  this removes the theory that would have revived it.
- **N3 is promoted** (see tier 1) from "cheap insurance, wall
  near-neutral" to the item that owns the actual planet streaming
  constraint.

### `--inject-prepass` A/B: SETTLED 2026-07-14 - cost is under 1 % at planet

**Six interleaved `--bench 1` cells at `fb743f6`, ON leading, alternating
ON/OFF/ON/OFF/ON/OFF:**

| pair | ON | OFF | delta |
|---|---:|---:|---:|
| 1 | 627.4 s (`8267419a`) | 621.8 s (`4ff288fb`) | ON +0.90 % |
| 2 | 644.6 s (`8333cbca`) | 645.4 s (`8cc542eb`) | ON -0.12 % |
| 3 | 646.3 s (`2a5b109a`) | 655.0 s (`2205c7d5`) | ON -1.33 % |

**Median ON 644.6 s vs median OFF 645.4 s: a 0.12 % gap, with the sign
flipping across all three pairs.** The injection cost is not measurable
at planet by wall. Record the bound as **<1 %**, close ADR-0007's gate
for good, and **stop measuring this** - a fourth attempt cannot resolve
what three designs could not.

The interleave earned its keep, and the run confirms why the previous
two attempts failed. Drift across the suite was large and monotonic
(walls climb 627 -> 655 s, +4.5 % in an hour), and the free readout
proves it on byte-identical work: first cell `s1a_id_shard_write_ms`
1,174,295 ms vs last cell 1,515,132 ms - **+29 % on exactly
149,225,518,932 shard bytes both times.** Any blocked A/B design would
have handed that +29 % to whichever arm ran last and called it a result.
That is precisely what produced the +5.5 % (2026-07-14 blocked
`--bench 3`) and the -5.3 % (2026-07-13 `--bench 1`) - same question,
opposite signs, both pure run order.

**Method note worth keeping:** matched sample counts are necessary but
not sufficient. `--bench N` is best-of-N *within* a cell, so a blocked
pair still samples one point in drive-state time. Alternate `--bench 1`
cells and compare medians; weigh sign consistency across pairs above the
median gap.

N7 (gate the closure/pins staging behind the flag) is no longer needed
as a measurement aid - there is nothing left to measure. It remains
worth doing on its own merits: it is free, and its sparse analogue was
measured at ~2.3 ns/ref (see `notes/altw.md`).

### Drift verdict: drive state, not code (SETTLED 2026-07-14)

The matched `--bench 3` pair settles it on the same-commit comparison:
`16e3694` measured **546.0 s in April and 589.9 s today** (`dc62a437`),
+8.0 % on byte-identical code. HEAD is 592.2 s (`b65fcad0`), 0.4 % from
the April commit re-run - inside noise, and conservative in the right
direction: the April cell ran late in the suite on a tired drive and
still matched rested HEAD. **There is no code regression between
`16e3694` and HEAD.** Do not bisect; the 546.0 s baseline is retired
rather than regressed against.

The mechanism was caught directly inside the same suite. The zstd:1
(`0e9d93cc`) and zlib:6 (`ed5dd6c5`) cells are the same binary an hour
apart, and `--compression` never touches stage 1. Both wrote **exactly**
149,225,518,932 bytes of id shards; `s1a_id_shard_write_ms` was 1140.7 s
cum vs 1495.6 s cum - **+31 % write time on identical bytes**. Stages 1+2
together (both codec-independent) differ by +31.7 s between those two
cells. That is the drift, measured live, on work that provably cannot
depend on the variable under test.

Bench-infra caveat, load-bearing for every planet number here: the SSD is
87 % full (469 G free of 3.6 T), which shrinks the dynamic SLC window, and
each ALTW planet run pushes ~705 GB of scratch through it. **Planet
single-sample walls on this host carry a ~5-8 % environmental band, and
run order inside a suite is a real variable.** Any A/B that matters needs
matched sample counts AND order-swapped or interleaved cells - adjacent
cells do not share drive state. See the inject-prepass A/B below for what
happens when this is ignored.

## Current architecture and planet cost model

Shape (post-A1 rankless join, `--inject-prepass` capable):

1. Blob metadata scan (pread-only HeaderWalker; also drives the tag-scan
   for node-blob filtering).
2. Stage 1: single parallel way pass (22 workers). Per ref, emits one
   12-byte IdRecord `(local_node_id u32 | CLOSURE_FLAG, blob_idx u32,
   blob_local_slot u32)` into 256 id-bucket shards (per-worker files),
   plus the blob-order refcount sidecars. Relation scan (sequential,
   pread-only over relation blobs) runs concurrently in the same scope.
3. Stage 2: 6 workers claim id buckets, read the bucket's shards back,
   sort by masked `local_node_id`, decode intersecting node blobs by
   indexdata range, merge-walk, emit 12-byte ResolvedEntry
   `(local_slot_pos u32, lat i32, lon i32)` into 256 shared slot buckets.
   Under `--inject-prepass`, shared-node pin decisions ride bit 0 of a
   doubled lat.
4. Stage 3 + stage 4 streaming: stage 3 (4 workers) reads slot buckets,
   scatters into dense per-bucket coord buffers, encodes per-way-blob
   delta-varint coord payloads into worker tmp files, publishes via the
   ConcurrentBlobLocationRouter; stage 4 (22 decode threads) preads input
   + payloads, wire-reframes way blobs, decodes/rebuilds node blobs
   (filtering untagged non-members), passthrough for relations, all
   through the generic parallel PbfWriter at zlib:6.

Planet volumes (run `abe2ebf2`, commit `856efc3`, plantasjen): 12.435 B
refs; 17,529 way blobs / 32,835 node blobs / 452 relation blobs; max node
id 13.59 B; bucket width 53.09 M; max refs in one way blob 2,434,168.

| Phase | Wall | Bound by | Key cumulative counters |
|---|---:|---|---|
| meta scan | 7.3 s | serial pread latency | 50,816 blobs x 2 preads |
| stage 1 | 82 s | scratch write bandwidth (149.2 GB at ~1.8 GB/s) | `s1a_id_emit_ms` 1009 s; pread 486 s; decompress 153 s; scan 103 s |
| stage 2 | 296 s | shard read-back + sort; 6 workers at 3.7 avg cores | `s2_bucket_load_ms` 951 s (read ~436, sort 477, parse 38); `s2_resolve_ms` 795 s (pread 302, decompress 196, extract 92, walk 144) |
| stage 3+4 | 248 s | zlib:6 output compression | `writer_compress_ns` 4306 s (~17 of 24 hw threads); `s4_send_ms` 1587 s; s3 read 456 s + scatter 107 s + encode 187 s |

Scratch traffic per planet run: 149.2 GB id shards written + read,
149.2 GB slot buckets written + read, 54.0 GB payload tmps written +
read = **~705 GB**, plus 181 GB input reads and 91 GB output. This is
also per-bench SSD wear.

Load-bearing regime facts:

- Stage 1 is **not** decompress-bound anymore (emit 1009 s cum vs
  decompress+scan 256 s cum). Anything that attacks way-blob
  decompression in stage 1 is attacking 12 s of wall.
- Stage 2's six workers average 3.7 cores: the workers stall on I/O,
  not on the dispatch cap per se.
- The streaming phase **looks** compression-saturated under zlib:6 (4306
  core-seconds of zlib:6, `s4_send_ms` 1587 s cum of decode workers
  blocked on the writer) but **is not compression-bound**: removing 80 %
  of that CPU via zstd:1 moved wall <=5 % (2026-07-14, `0e9d93cc` vs
  `ed5dd6c5`). Once the writer is cheap the phase binds on stage-3 ->
  stage-4 payload readiness instead (`s4_readiness_wait_ms` 1395 s cum).
  The zlib:6 `s4_send_ms` figure measures *where workers queue*, not
  *what limits throughput* - do not read it as a ceiling.
- The P1b node-blob skip never fires at planet:
  `s4_node_blobs_kept_by_tags` = 32,835 of 32,835. Every node blob is
  decoded and rebuilt on the default (drop untagged) path.
- `--inject-prepass` costs: closure detection + closure_slots staging in
  stage 1 currently run **unconditionally** (12.4 B Vec<bool> pushes per
  planet run even without the flag); pin-run logic in stage 2 and pin
  bitmap plumbing in stages 3/4 are gated.

Instrumentation added 2026-07-13 (lands in every subsequent sidecar):

- `s1a_id_shard_write_ms` / `_calls` - real shard write time at
  BufWriter drain granularity (256 KB); splits `s1a_id_emit_ms` into
  CPU-side emit vs disk writeback. This is the direct measurement the
  drift verdict and N1 both need.
- `s3_tmp_pwrite_ms` / `_calls` - the write half of the payload tmp
  round trip N3 wants to delete. The dead `s3_parse_ms` counter
  (always 0 since the scatter absorbed parsing) was removed.
- `extjoin_id_bucket_max_records` / `_min_records` /
  `extjoin_id_buckets_nonempty` and the slot-bucket twins - partition
  balance spread per the derivepar convention; min is over nonempty
  buckets.
- `WAIT_S4_SEND` (decode workers blocked on the consumer channel,
  gated behind try_send) and `WAIT_S4_ROUTER` (wait_ready slow path)
  stall spans for `brokkr sidecar --stalls`, emitted through a
  depth-gated StallGauge so N concurrent blockers produce one
  non-overlapping span per busy period instead of unpairable
  interleaved marker pairs.
- `#[hotpath::measure]` extended below the phase wrappers:
  `prepare_bucket`, `scatter_bucket_entries`,
  `emit_integrated_intersections`, `encode_blob_payload_from_record`,
  `reframe_way_blob_with_locations`, and the relation scan.

## Meta-lessons (pinned)

- **Desk estimates on this code path are systematically optimistic**
  (history doc: 1B batching predicted -6 s, measured +22.9 s; altw_v2
  sizing off 4-5x). Bound estimates with micro-benchmarks or skip to a
  small-dataset measurement.
- **Writer ceiling diagnostic - scope corrected 2026-07-14.** At *europe*
  real stage-4 CPU wins are invisible on wall under zlib:6 because freed
  decoder CPU refills the writer queue. **At planet this is false**: the
  zstd:1 cell removed 80 % of compression CPU for <=5 % of streaming wall
  (section above). Still measure stage-4-side keep/reverts under both
  codecs - but to *find* the ceiling, not to confirm a ceiling you have
  assumed. Core-seconds burned by a resource are not evidence that the
  resource binds; only removing it and watching wall is.
- **Physical NVMe floor.** Designs that do not reduce bytes moved cannot
  beat the device. The largest byte streams are now the two 149 GB
  scratch permutations, not the coord read.
- **Regime transfer is a trap.** "Flat" compression-level results from
  decode-bound regimes (Denmark/Japan pipelined writes) say nothing about
  compression-saturated regimes (planet ALTW streaming), and vice versa.
  Check which resource saturates before importing a conclusion.
- **Counter attribution: preads do not majflt.** Major-fault storms are
  mmap or swap, never `read_exact_at` cache misses. Attribute before
  optimizing.
- **The double permutation is fundamental.** Node-ID order to way-slot
  order over ~100 GB of coordinates on a 26 GB host requires two
  materialized passes; every in-RAM, direct-scatter, epoch, and
  accumulator alternative has been analyzed or measured into the ground
  (history doc + codex 2026-07-13 concur). Optimize around it, not
  against it.

---

## Tier 1: the live queue

Ordered. Each item lists what it absorbs from the previous edition.

### N1. Pack IdRecord into one u64 (absorbs the stage-2 sort thread; enables radix retry)

Layout: `local_node_id:27 | linear_slot_pos:36 | CLOSURE:1` - exactly 64
bits. Constraints at planet today: bucket_width 53.09 M < 2^27 (134 M),
total_slots 12.44 B < 2^36 (68.7 B). Put local id in the high bits and
the closure flag in the low bit so `sort_unstable` on raw `&mut [u64]`
yields (id, slot)-ordered runs directly; equal-id runs stay adjacent for
the stage-2 pin-run scan.

Why it is the top join-side lever (all planet numbers from `abe2ebf2`):

- Shards shrink 149.2 -> 99.5 GB: -50 GB of stage-1 writes (the phase is
  write-bandwidth-bound) and -50 GB of stage-2 read-back.
- The 12-byte parse step disappears (`s2_prepare_parse_ms` 37.8 s cum):
  read bytes into an aligned `Vec<u64>` and sort in place. The loader
  currently holds raw bytes AND parsed records - ~24 bytes/record
  resident drops to 8, roughly halving stage-2 worker RAM
  (`s2_max_worker_buf_bytes` 1.52 GB today).
- Sorting u64 keys is cheaper than 12-byte structs with key extraction
  (`s2_prepare_sort_ms` 476.8 s cum today).

**Known design obstacle (codex catch):** stage 1 does not know
`linear_slot_pos` at emit time - the `(blob_idx, blob_local_slot)`
decomposition exists precisely because blob slot prefixes only
materialize in the ordered receiver. Two viable fixes, either is fine:

- (a) **Via N2:** cat-produced metadata carries exact total refs per way
  blob, so `blob_start_slot[]` is computable from the metadata scan
  before stage 1 starts. Cleanest; also deletes the ref-count sidecar.
- (b) **Watermark sync inside stage 1:** workers buffer one blob's
  records (blob-local slots, ~1 MB avg), the ordered receiver publishes
  `blob_start_slot[k]` as it drains, workers add the prefix and flush
  when their blob's watermark clears. Stall bounded by the in-flight
  window (~22 blobs).

Fallback: keep the 12-byte layout behind the same emit/consume traits for
inputs whose bucket_width or total_slots exceed the bit budget (assert
and select at startup; both bounds have 2.5x+ headroom over 2026 planet).

Follow-up in the same arc: **re-try radix sort** (LSD over the high
bytes). The 2026-04-25 revert (`a231017`/`771b3fb`) blamed memory
pressure from data_buf + records + sort_scratch; u64-in-place satisfies
that revert's stated precondition (no separate parsed vec), and 8-byte
scratch is smaller than today's parsed vec alone. Sweep comparison sort
vs radix after the packed layout lands, stage 2 still at 6 workers - one
variable at a time.

Bench isolation: land packing with everything else frozen. Compare stage
1 wall, `s2_bucket_load_ms` split, stage 2 wall, peak anon.

### N2. cat metadata table: per-blob TOC + per-way-blob ref totals (absorbs L1, L2's contract role, and the meta-scan item)

One `pbfhogg cat` extension, three consumers:

- **Exact total refs per way blob** - unlocks N1's linear-slot record
  with no stage-1 synchronization, and deletes the ref-count sidecar
  write/read.
- **File-level blob directory** (frame/data offsets, sizes, kind, id
  bounds, count, tag state, compression codec per blob) - collapses the
  7.3 s serial HeaderWalker meta scan to one pread. For *external* alone
  this is 1 % of wall and would rank last; as the N1 enabler it rides
  along. **But it is worth far more to sparse** (measured 2026-07-14):
  sparse walks all headers four separate times, 107.4 s of a 363.1 s
  europe wall (`notes/altw.md` P4). The frame/kind/count columns listed
  above are exactly what sparse's pass-2 `BlobDescriptor` needs and what
  the existing `build_classify_schedules_split` cannot supply. If N2
  lands, sparse P4's second half is free - coordinate the column set with
  P4 rather than designing it for external's needs alone.
- **Codec field per blob** - prerequisite bookkeeping for any future
  per-blob-codec input experiments (see tier 2), and useful to degrade /
  repack diagnostics generally.

The input contract is already decided: ALTW external only supports
sorted-by-pbfhogg-cat inputs (the `--force` escape was removed; the
command hard-errors without indexdata). So there is no compatibility
question left, only the format design: BlobHeader unknown-field extension
vs trailing sidecar table. The `--inject-prepass` field-5/field-20 layer
(`58743ba`) is the proof that the extension mechanism works end-to-end.

Scope: cat encoder + reader plumbing + ALTW consumption. Moderate.
Payoff: enables N1(a); deletes sidecar round trip; -7 s meta scan.

### N3. Delete the payload tmp round trip: bounded in-RAM handoff (PROMOTED 2026-07-14)

**This item now owns the measured planet streaming constraint.** The
zstd:1 cell (section above) showed the phase is not compression-bound;
once the writer is cheap, decode threads block on payload readiness
(`s4_readiness_wait_ms` 1395 s cum, ~63 s per thread of a 240 s phase).
N3 is what removes that. It stays behind N1/N2 in the ordering only
because N1 shrinks the two largest byte streams and is the bigger
join-side win; if N1 slips, N3 is the next thing to start.

**The round trip is read-dominated 32:1** (`s4_coord_payload_pread_ms`
771.7 s cum vs `s3_tmp_pwrite_ms` 24.1 s cum at `ed5dd6c5`). Frame this
as deleting a *read*, not a write - the write half is already nearly
free, so a design that only defers or batches the pwrite side is
attacking 24 s of cum time and will measure flat.

Stage 3 pwrites 54.0 GB of payloads that stage 4 preads back seconds
later (`s3_worker_tmp_bytes` 54.0 GB), inside the most contended phase.
Way-blob payload production order (slot order) is way-file order - the
same order stage 4 consumes - and the router already holds straddler
payloads in RAM (0.8 GB at planet).

Design constraints (codex-hardened):

- Blob-ordered, byte-bounded RAM queue with producer backpressure
  (512 MB - 1 GB budget), NOT generic spill-on-overflow: while stage 4
  is slower than stage 3 (it is, under zlib:6), naive overflow would
  spill most of the 54 GB and rebuild the current path with more
  machinery.
- Deadlock hazard: 4 stage-3 bucket workers can fill the budget with
  later buckets while stage 4 waits on an earlier blob. Reserve capacity
  for the earliest unpublished blob, or make publication ordered.
- Stage 4 takes ownership of the payload Vec (no copy).
- The straddler state machine needs explicit ordered-publication and
  cancellation tests (AbortOnDrop paths).

Expected effect: -108 GB device traffic per run (real SSD wear win on
every bench), reduced page-cache churn in the streaming phase, and -
per the zstd:1 cell - the readiness stall that actually binds the phase
once compression is not in the way. Under zlib:6 output the wall win may
still be muted; measure under both codecs, and treat the zstd:1 cell as
the one that shows N3's ceiling.

### N4. Stage-2 worker sweep, after N1 (absorbs L13)

`.min(6)` in stage 2 dates from a heavier memory shape. After N1 halves
worker RAM, sweep 6 / 8 / 10 / 12. Cautions: 3.7 avg cores today points
at I/O stalls, so more workers may just queue on the device (especially
pre-N6); watch `s2_slot_flush_lock_wait_ms`, peak aggregate worker
memory (bucket skew makes worst-case >> average), and device queue
depth. On one drive expect modest gains at best; re-sweep after N6.
The stage-3 `.min(4)` and stage-1 fd-capped counts are separate knobs;
leave them unless a specific counter implicates them.

### N5. Second-NVMe scratch split (absorbs L14/L15; hardware-gated)

Unblocked in principle by the second-drive plan; still gated on the
drive actually matching 990-PRO class (the 2026-04-22 probe regressed
+30 % on the slower 970 EVO Plus - verify raw throughput before
benching). Not pure config: `ScratchDir` is single-rooted, so this needs
a small categorized-scratch change (id shards / slot buckets / payload
tmps routed independently).

Map by measured per-phase byte flows, not by category aesthetics. Stage
2 alone runs three concurrent streams (shard reads ~99-149 GB, node
preads 62 GB, slot writes 149 GB); streaming runs input reads, payload
traffic (if N3 has not landed), and output writes. Re-derive the mapping
after N3 changes the streaming profile. Also revisit where bench OUTPUT
lives (apply-changes measured -31 % planet just moving output to a
second device).

### N6. Scratch page-cache discipline: O_DIRECT / write-side eviction probe (new, gated)

The two 149 GB scratch streams are written once and read once; the
working set can never fit 26 GB of RAM, so their page-cache residency is
mostly pure churn against input-blob cache. Read-side DONTNEED already
exists (shards, slot buckets, under the linux-direct-io feature). The
open question is the write side.

Cautions (why this is a probe, not a plan): dirty pages cannot be
dropped - eager eviction forces writeback and can serialize the
producer; the tail of stage 1's writes IS useful cache for stage 2's
first reads (same for stage 2 -> 3); O_DIRECT across 5,632 shard files
needs aligned buffers and tail handling. Collect device writeback +
cache-hit evidence first; implement as a scratch-only I/O backend, not
coupled to the input `--direct-io` flag. N3 and N5 may shrink the
problem enough that this dies quietly - check again after they land.

### N7. Gate the closure/pins work on `--inject-prepass` (small, free)

**Scope: EXTERNAL ONLY.** `closure_slots` staging lives solely in
`external/stage1.rs`; the sparse backend has no closure staging. Do not
cite N7 as a suspect for sparse regressions - `notes/altw.md`'s japan
flag briefly did on 2026-07-14 and it was retracted the same day. Sparse
has its *own* unconditional-prepass-cost bug (an ungated
`referenced.get` per ref in the pass-0 serial union); same shape, same
root cause in the prepass landings, different code path, tracked there.

Stage 1 computes `closed = refs.first() == refs.last()` and stages a
per-blob `closure_slots: Vec<bool>` (12.4 B pushes per planet run)
whether or not prepass injection is on; the flag is only ever consumed
by the stage-2 pin logic, which is gated. Thread `inject_prepass`
(already a parameter, currently unused in pass A) into the emission loop
and skip the staging + flag OR on the plain path. Small but strictly
free; belongs bundled with the next stage-1 touch (N1).

Worth doing as a pair with the sparse fix: the prepass landings
(`58743ba` / `29e4eab`) added an ungated cost to the plain path of
**both** backends, which is a pattern, not two coincidences. Whatever
review missed it once missed it twice.

---

## Tier 2: speculative, worth a measurement

### S1. Selective per-blob input codec (zstd:1 way blobs) - input-side contract, NOT an output knob

Distinct from the settled output-compression axis: this changes what
`cat` writes into the **internal indexed intermediate** that only
pbfhogg consumes. Input decompress today costs 1068 s cum across stages
1/2/4 (`s1a_decompress` 153 + `s2_node_decompress` 196 + `s4_decompress`
719); zstd:1 decode is ~3-5x cheaper, and in the compression-saturated
streaming phase every freed core-second feeds the actual bottleneck.

The hazard that shapes the design (codex catch): stage 4 raw-passthrough
copies compressed frames verbatim (relations always; node blobs when
`keep_untagged_nodes`). A blanket-zstd input would leak zstd blobs into
a nominally-zlib output - ecosystem violation. Policy that works:

- Way blobs zstd:1 (always decoded + rewritten; they are the stage-1/2
  decompress cost).
- Relation blobs zlib (always passed through).
- Node blobs per mode (zlib if any supported mode passes them through).

Needs: cat transcode support (not just reframe), N2's codec field for
bookkeeping, and an output-purity assertion (every emitted data blob is
zlib). Measure way-only first: stage-specific decompress counters,
stage walls, `writer_compress_ns`, input file size delta (~+10-15 % for
the way section).

### S2. A3 / wire-format DenseNodes filter (DEMOTED 2026-07-14 - its gate was falsified)

Mechanism unchanged: the non-way stage-4 path decodes +
BlockBuilder-rebuilds all 32,835 node blobs (`s4_nonway_assemble_ms`
865 s cum at `ed5dd6c5`) because the tag-based blob skip never fires at
planet. A wire-level DenseNodes filter (preserve string table wholesale,
decode columns in lockstep, keep tagged/member nodes, fresh
indexdata/tagdata) removes most of that CPU, and the CPU win is real
(-53 % nonway assemble at europe, `4910fd9`).

**The gate that kept this alive is gone.** The old reasoning was
"invisible under zlib:6, but a wall item for zstd:1-output users, and
for the default path if the compression ceiling ever moves." The zstd:1
planet cell says the ceiling is not compression at all - so under zstd:1
the phase is bound by payload readiness, and S2's freed assemble CPU has
nowhere to go there either. **S2 does not become a wall item under
either codec at planet today.** It is not "waiting for the ceiling to
move"; the ceiling it was waiting on does not exist.

Reopen condition, narrowed: only after N3 removes the payload-readiness
stall, and only if a fresh planet measurement then shows stage-4 CPU
binding the phase. Do not start on the strength of the europe CPU number
alone - that is the regime-transfer trap twice over.

### S3. Coord payload format: shared-base encoding (absorbs L10/L11)

The 1.81x delta-varint format leaves absolute first-coords as 5-byte
varints; a shared per-way base could shave ~10-15 % off the 54 GB
payload stream (avg 10 refs/way means 2 absolute values per ~50 payload
bytes). Post-N3 the stream may live mostly in RAM, which halves the
motivation - re-evaluate after N3. The 7-byte-coord variant (old L11)
stays subsumed.

### S4. Offline block-compression sample of scratch streams (new, desk-cheap)

Before any scratch-compression engineering: dump a few real 256 KB id-
and slot-bucket buffers from a europe run and measure zstd:1 / lz4 ratio
+ throughput offline. The slot stream (random slot key + correlated
coordinates) might compress meaningfully; the id stream probably less
after N1 packs it. If the ratio is weak, the idea dies for the cost of a
script; if strong (>1.5x at multi-GB/s), independently-compressed blocks
could cut both 149 GB streams without touching the scatter algorithm.
History lesson 6 (scratch reread vs zlib decompress) does not apply
directly - this compresses streams that already exist rather than
trading a decompress pass for a reread - but the same measurement-first
discipline does.

### S5. L5 boundary-blob cache (kept, now quantified: small)

510 straddler node blobs are decoded twice at planet (~255 pairs,
`s2_node_straddler_blobs`); against 33,090 total stage-2 blob decodes
that is ~1.5 % of `s2_node_decompress_ms` (196 s cum) - roughly 3 s cum,
sub-second wall. A contiguous bucket assignment or one-blob decode cache
would recover it. Park unless someone is already inside the stage-2
dispatch loop.

### S6. L6 consolidated shard writers (kept as fallback)

Stage 1 holds 22 workers x 256 = 5,632 BufWriters (~1.4 GB buffer RAM).
Consolidating to 256 shared writers with batched flush trades that for
lock traffic. N1 shrinks the byte volume through these writers by a
third, which weakens the case further. Fallback territory only.

---

## Tier 3: hardware-gated

- **Blob-group downstream rewrite** (old L14 core): still shelved. The
  measured design tax (+3.6 % zlib:6, +9.4 % none at europe, commit
  chain `1ef0474`..`80ed3d7`) was record-width + read-amplification,
  not only drive asymmetry. A second fast NVMe re-opens only the
  cross-disk half of the idea (which is N5 without the rewrite).
- **L17 per-blob accumulators**: dead on 26 GB RAM (~100 GB working
  set). Reopens only on a much larger host.
- **L16 per-epoch u32 slot stream**: last-resort retry shape for the
  stage-2/3 seam; its own plan projected ~14 s at planet - inside noise.
  Keep only as the documented retry form.

## Tier 4: deep stretch (unchanged)

- **L18 single-decode node path** (stage 2 + stage 4 decode fusion):
  scheduler rewrite, medium-low conviction. The stage-4 node decode is
  718 s cum decompress + 933 s cum assemble, so the prize is real, but
  order mismatch (id-bucket vs file order) makes it a full redesign.
  Revisit only if S2 lands and the ceiling moves.
- **L19 overlap stages 1 and 4**: enormous complexity, not pre-1.0 work.
- **L20 io_uring small-write machinery**: only if some future shape
  creates many small concurrent writes; nothing current qualifies.

---

## Disposition ledger (previous edition -> this one)

| Old | Verdict | Where |
|---|---|---|
| L1 BlobHeader refcounts | absorbed, upgraded to enabler | N2 |
| L2 header node-ID lists | **retired** - attacked decompress+scan (~12 s wall post-A1) while stage 1 is emit/write-bound (1009 s cum); its side table would add ~30 GB of reads | ledger only |
| L3 rankless rewrite | landed (A1) | history doc |
| L4 segmented grouped emission | **retired** - its target counter no longer exists post-A1, and N1's cheap u64 sort removes the motivation | ledger only |
| L5 straddler cache | kept, quantified small | S5 |
| L6 consolidated writers | kept, fallback | S6 |
| L7/A2 output executor | stays shelved - milestone 1 measured on this host: pool consolidation lost 30 s planet; permit pool (64) is not the cap, compression CPU is | history doc |
| L8 zstd:1 output note | settled + documented (README, performance.md); removed as a lever | do-not-retry |
| L9/A3 DenseNodes filter | **demoted 2026-07-14** - its ceiling gate was falsified by the planet zstd:1 cell | S2 |
| L10/L11 payload format | kept, weakened by N3 | S3 |
| L12 presence-bitmap carry-along | still a carry-along: any stage-2 inner-loop reshape (N1) may carry a presence bit for free; do not land alone | note here |
| L13 worker caps | absorbed | N4 |
| L14/L15 cross-disk | absorbed / unblocked-with-gate | N5, tier 3 |
| L16/L17 | unchanged | tier 3 |
| L18/L19/L20 | unchanged | tier 4 |

## Do not re-try on this hardware

Preserved as negative results so these do not get re-proposed:

- **Output-compression knob sweeps (zlib levels, zstd presets) as a
  generic lever.** Swept repeatedly across the project's history:
  write-path plan item 2 measured pipelined mode flat across zlib/zstd
  levels at Denmark/Japan (~2.3 % spread); ALTW europe compression axis
  measured `none` -8.9 % / `zstd:1` -13.9 % (2026-04-27, `4fc8e35`);
  zstd:1 guidance for closed pipelines is already shipped documentation.
  The zlib:6 default is an ecosystem contract, not a tuning oversight.
  *The sole reopen condition is now SPENT (2026-07-14, `0e9d93cc`):* the
  one allowed planet cell ran, and the answer was that planet streaming
  is not compression-bound at all (-80 % compression CPU for <=-5 % wall;
  see the section above). The axis is closed in both directions - zstd:1
  is not a planet lever, and no further cell is licensed. Any future
  reopen needs a *new* reason, not this one.
  libdeflate-at-planet is owned by
  `notes/write-path-optimization-plan.md` item 2b; if it lands there,
  re-measure the ALTW streaming phase, do not fork the work here.
- #1 epoch-spill with the 16-byte spill record (2026-04-21, +10 %
  planet). Any retry must use the L16 12-byte form.
- #3 scratch-spool in any buffered/varint form (two attempts). zlib-rs
  decompresses way blobs faster than scratch rereads.
- #5 per-blob accumulators at 26 GB RAM (~100 GB working set).
- In-RAM coord table (altw_v2; OOM at europe).
- Rank-bucket counts beyond 256 (flat-to-regressive at 384/512).
- Stage-2 `rank_if_set` micro-opts (attribution shuffle; the loop no
  longer exists post-A1 anyway).
- pwritev for stage 3's contiguous-buffer shape.
- Non-way wire filter measured under zlib:6 output only (writer ceiling
  eats it; that is S2's gating, not a reason to re-measure blind).
- 12-byte ResolvedEntry shrink attempts: coordinates need 64 bits and a
  bucket-local slot ~26 more; 12 bytes is the fixed-width floor
  (re-derived independently 2026-07-13).

## Correctness invariants

Any stage 1-4 edit must preserve these or explicitly replace them.

- **Sorted + indexed cat-produced input.** `external_join` requires
  `Sort.Type_then_ID` and indexdata; `--force` is rejected on this path.
  The single-pass merge and all bucket range math depend on it.
- **Node-ID bucket coverage.** Every in-range referenced node id maps to
  exactly one bucket (`BucketLayout::locate`); ids above
  indexdata-derived `max_node_id` and negative refs return `None` and
  the slot stays unresolved (zero coords + `missing_locations`), matching
  `missing_node_refs_get_zero_coordinates`. Silent truncation of
  `local_node_id` is forbidden - N1's packed layout must keep the
  explicit width asserts.
- **Merge-walk upper bound is the decoded stream, not indexdata.**
  Stage 2 bounds record consumption by the last actually-decoded node id
  per blob, so loose indexdata cannot orphan records that a later blob
  resolves. Keep this under any stage-2 rewrite.
- **CLOSURE_FLAG is metadata, never identity.** Sort keys and id
  comparisons must mask with `LOCAL_ID_MASK`; the flag marks only the
  trailing ref of a closed ring. In N1's u64 layout the flag must sit
  below the slot bits so masking stays a single AND and equal-id runs
  stay adjacent.
- **Pin-bit lat packing bound.** Under `--inject-prepass`, stage 2 emits
  `(lat << 1) | pin`; this is lossless only because |decimicro lat| <=
  900,000,000 < 2^30. Any coordinate-width change must revisit the
  packing. Stage-3 unpack (`encode_blob_payload_from_record`) and
  stage-2 pack must stay exactly symmetric.
- **Prepass field order and parity.** Way payload field order is 9, 10,
  20 in both backends so sparse and external produce byte-identical way
  bodies; field-20 bitmaps are full ceil(refs/8) width; field-5 payloads
  are versioned (`WayMembers-v1`).
- **2-piece straddler invariant.** A way blob's slot range spans at most
  two adjacent slot buckets; `slot_bucket_count` is derived so every
  bucket width >= `max_blob_slots` (floor-division shape in
  `slot_bucket_bounds` / `ResolvedEntry::slot_bucket`).
- **Zero-coord sentinel.** Stage 3's dense scatter buffers are
  zero-filled; `(0,0)` means unresolved (Null Island ambiguity accepted,
  see CORRECTNESS.md). Any redesign that stops zeroing must add an
  explicit presence signal (L12 carry-along).
- **Per-way refcount ordering.** Refcount data (sidecar today, N2
  metadata tomorrow) is produced and consumed in PBF blob order; stage-4
  reframe trusts it for payload framing and validates way counts per
  blob, trailing bytes, and full payload consumption. Keep those
  fail-loud checks.
- **Straddler state machine.** Exhaustive None -> Left|Right -> Both;
  duplicate halves error; producer-done with an empty slot is a
  deterministic error; AbortOnDrop wakes the other side on panic. N3
  must preserve all four properties in the RAM-queue form.
- **Output codec purity.** Every emitted OsmData blob is zlib unless the
  user passed `--compression`; raw passthrough must never leak a
  different input codec into the output (binding constraint on S1).

## Implementation conventions

Load-bearing patterns; apply to any lead above.

- **Ns accumulators for per-item timing** (`AtomicU64` nanoseconds,
  convert at emit). Blob-local accumulation, one publish per blob - the
  per-way atomics were a measured cost at 1.16 B ways.
- **ReorderBuffer for parallel producer -> serialized consumer.** Used by
  stage 1 pass A receiver and the stage-4 consumer. Reuse, do not
  reinvent - including for N1(b)'s watermark and N3's ordered queue.
- **ScratchDir for all temp files**; lifetime-tied cleanup. N5 will
  extend it with categorized roots - keep the drop-cleanup semantics per
  root.
- **`#[hotpath::measure]` on functions > 1 ms wall**, annotate inner
  helpers, return `ExitCode` from main (a3795c2) so reports flush.
- **Pread-only header walks** via `HeaderWalker` (used by the meta scan);
  N2's TOC supersedes walking, but the walker remains the fallback for
  non-TOC inputs.
- **Worker-count convention**: `available_parallelism - 2, max 1`, with
  per-stage caps (`min(6)` stage 2, `min(4)` stage 3, uncapped stage 4
  decode). N4 owns changing them; justify any other deviation.
- **Counter naming** `s<stage><phase>_<thing>_ms|_bytes|_calls`; emit
  min/max/count balance diagnostics for partitioned work.
- **Prototype discipline.** Full coherent branch rewrites with
  keep/revert benching; no env-var probe farms. One variable per bench
  cell (N1 before N4; N4 before N5 re-sweep).
- **Fail-loud validation at seams.** Shard length multiples
  (`prepare_bucket`), sidecar way counts, payload full-consumption,
  router duplicate-publication - every new seam (N1 record width, N2
  metadata, N3 queue) gets the same class of checks.

## Suggested ordering

1. ~~Settle the drift question~~ **DONE 2026-07-14: drive state, not
   code.** Keep/revert deltas are trustworthy again, subject to the
   ~5-8 % planet band and the order-swap requirement in the drift
   verdict section.
2. **N2 + N1 together** (metadata enabler, packed u64 record, N7 riding
   along), stage 2 frozen at 6 workers. This is the biggest join-side
   change and it shrinks the two largest byte streams. Keep/revert
   comparand: europe `cdfa9453` 285.7 s bench 3, with hotpath
   `2db37ded` / alloc `a6806e34` at the same commit.
3. **Sort algorithm cell**: comparison vs radix on packed records.
4. **N3** payload RAM handoff - moved up: it owns the measured planet
   streaming constraint now that the compression ceiling is disproved.
5. **N4** stage-2 worker sweep.
6. **N5** scratch split when the second drive is real (re-sweep N4
   after).
7. **S1 / S4** as measurement budget allows. **S2 is not on this list
   any more** - its gate was falsified; see S2 for the narrowed reopen
   condition (post-N3 measurement only).
