# Implementation spec: ordered-pipeline batch rebuild (gate `PBFHOGG_BATCHED_PIPELINE`)

Rev 2 (2026-07-11): folds the R1 codex critique
(`notes/pipeline-rebuild-spec-R1.md`) - pump admission redesigned
(flush-before-blocking), morning adjudication is now a four-state
keep/revert matrix shared with item 3, overnight cells made executable and
variant-pinned, and item 4 owns all of its correctness tests.

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
brokkr-reachable time-filter cell can exercise the gate. Brick 5 replaces
that pair with a getparents-8k pair (genuine FullScan consumer, baseline
already shared with items 2 and 3). The history path's correctness is
covered at fixture scale by the CLI equivalence test in brick 3.

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
- **Optimization-ledger sweep:** `notes/altw-optimization-history.md` and
  the per-command "Don't re-attempt" sections hold no refuted attempt at a
  byte-bounded ordered batch engine; the nearest-sounding document
  (`notes/hybrid-batching-research.md`) concerns the pread-worker /
  `parallel_classify_phase` mutex seam, not `run_pipeline`. The only
  refuted shapes on this seam are ADR-0006's two, handled above.
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
3. **Layering contract this spec guarantees to item 3** (and the structural
   requirement it imposes back - `run_batched_pipeline` has no transform
   parameter, so the hook is an edit fusion makes, not a surface that
   already exists):
   - `for_each_block_pipelined`, `for_each_pipelined`, and
     `into_blocks_pipelined` keep their exact signatures and ordering
     contracts under both engines. Fusion may build any new surface on top
     of them.
   - Inside the batched engine, per-blob decode is factored as one named
     function (`decode_batch_entry`, section 2.5). Item 3's
     gate-on-both-gates arm (the combination cell) WILL EDIT
     `batched_pipeline.rs` to thread its transform through that function
     (a closure/parameter fusion's spec defines); its gate-on-fusion-only
     arm runs inside the default engine's rayon decode tasks and must be
     completely free-standing from this module. This split is binding on
     fusion's spec because it is what keeps the four-state morning matrix
     (section 5) executable: deleting `batched_pipeline.rs` + the dispatch
     arms must delete fusion's both-gates arm with it, leaving fusion's
     default-engine arm intact. Fusion's transform definitions and its
     command-side code live outside this module, always.
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
   row). The `-R` expression is pinned HERE, not deferred: brokkr's
   tags-filter surface is `-R` (bare flag `--omit-referenced`) plus
   `--filter EXPR`, and the pinned expression is `w/highway=primary`
   (brokkr's default and one of `verify_tags_filter.rs`'s three
   cross-validated expressions). Fusion's cells MUST use the identical
   form: `-R --filter w/highway=primary`.

### 1.7 Measurement record and baseline

Standing numbers this item is read against: the planet baselines table in
`reference/performance.md` (commit `16e3694` era rows plus the ADR-0006
gate cells of 2026-07-11: getparents planet-8k 62.0 s `595e8d7e`, getid
planet-8k 48.6 s `ddf6fed4` - a PLAIN-include getid run, context only; the
overnight getid cells are `--add-referenced` and read against item-3's
same-night `--add-referenced` baseline, never against `ddf6fed4` -
tags-filter `-R` 51.8 s `cf116a6b`), the
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

Gate-off byte-identity argument, with its scope defined: "byte-identical"
means (a) command-produced artifact bytes and (b) the sidecar counter set
(names and values at the same code paths) are unchanged while the gate is
unset. The proof is structural, not a golden-binary comparison:
`pipeline.rs` is edited in exactly three ways - the added config field,
the `bool_gate_from_env` helper, and `should_skip_blob` gaining
`pub(crate)` so the new module can import it (a visibility keyword, no
behavior) - none of which touch the default engine's execution; the
dispatch `false` arms are today's calls verbatim; the new field defaults
to `false` in `Default` and reads `false` when the var is unset; and the
batched engine's new counters are module-local (section 2.6), so
`PIPELINE_METRICS`'s emitted field set is untouched gate-off. The whole
existing test suite plus the gates in section 4 confirm structurally.

### 2.2 New module: `src/read/batched_pipeline.rs`

One self-contained module holding the engine and its primitives.

