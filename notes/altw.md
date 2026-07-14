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

Each phase builds its own blob schedule via its own header walk. That
looks like an obvious 4x redundancy and it is not worth fixing: the
redundant walks are nearly free whenever the headers are still cached,
and they are only expensive at scales where sparse is the wrong backend
anyway. Built and reverted twice; see P4 before touching it.

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
| japan | **11.3 s** | `13347065` | `add8d03` | 2026-07-14 | current, `--bench 3`. P0 recovered the ~0.9 s prepass regression; at/below the 11.4 s pre-prepass mark. Retires 12.3 s (`26203e64`), 15.5 s (`a3c46737`, cold-walk single sample) and 11.9 s |
| germany | **25.7 s** | `61f6c231` | `dcc445e` | 2026-07-14 | first sparse pin at this scale (P1 cell) |
| north-america | **116.4 s** | `340ba366` | `dcc445e` | 2026-07-14 | first sparse pin at this scale (P1 cell) |
| europe | **363.1 s** | `4ac11326` | `dcc445e` | 2026-07-14 | current; flat vs 359.7 s @ `c6f08ff` (+0.9 %). Post-P0 `89683dda` (`add8d03`) measured 369.8 s - NOT a regression and NOT a new baseline: single `--bench 1` cells 6 h apart, +1.8 % is inside the drift band, and P0 cut europe's pass-0 union -8.3 s on identical refs. Interleave before quoting any europe sparse wall delta |
| planet | untried | - | - | - | ~60 GB working set vs ~25 GB cache; expected thrash. External owns planet. |

Europe hotpath at `dcc445e`: `c790eb34` (310.5 s) - the first sparse
function-level profile below the phase wrappers, and the reference set
for P1-P3 keep/reverts.

Europe phase profile at `dcc445e` (`4ac11326`): pass 0 52.2 s, pass 1
46.6 s (28.94 GB store), rel-member 29.8 s, pass 2 230.5 s. Prior
profile (`f9a61784`, `c6f08ff`): pass 0 63.2 s, pass 1 57.7 s (29 GB,
avg cores 11.6), rel-member 1.2 s, pass 2 197.0 s (6.8 M majflt, 251 GB
disk read, avg cores 13.9).

> **The rel-member "1.2 -> 29.8 s regression" is NOT one** (resolved
> 2026-07-14). 29.3 s of the 29.8 s is the phase's *schedule header
> walk*, not the scan - see P4. The old profile's phases summed to
> 319 s against a 359.7 s wall, so ~40 s was simply unattributed back
> then; the walk was always there. The actual rel-member scan is still
> ~0.5-1 s, consistent with the `66cfa4a` shared-IdSet migration.
> Instrumentation coverage improved, the command did not regress.

Pass-0 anatomy is now exact: 26.6 s schedule walk + 25.5 s serial
union = 52.1 s of the 52.2 s phase. **The worker scan is fully hidden
behind the serial union** - there is essentially nothing else in the
phase. P2 attacks the 25.5 s, P4 attacks the 26.6 s, and together they
are the whole phase.

> The 26.6 s walk is not P4's to reclaim - that was measured and reverted
> twice (see P4). Read this split as the anatomy of one `dcc445e` profile,
> not as a stable property: the walk term is 26.6 s only when that walk
> ran cold, and it cannot be summed with the other phases' walk durations.
> P2 attacking the 25.5 s union is the live half of this paragraph.

