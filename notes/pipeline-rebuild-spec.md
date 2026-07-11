# Implementation spec: ordered-pipeline batch rebuild (gate `PBFHOGG_BATCHED_PIPELINE`)

Item 4 of `notes/env-gated-readpath-batch.md` (the plan of record, rev 2 -
its Mechanics section binds every gate rule used here). Written against
`reference/technical-implementation-spec.md`. Spawned from the TODO.md
"Ordered-pipeline batch rebuild" item; full motivating analysis in
`notes/read-path-architecture-reports.md` (both reports, section 5 of the
codex report and section 3.2 of the Fable report). Test placement and
tiering follow `reference/testing.md`.

**Process deviation, inherited from the plan:** the standing spec contract
forbids env-var experiment switches; the user explicitly overrode that for
this batch (2026-07-11). The gate is scaffolding with a pre-registered
morning verdict that either flips the default and deletes the gate, or
deletes the gated code. Everything else in the contract holds: exact
copy-pasteable gate commands, explicit `--dataset`/`--variant` pins,
pre-registered keep/revert thresholds.

**Hard constraint (plan rev 2, binding):** gate-off is byte-identical to
today's shipped behavior. The rebuild is a parallel path selected at the
gate; it does not refactor the seams the default path runs through. This
spec satisfies that without unreasonable duplication (the new engine is one
self-contained module; see "Duplication decision" below), so the item does
not return to the user under the escape clause.

---

## 1. Survey of the ground

### 1.1 The current ordered pipeline

`run_pipeline` in `src/read/pipeline.rs` is a three-stage per-blob
pipeline:

- **Stage 1** (spawned reader thread): `BlobReader::next` per frame, sends
  `(seq, Result<Blob>, Option<BytePermit>)` into a `sync_channel` of
  capacity `read_ahead` (default 16; count backstop 256 when
  `read_ahead_bytes` is set).
- **Stage 2** (spawned dispatcher thread): builds a dedicated rayon pool of
  `decode_thread_count.unwrap_or(available_parallelism - 2)` threads, then
  per blob: acquires an `AdmissionGate` permit (cap `decode_ahead`, default
  32; backstop 512 under a byte budget), optionally acquires a decoded-byte
  `BytePermit` (in sequence order, before dispatch - load-bearing against
  stranding under tiny budgets), and `rayon::spawn`s a `DecodeTask`. The
  task checks `should_skip_blob` (BlobFilter against indexdata/tagdata),
  decodes via `to_primitiveblock_inline_with_scratch` (thread-local
  `ST_SCRATCH`/`GR_SCRATCH`, shared `DecompressPool`), and sends
  `(seq, payload, permit, byte_permit)` into the decoded `sync_channel`
  (capacity `decode_ahead`). Non-OsmData blobs and filter-skipped blobs
  deliver `None`; panics are caught and converted to an Io error
  ("decode task panicked").
- **Stage 3** (calling thread, `drain_decoded`): a `ReorderBuffer` keyed by
  blob seq restores file order and calls the caller's
  `FnMut(PrimitiveBlock) -> Result<()>`. Permits drop only after ordered
  delivery, so admitted-but-undelivered decode work is capped.

Per-blob costs at the seams: one channel send/recv pair per stage, one
rayon task spawn, one admission-gate mutex round trip, one permit
alloc/drop, one reorder insertion. Both architecture reports converge on
the diagnosis: tolerable at planet-primary blob counts (50,816), structural
overhead at high blob count (planet-8k: 1,453,433 blobs). Direct in-tree
measured evidence that the per-blob machinery is a real scaling hazard:
ADR-0006 records that routing getid's scan through this pipeline measured
62 % slower than plain sequential streaming reads on the 8k planet
(53.9 s vs 33.2 s, "per-blob pipeline overhead times 1.45 M small blobs").

Shutdown/error semantics that must be preserved exactly:

- Errors (read/framing or decode) travel in sequence position; blocks
  before the erroring seq are delivered, then the error returns. The first
  error in file order wins.
- A `block_fn` error or an early consumer drop closes the decoded receiver;
  blocked senders fail, set the shutdown flag, release permits, and stage 1
  stops at the raw channel - promptly, without reading the rest of the
  file (pinned by `early_exit_does_not_read_whole_file`).
- The ordered `FnMut` callback runs serialized on the calling thread.
- `PIPELINE_METRICS.emit()` fires even on the error path.

### 1.2 Entry points and the gate's single read site

`ElementReader` (`src/read/reader.rs`) holds a `PipelineConfig` populated
by `PipelineConfig::from_env()` in every constructor (`new`, `from_path`,
`from_path_direct`, `open`). That function is already the single env entry
point for `PBFHOGG_READ_AHEAD_BYTES` / `PBFHOGG_DECODE_AHEAD_BYTES` (item 2,
landed at `f3a5bee`/`687fe84`); `block_queue_bytes_from_env` covers
`PBFHOGG_BLOCK_QUEUE_BYTES` the same way. The new gate follows the
identical pattern - env read once at `PipelineConfig::from_env`, plumbed as
a plain field below it - which is exactly the plan Mechanics' pinned
test mechanism (no `std::env::set_var` anywhere; tests drive the parameter
API or a child process).

