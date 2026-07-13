# add-locations-to-ways: sparse path

`pbfhogg add-locations-to-ways --index-type sparse` (the default index
type). The planet-scale backend, `external`, is documented in
[`notes/altw-external.md`](altw-external.md) and its optimization arc in
[`notes/altw-optimization-history.md`](altw-optimization-history.md);
this file is sparse only. `dense` was removed at `b70dd8c` - see
"Don't re-attempt".

Rewritten 2026-07-13 from a full code re-read (post-prepass, post
dense-removal) and an external design critique (codex gpt-5.6-sol at
xhigh; transcript
`~/.codex/sessions/2026/07/13/rollout-2026-07-13T11-57-06-*.jsonl`).
The previous edition's open items are dispositioned in the ledger near
the bottom; the landed-work and findings records are preserved - they
are this path's negative-results history.

Code:

- [`src/commands/altw/mod.rs`](../src/commands/altw/mod.rs) - dispatch
  + pass 0 + rel-member scan + pass 2 decode-all fallback +
  `inject_metrics`.
- [`src/commands/altw/sparse.rs`](../src/commands/altw/sparse.rs) -
  rank-indexed flat coordinate store (pass 1).
- [`src/commands/altw/passthrough.rs`](../src/commands/altw/passthrough.rs)
  - descriptor-first pass 2 (indexed input).
- [`src/commands/altw/reframe.rs`](../src/commands/altw/reframe.rs) -
  wire-format way reframe shared with the prepass field-5/20 builders.

## Phases

1. **Pass 0** (`collect_way_referenced_node_ids`) -
   `parallel_scan_blobs_raw` wire-only way scan; workers emit per-blob
   `Vec<i64>` of refs, the **main thread serially unions** them into
   one `IdSet`. Closed-ring trailing refs are trimmed before emission.
   Under `--inject-prepass` the union also detects shared nodes
   (already-set ids go into a second `shared` IdSet).
2. **Pass 1** (`build_node_index_sparse`) - rank-indexed flat store:
   `build_rank_index()`, `set_len(referenced_count * 8)` temp file,
   `MmapMut`; workers extract node tuples wire-only and store packed
   `(lat, lon)` via relaxed `AtomicU64` at byte offset
   `rank_if_set(id) * 8`. (These are mmap atomic stores, NOT pwrites -
   the distinction is load-bearing for the P5 evaluation below.)
3. **Rel-member scan** (`collect_relation_member_ids`) - fires when
   `keep_untagged_nodes=false` or `--inject-prepass`; shared-IdSet
   `parallel_classify_phase`; prepass additionally collects
   multipolygon/boundary member-way ids.
4. **Pass 2** - `write_output_passthrough` (indexed): descriptor-first
   pipeline, way blobs through the wire reframe (+ field-20 pins and
   field-5 WayMembers under prepass), node blobs through
   PrimitiveBlock + BlockBuilder filtering, relations passthrough.
   `write_output_decode_all` for `--force` non-indexed input.

## Baselines (plantasjen)

| Scale | Wall | UUID | Commit | Date | Note |
|---|---:|---|---|---|---|
| denmark | 4.0 s | `7bd88e83` | `6d6e158` | 2026-07-10 | default sparse |
| japan | 11.9 s | `158a86d7`-era | `c6f08ff` | 2026-04-30 | pre-prepass era |
| europe | 359.7 s (5:59) | `f9a61784` | `c6f08ff` | 2026-04-30 | pre-prepass era; refresh queued in tonight's suite |
| planet | untried | - | - | - | ~60 GB working set vs ~25 GB cache; expected thrash. External owns planet. |

Europe phase profile (`f9a61784`): pass 0 63.2 s, pass 1 57.7 s
(29 GB store written, avg cores 11.6), rel-member 1.2 s, pass 2
197.0 s (6.8 M majflt, 251 GB disk read, avg cores 13.9).

Sparse-vs-external at europe: sparse 359.7 s vs external 270.8 s
(`0b89f986`, both April baselines; tonight's suite re-pins both at
HEAD). At denmark and japan the ordering inverts hard - see P1.

## Findings

### The working set is the ceiling, and it cannot shrink losslessly

