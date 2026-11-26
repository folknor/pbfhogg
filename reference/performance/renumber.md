# Renumber

## Renumber (external mode)

Planet-scale renumber uses O(1) lookups in ranked ID sets
(`RankedIdSet`), replacing the original 256-bucket radix partition.
Wire-format splice rewriters
for all three element types - pass 1 (dense and plain nodes), stage 2d (ways),
and R2d (relations) - patch only the ID/ref fields and copy everything
else verbatim as raw bytes. No BlockBuilder, no PrimitiveBlock
construction. Pass 1 and stage 2d run work-stealing workers sized from
the host (pass 1 capped); both record old IDs in one shared,
pre-allocated `IdSet` per kind, one atomic OR per 64-ID word run.
R2d: parallel with inline rank() dispatch (relation_map replaced by
`relation_id_set.rank()`). All member-ref lookups via
`node_id_set.rank()` + `way_id_set.rank()` inline - no flat temp
files. Zero scratch disk usage. Single shared input fd across all
data phases (the header read and the schedule walk have their own).
Atomic index dispatch (no `Arc<Mutex<Receiver>>`). Output
defaults to zlib:1.

### Current baseline (planet, commit `b17276bc` plus word-batched ID sets, bygg, 2026-11-26)

`id-remap` `--iterations 1`, cells `53b1ae16` and `d436f9a5`: 66.1 s and
65.5 s. Pass 1 34.0 / 33.8 s, stage 2d 21.9 / 22.0 s, R1 2.4 s, R2D
1.3 s; peak anon 3.6-3.7 GB. See "Word-batched ID set insertion" below.

## History

### Renumber

#### Word-batched ID set insertion (planet, baseline `b17276bc`, bygg, 2026-11-26)

Pass 1 and stage 2d recorded each old ID with its own
`fetch_or(Relaxed)` on the shared set. They now go through
`AtomicWordBatch` (`crates/lib/src/idset.rs`), which ORs IDs of the
same 64-bit word into a local mask and publishes one `fetch_or` per run
of IDs in a word within a blob. One matched suite after `brokkr fstrim`,
order A B B A:

| Cell | Arm | Wall | Pass 1 span | `pass1_reframe_ns` | `stage2d_reframe_ns` |
|---|---|---:|---:|---:|---:|
| `675bcf67` | baseline | 67.1 s | 35.4 s | 80.6 s | 362 s |
| `53b1ae16` | batched | 66.1 s | 34.0 s | 45.0 s | 387 s |
| `d436f9a5` | batched | 65.5 s | 33.8 s | 44.6 s | 389 s |
| `6af731c3` | baseline | 72.1 s | 40.3 s | 78.8 s | 311 s |

Verdict: kept. Pass 1 rewrite worker time fell 44 % in both pairs; the
wall fell 1.0 s and 6.6 s, so the end-to-end size is not resolved beyond
"a few percent, same sign twice". Stage 2d's reframe counter is
unresolved (baseline spread 311-362 s; the way rewrite is dominated by
per-ref lookups, and ways are sparser per word than nodes). Peak anon
unchanged.

Considered and refused in the same work: building the maps from
worker-owned ID ranges with local pages and a checked join. The costs it
targeted are gone or negligible at planet: way shards and their serial
merge no longer exist, rank builds take 41-49 ms for nodes and 6 ms for
ways, and the per-node atomic is now per word. What it would still add
is plain stores in place of the remaining per-word atomics, not measured
and not worth ownership of ID ranges that must follow observed IDs.

### Planet (87.7 GB indexed, 11.6B elements, plantasjen)

Element counts: 10,447,738,627 nodes / 1,165,589,744 ways /
14,124,889 relations / 12,435,459,911 way refs.

### Memory

Peak anon 3.3 GB (commit `cb99106`). Pass 1 populates a shared node ID
set (now `IdSet`) using atomic writes. In that measurement, the node
bitset and rank index used ~1.5 GB, the way bitset and rank index used
~200 MB, and the relation bitset used ~20 MB.
Zero temp disk. Renumber ran with glibc capped at two arenas until
2026-11; without the cap the planet peak is 3.32 GB against 3.16 GB with
it, at the same wall (see "The CLI no longer caps malloc arenas").

### Phase breakdown (commit `cb99106`, planet, `--bench 1`, UUID `f9098cab`)

| Phase | Duration | Peak Anon | Share |
|---|---:|---:|---:|
| Schedule scan | **16.6 s** | - | 9% |
| PASS1 (4 workers, wire-format nodes) | **95.3 s** | 2.1 GB | 49% |
| STAGE2D (6 workers, fused way resolve + wire-format ways) | **76.8 s** | 3.3 GB | 40% |
| R1 (sequential wire-format relation ID scan) | **3.2 s** | - | 2% |
| R2D (parallel wire-format relations, inline rank()) | **1.9 s** | - | 1% |
| **TOTAL** | **194 s (3m14s)** | **3.3 GB** | - |