> **japan sparse +30 % flag - RESOLVED 2026-07-14. Two causes: ~3.1 s of
> cold-walk artifact, plus a real ~0.9 s pass-0 regression.** The
> `--commit c6f08ff` pair settles it (`26203e64` HEAD 12.3 s vs
> `33e0bb07` c6f08ff 11.4 s, both `--bench 3`, run 14 min apart).
>
> **Cause 1, the artifact (~3.1 s of the 3.6 s):** the flagged 15.5 s
> (`a3c46737`) was a `--bench 1` whose single iteration walked japan's
> headers cold. Today's HEAD `--bench 3` reproduces it exactly in
> iteration 0 and then escapes it:
>
> | HEAD `26203e64` | wall | pass 0 | schedule walks (collapsed) |
> |---|---:|---:|---|
> | run 0 | **15393 ms** | 5238 ms | x3, min 22.6 ms, **max 3087.4 ms** |
> | run 1 | 12231 ms | 2365 ms | x3, min 25.7 ms, max 54.4 ms |
>
> Run 0 is the 15.5 s. One cold walk (3087 ms) against two warm ones
> (22.6 / 54.4 ms) - japan's 2.4 GB file stays cached once touched, so
> only the *first* walk of a session pays. `a3c46737` ran seconds after
> the europe sparse hotpath evicted japan's headers, with no warm
> iteration to win best-of-N. Same artifact as the getparents planet
> "+16.8 % regression" closed the same day (reference/performance.md).
>
> **Cause 2, a real regression (~0.9 s):** comparing warm-walk iterations
> only, HEAD run 1 vs `c6f08ff` run 0:
>
> | phase | HEAD | `c6f08ff` | delta |
> |---|---:|---:|---:|
> | pass 0 | 2365 ms | 1463 ms | **+902 ms** |
> | pass 1 | 899 ms | 921 ms | -22 ms |
> | rel-member | 97 ms | 86 ms | +11 ms |
> | pass 2 | 7380 ms | 7146 ms | +234 ms |
>
> **The entire residual is pass 0** - which is exactly where the ungated
> `referenced.get` lives. Quantified: `altw_pass0_union_ms` 2128 ms over
> `altw_pass0_refs_total` 322.8 M refs = **6.6 ns/ref at HEAD**, against
> `c6f08ff`'s ~1400 ms pass-0 union = **~4.3 ns/ref**. The ungated `get`
> costs **~2.3 ns/ref**, i.e. ~740-900 ms at japan (matching the measured
> +902 ms) and **~10 s at europe's 4.37 B refs** (invisible there under a
> ~±18 s drift band, which is why europe looked flat).
>
> **The N7 suspect first named here was WRONG and is retracted** (code
> read): N7 is an *external* item - `closure_slots` staging exists only
> in `external/stage1.rs` and sparse has no closure staging. The right
> suspect was the sparse-local `get`, and it is now confirmed by
> measurement rather than inference.
>
> **New japan baseline: 12.3 s `--bench 3` (`26203e64`, `fb743f6`).**
> Retire 15.5 s (cold-walk single sample) and 11.9 s (pre-prepass).
> Re-pin japan with a *warm* first walk or the number measures cache
> state - and prefer `--bench 3`, whose best-of-N escapes the cold walk
> by construction.

Sparse-vs-external, all at `dcc445e` (2026-07-14) - see P1 for how to
read this:

| Dataset | pass-1 store | sparse | external | winner |
|---|---:|---:|---:|---|
| japan | ~2 GB | **12.3 s** (`26203e64`, bench 3) | (not pinned) | sparse |
| germany | 3.32 GB | 25.7 s | 25.2 s | tie (external +2 %) |
| north-america | 18.75 GB | **116.4 s** | 158.3 s | **sparse -26 %** |
| europe | 28.94 GB | 363.1 s | **285.7 s** | **external -21 %** |

**Read the margins, not the digits.** Two caveats, one of which matters:

- germany and north-america are `--bench 1` on *both* arms - matched, so
  those rows are fair, though both arms paid a cold first walk.
- **The europe row is NOT matched: sparse 363.1 s is `--bench 1`
  (`4ac11326`) while external 285.7 s is `--bench 3` (`cdfa9453`).**
  Sparse got one cold-walk sample; external got best-of-3. Sparse's four
  walks cost ~107 s at europe (P4), so a `--bench 3` sparse cell would
  land nearer ~337 s. External still wins, but by ~15 % rather than the
  21 % the table shows. **Do not quote the europe margin without
  re-running sparse at `--bench 3`.**

The ordering (sparse wins to north-america, external wins at europe) is
robust to both caveats - the crossover conclusion in P1 stands. The
margins are not.

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

### Pass 0's serial union is half the phase (CORRECTED 2026-07-14)