The store is 8 bytes per referenced node (`altw_referenced_node_ids`:
denmark 49 M, japan 299 M, europe 3.62 B, planet ~2x europe).
Coordinates need 63 bits (lat 31 + lon 32), so there is no lossless
encoding below 8 bytes/node; frame-of-reference tricks buy at best
~25 % for real complexity and an escape path. Europe's 29 GB sits just
above the ~25 GB cache budget - survivable (bounded fault rate);
planet's ~60 GB is architecturally out of reach on this host. External
owns planet; sparse owns small-to-medium and unsorted inputs. This
framing is settled.

### Sparse pass 2 is global-locality-bound at scale

Way refs scatter uniformly over the id space, so each blob's lookups
touch the whole store; with cache < store, the fault count is set by
"cache vs working set" and sorting only reorders the faults. Measured
twice (chunk-format OOM; per-block sorted resolve identical kill
point). Full record preserved below in the history sections.

### NEW 2026-07-13: europe pass 2 reads ~37 KB per major fault

251 GB disk read / 6.8 M majflt is ~37 KB per fault against a store
where a lookup needs one 4 KB page. That ratio points at mmap
readahead amplification on a random access pattern - the kernel is
speculating adjacent pages that mostly miss. This motivates the P3
`MADV_RANDOM` probe. Counter-signal to respect: external's stage-4
history measured `MADV_RANDOM` as *worse* on its 37 GB coord mmap
(killed useful readahead, majflt 374 K -> 9.2 M) - but that regime had
6 workers streaming semi-ordered payloads; sparse pass 2 is genuinely
random per ref. One cell decides it.

### NEW 2026-07-13: `--index-type auto` is scale-blind and misroutes small inputs

`auto` = external whenever sorted + indexed. Measured walls say that
rule is wrong below europe scale: denmark sparse 5.8 s vs external
12.3 s; japan sparse 11.9 s vs external several-x slower (external's
fixed scratch round trips dwarf japan-sized inputs). External wins
from somewhere between japan (2.4 GB) and europe (33.6 GB). The
sparse-selection hint text made the same false "at every scale" claim
(fixed 2026-07-13). P1 fixes the routing.

### Pass 0's serial union is the phase

Workers scan wire-only and emit per-blob ref vectors; the main thread
performs ~4.7 B `IdSet::set` calls single-threaded at europe. At
~5-10 ns per call that is most of the 63 s phase wall. P2 attacks
this. (Prepass doubles the serial work - a `get` before every `set`
for shared-node detection - so the win compounds under
`--inject-prepass`.)

### Pass 2 floor at zlib:6 is compression CPU (hotpath UUID `aa4fe496`)

Japan sparse pass 2: `frame_blob_into` (compress + frame) is ~10 cores
of the wall; decode work is ~3.4 cores. Compression sweep at japan:
zlib:6 11.9 s, none 9.7 s (-18 %), zstd:1 8.9 s (-25 %). **The
compression axis is settled - do not re-propose output-compression
knob sweeps.** zstd:1 for closed pipelines is documented in README.md
/ `reference/performance.md`; the writer-ceiling diagnostic (measure
any pass-2 CPU item under both zlib:6 and zstd:1/none) is the
operative lesson and is shared with the external doc.

### Rel-member scan IdSet bloat (fixed `66cfa4a`)

Per-worker IdSet accumulate peaked +9.7 GB anon at europe (would have
been ~72 GB at planet). Migrated to shared-IdSet
`parallel_classify_phase`: japan scan 4.2 s -> 0.76 s, peak anon
4.3 GB -> 0.82 GB.

### What is NOT the bottleneck

The `par_iter().map_init(BlockBuilder).collect()` shape. Measured
repeatedly; the ceilings are index-access-pattern and compression, not
the rayon-collect pattern. Shape != diagnosis.

## Prepass (`--inject-prepass`) on this path

Landed `58743ba` + `29e4eab` (2026-07). Producer-side metadata for
downstream geometry consumers, byte-parity with the external backend:

- Pass 0 detects shared nodes (ids referenced from >= 2 non-closure
  positions) into a `shared` IdSet during the union.
- Rel-member scan also collects multipolygon/boundary member-way ids.
- Way reframe emits field 20 (SharedNodePins-v1 bitmap, full
  ceil(refs/8) width) and per-blob field-5 WayMembers-v1 payloads;
  field order 9, 10, 20 matches external so both backends produce
  byte-identical way bodies.
