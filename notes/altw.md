# add-locations-to-ways: dense and sparse paths

`pbfhogg add-locations-to-ways --index-type dense|sparse`. The third
index type, `external`, is documented in
[`notes/altw-external.md`](altw-external.md) and its optimization arc
in [`notes/altw-optimization-history.md`](altw-optimization-history.md);
this file is dense / sparse only.

Code:
- [`src/commands/altw/mod.rs`](../src/commands/altw/mod.rs) (dispatch
  + pass 0 + pass 2 decode-all fallback).
- [`src/commands/altw/dense.rs`](../src/commands/altw/dense.rs).
- [`src/commands/altw/sparse.rs`](../src/commands/altw/sparse.rs).
- [`src/commands/altw/passthrough.rs`](../src/commands/altw/passthrough.rs)
  (pass 2 indexed path; the planet-recommended dispatch).

## Phases

Both dense and sparse share the surrounding pipeline:

1. **Pass 0** (`collect_way_referenced_node_ids`) -
   `parallel_classify_phase` with a single shared `IdSet`. Per-blob
   workers emit `Vec<i64>` of way refs; main thread unions them.
2. **Pass 1** (`build_node_index`) - diverges:
   - Dense: file-backed mmap (128 GB virtual, OS page cache),
     sequential blob walk on the main thread, then
     `tuples.par_iter().for_each` writes coords to mmap slots via
     `SharedDenseWriter`'s atomic stores.
   - Sparse: single-threaded sequential write through a `BufWriter`
     over a temp file. Chunk layout: 256 IDs per chunk with
     `start_pad` to skip leading empty slots.
3. **Optional rel-member scan**
   (`collect_relation_member_node_ids`, fires when
   `keep_untagged_nodes=false`) - `parallel_classify_accumulate` with
   per-worker `IdSet`, merged at the end.
4. **Pass 2** - dispatches on `indexdata_present`:
   - `write_output_passthrough` (indexed input): two-phase header /
     data read, batches `BatchSlot`s, dispatches to
     `process_slot_batch_dense` or `process_slot_batch` (sparse).
   - `write_output_decode_all` (`--force` on non-indexed input):
     `into_blocks_pipelined` + batch + `par_iter().map_init(
     BlockBuilder).collect()` + drain via `process_batch`.

## Measured baselines (commit 68806b0, plantasjen 2026-04-29)

Wall:

| Dataset | dense | sparse | sparse / dense |
|---|---|---|---|
| Denmark 700 MB | 11.9 s | 17.3 s | 1.45x |
| Japan 2.4 GB | 51.6 s | 1m18s | 1.52x |
| Europe 35 GB | thrashed, killed at 19 min | not run | n/a |

Per-phase wall and peak RSS:

| Phase | DK dense | DK sparse | JP dense | JP sparse |
|---|---|---|---|---|
| Pass 1 wall | 4.4 s | 2.2 s | 23.2 s | 10.7 s |
| Pass 1 peak RSS | 4.1 GB | 1.4 GB | 13.4 GB | 1.9 GB |
| Pass 1 majflt | 826 K | 0 | 3.1 M | 0 |
| Pass 1 avg cores | 9.6 | 1.0 | 8.3 | 1.0 |
| Pass 2 wall | 2.4 s | 11.2 s | 12.0 s | 56.7 s |
| Pass 2 peak RSS | 5.2 GB | 4.4 GB | 14.3 GB | 9.5 GB |
| Pass 2 avg cores | 16.5 | 4.2 | 18.1 | 4.1 |

UUIDs: `c5611d55` (DK dense), `262af1a0` (DK sparse), `a40c5ff7`
(JP dense), `8d035980` (JP sparse).

Counters:

| Counter | Denmark | Japan | Europe (partial) |
|---|---|---|---|
| `altw_referenced_node_ids` | 49 M | 299 M | 3,617 M |
| `altw_relation_member_node_ids` | 25 K | 193 K | 10.6 M |
| `altw_ways_written` | 6.6 M | 42.9 M | (killed) |

## Findings

### Dense fails above ~25 GB working set

Touched mmap pages scale linearly with `altw_referenced_node_ids` x
8 bytes:

- Denmark: 49 M x 8 = 393 MB. Fits trivially.
- Japan: 299 M x 8 = 2.4 GB. Fits, but 3.1 M pass-1 majflt
  show the page cache is already churning.
- Europe: 3,617 M x 8 = 29 GB. Exceeds the 27 GB-free host.
  Catastrophic page-thrash: 12 M majflt in pass 1 (4m18s), 23 M
  majflt in 13 minutes of pass 2 before SIGKILL. 2.1 TB read off
  disk.

This is architectural, not a tuning gap. Pre-pass-0 filtering already
restricts to way-referenced nodes; the working set IS those nodes
times 8 bytes. Above host free RAM, dense cannot work.

### Sparse pass 1 is single-threaded but already faster than dense

`build_node_index_sparse` runs on one thread (avg cores 1.0) writing
through a `BufWriter`. Despite that, it beats parallel dense pass 1
at every measured scale:

- Denmark: 2.2 s vs 4.4 s (sparse 2.0x faster).
- Japan: 10.7 s vs 23.2 s (sparse 2.2x faster).