**Duplication decision.** The module carries its own copies of the four
proven primitives from `par_fold_blobs` (`Batch`, `BatchQueue`,
`ByteBudget` with shutdown + oversized-when-empty, `CancelGuard`,
`BatchCharge`), adapted for ordering, rather than lifting the originals
out of `reader.rs` into a shared module. One deliberate deviation: this
module's `ByteBudget` gains `try_acquire` (section 2.5's
flush-before-blocking admission needs it). The par pump's
acquire-then-flush ordering is NOT copied - it is only deadlock-free
because `PAR_INFLIGHT_BUDGET` (256 MiB) comfortably exceeds the 4 MiB
batch target plus the maximum legal blob, a margin this engine's 32 MiB
default and arbitrary env overrides do not have. Record this in the
module comment so the KEEP-path dedup follow-up does not blindly unify
the two pumps. Rationale: the revert path must
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
  try-acquire raw budget per blob;        (worker-local scratch + pool)
    on would-block: flush partial       send (batch_seq, DecodedBatch)
    batch FIRST, then block             free compressed storage,
  acquire decoded budget per batch        release raw budget (BatchCharge)
    (in batch-seq order, at flush)
  push BatchMsg to BatchQueue
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
32 x 3.68 MB ~ 117.7 MB decoded. 32 MiB / 128 MiB are parity round-ups.
Precision caveat (so the comment does not overclaim): the decoded budget
charges `decoded_len_hint()` = max(declared decompressed capacity,
`retained_len()`) per blob (`blob.rs`), not the 2x estimate - the
estimate approximates the declared sizes at planet-primary shape, so
primary cells are EXPECTED ~neutral, and the pre-registered primary
no-regression controls (section 3, brick 5) are what actually establish
it. The 8k encoding (~67 KB/blob) admits ~25x more blobs in flight - the
same asymmetry logic item 2 pinned. `BATCH_TARGET_BYTES` at 4 MiB gives ~2-block batches on
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
3. Admission (redesigned in rev 2 - the R1 blocker: acquire-before-flush
   deadlocks because the partial batch holds charged bytes no worker can
   ever release; with `read_ahead_bytes(1)` blob 1 blocks forever behind
   an unpublished blob 0, and even the 32 MiB default deadlocks when a
   near-maximum legal blob follows a nonempty partial batch):
   `try_acquire(max(retained_len, MIN_BLOB_CHARGE))` on the raw budget.
   On would-block: FLUSH the current partial batch first (acquire its
   decoded charge and push it, per steps 4-5), then fall back to the
   blocking `acquire` (returns false on shutdown -> stop).
   **Pump blocking invariant** (module doc + `debug_assert!` at both
   blocking call sites): the pump never blocks in the raw budget while
   holding a nonempty partial batch, and blocks in the decoded budget
   only at a flush point. Every raw charge held during a raw-budget wait
   therefore belongs to a published batch, which some worker can always
   complete and release; the oversized-when-empty rule covers a lone
   blob larger than the whole cap.
4. Batch boundaries exactly as the par pump: byte target flushes BEFORE a
   blob that would carry the batch past `BATCH_TARGET_BYTES` (lone
   oversized blob forms its own batch, never split); count cap flushes AT
   `BATCH_MAX_BLOBS`, never past.
