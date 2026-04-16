# ALTW as renumber — ground-up reshape plan

> **⚠ EXPERIMENT FAILED — 2026-04-16. This plan's core thesis is disproven. Do not implement as written.**
>
> A working implementation was built (`src/commands/altw_v2.rs`) and tested. Denmark passes byte-identical output in 3.5 s (2.8× faster than the 4-stage pipeline). Europe **OOM-killed** at Phase 2's `coord_table` allocation: the actual unique-referenced-node count was **3.6 B** (29 GB coord table), not the ~1 B / ~8 GB estimated for Europe in this document's memory-budget section. Planet projects to ~10 B referenced / ~80 GB coord table. **The in-RAM coord table does not fit past Denmark-scale on a 30 GB host.**
>
> The plan's own step-1 ("measure `unique_referenced_nodes` on planet before committing to the 16 GB coord_table sizing") was the correct caveat — but the thesis was constructed around an estimate that was wrong by ~4–5× at every scale. Measurement-before-design would have caught this.
>
> **Reverse of the framing here**: the existing four-stage external-sort pipeline in `src/commands/altw/*` is not "the wrong shape" — it is **load-bearing for any input whose coord table does not fit in RAM**. Renumber's in-RAM form works because `new_id = start + rank(old_id)` is a pure function needing only a 2 GB bitmap; ALTW's resolver needs 8 bytes of data per referenced node, which scales linearly with PBF size (~73× coord-table ratio Denmark→Europe vs ~64× PBF size ratio).
>
> **Actively valid work going forward** is in [`altw-structural-reports.md`](altw-structural-reports.md) — the ranked specific-seam items there (stage-1 decompress duplication, finalize-removal routing table, stage-2 de-ranking, relation-member forward fold, BlobHeader refcount extension) all survive this result. Their payoffs are smaller individually than the reshape's claimed target, but they are real and they do not depend on a wrong sizing assumption.
>
> The rest of this document is preserved below as historical record of the thesis that failed and the reasoning that led to it.
>
> ---

## Thesis (DISPROVEN — see notice above)

ALTW today is a four-stage external sort (`stage 1 → stage 2 → stage 3 → stage 4`) with three disk-materialized intermediates totaling ~247 GB of read + ~247 GB of write on planet. It is the wrong shape for this problem.

The right shape is already in tree: [`renumber_external.rs`](../src/commands/renumber_external.rs) does one parallel pass per element type, zero temp files, O(1) lookup via `IdSetDense::resolve`, and processes planet in 194 s at 3.3 GB RSS.

ALTW differs from renumber in exactly one way: a resolved reference must return a coordinate, not a new ID. Renumber's resolver is pure: `new_id = start_id + rank(old_id)`. ALTW's resolver has to return `(lat, lon)` from actual node-blob data, so it needs a coord table. That single difference is why ALTW is four stages today — the coord table is large, and external partitioning was chosen to bound RAM.

That choice was correct before a hard 30 GB ceiling was pinned as the real operating constraint. It is wrong now. A planet coord table for **unique referenced nodes** (not all nodes) is ~16 GB. Under a 30 GB host ceiling, the coord table fits in RAM, and once it does ALTW collapses into renumber's shape: three passes, no temp files, no slot buckets, no straddlers, no `coord_payloads`, no finalize, no `NUM_BUCKETS`.

## Yardsticks

All three commands run on the same ~87 GB planet input on the same host:

| Command | Wall | Peak RSS | Notes |
|---|---:|---:|---|
| `cat --type way` | 44 s | 10 MB | raw-frame passthrough. Lower bound for "touch every way blob". |
| `renumber_external` | 194 s | 3.3 GB | full way-blob rewrite with inline ref resolution. |
| `altw --index-type external` (current, `4f059b67`) | 867 s | ~16.7 GB anon | 4-stage external sort. ~247 GB temp disk. |

Target for this plan: **wall within 1.3–2× of renumber** (250–400 s), **peak RSS ≤ 25 GB**, **zero temp disk**.

