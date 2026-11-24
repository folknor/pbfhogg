# Renumber

## Renumber (external mode)

Planet-scale renumber via IdSetDense rank-based O(1) lookup (replaces
the original 256-bucket radix partition). Wire-format splice rewriters
for all three element types - pass 1 (dense and plain nodes), stage 2d (ways),
and R2d (relations) - patch only the ID/ref fields and copy everything
else verbatim as raw bytes. No BlockBuilder, no PrimitiveBlock
construction. Pass 1: 4 work-stealing workers. Stage 2d: 6 workers.
R2d: parallel with inline rank() dispatch (relation_map replaced by
`relation_id_set.rank()`). All member-ref lookups via
`node_id_set.rank()` + `way_id_set.rank()` inline - no flat temp
files. Zero scratch disk usage. Single shared input fd across all
data phases (the header read and the schedule walk have their own).
Atomic index dispatch (no `Arc<Mutex<Receiver>>`). Output
defaults to zlib:1.

### Planet (87.7 GB indexed, 11.6B elements, plantasjen)

Element counts: 10,447,738,627 nodes / 1,165,589,744 ways /
14,124,889 relations / 12,435,459,911 way refs.

### Memory

Peak anon 3.3 GB (commit `cb99106`). Single shared `IdSetDense` with
`AtomicU8::fetch_or` for concurrent pass 1 writes (~1.5 GB node bitset
+ rank index, ~200 MB way bitset + rank, ~20 MB relation bitset).
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

## History

### Renumber

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