Workers scan wire-only and emit per-blob ref vectors; the main thread
performs the union single-threaded at europe. `altw_pass0_union_ms` now
measures this directly instead of inferring it: **25.5 s of the 52.2 s
pass-0 wall (49 %)** at `4ac11326`, over `altw_pass0_refs_total`
4.37 B refs (~5.8 ns per set call).

This **corrects the previous edition**, which claimed the union was
"most of the 63 s phase wall" from a desk estimate. It is half, and the
phase is smaller than it was. **P2's ceiling is 25.5 s, not 63 s** -
size the item against that. The other ~26 s is worker scan + dispatch,
which P2 does not touch. (Prepass doubles the serial work - a `get`
before every `set` for shared-node detection - so the win compounds
under `--inject-prepass`.)

This is the desk-estimate lesson from the external doc, reproduced
locally: the inference was off by 2x in the direction that flattered the
proposed item.

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
no pass-1/2 cost. **The shared-detection `get` is NOT gated** - the
previous edition claimed it was, and that was wrong (code read
2026-07-14). `collect_way_referenced_node_ids` runs
`if referenced.get(node_id) { .. } else { referenced.set(node_id) }` on
every ref whether or not `--inject-prepass` is set; only the
`shared.set` inside the taken branch is gated behind
`if let Some(shared)`. The plain path therefore pays a `get` per ref on
the *serial main-thread* union that it did not pay pre-prepass, where
the loop was a bare `set`. See the japan +30 % flag above; the fix is to
split the loop on `shared` rather than branch inside it.

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

### P1. Scale-aware `auto` routing (user-facing defect; top item) - MEASURED 2026-07-14, ready to implement

Route `auto` on input size: external only when sorted + indexed AND
above a threshold; sparse otherwise. The measurement cells have run
(germany + north-america, sparse and external, all at `dcc445e`); the
grid is in the Baselines section above.

**The walls are non-monotonic, which settles the criterion question.**
Germany ties, north-america (bigger) sparse wins by 26 %, europe
(bigger still) external wins by 21 %. A threshold fitted to "where the
walls cross" would have to cross twice and would misroute
north-america. P1's original rule - set the threshold from where sparse
pass-2 goes nonlinear, not from wall crossings - is vindicated; use it.

**The nonlinearity is clean and it is store-vs-page-cache.** Sparse
pass-2 `way_reframe_ms` per ref (this is the phase where every ref does
a random lookup into the mmap store):

| Dataset | store | refs | `way_reframe_ms` cum | per ref |
|---|---:|---:|---:|---:|
| north-america | 18.75 GB | 2.60 B | 376.0 s | 0.145 us |
| europe | 28.94 GB | 4.37 B | 3374.5 s | **0.772 us** |

**5.3x the per-ref cost for 1.68x the refs.** Pass 1 shows the same
knee (0.42 -> 0.65 -> 1.61 s/GB across germany / north-america /
europe). The host has ~28 GB RAM and the process holds ~2-3 GB anon, so
the page-cache budget is ~25 GB: north-america's 18.75 GB store is
~75 % of budget and behaves linearly; europe's 28.94 GB is ~116 % and
thrashes. This is the "working set is the ceiling" finding quantified,
and it is the same mechanism, not a new one.

**Threshold rule:** route to external when
`referenced_node_ids * 8 > ~0.8 * page_cache_budget`, where the budget
is derived from *runtime available RAM* minus the expected anon
footprint. **Do not hardcode a byte constant** - the knee is defined
relative to the host's RAM, and a constant fitted to this 28 GB box
misroutes every other machine. On this host the rule puts the boundary
near a ~20 GB store, which routes north-america to sparse (correct,
sparse wins by 26 %) and europe to external (correct, external wins by
21 %).

Caveats to respect when implementing:

- **The bracket is wide.** Two points 1.54x apart in store bracket the
  knee (18.75 GB fine, 28.94 GB thrashing). The 0.8 coefficient is a
  bias-toward-external choice inside that gap, not a measured optimum.
  A tighter threshold needs a dataset between north-america and europe;
  do not over-fit what is there now.
- Bias toward external near the boundary stands: routing a smallish
  input to external wastes seconds, routing an oversized input to
  sparse costs minutes.