`cat --type way` is the pure decompress-copy-recompress floor; ALTW cannot match it because ALTW must modify the way blob (splice fields 9, 10). `renumber_external` is the realistic target because it does a structurally similar thing (rewrite a way blob using an ID-set-backed resolver).

## Why ALTW is 4.5× slower than renumber

Renumber can resolve every ref in a single rank query because `start + rank(old_id)` is a pure function of the bitmap. ALTW cannot — coords are data, not structure. So ALTW built external partitioning to hold the coord table without exceeding RAM:

- **Pass A** (stage1.rs) decompresses every way blob to build `IdSetDense` + ref-count sidecars.
- **Pass B** (stage1.rs) re-decompresses the same way blobs to emit rank-bucketed `(local_rank, slot_pos)` records into 256 shard files (~80 GB).
- **Stage 2** (stage2.rs) streams node blobs, resolves coords per rank-bucket, writes ~112 GB of slot-bucket files.
- **Stage 3** (stage3.rs) reads slot buckets, re-sorts into blob-order delta-varint payloads, writes ~55 GB of `coord_payloads`.
- **Stage 4** (stage4.rs) re-reads way blobs and preads per-blob payloads to reframe output.

Decompression count: **3× way + 1× node**. Full detail in `altw-structural-reports.md`.

Every one of those four stages exists to partition the coord table. Once the coord table fits in RAM, **all of them collapse**.

## New architecture — 3 phases, one parallel pass each

Three phases, each modeled line-for-line on a specific renumber function.

### Phase 1 — way scan, build `node_id_set`