`run_pipeline` has exactly two callers, both in `reader.rs`:

- `for_each_block_pipelined` (which `for_each_pipelined` wraps, adding the
  Sort.Type_then_ID debug assertion above the seam), and
- `into_blocks_pipelined`, which runs `run_pipeline` on a background thread
  behind a `BLOCK_QUEUE`-bounded channel plus optional
  `block_queue_bytes` budget, yielding `PipelinedBlocks`.

Nothing else in the tree calls `run_pipeline`. The dispatch to the new
engine therefore lives at these two call sites and nowhere else.

### 1.3 Genuine consumers (cell-reachability, reconciled)

Confirmed by call-site inventory, agreeing with the plan's rev-2
corrections and `reference/pipelined-reader-paths.md`:

| Consumer | Path | Reaches `run_pipeline` when |
|---|---|---|
| bench-read pipelined variant | `cli/src/main.rs` `run_bench_read`, `for_each_pipelined` | always (the `brokkr read` pipelined row) |
| time-filter | `src/commands/time_filter/mod.rs`, `for_each_pipelined` | **history input only** - snapshot input dispatches to `time_filter_snapshot` (`parallel_classify_phase`, pread) |
| build-geocode-index pass 1 | `src/geocode_index/builder/pass1.rs`, `for_each_block_pipelined` + `only_relations` filter | always |
| getid `--add-referenced` pass 2 | `src/commands/getid/mod.rs`, `into_blocks_pipelined` + `for_each_primitive_block_batch` | always in add-referenced mode (plain include mode never runs pass 2; the 8k include arm is deliberately sequential streaming per ADR-0006) |
| getparents FullScan arm | `src/commands/getparents/mod.rs`, `into_blocks_pipelined` + batch classify | estimated OSMData blobs >= 150,000 (ADR-0006); planet primary (~36 k) takes the walker arm |
| tags-filter `-R` single-pass | `src/commands/tags_filter/mod.rs`, `into_blocks_pipelined` + batch | `-R` only; default/transitive is the two-pass pread path |
| altw decode-all fallback | `src/commands/altw/mod.rs` `write_output_decode_all`, `into_blocks_pipelined` | non-indexed input with non-external index type only |

**Correction the plan must absorb (miswired-cell class):** the plan's item-4
overnight pair `run brokkr time-filter --dataset planet --bench 1` is
INERT for this gate. Planet primary is a snapshot PBF (no
`HistoricalInformation`), so `time-filter` dispatches to the snapshot path,
which uses `parallel_classify_phase` and never executes `run_pipeline`.
No history dataset is configured in `brokkr.toml` on any host, so no
brokkr-reachable time-filter cell can exercise the gate. Section 5 replaces
that pair with a getparents-8k pair (genuine FullScan consumer, baseline
already shared with items 2 and 3). The history path's correctness is
covered at fixture scale by the CLI equivalence test in brick 4.

Also noted for the batch's doc rider (plan item 7, not this spec's brick):
`reference/pipelined-reader-paths.md` still lists getparents under
"deliberately not pipelined" - stale since the ADR-0006 FullScan arm.

### 1.4 Precedent: the landed par_map_reduce fold

`par_fold_blobs` in `reader.rs` (commit `7532021`) already implements the
byte-bounded batch + long-lived-worker shape this rebuild needs, minus
ordering: a frame pump on the calling thread admits compressed blobs
against a shutdown-capable `ByteBudget` (oversized-when-empty rule, so a
lone giant blob cannot deadlock), gathers count- and byte-capped `Batch`es
into a closable `BatchQueue`, and long-lived scoped workers decode with
worker-local scratch. Its RAII inventory (`BatchCharge` frees compressed
storage before releasing budget; `CancelGuard` turns any unwind into
queue-close + budget-shutdown so no thread strands) is the hardened
solution to exactly the deadlock/panic surface the ordered engine will
share. The ordered engine copies these shapes (see Duplication decision)
and adds: batch sequence numbers, a decoded-byte budget acquired by the
pump in sequence order, and an ordered consumer with a batch-level
`ReorderBuffer`.

### 1.5 Failure history and standing decisions