- `decode_one` hard-errors if a Way appears in a non-way-classified
  blob under prepass (would silently break the WayMembers superset).
- Diagnostics via `inject_metrics` counters (`altw_pinned_refs`,
  `altw_field20_ways_emitted`, `altw_field5_bytes`,
  `altw_member_ways`).

Cost when the flag is off: closure-ref trimming in pass 0 (cheap);
the shared-detection `get` is gated. No pass-1/2 cost.

## Instrumentation added 2026-07-13 (lands in every subsequent sidecar)

- `altw_pass0_union_ms` / `altw_pass0_refs_total` - main-thread serial
  union time and total (with-duplicates) ref count; the direct
  measurement P2 needs instead of inference. `altw_pass0_shared_node_ids`
  under prepass.
- `altw_pass1_rank_index_ms` / `_store_bytes` / `_tuples_scanned` /
  `_coords_stored` / `_rank_span_slots` - pass 1 was previously
  counter-free. `rank_span_slots / coords_stored` is the P5
  write-amplification gate.
- `altw_pass2_*` worker splits (`pread_ms`, `decompress_ms`,
  `way_reframe_ms`, `nonway_ms`, `send_ms`, `bytes_read`, blob counts
  by kind), consumer splits (`consumer_recv_ms`, `consumer_write_ms`,
  `passthrough_pread_ms` / `_bytes` / `_blobs`), and schedule
  composition (`decode_items`, `passthrough_items`, per-kind blob
  counts, `decode_threads`). Writer-side compression was already
  covered by the shared `writer_*` counters.
- `WAIT_P2_SEND` stall span (decode workers blocked on the consumer
  channel), via the depth-gated StallGauge shared with external,
  entered only after a failed try_send.
- `altw_pass2_blobs_dispatched` now emits every 64 blobs instead of
  every blob (was ~22 K FIFO writes at europe), plus a final total.
- `#[hotpath::measure]` added on `decode_one`, `build_schedule`, and
  `reframe.rs::reframe_way_blob_with_locations`, so sparse hotpath
  runs attribute below the phase wrappers (per-way helpers stay
  unannotated per the stripped-timer lesson).

---

## The queue (2026-07-13)

### P1. Scale-aware `auto` routing (user-facing defect; top item)

Route `auto` on input size: external only when sorted + indexed AND
above a threshold; sparse otherwise. Conservative threshold - routing
a smallish input to external wastes seconds; routing an oversized
input to sparse costs minutes of cache thrash - so bias toward
external near the boundary.

Measurement first (codex-hardened): compressed file size is only a
proxy for the 8-byte-per-referenced-node working set (repack level /
blob size moves file size without moving the store). Run sparse and
external sequentially on **germany** and **north-america** (both
registered datasets, sitting inside the 2.4-33.6 GB gap), capture
total + phase walls, store bytes, pass-2 majflt and disk read, and
set the threshold below the point where sparse pass-2 disk read goes
nonlinear - not merely where the walls cross. Sanity-check against a
repacked variant of the same region.

The eventual clean selector estimates the store from node counts in
blob metadata (P4) instead of file size. Also fixed alongside this
item: the sparse-selection hint text no longer claims external wins
at every scale (corrected 2026-07-13).

### P2. Pass-0 parallel union via `set_atomic_if_new`

Replace the serial main-thread union with worker-side atomic bit
sets. Shared-node detection stays exactly correct:
`if !referenced.set_atomic_if_new(id) { shared.set_atomic(id) }` -
the fetch_or linearizes, exactly one occurrence observes "new", every
other lands in `shared`. Closure-ref trimming stays where it is
(before emission).

Constraints (codex catch): `IdSet::pre_allocate` needs an upper id
bound before workers start and allocates every chunk through that
bound. Shape: indexed input derives the bound from the largest node
blob `max_id`; refs above the bound fall back to the existing
per-blob vectors and a tiny serial merge after the join (preserves
dangling-ref behaviour); unindexed input keeps the serial union.
`shared` needs the same pre-allocation under prepass.