- Compressed file size remains only a proxy for the store (repack level
  / blob size move file size without moving the store) - germany and
  north-america differ 5.6x in store, which no file-size heuristic
  tracks reliably. The selector must estimate the store from node
  counts in blob metadata (P4). P4 is therefore P1's real prerequisite
  for the clean version; a file-size stopgap would reintroduce the
  defect on repacked inputs.

Also fixed alongside this item: the sparse-selection hint text no
longer claims external wins at every scale (corrected 2026-07-13).

### P0. Gate the pass-0 shared-detection `get` (LANDED + MEASURED 2026-07-14, KEEP - CLOSED)

`collect_way_referenced_node_ids` (`altw/mod.rs`) runs
`referenced.get(node_id)` on **every ref regardless of
`--inject-prepass`**; only the `shared.set` inside the taken branch is
gated behind `if let Some(shared)`. Pre-prepass the loop was a bare
`set`. The prepass landings (`58743ba` / `29e4eab`) therefore added a
`get` per ref to the *serial main-thread* union for every plain-path
user.

Measured 2026-07-14 (japan, warm-walk iterations, HEAD vs `c6f08ff`):
pass 0 **2365 ms vs 1463 ms = +902 ms**, with the whole residual in
pass 0 and none in passes 1/2. Union cost **6.6 ns/ref at HEAD vs
~4.3 ns/ref at `c6f08ff`** = **~2.3 ns/ref** for the ungated `get`.
Extrapolates to **~10 s at europe** (4.37 B refs) - real, but inside
europe's drift band, which is why only japan surfaced it.

Fix: split the loop on `shared` instead of branching inside it, so the
plain path restores the bare-`set` shape:

```rust
match &mut shared {
    Some(shared) => for &id in &refs_vec {
        if referenced.get(id) { shared.set(id) } else { referenced.set(id) }
    },
    None => for &id in &refs_vec { referenced.set(id) },
}
```

Expected: -0.9 s japan (~7 % of a 12.3 s wall), ~-10 s europe (~3 %).
Keep/revert against `26203e64` (japan 12.3 s) and `4ac11326` (europe
363.1 s). P2 supersedes this loop entirely, so if P2 lands first, fold
the gate into it rather than doing both. The external twin is N7 in
`notes/altw-external.md` - same bug, same landing, different backend;
fix them together.

**LANDED + MEASURED 2026-07-14 (`add8d03`), KEEP.** The loop split went
in exactly as sketched above, with N7 in the same commit - the prepass
landings added an ungated plain-path cost to *both* backends, one pattern
rather than two coincidences. `verify add-locations-to-ways --dataset
denmark` passes on both `--mode sparse` and `--mode external`.

**`altw_pass0_union_ms` is the load-bearing readout, not the wall.** Both
cells union over a byte-identical ref count, so the counter is immune to
the drive drift that makes europe walls unreadable at this effect size:

| cell | refs | union before | union after | delta |
|---|---|---|---|---|
| japan (`26203e64` -> `13347065`) | 322,807,555 | 2128 ms | 1235 ms | **-893 ms** |
| europe (`4ac11326` -> `89683dda`) | 4,372,934,465 | 25522 ms | 17176 ms | **-8.3 s** |

Japan `--bench 3`, medians 2134 -> 1389 ms, distributions non-overlapping
(new 1235/1389/1539 vs old 2128/2134/2318). Union **6.61 -> 4.30 ns/ref
at the median**, and 4.30 matches `c6f08ff`'s pre-prepass 4.3 almost
exactly: the gate *restores* the pre-prepass union shape rather than
merely improving it. Japan wall 12.3 s -> **11.3 s** (-1.0 s, predicted
-0.9 s), at/below `c6f08ff`'s 11.4 s, with no cold-walk outlier in any
iteration.

**Europe's wall is NOT evidence here and no wall claim is made.** The
cell measured 369.8 s against the 363.1 s comparand - **+1.8 %, opposite
sign to the -8.3 s the counter proves**. Both are single `--bench 1`
cells ~6 h apart, and a ~2.8 % expected win is inside this host's 5-8 %
drift band. Six interleaved cells could settle it; not worth ~40 min and
the drive wear to confirm a mechanism the drift-immune counter already
establishes on a strictly-less-work change. If someone later wants the
europe wall number, interleave it - do not quote 369.8 s as a regression.