- **ADR-0006** (`decisions/0006-blob-count-threshold-dispatch.md`) governs
  which arm getid/getparents run. This spec changes NOTHING about
  dispatch: the batched engine replaces the machinery *behind* the
  pipelined arm, gate-on only. The two refuted shapes recorded there
  (consumer-thread classify; pipelined reader as getid's scan arm) are not
  re-proposed - the second one is this item's *motivation* read from the
  other side: if batching removes the per-blob overhead that lost 62 % to
  sequential streaming, the FullScan arm gets faster where ADR-0006 already
  dispatches to it. Re-litigating the 150 k threshold is explicitly out of
  scope and would be a future ADR-0006 amendment with its own measurements.
- **`reference/pipelined-reader-paths.md` standing rule:** never convert a
  pipelined caller to sequential decode (getparents sequential conversion
  regressed 4.7x on denmark, commit `c912e4d`, reverted). The rebuild keeps
  parallel decode; the rule is honored.
- **ADR-0005** (latent-invariant debug asserts): the new engine's clean-EOF
  reorder-drained invariant gets a `debug_assert!`, per that precedent.
- `CORRECTNESS.md` / `DEVIATIONS.md`: untouched - no parser, encoder, or
  osmium-parity behavior changes; element delivery order and error
  semantics are preserved bit-for-bit at the API surface.
- **ADR deliverable:** a KEEP verdict makes the batched ordered engine the
  production spine - that is ADR-worthy. `decisions/0008-batched-ordered-
  pipeline.md` is a named deliverable of the morning keep path (section 6),
  not of the gated landing.

### 1.6 Reconciliation with item 3 (command-transform fusion) - same seam

The fusion spec (`notes/fusion-spec.md`) is written after this one and
layers on it. Binding shared facts, so neither spec's cells adjudicate
dead code:

1. All four fusion targets reach the seam through `into_blocks_pipelined`
   followed by `for_each_primitive_block_batch` (or a direct block loop for
   altw decode-all). Reachability corrections (getid `--add-referenced`
   only; getparents 8k = FullScan signal, primary = walker control;
   tags-filter must pin `-R`; altw decode-all = raw + non-external only)
   are confirmed by this survey - section 1.3's table is the shared ground
   truth.
2. This item does NOT touch `for_each_primitive_block_batch`,
   `for_each_primitive_block_batch_budgeted`, or any command module.
   Fusion owns those seams; zero file overlap on the command side.
3. **Layering contract this spec guarantees to item 3:**
   - `for_each_block_pipelined`, `for_each_pipelined`, and
     `into_blocks_pipelined` keep their exact signatures and ordering
     contracts under both engines. Fusion may build any new surface on top
     of them.
   - Inside the batched engine, per-blob decode is factored as one named
     function (`decode_batch_entry`, section 2.5) so a worker-side
     transform hook can interpose after decode without re-deriving the
     engine. Item 3's gate-on-both-gates arm (the combination cell) runs
     its transform inside these batch workers; its gate-on-fusion-only arm
     runs inside the default engine's rayon decode tasks. Fusion must
     define both arms; this spec only reserves the seam.
   - Budget accounting under fusion: the decoded-byte permit is charged
     for the decoded block regardless of what the transform emits, and is
     released at ordered delivery of the batch. Fusion must not release it
     early even when the transform discards the block.
4. **Gate interactions (plan Mechanics):** the only supported combination
   is `PBFHOGG_BATCHED_PIPELINE=1 PBFHOGG_FUSE_TRANSFORM=1`. This spec's
   knob decision (section 2.7): the batched engine HONORS
   `PBFHOGG_READ_AHEAD_BYTES` and `PBFHOGG_DECODE_AHEAD_BYTES` as budget-cap
   overrides and IGNORES the count knobs (`read_ahead`/`decode_ahead`
   builder values and their defaults - the batched engine's admission is
   byte-native). `PBFHOGG_BLOCK_QUEUE_BYTES` and `PBFHOGG_CMD_BATCH_BYTES`
   sit above the `run_pipeline` seam and apply unchanged under either
   engine. A code comment in the new module states all of this (plan
   requirement).
5. The tags-filter `-R` overnight pairs are shared: one baseline per
   dataset serves both items, with one gated twin per item per dataset
   (env var distinguishes them; `capture_env` stores it on the result
   row). The `-R` expression is pinned by the fusion spec and MUST be the
   same expression in this item's cells.

### 1.7 Measurement record and baseline

Standing numbers this item is read against: the planet baselines table in
`reference/performance.md` (commit `16e3694` era rows plus the ADR-0006
gate cells of 2026-07-11: getparents planet-8k 62.0 s `595e8d7e`, getid
planet-8k 48.6 s `ddf6fed4`, tags-filter `-R` 51.8 s `cf116a6b`), the
blob-density matrix in `reference/blob-density.md`, and raw rows in
`.brokkr/results.db`. Per the plan, the authoritative pre-change baseline
for every verdict is the SAME-NIGHT baseline cell measured by
`overnight.sh` at the batch's HEAD commit on plantasjen - one binary, one
commit, same-day A/B by construction (the ADR-0006 landing showed
cross-day I/O-bound walls can drift ~45 % from environment alone).
Read-pair verdicts are wall-only (brokkr stores no sidecar for `read`
rows); command pairs may additionally read `brokkr sidecar --compare` for
per-phase RSS. After the morning verdict, numbers settle into
`reference/performance.md` (new current) and
`reference/performance-history.md` (arc + superseded baselines), per the
plan's step 5.

---

## 2. Target artifact

### 2.1 Gate plumbing

`PipelineConfig` (in `src/read/pipeline.rs`) gains one field:

```rust
pub(crate) struct PipelineConfig {
    pub(crate) read_ahead: usize,
    pub(crate) decode_ahead: usize,
    pub(crate) read_ahead_bytes: Option<usize>,
    pub(crate) decode_ahead_bytes: Option<usize>,
    pub(crate) batched: bool,          // NEW - default false
}
```

`PipelineConfig::from_env()` reads the gate - the single env read site:

```rust
batched: bool_gate_from_env("PBFHOGG_BATCHED_PIPELINE")?,
```

`bool_gate_from_env(var) -> Result<bool>` (new, next to
`byte_budget_from_env`): unset -> `false`; `"1"` -> `true`; `"0"` ->
`false`; any other value (including non-unicode) -> the same
`InvalidInput`-flavored hard error shape `byte_budget_from_env` uses,
naming the variable. Silent misspellings of the value must not read as
"off".

Scaffolding builder on `ElementReader` (deleted with the gate on either
verdict; `ElementReader` is on the stable test allowlist so
`tests/read_paths.rs` may call it):

```rust
#[doc(hidden)]
pub fn batched_pipeline(mut self, enabled: bool) -> Self {
    self.pipeline_config.batched = enabled;
    self
}
```

Dispatch at the two existing `run_pipeline` call sites in `reader.rs`,
each a two-arm branch whose `false` arm is the existing call, verbatim:

```rust
// in for_each_block_pipelined:
if self.pipeline_config.batched {
    super::batched_pipeline::run_batched_pipeline(
        self.blob_iter, self.decode_threads, self.pipeline_config,
        self.blob_filter, f)
} else {
    super::pipeline::run_pipeline(
        self.blob_iter, self.decode_threads, self.pipeline_config,
        self.blob_filter, f)
}
```

`into_blocks_pipelined` gets the same branch inside its spawned-thread
closure, wrapping the identical `|block| { queue permit + tx.send }`
closure (bound to a local so it is written once). The `BLOCK_QUEUE` /
`block_queue_bytes` machinery around it is untouched and applies to both
engines.

Gate-off byte-identity argument: `pipeline.rs`'s engine is not edited
beyond the one added config field + parse helper; the dispatch `false`
arms are today's calls; the new field defaults to `false` in `Default`
and reads `false` when the var is unset. The whole existing test suite
plus the gates in section 4 confirm structurally.

### 2.2 New module: `src/read/batched_pipeline.rs`

One self-contained module holding the engine and its primitives.

**Duplication decision.** The module carries its own copies of the four
proven primitives from `par_fold_blobs` (`Batch`, `BatchQueue`,
`ByteBudget` with shutdown + oversized-when-empty, `CancelGuard`,
`BatchCharge`), adapted for ordering, rather than lifting the originals
out of `reader.rs` into a shared module. Rationale: the revert path must
be "delete one module + the dispatch arms" - moving shared primitives
would entangle the unconditionally-shipped `par_map_reduce` with gated
scaffolding and make revert a re-refactor. The duplication is ~150 lines
and temporary either way: on KEEP, a named follow-up (out of scope here)
merges `par_fold_blobs` onto the surviving primitives. This is the
"unreasonable duplication" clause answered: bounded, deliberate,
lifecycle-driven.

### 2.3 Engine signature

```rust
pub(crate) fn run_batched_pipeline<R, F>(
    mut blob_reader: BlobReader<R>,
    decode_thread_count: Option<usize>,
    pipeline_config: PipelineConfig,
    blob_filter: Option<BlobFilter>,
    mut block_fn: F,
) -> Result<()>
where
    R: Read + Send,
    F: FnMut(PrimitiveBlock) -> Result<()>,
```

Identical to `run_pipeline`, including generic `R: Read + Send` (no pread,
no seeking - the pump is a sequential reader, so non-file readers keep
working and kernel readahead is preserved). Annotated `#[hotpath::measure]`
like its sibling. Entry behavior mirrors `run_pipeline`: set
`parse_tagdata` iff the filter has a tag filter, `parse_indexdata` iff any
filter is set.

### 2.4 Thread topology and data flow

```text
pump thread (spawned)                 N worker threads (spawned, scoped)
  read blobs sequentially               pop BatchMsg from BatchQueue
  skip non-OsmData                      per blob: filter-check, decode
  acquire raw budget per blob             (worker-local scratch + pool)
  acquire decoded budget per batch      send (batch_seq, DecodedBatch)
    (in batch-seq order)                free compressed storage,
  push BatchMsg to BatchQueue             release raw budget (BatchCharge)
                    \                        |
                     v                       v
              consumer = CALLING thread: recv, ReorderBuffer by batch_seq,
              deliver blocks in file order to block_fn, drop decoded permit
              after the batch's last block
```

Threads: caller (consumer) + pump + `worker_count` workers, where
`worker_count = decode_thread_count.unwrap_or(available_parallelism - 2,
min 1, fallback 4)` - same sizing rule as today's decode pool, but the
dispatcher thread is gone and thread accounting is finally honest (a
defect both reports called out). No rayon. All threads scoped
(`std::thread::scope`), so borrowing `block_fn` and the filter is safe.

Types:

```rust
struct BatchMsg {
    batch_seq: usize,
    blobs: Vec<Blob>,
    raw_bytes: u64,                        // sum of admitted raw charges
    decoded_permit: Option<DecodedPermit>, // acquired by pump, rides through
}

struct DecodedBatch {
    entries: Vec<Result<PrimitiveBlock>>,  // file order within the batch
    decoded_permit: Option<DecodedPermit>,
}
// worker -> consumer channel item: (usize /* batch_seq */, DecodedBatch)
```

The worker->consumer channel is a `sync_channel` with capacity
`worker_count * 2` - a slot backstop only; real decoded-memory bounding is
the decoded budget. The pump holds a sender clone for direct error
delivery (below); channel closes when pump + all workers have dropped
their senders.

Constants (all in the new module, each with its sizing arithmetic in a
code comment, per the plan's implementer-computes rule):

```rust
const BATCH_MAX_BLOBS: usize = 64;              // count cap, exact
const BATCH_TARGET_BYTES: u64 = 4 * 1024 * 1024; // compressed flush target
const RAW_INFLIGHT_BUDGET: u64 = 32 * 1024 * 1024;
const DECODED_INFLIGHT_BUDGET: u64 = 128 * 1024 * 1024;
const MIN_BLOB_CHARGE: u64 = 1024;              // slot-bounding floor
```

Sizing rationale (goes in the comment): planet primary averages 1,838,309
compressed bytes/blob and ~3,676,618 decoded bytes/blob (the 2x zlib
estimate already recorded in `pipeline.rs`), so today's effective primary
footprints are read_ahead 16 x 1.84 MB ~ 29.4 MB raw and decode_ahead
32 x 3.68 MB ~ 117.7 MB decoded. 32 MiB / 128 MiB are parity round-ups:
primary cells read ~neutral by construction, while the 8k encoding
(~67 KB/blob) admits ~25x more blobs in flight - the same asymmetry logic
item 2 pinned. `BATCH_TARGET_BYTES` at 4 MiB gives ~2-block batches on
primary and ~60-block batches on 8k, mirroring `PAR_BATCH_MAX_BYTES`.
Every blob is charged `max(retained_len(), MIN_BLOB_CHARGE)` against both
budgets' arithmetic inputs (`decoded_len_hint()` for the decoded side,
floored the same way) so a pathological flood of zero-datasize blobs
cannot make queue slot count unbounded under a byte cap.

### 2.5 Pump, worker, consumer - exact behavior

**Pump** (mirrors `pump_blobs`, plus ordering duties):

1. Iterate `blob_reader`. A read/framing `Err` is resolved BEFORE the
   shutdown check (read errors win over decode errors, deterministically -
   same rule the par pump pins). On `Err`: flush the current partial batch
   (with its decoded permit), then send `(next_batch_seq, DecodedBatch {
   entries: vec![Err(e)], decoded_permit: None })` DIRECTLY on the
   consumer channel, then stop. The consumer's reorder delivers it in
   sequence position - blocks before it are delivered first, matching
   today's `send_direct_error` semantics.
2. Skip non-OsmData blobs (they carry no elements; parity with today's
   decode-task `None`).
3. Acquire `max(retained_len, MIN_BLOB_CHARGE)` from the raw budget
   (blocks; returns false on shutdown -> stop).
4. Batch boundaries exactly as the par pump: byte target flushes BEFORE a
   blob that would carry the batch past `BATCH_TARGET_BYTES` (lone
   oversized blob forms its own batch, never split); count cap flushes AT
   `BATCH_MAX_BLOBS`, never past.
5. Before pushing a batch: acquire its decoded charge (sum of floored
   `decoded_len_hint()` over the batch) from the decoded budget as one
   `DecodedPermit`. The pump is the SOLE acquirer of both budgets and
   acquires in batch-seq order - this is the deadlock-freedom argument:
   the consumer always drains its channel into the reorder buffer, workers
   pop the FIFO queue so batch k is decoded no later than k+j, and the
   in-order-acquire rule means capacity for the next-to-deliver batch is
   never stolen by a later one. (Same reasoning as the "reserve before
   dispatch, in sequence order" comment in today's stage 2.)
6. EOF: flush partial batch, drop the queue handle and channel sender,
   `queue.close()`.

**Worker** (long-lived, one loop per thread; mirrors `run_par_worker`):

1. `queue.pop()` until `None`. On `budget.is_shutdown()`, drop the charge
   and exit.
2. `BatchCharge` owns the compressed batch; on ANY exit it frees the blob
   storage first, then releases the raw bytes.
3. For each blob, in order, `decode_batch_entry(blob, filter, pool,
   st_scratch, gr_scratch) -> Option<Result<PrimitiveBlock>>` - the named
   seam item 3 hooks (section 1.6): filter check via the same
   `should_skip_blob` (imported from `pipeline.rs` - a pure function, not
   an engine seam) returning `None` for skips and bumping the skip
   counter; otherwise `to_primitiveblock_inline_with_scratch` with
   worker-local scratch Vecs and the shared `DecompressPool`.
4. First `Err` entry: push it, discard the batch remainder (equivalent to
   today: post-error blocks were decoded but never delivered), and keep
   the worker ALIVE - decode errors do not proactively cancel; the
   consumer returns the error when the reorder reaches it, exactly as
   today. Send the `DecodedBatch`.
5. The whole per-batch decode runs under `catch_unwind`; a panic converts
   to the same "decode task panicked" Io error at that entry position
   (parity with `spawn_decode_task`).
6. Send failure (consumer gone): shut both budgets down, close the queue,
   exit. A worker-scoped `CancelGuard` (armed at loop entry, disarmed on
   clean exit) covers unwinds that escape the catch.

**Consumer** (calling thread, inside the scope, after spawning pump +
workers; a consumer-scoped `CancelGuard` is armed BEFORE the first spawn):

```rust
let mut pending: ReorderBuffer<DecodedBatch> = ReorderBuffer::with_capacity(8);
while let Ok((seq, batch)) = rx.recv() {
    pending.push(seq, batch);
    while let Some(batch) = pending.pop_ready() {
        for entry in batch.entries {
            block_fn(entry?)?;      // Err returns through the guard path
        }
        drop(batch.decoded_permit); // after the batch's last block
    }
}
debug_assert!(pending.filled_len() == 0, "batches lost at clean EOF"); // Ok path only
```

Early return (block_fn error or an `Err` entry): explicitly shut both
budgets down, close the queue, drop `rx`, disarm the guard, let the scope
join, return the error. Consumer panic (block_fn panics): the armed
`CancelGuard` performs the same shutdown during unwind - closing the gap
where idle workers (blocked in `queue.pop`) and a budget-blocked pump
would otherwise deadlock the scope join, the exact hazard the par
engine's guard was built for. Clean EOF: channel closes when all senders
drop; disarm; reduce nothing; return `Ok`.

Reuses `crate::reorder_buffer::ReorderBuffer` unchanged (its stale/dup
asserts hold: batch seqs are pump-assigned, monotonic, unique).

Ordering proof sketch (for the module doc): pump assigns batch seqs
monotonically in read order; blobs within a batch stay in read order in
`entries`; the reorder buffer delivers batches in seq order; therefore
blocks reach `block_fn` in exact file order - the same guarantee
`for_each_pipelined`'s Sort.Type_then_ID debug assertion (unchanged,
above the seam) will keep enforcing on sorted inputs in every debug-build
test run.

### 2.6 Metrics and hooks

The engine calls `PIPELINE_METRICS.emit()` on every exit (parity, error
path included) and populates the counters whose meaning carries over:
`decode_tasks` (one per decoded blob), `blobs_skipped_by_filter`,
`decoded_recv_wait_ns` (consumer recv wait). Per-blob channel/admission
counters that have no batched equivalent stay zero. New counters, emitted
directly: `pipeline_batches`, `pipeline_batch_raw_wait_ns` and
`pipeline_batch_decoded_wait_ns` (pump budget-wait time),
`pipeline_batched_reorder_high_water` (BATCH units - documented as such so
nobody cross-reads it against the per-blob `pipeline_reorder_high_water`).

Test hooks: a `#[cfg(feature = "test-hooks")] pub(crate) mod test_hooks`
INSIDE `batched_pipeline.rs` (per the testing.md "don't consolidate hooks"
rule), static atomics: `STALL_BATCH_SEQ` / `STALLED_READY` /
`RELEASE_STALLED` (stall one worker on a chosen batch to force reorder
skew) and `PANIC_BATCH_SEQ` (panic inside decode for the worker-panic
test), plus a `reset()`. Re-exported alongside the existing
`pipeline_test_hooks` export in `src/read/mod.rs` for `tests/read_paths.rs`.

### 2.7 Knob semantics (plan-required decision, stated in a code comment)

- `PBFHOGG_READ_AHEAD_BYTES` -> overrides `RAW_INFLIGHT_BUDGET` cap.
- `PBFHOGG_DECODE_AHEAD_BYTES` -> overrides `DECODED_INFLIGHT_BUDGET` cap.
- Count knobs (`read_ahead`, `decode_ahead`, their defaults and 16x
  backstops) are IGNORED by the batched engine - its admission is
  byte-native and `BATCH_MAX_BLOBS` is the only count bound. The builder
  methods keep working for the default engine.
- `PBFHOGG_BLOCK_QUEUE_BYTES` (into_blocks queue) and
  `PBFHOGG_CMD_BATCH_BYTES` (command batching) live above this seam and
  are unaffected by the gate.

If item 2's morning verdict deletes those byte knobs while item 4 keeps,
the overrides collapse to the constants - no coupling beyond the two
`Option<usize>` fields both items already share.

---

## 3. Bricks

Each brick lands separately with its gate green before the next; the
sequence keeps `brokkr check` green at every boundary. Datasets per gate
are the smallest that answer the gate's question: fixture-scale for
wiring/ordering, denmark for real-data cross-validation, planet/europe
only in the overnight (scale questions only).

### Brick 0 - CliInvoker env support (instrument)

`tests/common/cli.rs`: add

```rust
pub fn env<K: AsRef<OsStr>, V: AsRef<OsStr>>(mut self, key: K, val: V) -> Self {
    self.cmd.env(key, val);
    self
}
```

No test uses it yet; it is the instrument brick 4's CLI tests (and item
3's, later) require - the plan Mechanics' pinned mechanism for real
env-path coverage. Gate: `brokkr check`.

### Brick 1 - the engine module

`src/read/batched_pipeline.rs` per section 2, registered in
`src/read/mod.rs`. NOT yet reachable from any public path (no dispatch
yet), so gate-off behavior is trivially untouched. Includes inline
`#[cfg(test)]` tier-1 unit tests (die with the module on revert, which is
the point):

- budget: blocks at cap until release / admits oversized when empty /
  shutdown wakes a blocked acquirer (adapted from the par-engine tests);
- queue: drains then closes; pop blocks without stalling peers;
- pump batching: count cap exact, byte target flushes before overflow,
  lone oversized blob forms its own batch, floored charges applied;
- direct-error sequencing: read error is delivered after all
  earlier-seq batches (fixture-level, via a small in-memory `Cursor`
  reader with a corrupt tail frame).

Gate: `brokkr check`.

### Brick 2 - gate plumbing

`PipelineConfig.batched` + `bool_gate_from_env` + the `#[doc(hidden)]`
builder + the two dispatch arms in `reader.rs` (section 2.1). Gate-off
byte-identity is structural (false arms verbatim). Gates:

- `brokkr check` (full default suite runs gate-off through the untouched
  engine),
- `scripts/envrun.sh PBFHOGG_BATCHED_PIPELINE=1 brokkr read --dataset denmark`
  (execution smoke on real data, all four variants complete; denmark
  cannot show the win and is not read for one).

### Brick 3 - both-states equivalence, shutdown, ordering tests

In `tests/read_paths.rs` (stable-allowlist file; `ElementReader` +
`BlobFilter` + `Element` only), gate-on twins via the parameter API
`.batched_pipeline(true)` - never `set_var`:

- `batched_pipelined_matches_sequential`
- `batched_for_each_matches_pipelined_across_compressions` (full element
  materialization, None/Zlib/Zstd - the deep-equivalence template that
  already exists for the default engine)
- `batched_block_iterator_matches_pipelined`
- `batched_block_iterator_early_drop_under_pressure` (tiny byte budgets
  via `.read_ahead_bytes(1).decode_ahead_bytes(1)`, recv_timeout guard)
- `batched_block_fn_error_stops_pipeline`
- `batched_early_exit_does_not_read_whole_file` (CountingRead harness)
- `batched_matches_sequential_tiny_byte_budgets` (budgets bind, order holds)
- `batched_blobfilter_only_ways_skips_node_blobs_on_indexed_input` and the
  non-indexed conservative-pass-through twin
- `batched_decode_error_surfaces_after_prior_blocks` (adversarial payload
  mutation via `tests/common/adversarial.rs`; asserts Err surfaces and the
  pre-error block count matches the default engine's)
- `#[cfg(feature = "test-hooks")] batched_ordering_bounded_under_stalled_worker`
  (stall hook on batch 1; asserts completion, exact order, and
  reorder-high-water bounded)
- `#[cfg(feature = "test-hooks")] batched_worker_panic_reports_error`
  (PANIC hook; asserts the "decode task panicked" error, no hang)
- `batched_thread_count_parity` (`.decode_threads(1)` vs
  `.decode_threads(8)` identical output - testing.md's `-j` parity leg;
  the scratch-leak leg is N/A: the engine creates no files, stated here so
  the triple is accounted for)

CLI env-path tests (real child process, brick-0 `.env()`):

- `tests/cli_time_filter.rs::batched_gate_env_matches_default` - the
  existing history fixture, run twice (env unset vs
  `PBFHOGG_BATCHED_PIPELINE=1`), output files byte-compared. This is the
  one genuine `run_pipeline` consumer with an existing CLI fixture, and it
  covers the history path the overnight cannot reach (section 1.3).
- `tests/cli_time_filter.rs::batched_gate_rejects_invalid_value` - env
  `PBFHOGG_BATCHED_PIPELINE=2`, assert_failure, stderr names the variable.

Placement: root/tier-1 modules of their files (fixture-scale, cheap,
every-edit contracts). Gates:

- `brokkr check`
- `brokkr test batched_for_each_matches_pipelined_across_compressions --sweep all`
- `brokkr test batched_gate_env_matches_default --sweep all`

### Brick 4 - denmark cross-validation (the landing gate)

- `scripts/envrun.sh PBFHOGG_BATCHED_PIPELINE=1 brokkr verify all --dataset denmark --variant indexed`
  - zero diffs, modulo the documented parity exceptions in
  `reference/osmium-parity.md`. Denmark suffices: correctness here is
  wiring and ordering, not scale, and denmark is the smallest dataset the
  verify harness cross-validates every consumer command on. Gate-on
  reaches the batched engine through every pipelined consumer the verify
  suite drives; fixture tests (brick 3) carry the consumers verify cannot
  reach at denmark (history time-filter, altw decode-all raw fallback,
  forced-low-threshold getparents FullScan - the latter two also get
  CliInvoker equivalence coverage from item 3's spec per its plan entry,
  reconciled in section 1.6).
- Re-run `brokkr check` at the commit boundary.

Commit (one commit, gate documented in the body, results.db + dirty
markdown ride along per the standing rules). Benchmark discipline: commit
first; the overnight measures this commit.

### Brick 5 - overnight cells (handoff to the plan's overnight.sh rewrite)

Item-4 cells, corrected per section 1.3 (the orchestrator writes
overnight.sh; this spec pins the cells and the correction):

- Read pairs (shared baselines per the plan's layout; verdict = pipelined
  variant wall only, fourth-to-fourth UUID pairing, wall-only):
  - `run brokkr read --dataset planet --bench 1` +
    `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr read --dataset planet --bench 1`
  - `run brokkr read --snapshot 8k --dataset planet --bench 1` +
    `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr read --snapshot 8k --dataset planet --bench 1`
  - `run brokkr read --dataset europe --bench 1` +
    `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr read --dataset europe --bench 1`
- **REPLACED CELL** (was the time-filter planet pair, inert per section
  1.3): getparents 8k pair, baseline shared with items 2/3:
  - `run brokkr getparents --dataset planet --snapshot 8k --bench 1` +
    `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr getparents --dataset planet --snapshot 8k --bench 1`
- getid single-gate isolation cell (shares item-3's 8k getid baseline;
  separates BATCHED's solo command effect from the combination):
  - `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr getid --dataset planet --snapshot 8k --add-referenced --bench 1`
- tags-filter `-R` gated twins on the item-3-shared baselines (expression
  pinned by the fusion spec, identical across both items' cells):
  - `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr tags-filter --dataset planet -R <expr> --bench 1`
  - same with `--snapshot 8k`
- Combination cell (the end-state candidate, vs item-3's 8k getid
  baseline):
  - `run env PBFHOGG_BATCHED_PIPELINE=1 PBFHOGG_FUSE_TRANSFORM=1 brokkr getid --dataset planet --snapshot 8k --add-referenced --bench 1`

Budget note for the plan: dropping the two time-filter cells funds the
getparents gated twin (~1 min) and the getid isolation cell (~1 min) with
room to spare (time-filter planet was one of the heavy command cells).

**Pre-registered verdict (plan noise floor):** KEEP requires >= 3 %
improvement on at least one signal cell - 8k read (pipelined variant),
europe read (pipelined variant), getparents 8k, tags-filter `-R` 8k, or
the getid isolation cell. Planet-primary pairs must sit inside +/-3 %
both directions (no-regression controls). Any paired cell regressing
> 3 % is an automatic revert. Read verdicts wall-only; command verdicts
wall plus `brokkr sidecar --compare` RSS as supporting evidence.

---

## 4. Full re-verification protocol (the TODO item's named obligation)

The TODO entry carries "verify all, shutdown/early-exit/ordering tests,
europe benches before planet". Mapping, complete:

| Obligation | Where satisfied |
|---|---|
| `verify all` | Brick 4 denmark gate-on (exact command above); gate-off covered structurally + by the full default suite |
| Shutdown / early-exit tests | Brick 3: early drop, early drop under pressure, block_fn error, early-exit-does-not-read-whole-file, worker panic - all gate-on twins of the pinned defaults |
| Ordering tests | Brick 3 equivalence set + stalled-worker skew test + the standing Sort.Type_then_ID debug assertion running above the seam in every debug test |
| Both gate states equivalence-tested | Parameter API twins (brick 3) + CliInvoker env-path byte-compare (brick 3), per the plan Mechanics' pinned mechanism |
| Europe before planet | Overnight layout runs europe read pair alongside planet pairs in the same night at the same commit; no planet-blocking landing gate exists by user override, and europe/planet adjudicate together in the morning |
| Memory ceiling | Bounded by construction (raw + decoded budgets, permits ride to delivery); brick 3's tiny-budget and stalled-worker tests pin the binding behavior; command-cell sidecar RSS read in the morning |

---

## 5. Morning verdict paths (executed after the read-out, own gates)

**KEEP:** flip `PipelineConfig::batched` semantics to always-on by
deleting the field, the env parse, and the dispatch arms (batched engine
becomes the only ordered engine); delete `run_pipeline`,
`AdmissionGate`/`Permit`, `DecodeTask`/`spawn_decode_task`,
`drain_decoded`, and the old engine's test hooks from `pipeline.rs`
(`should_skip_blob` and the PipelineConfig byte-budget parsing survive,
subject to item 2's own verdict); retire the old-engine-only tests and
promote the `batched_*` twins to the unprefixed names; delete the
`#[doc(hidden)]` builder; author
`decisions/0008-batched-ordered-pipeline.md`; settle numbers into
`reference/performance.md` + `reference/performance-history.md`; update
`reference/pipelined-reader-paths.md` and the stale `run_pipeline` doc
claims; CHANGELOG entry (user-visible perf headline, per the CHANGELOG
bar). Gates: `brokkr check` +
`brokkr verify all --dataset denmark --variant indexed`.

**REVERT:** delete `src/read/batched_pipeline.rs`, the dispatch arms, the
config field + parse helper, the builder, and every `batched_*` test;
CliInvoker `.env()` stays (generic instrument, item 3 uses it). Record
the measured verdict in TODO.md and close the item. Gate: `brokkr check`.

Either way the end state has zero env vars for this item, restoring the
standing contract.

---

## 6. Stopping rule

In scope: the new module, the one config field + parse helper + builder,
the two dispatch arms, brick-0/3 test additions, the overnight cells
above. Out of scope, explicitly: `par_map_reduce`/`par_fold_blobs` (landed,
untouched - primitive dedup is a named KEEP follow-up);
`for_each_primitive_block_batch*` and every command module (item 3's
ground); ADR-0006 dispatch thresholds and arms; the scan/classify pull
engine and any engine unification (report 2's section 3.2 end state - a
future item that would carry its own spec); `BlobReader`, `Blob` decode
internals, `PrimitiveBlock` constructors, writer paths; the
`reference/pipelined-reader-paths.md` getparents staleness fix (plan item
7's doc rider); brokkr's read-sidecar gap (separate brokkr-repo ask).