Pattern: [`pass1_parallel_scan`](../src/commands/renumber_external.rs#L615) inverted from "scan + rewrite" to "scan only".

- Work-stealing dispatch over way blobs via `AtomicUsize::fetch_add` on the way schedule from [`build_all_blob_schedules`](../src/commands/renumber_external.rs#L557).
- Shared pre-allocated `IdSetDense` (one bitmap, no per-worker shards). `pre_allocate(MAX_NODE_ID)` before spawning workers.
- Each worker: `pread` → `decompress_blob_raw` → [`scan_way_refs`](../src/commands/way_scanner.rs#L24) → for each referenced `node_id`, call `node_id_set.set_atomic(node_id)`.
- Each worker emits the per-blob refcount vector through a `sync_channel(32)`; the consumer writes two sidecars in PBF order (`way-ref-counts`, `per-way-refcounts`). Phase 3's reframe still needs these to drive the field 9/10 splice loop.
- After thread-join: `node_id_set.build_rank_index()`.

Cost: 1× way-blob decompress. No Pass B. No shard files.

### Phase 2 — node scan populating an in-RAM coord table

Pattern: same worker shape as Phase 1, different operation.

- Pre-compute [`NodeBlobInfo`](../src/commands/altw/mod.rs#L67)-style `(ref_rank_start, ref_rank_end)` per node blob via `node_id_set.count_below(min_id)` / `count_below(max_id + 1)`. Same machinery as [stage1.rs:249](../src/commands/altw/stage1.rs#L249); retain it.
- Allocate `coord_table: Vec<u8>` sized `unique_referenced * 8` bytes (~16 GB at planet). **Zero-initialize** — orphan/unresolved refs in Phase 3 will read `(0, 0)`, matching the existing zero-coord sentinel.
- Work-stealing dispatch over node blobs.
- Each worker: `pread` → `decompress_blob_raw` → [`extract_node_tuples`](../src/commands/node_scanner.rs#L46) → iterate tuples in ID-sorted order (DenseNodes stores IDs sorted by delta-encoding construction). Initialize `rank = blob.ref_rank_start` at blob entry.
- Per tuple: `if node_id_set.get(id) { coord_table[rank * 8 ..].write(lat, lon); rank += 1; }`. Uses `get` (O(1) bit test), **not** `rank_if_set` (O(block scan + popcount)).
- **No atomics.** By `NodeBlobInfo` monotonicity, each blob's rank range is disjoint from every other blob's. Workers write disjoint slices of `coord_table`.

Cost: 1× node-blob decompress. Memset of 16 GB once (~5 s on DDR4).

### Phase 3 — way rewrite with coord splice

Pattern: [`stage2d_parallel_way_assembly`](../src/commands/renumber_external.rs#L307) + [`stage2d_worker`](../src/commands/renumber_external.rs#L418).

- Work-stealing dispatch over way blobs (second way pass, not third).
- Each worker: `pread` → `decompress_blob_raw` → reframe via a variant of the current [`reframe_way_blob_with_locations`](../src/commands/altw/stage4.rs#L920), but with the coord source changed from "pread from `coord_payloads` file" to "index into `coord_table`".
- Per ref within a way:
  ```
  match node_id_set.rank_if_set(ref_id) {
      Some(rank) => splice coord_table[rank * 8 ..] as (lat, lon) deltas,
      None       => splice (0, 0),
  }
  ```
- Output via `sync_channel(32)` → `ReorderBuffer<Vec<OwnedBlock>>` with capacity 64 → `writer.write_primitive_block_owned`. Exactly the renumber pattern at [renumber_external.rs:670–704](../src/commands/renumber_external.rs#L670).

Cost: 1× way-blob decompress. Rank lookups: ~6.5B × ~30–50 ns = 30–60 s aggregate across 6 workers. Renumber pays the same cost and still hits 194 s, so this is not a new bottleneck.

### Non-way blobs

- **Relation blobs:** raw passthrough via `write_raw_owned`. Same as today's stage 4.
- **Node blobs (when `!keep_untagged_nodes`):** pre-compute kept-blob set via the existing relation-member scan ([`collect_relation_member_node_ids`](../src/commands/add_locations_to_ways.rs)). Gated per-blob in Phase 3's dispatcher the same way current stage 4 does. Unchanged logic; no coord splicing involved.

## Memory budget (planet) — MEASURED, plan was wrong

Planned (at the time of writing):

| Component | Planned size |
|---|---:|
| `IdSetDense` bitmap | ~2.0 GB |
| `IdSetDense` rank index | ~0.1 GB |
| `coord_table` (≈2 B × 8 bytes) | ~16 GB |
| Per-worker read + decompress + reframe buffers × 6 | ~1.5 GB |
| Writer pipeline (`PIPELINE_DISPATCH_PERMITS` × block) | ~0.3 GB |
| `ReorderBuffer` (64 × block) | ~0.3 GB |
| Sidecars held in RAM (ref-counts, per-way) | <0.3 GB |
| **Subtotal** | **~20–21 GB (planned)** |

Actual (measured 2026-04-16 via `altw_v2` implementation):

| Dataset | Unique referenced | Coord table | Outcome |
|---|---:|---:|---|
| Denmark | 49 M | 394 MB | works, 3.5 s (2.8× speedup vs old external) |
| Europe | **3.6 B** | **29 GB** | **OOM-killed at Phase 2 coord_table alloc on 30 GB host** |
| Planet (projected) | **~10 B** | **~80 GB** | would fail worse |

The plan's written-in caveat ("if `unique_referenced` on planet is materially higher than 2 B, fall back to the current external-sort shape") was the right caveat, reached faster than expected — Europe alone already violates it. The "fail-soft" branch of the plan is the only viable one: **fall back to the current external-sort shape**. The compact-encoding escape hatch (7 bytes/coord) would save 12 % at planet; still doesn't fit.

## Decompression and I/O

| | Current | Proposed | Savings |
|---|---:|---:|---:|
| Way-blob decompressions | 3× | 2× | 1× way pass (~50–80 s at planet) |
| Node-blob decompressions | 1× | 1× | — |
| Temp disk bytes written | ~247 GB | 0 | all of it |
| Temp disk bytes read | ~247 GB | 0 | all of it |
| Disk-seam stages | 3 (stage 2→3, 3→finalize, 3→stage 4) | 0 | all of it |

The single way-pass savings is smaller than it sounds in isolation (~50–80 s). The real win is the ~400+ GB of temp-disk traffic deleted, which is wall time eaten by the filesystem on every pipeline barrier.

## What gets deleted

Essentially the entirety of `src/commands/altw/` except the top-level entry point and the shared wire-format helpers:

**Deleted outright:**

- `stage1.rs` — replaced by Phase 1 worker loop.
- `stage2.rs` — replaced by Phase 2 worker loop.
- `stage3.rs` — **gone**. No slot-reorder phase exists.
- `stage4.rs`'s reframe consumer — replaced by Phase 3 worker loop; coord source changes from `CoordPayloadsReader::pread_blob_payload` to `&coord_table[rank * 8 ..]`.
- `coord_payloads.rs` in its entirety — **gone**. No `CoordPayloadsReader`, no `finalize_coord_payloads`, no `StraddlerSlot`, no `ManifestEntry`, no `PerWayRcs` lazy decode.
- `blob_bucket_index.rs` — **gone**. No slot/rank bucket intersection classification.
- `NUM_BUCKETS = 256` and every derived constant (`RANK_RECORD_SIZE`, `RESOLVED_ENTRY_SIZE`, `COORD_SLOT_SIZE` constants tied to the shard format) — **gone**.
- `RankRecord`, `ResolvedEntry`, `SlotBucketRef`, `SharedSlotBuckets`, `slot_bucket_bounds`, `NodeBlobInfo` rank-bucket logic — **gone** (keep `NodeBlobInfo` itself for Phase 2's blob→rank-range mapping).
- Every `rank-W*-*`, `slot-*`, `payloads-W*`, and scratch-directory path — **gone**.
- `ScratchDir` usage in ALTW — **gone**. Keep `ScratchDir` in `external_radix.rs` for other callers (e.g. `renumber`'s pathway if it uses it — it doesn't currently, but the module stays).
- Both Phase-A/Phase-B per-worker rank-bucket writer scaffolding — **gone**.

**Retained (surface unchanged):**

- [`blob_meta.rs`](../src/commands/altw/blob_meta.rs) — the header-only metadata scan is still wanted; build_all_blob_schedules in renumber_external is essentially the same thing, so consider unifying these, but not on the critical path.
- Per-way refcount sidecar (binary format unchanged). Phase 1 writes it; Phase 3 reads it to drive field-9/10 splice.
- Wire-format way-reframe splice core from [`reframe_way_blob_with_locations`](../src/commands/altw/stage4.rs#L920) — keep the group-walking + field-9/10-emission machinery; change only the coord source.
- [`scan_way_refs`](../src/commands/way_scanner.rs), [`extract_node_tuples`](../src/commands/node_scanner.rs) — untouched.
- `require_indexdata` / `require_sorted` gating — unchanged.
- `IdSetDense` — **no API change**. Use exactly as renumber does.
- `PbfWriter` — **no API change**. Use exactly as renumber does.

Estimated net code delta: **`src/commands/altw/` drops from 8 files / ~2100 LOC to 2 files / ~600 LOC**, living as a close sibling of `renumber_external.rs`.

## What survives from prior plans

Prior structural notes (`altw-structural-reports.md`, `altw-external-optimization-plan.md`) propose a long list of incremental patches. Under this reshape, most of them become moot because the seams they attack don't exist. What survives:

- **Counter-based ranking in the node scan (Phase 2).** `get(id) + counter` instead of `rank_if_set` per tuple. Saves ~4 B rank calls (non-referenced tuples) × ~30 ns ≈ 20 s aggregate at planet. Kept.
- **`cat`-side refcounts in BlobHeader field 5.** Still a platform-level win: Phase 1's per-way refcount sidecar could be replaced by a read from BlobHeader, avoiding the sidecar-write pass. Small against a 3-pass architecture. **Defer** — land it as a follow-up once this reshape ships.

Everything else — stage-2→3 epoch-spill, coord_payloads streaming, slot-bucket record shrinking, rank-bucket tuning, relation-member forward-fold, cat-side referenced-node bitmaps, three-level straddler state machines — is moot under the new architecture. The seams don't exist.

## Correctness invariants

- **Sorted + indexed PBF precondition.** Enforced at `external_join` entry. Unchanged.
- **Orphan refs emit `(0, 0)`.** A ref whose `node_id` is not in `IdSetDense` hits the `None` branch in Phase 3's lookup and emits `(0, 0)`. Matches current semantics.
- **Zero-coord sentinel.** `coord_table` is zero-initialized once. Phase 2 only writes at ranks Phase 1 registered via `set_atomic`. Phase 3's `rank_if_set` returns `Some(rank)` iff `set_atomic` was called, so an unwritten slot in `coord_table` can only be read when `None` is returned (and we emit `(0, 0)` explicitly) — never via a `Some(rank)` path. Safety: the zero-initialization is still required for `MADV_DONTNEED`-recovered pages that get re-touched during writes.
- **Per-way refcount ordering.** Phase 1 writes sidecars in PBF blob order via the reorder buffer; Phase 3 consumes in PBF blob order. Unchanged.
- **`missing_locations` stat.** Computed inline in Phase 3 by counting `None` branches of `rank_if_set`. Matches `total_slots - resolved_count` from the current pipeline.
- **`LocationsOnWays` output feature flag.** Set on the output header. Unchanged.

## Plan of attack (executed 2026-04-16 — failed at step 4)

1. ❌ ~~**Measure `unique_referenced_nodes` on planet.**~~ — **skipped** in practice; implementation ran first. Had this step actually gone first, the 3.6 B Europe / ~10 B planet figures would have killed the plan before any code was written. The caveat inside this step ("if ≥ 3 B, adopt compact coord encoding or defer the reshape") was correct.
2. ✅ **Built `src/commands/altw_v2.rs`** alongside `altw/`. Wired `--index-type external` dispatch to it.
3. ✅ **Cross-validated on Denmark.** Byte-identical output to sparse, dense, and prior external variants.
4. ❌ **Benchmarked.** Denmark 3.5 s (2.8× speedup vs old external's 9.7 s). **Europe OOM-killed** — unique referenced 3.6 B → coord_table 29 GB on a 30 GB host. Planet not attempted.
5. N/A **If targets met.** Not reached.
6. ✅ **Fallback branch applies.** Revert to the external-sort shape — current `altw/*` stays as the production path. Retain `altw_v2` only as a small-input fast path if useful, or delete.

Lesson: when a plan rests on an in-RAM sizing assumption that scales with input data, **measure the real size at the target scale before writing code**. The cost of measurement is minutes; the cost of implementing-first is hours plus a failed OOM run. This applies not just to step 1 here but to any plan elsewhere in the repo that assumes a particular "unique referenced" or similar runtime-measured quantity.

## Open questions

- **Peak `coord_table` RSS vs page-cache pressure.** 16 GB of anon pages from Phase 2 write + 10+ GB of input-PBF page cache during Phase 3 may trigger kernel reclaim. Possibly advise `MADV_RANDOM` on `coord_table` so the kernel doesn't aggressively pre-fetch (Phase 3 access is rank-sparse and mostly uncorrelated).
- **Phase 3 coord-lookup locality.** `coord_table[rank * 8]` where rank is derived from way-local node IDs — locality depends on how much OSM geographic clustering survives into rank ordering. Renumber pays the same rank-lookup cost and hits 194 s; there's no reason ALTW should be worse, but worth an L3-miss-rate check on a Phase 3 dry run.
- **Phase 2 disjoint-slice write safety.** The claim is that `NodeBlobInfo` monotonicity makes each blob's rank write range disjoint. Failure mode is silent coord corruption. Worth a debug-only assertion (`debug_assert!(rank < blob.ref_rank_end)`) in Phase 2's hot loop.
- **Memset overhead of 16 GB coord_table zero-init.** At ~10 GB/s DDR bandwidth, ~1.6 s. Small but nonzero. If visible, parallelize the memset across workers at Phase 2 entry.
- **Interaction with `--direct-io` and `--io-uring`.** Current ALTW supports both. Reshape should inherit them unchanged — all I/O is input-PBF pread and output-PBF write, no temp files.