Note `c6f08ff` predates the `altw_pass0_union_ms` counter and emits no
`pass0` counters at all, so its "4.3 ns/ref" was inferred from the phase
wall, not measured. The 6.6 ns/ref figure is directly confirmed.

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

### P4. Unify the redundant header walks - CLOSED 2026-07-14, BUILT AND REVERTED. DO NOT RE-ATTEMPT.

> **This is the second failure of this idea in six months. It buys
> nothing. Read this section before proposing it a third time - the
> arithmetic below is what the wall numbers hide, and the item is
> seductive precisely because the naive accounting says +30 %.**
>
> Both attempts were built, measured, and reverted. The 2026-07-14 attempt
> reached a full single-walk implementation, passed every correctness gate,
> and *still* bought nothing. Passing tests and a plausible mechanism are
> not the bar; the bar is a wall delta that survives the baseline's own
> variance, and this one does not.
>
> The verdict in one line: **the saving equals the baseline's cold-walk
> penalty and nothing more, and at the scales where sparse is the right
> backend the baseline usually pays no such penalty.**

The original write-up follows, preserved because its numbers are real and
its conclusion is wrong; the "Why it buys nothing" section after it is the
disposition.

Sparse builds its blob schedule **four separate times**, and at
europe/north-america scale each build is a serial header walk over
every blob in the file. From `4ac11326` (europe, `dcc445e`) durations
plus the phase table:

| Phase | schedule walk | avg cores | header bytes read |
|---|---:|---:|---:|
| pass 0 (`build_classify_schedule(Way)`) | 26.6 s | 0.1 | 2.59 GB |
| pass 1 (`build_classify_schedule(Node)`) | 25.0 s | 0.1 | 2.39 GB |
| rel-member (`build_classify_schedule(Relation)`) | 29.3 s | 0.1 | 2.63 GB |
| pass 2 (`passthrough.rs::build_schedule`) | 26.5 s | 0.1 | 2.56 GB |
| **total** | **107.4 s of a 363.1 s wall = 29.6 %** | | |

Each walk is ~522,168 QD=1 header preads at ~50 us/blob and ~374 K
voluntary context switches, single-threaded. That rate independently
matches `notes/getparents.md` (~45 us/blob) and
`reference/blob-density.md`. The headers do not survive in page cache
between phases because pass 1 writes a 29 GB store and pass 2 reads
251 GB, evicting everything in between. External does not have this
problem - it does ONE `EXTJOIN_META_SCAN` (7.4 s at planet).

**Half the fix already exists and sparse just never adopted it.**
`scan::classify::build_classify_schedules_split` does one walk and
returns all three per-kind schedules. Its own docstring describes this
exact bug:

> "At planet / Europe scale the header walk is itself ~15 s; callers
> that need all three kinds (currently `check_refs`) would otherwise pay
> that cost three times."

Sparse needs all three kinds (Way pass 0, Node pass 1, Relation
rel-member) and calls the single-kind builder three times. Swapping
those three calls for one `build_classify_schedules_split` at command
entry is a small, mechanical change:

- europe: 26.6 + 25.0 + 29.3 = 80.9 s becomes ~26.6 s. **Saves ~54 s,
  ~15 % of wall.**
- north-america: 21.6 + 0.2 + 18.1 = 39.9 s becomes ~21.6 s. **Saves
  ~18 s, ~15.6 % of wall.** (Note na's pass-1 walk was only 183 ms - its
  headers survived from pass 0 - while rel-member's was cold again at
  18.1 s. The eviction pattern varies by scale; the redundancy does not.)
- Semantics already match: the split builder includes indexdata-less
  blobs in all three schedules, exactly as `build_classify_schedule(..,
  Some(kind))` does. Memory is trivial (~12.5 MB of entries at europe).
- The rel schedule is built unconditionally even when the rel-member
  scan does not fire; that is ~1,029 entries at europe. Ignore it.