#### Host-sized rewrite stages (planet, baseline `c73f2a73`, bygg, 2026-11-26)

Pass 1 and stage 2d were fixed at 4 and 6 workers, and their worker-time
counters summed to exactly that many busy threads (`d24df488`: stage 2d
reframe + decompress + pread 283 s over a 47.3 s span = 6.0; pass 1
268.7 s over 67.2 s = 4.0): the stages were thread-starved on a
32-thread host, not bound by per-ref lookup cost. Both are now sized from
the host (width less two; pass 1 capped at 12 because past that it waits
on the writer), and stage 2d records way IDs in one shared, pre-allocated
`IdSet` through atomic inserts instead of one shard per worker merged in
R1, so the set's storage does not grow with the width.

Exploration, uncommitted, before the suite: Germany 7.8 s at HEAD
(`3e60f5a1`); stage 2d at 16 workers alone 4.9 s (`8d42a9fd`, stage 2d
3131 to 1349 ms); pass 1 at 12 as well 3.7 s (`30c7b862`, pass 1 2296 to
1062 ms); 28 and 28 4.0 s (`c1cb88ff`, no better: pass 1
`ordered_pool_result_send_wait_ns` 14.3 s, writer-bound). Planet with
both at 16 and per-worker shards kept: 79.8 s (`ba0d1a22`), peak anon
5.9 GB against 3.2 GB, the growth in stage 2d's shards, which is what the
shared set removes.

Matched suite after one `fstrim`, A B B A, baseline built with
`--commit c73f2a73`, variant uncommitted on it: baseline 2m06s
(`9766e8c7`) and 2m11s (`5985d34e`), variant 1m20s (`9c8055f4`) and
1m08s (`de92105d`). Phase split, baseline against variant by pair:
pass 1 68.7 / 48.2 s and 74.4 / 37.3 s, stage 2d 46.0 / 22.4 s and
47.7 / 21.8 s, R1 2.45 / 2.39 s and 2.47 / 2.39 s. Peak anon 3.25 GB
against 3.58 GB and 3.26 GB against 3.66 GB. Verdict: landed, about
-40 % wall; Germany `pbfhogg-verify renumber` passes. Per-ref lookup
CPU now matters more, since the stage runs about 28 cores busy instead
of 6; that work is unmeasured at this width.

#### Compressed-body trial, re-encoding cut and the zlib worst-case reserve (planet, baseline `62a911d7`, bygg, 2026-11-24)

The ordered consumer now compares each block with the largest payload
whose worst-case compressed body fits (about 29.8 MB under zlib) instead
of the raw limit, and the zlib encoder reserves an eighth over its input
up front (capacity only) and continues a call that stops short. Matched
`id-remap` `--iterations 1` cells, A B B A, baseline built with
`--commit 62a911d7`, variant uncommitted on it: baseline 2m04s
(`b0c42038`) and 2m14s (`1f5eef92`), variant 2m03s (`a71bfc84`) and
2m12s (`06296e15`). Each pair is within 2 s; the second pair's drift is
shared by both arms. Neutral; no planet block reaches the trial.

#### One rule for uninterpreted block and group fields (planet, baseline `0a986fbc`, bygg, 2026-11-24)

The way and relation group loops drop every non-element field (the way
arm used to copy it) and refuse a mistyped element field of their own
kind; one more arm in the per-group-field loop that runs once per way.
Matched `id-remap` `--iterations 1` cells, A B B A, baseline built with
`--commit 0a986fbc`, variant uncommitted on it: baseline 2m05s
(`e621941c`) and 1m58s (`d24df488`), variant 1m57s (`2a27acb5`) and
1m57s (`3034f3cd`). `stage2d_reframe_ns` 174.4 / 175.4 s baseline
against 166.4 / 165.0 s variant; `pass1_reframe_ns` 59.1 / 58.8 s
against 58.5 / 59.0 s. Neutral on the wall; the stage 2d CPU drop is
within layout noise and not claimed.

#### Cutting a rewritten block past the payload limit (planet, baseline `56869b08`, bygg, 2026-11-24)

The ordered consumer compares each rewritten block's length with the
writer's payload limit and cuts only a block over it, so the per-element
rewrite loops are unchanged. Matched `id-remap` `--iterations 1` cells,
A B B A, baseline built with `--commit 56869b08`, variant uncommitted on
it: baseline 2m05s (`05b60b8a`) and 2m08s (`e3f7822d`), variant 2m04s
(`ef5d6f46`) and 2m03s (`d38b538e`). Phase split (`05b60b8a` against
`d38b538e`): pass 1 66.5 s against 66.3 s, stage 2d 47.0 s against
47.0 s. Neutral; no planet block is cut.

#### Plain node groups and ways without refs (planet, baseline `19f32e61`, plantasjen, 2026-11-20)