5. At every flush, before pushing the batch: acquire its decoded charge
   (sum of floored `decoded_len_hint()` over the batch) from the decoded
   budget as one `DecodedPermit`. The pump is the SOLE acquirer of both
   budgets and acquires in batch-seq order. Deadlock-freedom argument
   (liveness, not bounded skew - FIFO dequeue does NOT bound how long a
   dequeued batch takes, so no "batch k is decoded no later than k+j"
   claim is made): when the pump blocks acquiring batch k's decoded
   charge, every held decoded permit belongs to a published batch < k;
   workers pop the FIFO queue, so those batches all get decoded; the
   consumer always drains its channel into the reorder buffer and
   delivers batches < k without needing batch k, releasing their permits;
   so the wait always terminates (or shutdown wakes it). The raw side
   terminates by the step-3 invariant. In-order acquisition additionally
   guarantees capacity for the next-to-deliver batch is never stolen by
   a later one (same reasoning as the "reserve before dispatch, in
   sequence order" comment in today's stage 2).
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
counters that have no batched equivalent stay zero. New counters live as
MODULE-LOCAL statics in `batched_pipeline.rs` and are emitted via
`crate::debug::emit_counter` from the batched engine only - the shared
`PipelineMetrics` struct is NOT extended, because its `emit()` prints
every field unconditionally and extending it would add counter lines to
gate-off sidecar output (breaking the section-2.1 identity scope). The
new names: `pipeline_batches`, `pipeline_batch_raw_wait_ns` and
`pipeline_batch_decoded_wait_ns` (pump budget-wait time; blocking
acquires only - the `try_acquire` fast path is uncounted),
`pipeline_batched_reorder_high_water` (BATCH units - documented as such so
nobody cross-reads it against the per-blob `pipeline_reorder_high_water`).

Test hooks: a `#[cfg(feature = "test-hooks")] pub(crate) mod test_hooks`
INSIDE `batched_pipeline.rs` (per the testing.md "don't consolidate hooks"
rule), static atomics: `STALL_BATCH_SEQ` / `STALLED_READY` /
`RELEASE_STALLED` (stall one worker on a chosen batch to force reorder
skew) and `PANIC_BATCH_SEQ` (panic inside decode for the worker-panic
test), plus a `reset()`. Re-exported next to the existing
`pipeline_test_hooks` export in `src/read/mod.rs`. Placement of the tests
that SET these hooks (testing.md's two-hook-shapes picker): static
atomics are race-free only under per-binary isolation, and a leaked
`PANIC_BATCH_SEQ` could fault a sibling test's pipeline mid-suite, so the
two hook-driven tests live in their own integration binary,
`tests/fault_batched_pipeline.rs` (the fault-injection placement pattern;
own process, no sibling pipelines). Per-instance hooks were considered
and rejected: `PipelineConfig` is `Copy` and hook state is not, so a
per-instance route would refactor the default path's config plumbing,
violating the gate-off minimal-edit rule. (The existing default-engine
hook test sitting at `tests/read_paths.rs` file root predates this
reading and is out of scope here.)

### 2.7 Knob semantics (plan-required decision, stated in a code comment)

- `PBFHOGG_READ_AHEAD_BYTES` -> overrides `RAW_INFLIGHT_BUDGET` cap.
  **Domain change, stated explicitly:** under the default engine the raw
  permit is released when the dispatcher takes the blob off the raw
  channel (`_raw_permit` drops per loop iteration in stage 2), so the
  knob bounds raw-channel occupancy only; under the batched engine the
  raw charge is held until a worker finishes the whole batch
  (`BatchCharge`), so the same knob bounds raw bytes in flight THROUGH
  decode. Same name, wider domain - the code comment says so, and a KEEP
  verdict re-documents the knob (if item 2 also keeps it).
- `PBFHOGG_DECODE_AHEAD_BYTES` -> overrides `DECODED_INFLIGHT_BUDGET` cap.
- Count knobs (`read_ahead`, `decode_ahead`, their defaults and 16x
  backstops) are IGNORED by the batched engine - its admission is
  byte-native and `BATCH_MAX_BLOBS` is the only count bound. The builder
  methods keep working for the default engine (their KEEP-path fate:
  section 5).
- `PBFHOGG_BLOCK_QUEUE_BYTES` (into_blocks queue) and
  `PBFHOGG_CMD_BATCH_BYTES` (command batching) live above this seam and
  are unaffected by the gate.
- `PBFHOGG_FADVISE_BATCH_BYTES` (item 1) lives BELOW this seam, inside
  `BlobReader::open` (`blob.rs`), and composes identically under either
  engine - the pump is a plain `BlobReader` consumer. No interaction.
- Combination scope: per the plan Mechanics, the only verdict-bearing
  gate combination is `PBFHOGG_BATCHED_PIPELINE=1 PBFHOGG_FUSE_TRANSFORM=1`.
  BATCHED x byte-knob combinations are supported for CORRECTNESS only
  (the brick-3 tiny-budget equivalence tests pin them at fixture scale);
  no overnight cell measures them and no verdict is read off them.

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

### Brick 0 - test instruments (CliInvoker env + FullScan forcing flag)

`tests/common/cli.rs`: add

```rust
pub fn env<K: AsRef<OsStr>, V: AsRef<OsStr>>(mut self, key: K, val: V) -> Self {
    self.cmd.env(key, val);
    self
}
```

No test uses it yet; it is the instrument brick 3's CLI tests (and item
3's, later) require - the plan Mechanics' pinned mechanism for real
env-path coverage.

Second instrument, same brick (per the contract's "building the
instrument is itself a brick" rule): the getparents FullScan arm cannot
be reached from the CLI at fixture scale - ADR-0006 dispatches on an
estimated blob count >= 150,000 and the `min_blobs` injection point
(`getparents_dispatched`) is module-internal, unit-test-only. Add a
hidden CLI arg to getparents in `cli/src/main.rs`:

```rust
/// Test instrument: override the ADR-0006 FullScan dispatch threshold.
#[arg(long, hide = true)]
full_scan_min_blobs: Option<u64>,
```

plumbed to `getparents_dispatched`'s existing `min_blobs` parameter
(default `FULL_SCAN_ARM_MIN_BLOBS` when absent). It changes no default
behavior and, like `.env()`, survives either verdict as a durable test
instrument. Gate: `brokkr check`.

### Brick 1 - the engine module

`src/read/batched_pipeline.rs` per section 2, registered in
`src/read/mod.rs`. NOT yet reachable from any public path (no dispatch
yet), so gate-off behavior is trivially untouched. Includes inline
`#[cfg(test)]` tier-1 unit tests (die with the module on revert, which is
the point):

- budget: blocks at cap until release / admits oversized when empty /
  shutdown wakes a blocked acquirer / `try_acquire` never blocks
  (adapted from the par-engine tests);
- queue: drains then closes; pop blocks without stalling peers;
- pump batching: count cap exact, byte target flushes before overflow,
  lone oversized blob forms its own batch, floored charges applied;
- pump admission liveness (the R1-blocker regression tests, each under
  the `assert_completes` watchdog pattern from `reader.rs`'s par tests,
  driving the full engine on in-memory PBFs):
  - two blobs, one worker, 1-byte raw cap: completes with correct output
    (would deadlock under acquire-before-flush);
  - nonempty partial batch followed by a blob sized near the raw cap:
    completes (the default-cap variant of the same cycle);
  - early consumer drop while the pump is blocked in the raw budget:
    shutdown wakes the pump, scope joins, no hang;
  - the same with the pump blocked in the decoded budget;
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
- `scripts/envrun.sh PBFHOGG_BATCHED_PIPELINE=1 brokkr read --dataset denmark --variant indexed`
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
- `batched_thread_count_parity` (`.decode_threads(1)` vs
  `.decode_threads(8)` identical output - testing.md's `-j` parity leg;
  the scratch-leak leg is N/A: the engine creates no files, stated here so
  the triple is accounted for)

Hook-driven tests, in their own binary `tests/fault_batched_pipeline.rs`
(per-binary isolation for the static atomics, section 2.6):

- `#[cfg(feature = "test-hooks")] batched_ordering_bounded_under_stalled_worker`
  (stall hook on batch 1; asserts completion, exact order, and
  reorder-high-water bounded)
- `#[cfg(feature = "test-hooks")] batched_worker_panic_reports_error`
  (PANIC hook; asserts the "decode task panicked" error, no hang)

CLI env-path equivalence tests (real child process, brick-0 `.env()`),
all in one new file `tests/cli_batched_gate.rs` so the REVERT path is a
single-file delete - and all owned by THIS item, not deferred to the
fusion spec (which does not exist yet; the contract forbids that
deferral). Each runs its command twice - env unset vs
`PBFHOGG_BATCHED_PIPELINE=1` - and byte-compares the outputs:

- `batched_gate_time_filter_history_matches_default` - the existing
  history fixture (reuse via `tests/common` / `fixture_helpers`); covers
  the history path the overnight cannot reach (section 1.3).
- `batched_gate_getid_add_referenced_matches_default` - mixed-element
  fixture, `getid --add-referenced` (the pass-2 `into_blocks_pipelined`
  consumer).
- `batched_gate_getparents_full_scan_matches_default` - mixed-element
  fixture, `getparents --full-scan-min-blobs 0` (brick-0 instrument
  forces the ADR-0006 FullScan arm at fixture scale).
- `batched_gate_altw_decode_all_matches_default` - fixture with
  indexdata stripped via `tests/common/adversarial.rs`
  (`mutate_blob_header_indexdata`), `add-locations-to-ways
  --index-type sparse --force` (the raw decode-all fallback).
- `batched_gate_rejects_invalid_value` - env
  `PBFHOGG_BATCHED_PIPELINE=2`, assert_failure, stderr names the variable.

Placement: root/tier-1 modules of their files (fixture-scale, cheap,
every-edit contracts). Gates:

- `brokkr check`
- `brokkr test batched_for_each_matches_pipelined_across_compressions --sweep all`
- `brokkr test batched_gate_getparents_full_scan_matches_default --sweep all`

### Brick 4 - denmark cross-validation (the landing gate)

Coverage honesty first (R1 finding, verified against brokkr's
`verify_all.rs`): gate-on `verify all` reaches the batched engine through
exactly ONE consumer - `verify_tags_filter.rs` runs three `-R`
expressions through the single-pass `into_blocks_pipelined` path. The
suite's getid runs are plain include (never pass 2), its altw runs are on
the indexed variant (passthrough/external, never decode-all), and it has
no getparents, time-filter, or build-geocode-index checks at all. The
remaining consumers are carried by tests THIS spec owns: the brick-3
`cli_batched_gate.rs` equivalence set (history time-filter, getid
`--add-referenced`, forced FullScan getparents, raw altw decode-all) plus
the two geocode/roundtrip gates below.

- `scripts/envrun.sh PBFHOGG_BATCHED_PIPELINE=1 brokkr verify all --dataset denmark --variant indexed`
  - zero diffs, modulo the documented parity exceptions in
  `reference/osmium-parity.md`. Denmark suffices: correctness here is
  wiring and ordering, not scale, and denmark is the smallest dataset the
  verify harness cross-validates on.
- `scripts/envrun.sh PBFHOGG_BATCHED_PIPELINE=1 brokkr test geocode_index --timeout 280`
  - gate-on run of the ignored real-data geocode correctness test; this
  is the build-geocode-index pass-1 coverage (`pass1.rs` always calls
  `for_each_block_pipelined` with `only_relations`), which no verify
  check reaches. The env var propagates to the test child, whose readers
  construct via `PipelineConfig::from_env`.
- `scripts/envrun.sh PBFHOGG_BATCHED_PIPELINE=1 brokkr test roundtrip_denmark --timeout 120`
  - the contract's ignored real-data roundtrip gate for reader changes
  (`technical-implementation-spec.md` point 5), run gate-on; gate-off is
  today's engine, already covered by the standing suite.
- Re-run `brokkr check` at the commit boundary.

Commit (one commit, gate documented in the body, results.db + dirty
markdown ride along per the standing rules). Benchmark discipline: commit
first; the overnight measures this commit.

### Brick 5 - overnight cells (handoff to the plan's overnight.sh rewrite)

Item-4 cells, corrected per section 1.3 (the orchestrator writes
overnight.sh; this spec pins the cells and the corrections). Every cell
pins `--variant indexed` explicitly (brokkr's default, but the contract
requires the pin).

Read-pair read-out protocol, corrected (R1 finding): one baseline
`brokkr read` invocation stores four UUIDs in mode order
sequential/parallel/pipelined/blobreader - the pipelined row is the
THIRD, not the fourth (fourth is blobreader, which never enters this
engine). The morning read-out identifies rows by the mode in stored
`cli_args`, never by position. The item-4 gated twins run
`--modes pipelined` only: the other three modes never reach
`run_pipeline`, it saves ~35+ min of night budget, and it avoids
re-running the 8k parallel variant, which is documented OOM-killed in
`reference/blob-density.md`. The shared BASELINE cells stay four-mode
(items 1 and 2 adjudicate other rows off them).

- Read pairs (shared baselines per the plan's layout; verdict = pipelined
  wall only):
  - `run brokkr read --dataset planet --variant indexed --bench 1` +
    `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr read --dataset planet --variant indexed --modes pipelined --bench 1`
  - `run brokkr read --dataset planet --variant indexed --snapshot 8k --bench 1` +
    `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr read --dataset planet --variant indexed --snapshot 8k --modes pipelined --bench 1`
  - `run brokkr read --dataset europe --variant indexed --bench 1` +
    `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr read --dataset europe --variant indexed --modes pipelined --bench 1`
- **REPLACED CELL** (was the time-filter planet pair, inert per section
  1.3): getparents 8k pair, baseline shared with items 2/3:
  - `run brokkr getparents --dataset planet --variant indexed --snapshot 8k --bench 1` +
    `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr getparents --dataset planet --variant indexed --snapshot 8k --bench 1`
- getid single-gate isolation cell (shares item-3's same-night 8k
  `--add-referenced` baseline; separates BATCHED's solo command effect
  from the combination):
  - `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr getid --dataset planet --variant indexed --snapshot 8k --add-referenced --bench 1`
- tags-filter `-R` gated twins on the item-3-shared baselines. Command
  form corrected (R1 finding, verified against brokkr's schema: `-R` is
  a bare flag, the expression rides `--filter`); expression pinned in
  section 1.6 point 5:
  - `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr tags-filter --dataset planet --variant indexed -R --filter w/highway=primary --bench 1`
  - `run env PBFHOGG_BATCHED_PIPELINE=1 brokkr tags-filter --dataset planet --variant indexed --snapshot 8k -R --filter w/highway=primary --bench 1`
- Combination cell (the end-state candidate, vs item-3's same-night 8k
  `--add-referenced` baseline):
  - `run env PBFHOGG_BATCHED_PIPELINE=1 PBFHOGG_FUSE_TRANSFORM=1 brokkr getid --dataset planet --variant indexed --snapshot 8k --add-referenced --bench 1`

No dedicated geocode cell: build-geocode-index pass 1 is
relation-filtered (skips the overwhelming node/way majority pre-decode)
and the whole build is dominated by passes 2-4, so a planet-scale pair
could not move the keep/revert verdict; its engine correctness is the
brick-4 gate-on `geocode_index` test, and its scale behavior rides the
same engine the read/command signal cells adjudicate.

Budget note for the plan: dropping the two time-filter cells and
trimming the three gated read twins to `--modes pipelined` funds the
getparents gated twin (~1 min) and the getid isolation cell (~1 min)
with room to spare (time-filter planet was one of the heavy command
cells).

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
| `verify all` | Brick 4 denmark gate-on (exact command above; reaches the engine via tags-filter `-R` - the other consumers are carried by the named brick-3/4 tests, see brick 4's coverage note); gate-off covered structurally + by the full default suite |
| Shutdown / early-exit tests | Brick 1: pump-blocked-in-budget early drops (watchdogged); brick 3: early drop, early drop under pressure, block_fn error, early-exit-does-not-read-whole-file; fault binary: worker panic - all gate-on twins of the pinned defaults |
| Ordering tests | Brick 3 equivalence set + stalled-worker skew test (fault binary) + the standing Sort.Type_then_ID debug assertion running above the seam in every debug test |
| Both gate states equivalence-tested | Parameter API twins (brick 3) + CliInvoker env-path byte-compare across all five gated consumers (`cli_batched_gate.rs`), per the plan Mechanics' pinned mechanism |
| Real-data roundtrip (reader change) | Brick 4 gate-on `brokkr test roundtrip_denmark --timeout 120` under `envrun.sh` |
| Europe before planet | Overnight layout runs europe read pair alongside planet pairs in the same night at the same commit; no planet-blocking landing gate exists by user override, and europe/planet adjudicate together in the morning |
| Memory ceiling | Bounded by construction (raw + decoded budgets, per-blob charges acquired before a blob joins a batch, permits ride to delivery; the only cap excess is the standing oversized-when-empty allowance); brick 3's tiny-budget and stalled-worker tests pin the binding behavior; command-cell sidecar RSS read in the morning |

---

## 5. Morning verdict paths (executed after the read-out, own gates)

Items 3 and 4 share a seam, so their verdicts do NOT adjudicate
independently: the morning executes one of FOUR states (R1 blocker).
Standing ordering rule for every state: fusion's edits (keep or revert)
are applied FIRST, batching's second - fusion layers on top of this
module, so unwinding in the reverse order would leave fusion's both-gates
arm referencing deleted machinery mid-sequence. The structural
precondition that makes states 3 and 4 executable at all is pinned in
section 1.6 point 3: fusion's transform definitions, command-side code,
and default-engine arm live entirely outside `batched_pipeline.rs`; only
its both-gates arm edits this module.

**This item's KEEP edits** (applied in states 1 and 2): flip
`PipelineConfig::batched` semantics to always-on by deleting the field,
the env parse, and the dispatch arms (batched engine becomes the only
ordered engine); delete `run_pipeline`, `AdmissionGate`/`Permit`,
`DecodeTask`/`spawn_decode_task`, `drain_decoded`, and the old engine's
test hooks from `pipeline.rs` (`should_skip_blob` and the PipelineConfig
byte-budget parsing survive, subject to item 2's own verdict); delete
the now-dead count-knob surface - the public `read_ahead()` /
`decode_ahead()` builder methods and the `read_ahead`/`decode_ahead`
config fields - rather than leaving silently-no-op public methods with
stale docs (pre-1.0 breaking is legal; the few callers/tests migrate to
the byte knobs or defaults); retire the old-engine-only tests and
promote the `batched_*` twins to the unprefixed names; delete the
`#[doc(hidden)]` builder; author
`decisions/0008-batched-ordered-pipeline.md`; settle numbers into
`reference/performance.md` + `reference/performance-history.md`; update
`reference/pipelined-reader-paths.md` and the stale `run_pipeline` doc
claims; CHANGELOG entry (user-visible perf headline, per the CHANGELOG
bar).

**This item's REVERT edits** (applied in states 3 and 4), the complete
inventory: delete `src/read/batched_pipeline.rs` (which deletes fusion's
both-gates arm with it, and the module-local counters - no metric
storage exists outside the module by section 2.6); remove the module
registration and the `batched_pipeline` test-hook re-export from
`src/read/mod.rs`; delete the two dispatch arms in `reader.rs` (restoring
the bare `run_pipeline` calls); delete the `batched` config field (incl.
its `Default` line), the `from_env` gate read, and `bool_gate_from_env`
from `pipeline.rs`; revert `should_skip_blob` to private; delete the
`#[doc(hidden)]` builder; delete `tests/fault_batched_pipeline.rs`,
`tests/cli_batched_gate.rs`, and every `batched_*` test in
`tests/read_paths.rs`. CliInvoker `.env()` and the getparents
`--full-scan-min-blobs` instrument stay (durable, item 3 and future
work use them). Record the measured verdict in TODO.md and close the
item.

The four states:

1. **Keep both.** Fusion's KEEP edits (its spec's path, including
   promoting the `decode_batch_entry` transform hook to production),
   then this item's KEEP edits. ADR-0008 records the batched engine;
   fusion's ADR (its spec names it) records the fused end state.
   Gates: `brokkr check` +
   `brokkr verify all --dataset denmark --variant indexed`.
2. **Keep batching, revert fusion.** Fusion's REVERT edits first (its
   spec's path; inside this module that means `decode_batch_entry`
   returns to the plain no-transform signature of section 2.5), then
   this item's KEEP edits. Gates: `brokkr check` +
   `brokkr verify all --dataset denmark --variant indexed`.
3. **Revert batching, keep fusion.** Fusion's KEEP edits restricted to
   its default-engine arm (its both-gates arm dies with this module -
   fusion's spec must define its KEEP path to be valid in this state),
   then this item's REVERT edits. Gates: `brokkr check` +
   `brokkr verify all --dataset denmark --variant indexed` (fusion's
   kept arm still touches command output).
4. **Revert both.** Fusion's REVERT edits, then this item's REVERT
   edits. Gate: `brokkr check`.

Either way the end state has zero env vars for this item, restoring the
standing contract.

---

## 6. Stopping rule

In scope: the new module, the one config field + parse helper + builder
+ `should_skip_blob` visibility, the two dispatch arms, the brick-0
instruments (CliInvoker `.env()`, getparents `--full-scan-min-blobs`),
the brick-1/3 test additions (`tests/fault_batched_pipeline.rs`,
`tests/cli_batched_gate.rs`, `batched_*` twins), the overnight cells
above. Out of scope, explicitly: `par_map_reduce`/`par_fold_blobs` (landed,
untouched - primitive dedup is a named KEEP follow-up);
`for_each_primitive_block_batch*` and every command module (item 3's
ground); ADR-0006 dispatch thresholds and arms; the scan/classify pull
engine and any engine unification (report 2's section 3.2 end state - a
future item that would carry its own spec); `BlobReader`, `Blob` decode
internals, `PrimitiveBlock` constructors, writer paths; the
`reference/pipelined-reader-paths.md` getparents staleness fix (plan item
7's doc rider); brokkr's read-sidecar gap (separate brokkr-repo ask).