**Pass 2's walk is the remaining half and needs real work.**
`passthrough.rs::build_schedule` builds `BlobDescriptor`s carrying
`frame_offset`, `frame_size`, `kind` and `count`, plus it reads the
OSMHeader blob body to construct the output header. `ScheduleEntry` is
only `(seq, data_offset, data_size)`, so the existing split builder
cannot serve pass 2 as-is. Extending the shared walk to carry the
frame/kind/count columns pulls pass 2 in too and takes the total to
~81 s saved (~22 %); a cat TOC (external N2) collapses the surviving
walk to a single pread and takes it to ~107 s (~30 %).

Precedents worth copying rather than re-deriving: `extract/smart.rs`
reuses its PASS1 schedule and documents "~16 % wall on Europe" for
exactly this move; the geocode builder consolidated its walker for the
same reason (`geocode_index/builder/pass1_5.rs`).

`reference/blob-density.md` already classifies the commands that call
`build_classify_schedules_split` (check-refs, check-ids, cat --clean,
repack, degrade, extract --smart) as "shape 3": one single-threaded
serial schedule walk that regresses on blob count. **Sparse ALTW is a
shape-3 command that never joined the family and pays the walk four
times instead of once** - it belongs in that table, and adopting the
split builder is what moves it there.

**Second argument for P4, independent of the wall win: the walk is the
single most cache-sensitive thing sparse does, and it is why sparse's
small-scale benches are untrustworthy.** A cold walk costs ~50-88 us per
blob; a warm one is ~free. Measured swings on identical code: japan pass
0 walk 3116 ms cold vs 29 ms warm two phases later (45x+); getparents
planet 4457 ms / 405 MB cold vs 97 ms / **0 kB** warm
(reference/performance.md). Both of the 2026-07-14 "regressions" were
this artifact wearing a code-regression costume. Four walks means four
independent chances to be measured cold; one walk means one, and a cat
TOC (external N2) means none. **Every walk deleted is a benchmark made
reproducible**, which at japan/germany scale matters more than the
seconds do.

Suggested split: land the `build_classify_schedules_split` swap first
(small, ~15 %, primitive already exists and is already exercised by
check-refs / check-ids / repack / degrade / cat), then decide whether
the pass-2 descriptor merge rides here or waits for external N2. This
still feeds P1's real selector (node counts) and P2's max-id bound, so
it remains their enabler - it is simply worth doing on its own merits
first.

#### Why it buys nothing (2026-07-14, built and reverted)

Everything above this line is the case for the item. It does not survive
contact with a matched baseline. Two arrangements were built on top of
`add8d03` and measured at north-america against same-day `--commit
add8d03` cells:

- **Consolidate passes 0/1/rel only** (the "suggested split" above, using
  the existing `build_classify_schedules_split`): 120.4 s, 133.7 s.
- **One walk serving all four phases** (new `build_blob_table` ->
  `BlobTable` carrying frame/data offsets + kind + count + the OSMHeader
  body location; pass 2 projects `BlobDescriptor`s off it instead of
  walking): 105.2 s, 101.3 s.

Both were correct - `verify add-locations-to-ways --mode sparse`, `verify
check-refs` and `verify cat` all passed, `brokkr check` clean. Both were
reverted.

**Subtract pass 2 - the phase neither arrangement changes the work of -
and the effect disappears.** "rest" is walk + pass 0 + pass 1 + rel, i.e.
exactly what this item touches:

| run | wall | pass 2 | **rest** |
|---|---:|---:|---:|
| baseline, morning (`340ba366`) | 115.1 s | 49.8 s | **65.3 s** |
| baseline, afternoon (`cd213ac5`) | 114.5 s | 66.4 s | **48.1 s** |
| one-walk, run 1 | 103.5 s | 55.1 s | **48.4 s** |
| one-walk, run 2 | 99.8 s | 55.5 s | **44.3 s** |

**The afternoon baseline's rest is 48.1 s. The one-walk build's rest is
48.4 s. That is a tie** - and the afternoon baseline is the one whose
walks came back warm (24.9 s of classify walk: 22.07 + 0.18 + 2.65).
The two baselines differ from *each other* by 17.2 s, which is larger than
the effect being claimed, and pass-2 work swings 48.8 -> 66.4 s across
runs of code nobody touched. **The -11 % that this section originally
reported was that variance, read in the favourable direction.**

