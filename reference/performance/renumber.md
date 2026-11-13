# Renumber

## Renumber (external mode)

Planet-scale renumber via IdSetDense rank-based O(1) lookup (replaces
the original 256-bucket radix partition). Wire-format splice rewriters
for all three element types - pass 1 (DenseNodes), stage 2d (ways),
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