Ceiling: the whole 63 s europe phase; realistic win is tens of
seconds (4.7 B locked byte-RMWs do not scale linearly to 22 cores;
hot shared nodes cause coherence traffic). Japan expect flat (2.5 s
phase). Measure before building: worker result-channel stall time on
the current shape, then A/B pass-0 wall, avg cores, chunk allocation
count, peak anon, and exact set-equality against the serial
implementation (closure ways, repeated refs, dangling high refs,
corrupt-indexdata fixtures).

### P3. `MADV_RANDOM` probe on the pass-2 coord mmap (one cell)

One `madvise` call on the sparse store before pass 2, one europe
bench. Justified by the 37 KB-per-majflt readahead signal above;
risked by external's opposite result in a different regime. If wall
improves or disk read collapses without wall regression, keep; if it
reproduces external's readahead-loss regression, record and close.
Aimed directly at the 197 s dominant phase - highest
information-per-engineering-minute item on this list.

### P4. Shared blob-metadata plan (Reviewer 4's item, upgraded to enabler)

One header walk (or, later, one cat-TOC pread - external doc N2)
feeding: auto selection (node counts for P1's real selector), pass 0
(max-id bound for P2), pass 1 schedule, rel-member scan, and pass 2
descriptors. Today these are four separate walks/schedule builds.
Converges with external N2; whichever lands first should carry the
other's requirements.

### P5. Pass-1 per-blob buffered pwrite (demoted; gated on a counter)

The idea: on sorted inputs each node blob's referenced nodes occupy
one contiguous rank interval, so a worker could scatter locally and
issue one ~2 MB pwrite per blob instead of 29 GB of dirty mmap
stores. Codex's critique demotes it: buffered pwrite dirties the same
page cache (only O_DIRECT would not, and that leaves pass 2 cold);
the current mmap stores on sorted input are already near-sequential
(file-order dispatch, ascending ids, bounded worker window - the
write frontier is tens of blobs wide); and the pwrite path adds
zero-fill + userspace scatter + kernel copy that atomic stores do not
pay. Correctness landmine: zero-filled spans could clobber a
neighbour blob's coords if id ranges overlap - the fast path would
need a range-disjointness proof (P4 metadata), not just the sorted
header flag.

Gate: add per-blob rank-span/resolved/hole counters first and compute
`sum(rank_span_bytes) / resolved_bytes`. If materially above 1.0 the
path has write amplification before it starts. Only A/B at europe if
the ratio is clean AND P2 has landed. Expected direction per codex:
uncertain, flat-or-worse plausible.

### Parked (unchanged from previous edition, reasoning intact)

- **Encoding shrink for planet sparse**: no lossless path below
  8 B/node; external owns planet. Reopen only with a real workload
  need external cannot serve.
- **Per-batch parallel resolve**: dead for the working-set reason;
  see also the merge-join arithmetic below.
- **Untagged-node partial wire edit (pass 2 node path)**: real CPU,
  invisible under the zlib:6 writer ceiling; same gating as external
  S2. Re-measure only with a ceiling story.
- **Wire-only rel-member scanner**: target shrank to 1.2 s at europe
  after `66cfa4a`; not worth the surface.
- **Per-blob dedup before union**: global dup ratio is only 1.30
  (4.7 B refs / 3.62 B unique), so perfect global dedup saves <= 23 %
  of set calls and per-blob dedup saves less. Measure the per-blob
  unique ratio before ever pursuing; P2 likely obsoletes it.

---

## Measured history

### Arc 1 (2026-04-29, `68806b0` -> `8e0cef9`): parallel pass 1, inline pass-2 lookups

| Dataset | Mode | `68806b0` (pre) | `29683ee` (parallel pass 1) | `8e0cef9` (inline pass 2) |
|---------|------|-----------------|------------------------------|----------------------------|
| Denmark | dense | 11.9 s | - | - |
| Denmark | sparse | 17.3 s | 15.6 s | **5.8 s** |
| Japan | dense | 51.6 s | - | - |
| Japan | sparse | 78.4 s | 71.7 s | **20.9 s** |

Inline `NodeIndex::get` (`8e0cef9`) removed the serial
`resolve_batch_locations` pre-pass that capped pass 2 at ~4 cores;
japan went from 1.5x slower than dense to 2.5x faster.

### Arc 2 (2026-04-30, `66cfa4a` -> `c6f08ff`): reviewer plan + rank-indexed flat

| Dataset | Mode | `8e0cef9` | `e63d0b6` (5-item) | `c6f08ff` (rank flat) |
|---------|------|-----------|---------------------|------------------------|
| Japan | sparse | 20.9 s | 14.3 s | 11.9 s |
| Europe | sparse | OOM at 9:56 | not measured | **5:59** |

Landed in arc 2, condensed (full measurements in git history of this
doc):

- **Rel-member shared-IdSet migration** (`66cfa4a`): japan scan
  4.2 -> 0.76 s, peak anon 4.3 -> 0.82 GB.
- **Pass 0 wire-only scan** (`87f53eb`): `parallel_scan_blobs_raw` +
  `scan_way_refs`; per-blob CPU dropped (cores 5.3 -> 4.5 at flat
  wall - workers now idle on the dispatcher).
- **Wire-format way reframe** (`cb31654`): lifted from external stage
  4 into `reframe.rs`; pass 2 7.9 -> 7.5 s at japan (writer-bound;
  the CPU win is real but absorbed at zlib:6).
- **`to_path_parallel` writer** (`7169216`): invisible at japan
  (below the write ceiling); reserved for large outputs.
- **Descriptor-first pass 2** (`e63d0b6`): read/decode/write overlap;
  wall flat at japan (already CPU-saturated); the shape win is
  reserved for larger scales. `copy_file_range` deliberately not
  pursued (stage 4 lives without it).
- **Rank-indexed flat pass 1** (`c6f08ff`): chunk format + serial
  consumer deleted; japan pass 1 3.45 -> 0.82 s (21.1 avg cores);
  disk 2.4-2.8x smaller; strictly-increasing-id precondition gone
  (random arrival works; each rank slot unique + atomic). First
  attempt used per-tuple pwrite - 10x slower - mmap + AtomicU64
  recovered it. Europe went from OOM-at-9:56 to 5:59 (65 % fewer
  pass-2 majflt, 7x less pass-2 disk read than the chunk format).

Cross-validation throughout: `brokkr verify add-locations-to-ways
--dataset denmark` - all backends byte-identical output.

### Dense removal (`b70dd8c`, cleanup `13eed79`)

Rank-flat sparse dominated dense at every measured scale (japan dense
51.6 s vs sparse 11.9 s; europe dense OOM vs sparse 5:59) and works
where dense could not. The reviewer items that would have "fixed"
dense converged it to exactly the sparse rank-flat shape - same
encoding, same access pattern. `--index-type dense` now returns a
hard parse error with a migration hint.

---

## Don't re-attempt

- **Output-compression knob sweeps.** Settled; japan sweep above,
  europe axis in `reference/performance.md`, zstd:1 guidance shipped.
  Same reopen condition as the external doc (a single planet cell if
  a ceiling-gated item needs a decision number), owned there.
- **`parallel_classify_accumulate` with per-worker IdSet at scale.**
  See the classify.rs choice criteria; the rel-member scan was the
  worked example (+9.7 GB anon at europe).
- **Dense at planet / re-introducing `--index-type dense`.**
  Architectural page-thrash; superseded by sparse rank-flat which is
  the same shape done right.
- **Per-block sorted resolve as a europe pass-2 fix** (`d9edb5f`,
  reverted). Short sorted runs evicted before reuse; identical kill
  point as inline, +20 % japan overhead.
- **Per-tuple pwrite in pass 1** (measured 10x slower than mmap
  stores during the `c6f08ff` work). P5's per-blob buffered variant
  is a different shape but carries its own gate - see P5.
- **Untagged-node skip-entirely.** Zero blobs qualified at japan
  (every node blob has a tagged node or member overlap); also
  writer-ceiling-gated. Re-attempt only at planet scale under
  zstd:1/none, and only if the skip predicate actually fires.
- **Treating shape as the diagnosis.** The rayon-collect pattern was
  twice suspected, twice exonerated by measurement.
- **Batched merge-join as a backend replacement (Reviewer 2,
  declined; arithmetic hardened 2026-07-13).** The design shards
  coords into K sorted files, then per way-batch sorts ~5 M requests
  and forward-scans each shard to resolve. Order-statistic kill: with
  uniform refs, a batch puts ~19.5 K requests into each of 256 shards
  and the largest request lands at ~99.995 % of the shard, so every
  batch reads effectively the entire shard set. Europe: 4.7 B refs /
  5 M per batch = 940 batches x 29 GB = **~27 TB of coordinate
  reads**; planet is tens of TB. Larger K does not help (thousands of
  requests still hit every shard); larger batches just move the RAM
  bound. Escaping the rescan requires globally sorting all requests
  and resolving once - which IS the external architecture with
  per-ref storage as the price. The design is arithmetically dead,
  not merely unproven. (Reviewer 1's slot/join fold-in is the same
  conclusion from the other direction: it converges to external.)
- **`madvise(WILLNEED)` over sorted ref ranges** (Reviewer 2
  anti-rec): advisory hints cannot fix working set > RAM. Note this
  does NOT close P3's `MADV_RANDOM` probe, which changes readahead
  behaviour rather than pretending to fit the working set.

## Disposition ledger (previous edition -> this one)

| Old item | Verdict | Where |
|---|---|---|
| Remaining work 1: encoding shrink | parked, unchanged | Parked |
| Remaining work 2: per-batch resolve | parked; merge-join arithmetic makes it deader | Parked / don't-re-attempt |
| Reviewer: pass 0 wire-only | landed `87f53eb` | history |
| Reviewer: pass 0 + rel scan concurrent | retired - rel scan is 1.2 s at europe post-`66cfa4a`; the overlap buys ~1 s | ledger only |
| Reviewer: dense pass-1 items | obsolete (dense removed) | history |
| Reviewer: sparse pass-1 consumer / rank-flat | landed `c6f08ff` | history |
| Reviewer: rel-member wire-only scanner | retired - target too small | Parked |
| Reviewer: reuse external relation_scan via shared blob plan | absorbed | P4 |
| Reviewer: way reframe | landed `cb31654` | history |
| Reviewer: refs-as-iterator in reframe | retired - reframe is not wall-critical (writer-bound at zlib:6; ~14 % of pass-2 CPU total for the whole non-compress side) | ledger only |
| Reviewer: strip existing fields 9/10 | landed (in reframe.rs; also field 20) | history |
| Reviewer: descriptor-first pipeline | landed `e63d0b6` | history |
| Reviewer: `to_path_parallel` | landed `7169216` | history |
| Reviewer: untagged-node skip / partial wire edit | parked, ceiling-gated | Parked |
| Reviewer: drop per-worker `Vec<OwnedBlock>` | retired - obsoleted by the descriptor pipeline's shape | ledger only |
| Reviewer: shared BlobMeta plan | upgraded to enabler | P4 |
| Reviewer anti-recs (knob tuning, get micro-opt, WILLNEED, ramcheck mode, env-var variants) | all still in force | don't-re-attempt / conventions |
| Reviewer 1/2 planet rewrites | declined; decline now carries the order-statistic arithmetic | don't-re-attempt |
| Doc bug: sparse help text vs sorted precondition | resolved by `c6f08ff` | history |

## Measurement notes

- Pass-2 CPU wins can be invisible under zlib:6 (writer ceiling);
  measure under both zlib:6 and zstd:1/none. Shared lesson with the
  external doc.
- Sparse numbers above predate the 2026-05..07 tree (zlib-rs bump
  `91f1786`, prepass landings, fused transforms, MSRV 1.96). Tonight's
  suite (2026-07-13) re-pins denmark-era numbers at europe; japan is
  not queued and should be re-pinned before any P-item keep/revert
  uses it as a gate.
- The `--inject-prepass` A/B at planet external showed run-order /
  drive-state effects larger than most single-item wins (see the
  external doc's drift section). Sparse europe cells inherit the same
  caution: matched sample counts, rested drive.

## Cross-references

- [`notes/altw-external.md`](altw-external.md) - external backend;
  N2 (cat metadata TOC) is P4's convergence target; S1 (selective
  input codec) would cut sparse decompress too.
- [`notes/altw-optimization-history.md`](altw-optimization-history.md)
  - external's arc; the writer-ceiling and desk-estimate lessons
  cited here live there in full.
- [`src/scan/classify.rs`](../src/scan/classify.rs) -
  `parallel_classify_phase` vs `parallel_classify_accumulate` choice
  criteria (load-bearing).
- Migration template precedents: time-filter snapshot (`83183fb`),
  tags-filter way-deps (`17b116c`), `cat --clean` (`b347c0a`),
  `check --ids` streaming (`516129e`).