**The structural reason, which is the part that generalises.** The saving
is exactly equal to the baseline's cold-walk penalty and nothing more. A
walk is only expensive if something evicted the headers since the last
one; pass 1's store write is the only thing in sparse that does. So:

- **Where the store fits page cache, the baseline's extra walks are
  already free and unifying them recovers zero.** north-america's store is
  18.75 GB against a ~25 GB budget (75 %) - it usually does not evict, and
  the afternoon baseline's warm 0.18 s / 2.65 s walks are what that looks
  like. The morning baseline's cold 18.1 s rel-member walk is the coin
  landing the other way; the win at this scale is a coin flip between 0 s
  and ~18 s, and it is not separable from pass-2 noise of the same size.
- **Where the walks reliably cost, sparse is the wrong backend.** Europe's
  28.94 GB store guarantees eviction, so the europe baseline really does
  pay ~2-3 cold walks and unifying really does save ~50 s there. But that
  is the regime **P1 exists to route to external**. The win lives only in
  the configuration P1 removes.

That is the whole disposition: **P4 pays off only where P1 says do not be
here.** No arrangement of the walks changes that, which is why this has now
failed twice.

**The half-measure is worse than doing nothing** (+3.8 %), and that part is
solid: it saved 20.7 s across the three consolidated phases and lost 24.1 s
in pass 2, because it deleted pass 2's free ride and forced it to walk cold
on the far side of pass 1's eviction. **Pass 2's walk measured 23.0 s cold**
via the new `ALTW_PASS2_SCHEDULE_WALK` marker, against ~1 s when the
relation-member scan walked just before it. If anyone proposes the
`build_classify_schedules_split` swap again, this is the answer.

**The 107.4 s / 29.6 % figure in the table above is an overcount and is the
main reason this item keeps coming back.** It sums four durations that are
not four independent costs. Two baseline runs of byte-identical code prove
it by disagreeing about *where* the cost landed while agreeing on the
total: morning had classify walks 39.8 s (rel-member cold 18.1 s) and pass
2 at 49.8 s; afternoon had classify walks 24.9 s (rel-member warm 2.6 s)
and pass 2 at 66.4 s - phase totals 113.7 s vs 113.1 s. Whichever phase
walks first after the eviction pays; the next rides free.

**Rules that outlive this item:**
- **Never sum per-phase walk durations.** The sum is an upper bound that is
  only reached if every walk is cold. This applies to every shape-3 command
  in `reference/blob-density.md`, not just sparse.
- **Never read a wall delta on sparse north-america/europe without
  subtracting pass 2.** Pass-2 work varies ~±15 % run to run and is over
  half the wall; it will manufacture any result you want at this effect
  size.
- **A walk below pass 1 costs a full cold walk** (23.0 s at
  north-america) however cheap the walks above it look. Pinned at
  `passthrough.rs::build_schedule`.

**What was kept:** only the `ALTW_PASS2_SCHEDULE_WALK` marker pair. Pass
2's walk was invisible before it, and this argument cannot be had without
it. Everything else was reverted to `add8d03`.

**Not evidence, recorded to stop it being re-litigated:** the europe cells
run during this work (baseline 372.5 s `7b84e6c0` / 351.7 s `18237ed9`, one
one-walk cell at 321.6 s, three OOM kills across both arms) are
RAM-confounded - the host gained 2.75 GB mid-suite when an unrelated
process exited, and europe sparse straddles the page-cache budget. The OOMs
hit **both** arms (baseline 2 of 3 succeeded, one-walk 1 of 3) and are a
property of europe sparse at 116 % of budget, **not** of this change - they
are P1's argument, and they say nothing about the walks either way.

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

**Gate result (2026-07-14): PASSED, and it proved more than it was asked
to.** `altw_pass1_rank_span_slots / altw_pass1_coords_stored` is
**exactly 1.000** on all three datasets - germany 414,849,582 /
414,849,582; north-america 2,344,309,356 / 2,344,309,356; europe
3,617,893,513 / 3,617,893,513. Zero write amplification.