Pass 1 classifies each node group (dense, plain, or no elements) and
rewrites plain `Node` messages by a one-range id splice; stage 2d takes a
way with no `refs` field as an empty list. Matched `id-remap`
`--iterations 1` cells, baseline built with `--commit 19f32e61`, variants
uncommitted on it, no fstrim, other projects' checks running between
cells. The walls moved with the input reads (`pass1_pread_ns` 102 to
213 s across cells of either arm, drifting upward through the session),
so the verdicts are read from the rewrite counters, which do not depend
on the drive.

- Baseline: 174.8 / 187.9 / 195.7 s (UUIDs `58a66045`, `1744d7a1`,
  `ba539b69`); `pass1_reframe_ns` 54.5 / 55.0 / 53.7 s,
  `stage2d_reframe_ns` 252.4 / 250.2 / 250.7 s.
- First form, a refusal arm for a non-packed, non-varint `refs` in the
  way splice's per-field loop: 208.4 / 180.1 s (`257410d0`, `13531e3c`),
  `stage2d_reframe_ns` 273.2 / 272.0 s, about 9 % over the baseline in
  both cells; pass 1 unchanged (54.9 / 53.5 s). The same with that
  refusal built as a cold out-of-line function and the absent case as an
  empty splice range: 182.2 s (`022a49bc`), stage 2d 275.3 s. With the
  original way splice restored (isolation build, `14bd921f`) stage 2d
  read 249.4 s, so the cost followed the way-loop change.
- The loop as before and the stray-field check moved into the
  absent-refs path: 177.3 / 212.6 s (`c10a910f`, `c1b1fb00`; the second
  cell's pass 1 read took 180 s), `pass1_reframe_ns` 54.3 / 53.7 s,
  `stage2d_reframe_ns` 250.1 / 250.3 s.
- Landed form, the same check run for every way with no refs (absent or
  one empty segment): 181.6 s (`cfdce1c1`), `pass1_reframe_ns` 52.4 s,
  `stage2d_reframe_ns` 248.9 s. Verdict: neutral.

#### Signed remap and orphan bijection (Germany, baseline `b974262f`, bygg, 2026-11-18)

Negative IDs accepted: canonical order checks, a negative start counting
down, wrapped deltas, and orphans named by a rank-index bijection instead
of the `start + count + old` shift. Matched `id-remap` `--iterations 1`
cells after `fstrim`, baseline built with `--commit b974262f`, variant
uncommitted on it.

- First variant, orphan mapping inlined into the ref lookup, A B B A:
  6409 / 6365 / 6560 / 6344 ms (UUIDs `cf3679d5`, `5ce4af3e`, `bc33a17e`,
  `cb201341`). The wall is within noise (+1.3 % by median, pairs disagree
  in sign), but stage 2d ran +6 % in both pairs (2822 to 3005 ms, 2827 to
  2999 ms): the orphan path had swelled the per-ref resolve.
- Orphan mapping moved out of line (`#[cold]`, never inlined), B A B A:
  6167 / 6183 / 6326 / 6076 ms (UUIDs `5bd189c3`, `04bb8606`, `446ba950`,
  `508e1377`). Stage 2d 2830 against 2823 ms and 2804 against 2817 ms; the
  wall spread is the `R2D_END` flush tail, which moves either way.
  Verdict: neutral. This is the landed form.

#### Signed `IdSet` (Germany, baseline `2b4e511c`, bygg, 2026-11-18)

The set became signed and renumber's live map ranks in canonical order.
Matched `id-remap` `--iterations 1` cells, A B B A, baseline built with
`--commit 2b4e511c`, variant uncommitted on it: 7.6 / 6.9 / 6.5 / 6.8 s
(UUIDs `9e86c30a`, `31419a11`, `dadf81f8`, `cebebd5a`). Verdict: neutral.

#### Optimization history

Planet 3,456 s → 194 s (-94 %) across ~30 commits from `e156e97` to
`cb99106`. Key landings: work-stealing dispatch (resolved two
intermediate OOM kills at ~26 GB anon RSS), DenseNodes + way wire-format
rewriters, `IdSetDense` rank fusion (collapsed stages 2a+2b+2c), fused
R2A/R2B/R2d relation pipeline, atomic index dispatch, shared-atomic
`IdSetDense` with concurrent `AtomicU8::fetch_or` pass 1 writes. Each
commit verified on Denmark via `brokkr run --with pbfhogg pbfhogg-verify -- renumber` (306-relation
orphan delta preserved exactly).

Element counts: 10,447,738,627 nodes / 1,165,589,744 ways /
14,124,889 relations / 12,435,459,911 way refs (stable across the
optimization arc).

The intermediate `6165394` breakdown with stages 2a/2b/2c that totaled
1,092 s / 18.2 min was preserved while the architecture matched; that
breakdown is now obsolete - `IdSetDense` rank fusion collapsed the
multi-stage 2a/2b/2c into a single rank-fused stage and the older
breakdown no longer maps onto the code.

---