Dense's parallel write is no faster because mmap page-fault
throughput is the bottleneck, not CPU. Sparse skips the mmap, writes
contiguous bytes to disk, and even single-threaded outpaces it.

### Sparse pass 2 is bottlenecked at ~4 cores

Avg cores 4.2 (DK) / 4.1 (JP) vs dense's 16-18. The bottleneck is
`resolve_batch_locations` running serially before the per-blob
`par_iter` dispatch: one thread dedups + sorts by mmap offset +
sequentially scans the entire batch's unique node refs through the
mmap before workers get to start.

### Rel-member scan IdSet bloat scales hard

`collect_relation_member_node_ids` uses `parallel_classify_accumulate`
with one IdSet per worker. Per-phase anon delta during the scan:

- Denmark: +0.9 GB.
- Japan: +2.5 GB.
- Europe: +9.7 GB.

24 workers x ~400 MB per IdSet at europe. The mod.rs doc comment
claimed ~68 MB / worker; measurement disagrees by 6x. Linear
extrapolation: planet ~24 x ~3 GB = **~72 GB**. Same shape that bit
tags-filter `--invert-match` (28.3 GB peak anon -> 7.0 GB after the
2026-04-28 migration); same fix template available.

### What is NOT the bottleneck

The `par_iter().map_init(BlockBuilder).collect()` shape in pass 2
(mod.rs `process_batch`, passthrough.rs `process_slot_batch_dense`
and `process_slot_batch`). Peak anon stays under 4 GB at every
measured scale, including europe before the dense mmap thrash
dominated the sidecar profile.

The shape != root cause lesson holds; see commit `48685ba` (getid
add-referenced) and the tags-filter `9d41465` doc landing for two
prior incidents where the same shape was suspected and measurement
ruled it out.

## Ranked work

Targets in priority order. None landed.

### 1. Migrate rel-member scan off `parallel_classify_accumulate`

**Why first:** real planet-scale blocker independent of index-type
choice. Affects every `keep_untagged_nodes=false` run, which is the
default.

**Shape:** mirror the tags-filter way-deps migration (commit
`17b116c`). Replace `parallel_classify_accumulate` with
`parallel_classify_phase`: per-blob worker emits `Vec<i64>` of
relation member node IDs, main thread unions into a single shared
IdSet. Bounds memory to one IdSet plus per-blob transient vectors,
not N-workers x per-worker IdSet.

**Test surface:** existing CLI integration tests cover the
keep / drop semantics; set-union is commutative so the migration
preserves correctness by definition.

### 2. Parallelize sparse `build_node_index_sparse`

**Why:** sparse is structurally needed at scale (dense fails) and is
single-threaded today. Compounds the win sparse already shows over
dense.

**Constraint:** "strictly increasing node IDs" comes from the chunk
layout (each chunk is a contiguous run with `start_pad`). Inter-blob
ordering is satisfied by the input PBF's sort; parallel workers can
violate that if they emit out of seq order.

**Shape sketch A:** `parallel_classify_phase` with workers emitting
`(seq, chunk_run)` outputs; main thread drains in seq order through a
`ReorderBuffer` and writes the chunk runs to the `BufWriter`. Each
chunk run is the same internal layout as today (sentinel-padded run
of (lat, lon) pairs plus chunk-index updates).

**Shape sketch B:** workers write per-blob temp files; main thread
concatenates in seq order. Simpler than streaming through the shared
writer but adds N file handles and a final concatenation pass.

### 3. Parallelize sparse pass 2 `resolve_batch_locations`

**Why:** sparse pass 2 avg cores 4.2 vs dense's 16-18. The serial
resolve step is leaving 75% of CPU idle.

**Constraint:** the batched sorted lookup is sequential-mmap-friendly
precisely because the sort + scan happens once over the whole batch.
Splitting across workers loses some of that locality.

**Shape:** measure-first. Two candidates:

- Per-worker resolve over a slice of the batch's blocks. Each
  worker dedups + sorts + scans its own slice. Loses cross-block
  dedup, but at typical batch sizes the cross-block overlap is
  small.
- Fold the resolve into the per-worker process loop: each worker
  random-reads its block's refs against the sparse mmap as it
  processes. Loses the sequential-scan advantage; at small batches
  the random reads might still hit page cache.

## Don't re-attempt

- **`parallel_classify_accumulate` with per-worker IdSet at scale.**
  The doc caution at `src/scan/classify.rs:300-317` lists the
  criteria. The rel-member scan above is an open example.
- **Dense at planet without 30+ GB free RAM.** Page-thrashing is
  architectural, not a tuning gap. External or sparse, not dense.

## Cross-references

- [`notes/altw-external.md`](altw-external.md): the third index
  type, structurally different (external join via double radix
  permutation), already optimized.
- [`notes/altw-optimization-history.md`](altw-optimization-history.md):
  the external optimization arc.
- [`src/scan/classify.rs`](../src/scan/classify.rs):
  `parallel_classify_phase` (streaming, single shared state) vs
  `parallel_classify_accumulate` (per-worker state, merged at end).
  The choice criteria at lines 300-317 are load-bearing.
- Migration template precedents: time-filter snapshot (commit
  `83183fb`), tags-filter way-deps (`17b116c`), `cat --clean`
  (`b347c0a`), `check --ids` streaming (`516129e`).