Exact equality is stronger than "the intervals are dense". Since the
counter sums `blob_max_rank - blob_min_rank + 1` per blob, `sum(span) ==
total_stored` can only hold if the per-blob rank intervals are dense
**and mutually disjoint** - which is precisely the range-disjointness
proof the correctness landmine above demanded (zero-filled spans
clobbering a neighbour blob's coords). Both of P5's gates are therefore
clear on sorted + indexed input.

Read this carefully before treating it as a green light:

- **The equality is structural, not lucky.** Ranks are assigned over
  referenced ids in ascending order, and sorted node blobs cover
  disjoint id ranges, so a blob's referenced nodes *must* occupy one
  contiguous hole-free rank interval. The counter will read 1.000 on any
  sorted + indexed input forever; its discriminating power exists only
  for unsorted input. Do not re-run this to "confirm" it.
- **Passing the gate removes a disqualifier; it does not make P5 a win.**
  Codex's substantive objections are untouched: buffered pwrite dirties
  the same page cache, the current mmap stores on sorted input are
  already near-sequential, and the pwrite path adds zero-fill +
  userspace scatter + kernel copy that atomic stores do not pay. P5
  stays demoted, and the A/B still waits on P2. Expected direction
  remains uncertain, flat-or-worse plausible.

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

- **Unifying the four header walks (any arrangement). Built and reverted
  TWICE - 2026-01-ish and 2026-07-14.** It buys nothing, and the naive
  accounting says +30 %, which is why it keeps coming back. The saving
  equals the baseline's cold-walk penalty and no more: where the store fits
  page cache the redundant walks are already free (north-america rest 48.1 s
  baseline vs 48.4 s unified - a tie), and where they reliably cost, sparse
  is the wrong backend and P1 routes away. **The partial version
  (`build_classify_schedules_split` for passes 0/1/rel, leaving pass 2 to
  walk) is strictly worse than doing nothing** (+3.8 %): it deletes pass 2's
  free ride and forces a 23.0 s cold walk. Full arithmetic, both
  arrangements, and the baseline-variance trap in P4. Do not re-open on the
  strength of the 107.4 s figure - that figure is the overcount.
- **Output-compression knob sweeps.** Settled; japan sweep above,
  europe axis in `reference/performance.md`, zstd:1 guidance shipped.
  The external doc's single-planet-cell reopen condition was **spent
  2026-07-14** and came back negative (planet streaming is not
  compression-bound: -80 % compression CPU for <=5 % wall). The axis is
  closed in both directions and no further cell is licensed - a reopen
  now needs a genuinely new reason, not "we never measured planet".
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
  (every node blob has a tagged node or member overlap). The
  "re-attempt at planet under zstd:1/none" escape is **narrower than it
  looks**: the planet zstd:1 cell (external doc) found no compression
  ceiling to relieve, so a CPU win there has nowhere to go either. The
  binding gate is now the skip predicate itself - and it does not fire
  (external's `s4_node_blobs_kept_by_tags` is 32,835 of 32,835 at
  planet). Re-attempt only if the predicate starts firing.
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
- ~~Sparse numbers predate the 2026-05..07 tree~~ **re-pinned
  2026-07-14** at `dcc445e` (japan, germany, north-america, europe -
  see Baselines). Europe came back flat; japan came back +30 % and is
  flagged above. The caution that produced that flag was correct and is
  worth keeping: re-pin before gating on a stale number.
- **Run order is a variable, not a nuisance.** The external doc's drift
  verdict measured +31 % write time on byte-identical stage-1 work
  between two cells an hour apart in the same suite. Planet single
  samples carry a ~5-8 % environmental band. Sparse europe cells inherit
  this: matched sample counts are necessary but **not sufficient** - an
  A/B whose two cells run adjacently is confounded by drive state
  regardless of sample count. Order-swap or interleave.
- **Core-seconds are not a ceiling.** The external doc assumed
  compression bound the planet streaming phase because it burned 4306
  core-seconds; removing 80 % of it moved wall <=5 %. Before gating any
  sparse item on "X dominates the phase", check that removing X actually
  moves wall. This applies directly to the pass-2 zlib:6 writer-ceiling
  reasoning cited above, which is a *japan* result.

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
