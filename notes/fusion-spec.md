# Implementation spec: command-transform fusion into decode workers (gate `PBFHOGG_FUSE_TRANSFORM`)

Rev 2 (2026-07-11): folds the R1 codex critique
(`notes/fusion-spec-R1.md`), all eight findings. The blockers: the
brokkr `--force` forwarding gap is resolved via a named external brokkr
brick with a pinned in-tree fallback (raw altw cells conditional,
section 3 brick 4); command cells move to `--bench 3` and the inert
getparents-primary control now invalidates the night instead of
reverting the item (section 4); the KEEP inventory preserves
`BATCH_SIZE` for extract (section 5). The highs and mediums: the shared
batched pump is pinned as a generic artifact (section 2.4); altw's
decoded-byte expansion is stated and bounded (section 1.5 point 4); the
execution-proof counters are strengthened (section 2.1); the gate-off
identity claim is narrowed and the reader roundtrip gate is owed and
added (sections 2.7, 3 brick 5). Cross-amendment:
`notes/pipeline-rebuild-spec.md` is amended to rev 3 - the
`decode_batch_entry` contradiction (R1 finding 4) resolves in this
spec's direction (section 1.5 point 3).

Item 3 of `notes/env-gated-readpath-batch.md` (the plan of record, rev 2 -
its Mechanics section binds every gate rule used here: envrun.sh for
in-session gates, `capture_env` row metadata, the no-`set_var` test
mechanism, the +/-3 % noise floor). Written against
`reference/technical-implementation-spec.md`. Spawned from the TODO.md
"Command-transform fusion into decode workers" item; full motivating
analysis in `notes/read-path-architecture-reports.md` (codex report
section 6; the Fable report's section 3.2 pull-engine direction is
adjacent context, not this item). This spec LAYERS ON
`notes/pipeline-rebuild-spec.md` (item 4, rev 2) and honors its section
1.6 layering contract exactly - see section 1.5 below, which restates
that contract from fusion's side and records one refinement. Test
placement and tiering follow `reference/testing.md`.

**Process deviation, inherited from the plan:** the standing spec
contract forbids env-var experiment switches; the user explicitly
overrode that for this batch (2026-07-11). The gate is scaffolding with
a pre-registered morning verdict that either flips the default and
deletes the gate, or deletes the gated code. Everything else in the
contract holds: exact copy-pasteable gate commands, explicit
`--dataset`/`--variant` pins, pre-registered keep/revert thresholds.

**Sequencing dependency:** item 4 implements FIRST (plan staffing step
3). This spec consumes three of its landed artifacts: `bool_gate_from_env`
(pipeline.rs, item-4 brick 2), `CliInvoker::env()` (tests/common/cli.rs,
item-4 brick 0), and the getparents `--full-scan-min-blobs` hidden CLI
instrument (item-4 brick 0). None is re-specified here.

---

## 1. Survey of the ground

### 1.1 The four target paths today

All four targets share one seam shape: the pipelined reader decodes
blobs on its private rayon pool, decoded `PrimitiveBlock`s cross the
`into_blocks_pipelined` block queue to the consumer thread, the consumer
materializes them into 64-block batches, and a SECOND rayon dispatch (the
global pool, `par_iter().map_init(BlockBuilder::new)`) runs the command
transform, whose results drain back to the consumer thread for ordered
writing. Per batch that is one full batch of decoded blocks held live
(~235 MB at planet-primary decoded sizes of ~3.7 MB/block; ~8.6 MB at
the 8k encoding's ~134 KB/block), one serial batching thread, and one
pool-to-pool handoff of every block.

| Target | Entry | Reached when | Batch mechanism | Per-block transform |
|---|---|---|---|---|
| getid `--add-referenced` pass 2 | `getid_with_refs`, `src/commands/getid/mod.rs` | always in add-referenced mode, every encoding (pass 2 has no ADR-0006 dispatch) | `for_each_primitive_block_batch` + `process_filter_batch` | `process_block` -> `(Vec<OwnedBlock>, (u64,u64,u64))` |
| getparents FullScan arm | `getparents_pipelined`, `src/commands/getparents/mod.rs` | ADR-0006 estimate >= 150,000 OSMData blobs | `for_each_primitive_block_batch` + `process_batch` | `process_block` -> `(Vec<OwnedBlock>, (u64,u64,u64))` |
| tags-filter `-R` single-pass | `tags_filter_single_pass`, `src/commands/tags_filter/mod.rs` | `-R` only (default is the two-pass pread path) | `for_each_primitive_block_batch` + `process_filter_batch` | `filter_block_parallel` -> `(Vec<OwnedBlock>, TagsFilterStats)` |
| altw decode-all fallback | `write_output_decode_all`, `src/commands/altw/mod.rs` | non-indexed input AND non-external index type (i.e. `--force` on raw input with sparse) | manual `BATCH_SIZE` loop + `process_batch` | `process_block` -> `(Vec<OwnedBlock>, Stats)` |

Every transform already produces the same shape - a `Vec<OwnedBlock>`
plus a small owned stats value - and every consumer already does the
same two things with it in file order: `write_primitive_block_owned`
per block, integer-sum the stats. That is the shape fusion standardizes
on: the transform moves INTO the decode worker, the pipeline delivers
`(Vec<OwnedBlock>, stats)` in file order, and the batch machinery
between the two rayon stages disappears.

Call-site inventory (grounds the KEEP deletion inventory in section 5):
`for_each_primitive_block_batch` has exactly three production callers -
the getid, getparents, and tags-filter rows above.
`for_each_primitive_block_batch_budgeted` has no production caller
outside `for_each_primitive_block_batch` itself (plus its own unit
tests). altw's decode-all is the fourth batch consumer via its manual
`BATCH_SIZE` loop. `extract/simple.rs` also imports `BATCH_SIZE` for
its own unsorted pass-2 batch loop (R1 finding 3) - NOT a fusion
target, but load-bearing on the KEEP deletion inventory (section 5).
`PBFHOGG_CMD_BATCH_BYTES` (item 2) is read inside
`for_each_primitive_block_batch` and nowhere else.

### 1.2 Path-reachability corrections (plan rev 2, binding on this spec)

- **getid:** plain include mode NEVER runs pass 2 - ADR-0006 dispatches
  it to the walker or the deliberately-sequential streaming arm. Every
  fusion cell, gate command, and test MUST use `--add-referenced`.
  `removeid` is walker-pinned and untouched.
- **getparents:** ADR-0006 dispatches planet primary (estimate 36,063
  blobs) to the walker arm; the fused arm runs only on high-blob-count
  encodings (planet-8k estimate 899,866). The 8k pair is the signal;
  the primary pair is a labeled no-regression control on which the
  fused arm is expected NOT to execute. No denmark-scale brokkr
  invocation can reach the FullScan arm either (every denmark snapshot,
  including `1k`, sits far under 150 k blobs) - fixture-scale coverage
  goes through the `--full-scan-min-blobs` instrument.
- **tags-filter:** the default brokkr bench is the two-pass pread path;
  every fusion cell MUST pin `-R`. The `-R` expression is pinned by the
  item-4 spec (section 1.6 point 5) and shared verbatim:
  `-R --filter w/highway=primary` on the brokkr side,
  positional `w/highway=primary` with `-R` on the pbfhogg CLI. The
  pbfhogg CLI rejects `-R` combined with `-j`, so no jobs plumbing
  exists on this path.
- **altw decode-all:** reached only on non-indexed input with a
  non-external index type. pbfhogg's `require_indexdata` hard-errors on
  raw input unless `--force`, so every decode-all invocation carries
  `--force` and `--index-type sparse`. Cells use `--variant raw`.
  **brokkr cannot currently forward pbfhogg's `--force`** (R1 finding
  1, confirmed by dry-run: brokkr's `--force` is only its own
  dirty-tree override; the altw argument builder never appends
  `--force`, so the child command rejects raw input). Every
  brokkr-routed raw altw invocation in this spec is therefore
  CONDITIONAL on the external brokkr brick defined in section 3
  brick 4. Europe raw IS configured on plantasjen
  (`europe-20260301-seq4714.osm.pbf`, `[plantasjen.datasets.europe.pbf.raw]`),
  so the plan's denmark-raw substitution clause does not trigger; the
  sizing decision is in section 4. On raw input the pass-0/pass-1
  classify schedules include every blob conservatively
  (`build_classify_schedule`'s kind filter skips a blob only when
  `meta.index` is present AND mismatched), so the whole altw run is
  full-decode - that is the priced regime, not a surprise.

### 1.3 Precedent

- `parallel_classify_phase` / `parallel_classify_accumulate`
  (`src/scan/classify.rs`): the in-tree, planet-proven proof of exactly
  this ownership shape - workers decode AND transform, only compact
  results cross threads, a consumer-side `ReorderBuffer` restores file
  order. The plan names it as the precedent; production migrations onto
  it (check-refs, tags-filter two-pass, altw scans, geocode passes)
  carry the measured record.
- `par_fold_blobs` (`src/read/reader.rs`, commit `7532021`): map_op
  runs on the decode worker; the unordered variant of the same idea.
- getparents' walker arm already materializes a fresh `BlockBuilder`
  per blob on worker threads (`BlockBuilder` holds `Rc<str>` and is not
  Send) - the per-block-builder lifecycle the fused transform adopts.

### 1.4 Failure history and standing decisions

- **ADR-0006** (`decisions/0006-blob-count-threshold-dispatch.md`): the
  two refuted shapes are not re-proposed. Consumer-thread classify
  (getparents 8k 142.8 s) failed by serializing the transform on ONE
  thread; fusion moves the transform to ALL decode workers - the
  opposite correction. The pipelined-reader-as-getid-scan-arm failure
  (62 % slower than sequential streaming) concerned plain include mode,
  which this item does not touch. Dispatch thresholds and arms are
  unchanged; re-litigating 150 k is out of scope.
- **`reference/pipelined-reader-paths.md` standing rule** (never convert
  a pipelined caller to sequential decode; getparents sequential
  conversion `c912e4d` regressed 4.7x and was reverted): honored -
  fusion keeps parallel decode and ADDS parallel transform on the same
  workers.
- **altw ledger** (`notes/altw.md` "Don't re-attempt",
  `notes/altw-optimization-history.md`): no refuted attempt at
  transform-in-decode-worker. Two lessons are absorbed rather than
  violated: (a) "treating shape as the diagnosis" - the
  par_iter+collect+drain pattern was once suspected for sparse pass-2
  thrashing and measurement pointed elsewhere, so this spec's verdict
  is measured, never assumed; (b) the compression-CPU floor finding
  (altw pass 2 wall is bounded by `frame_blob_into` at zlib:6 - freed
  decoder CPU just refills the writer queue), which drives the altw
  cell's `zstd:1` compression pin in section 4.
- `notes/hybrid-batching-research.md` concerns the pread-worker /
  `parallel_classify_phase` mutex seam, not this one.
- `notes/streaming-pipeline-composition.md` concerns cross-command
  piping, not intra-command stage fusion; no overlap.
- `CORRECTNESS.md` / `DEVIATIONS.md`: untouched - no parser, encoder,
  or osmium-parity behavior changes. Gate-off is byte-identical
  structurally (section 2.7); gate-on output is byte-identical to
  gate-off by test (section 2.6 argues why byte-compare is a valid
  oracle).
- **ADR deliverable:** a KEEP verdict makes worker-side command
  transforms the standing shape for full-scan command paths and deletes
  the batch machinery - ADR-worthy.
  `decisions/0009-fused-command-transforms.md` is a named deliverable
  of the morning keep path (section 5), not of the gated landing. (If
  item 4 reverts and this item keeps, the number 0008 is free and this
  ADR takes it instead; numbering is allocation-order.)

### 1.5 Reconciliation with item 4 (same seam) - the layering contract, from fusion's side

`notes/pipeline-rebuild-spec.md` section 1.6 is binding. Restated as
obligations on THIS spec, with one refinement:

1. **Reader surface:** `for_each_block_pipelined`, `for_each_pipelined`,
   and `into_blocks_pipelined` keep their exact signatures and ordering
   contracts. Fusion builds one NEW surface beside them
   (`ElementReader::for_each_fused_block`, section 2.2) and touches
   nothing about the existing three.
2. **Fusion-only arm (gate FUSE=1, BATCHED unset):** runs inside the
   default engine's rayon decode tasks, completely free-standing from
   `batched_pipeline.rs`. Concretely: `pipeline.rs` gains an ADDITIVE
   generic sibling engine (`run_pipeline_fused`, section 2.3); the
   existing `run_pipeline`, `DecodeTask`, and `drain_decoded` are not
   edited.
3. **Both-gates arm (FUSE=1 BATCHED=1):** an additive, clearly-marked
   "fusion section" inside `batched_pipeline.rs`
   (`run_batched_pipeline_fused`, section 2.4). **Cross-amendment (R1
   finding 4):** item 4's rev 2 expected fusion to thread a transform
   parameter through `decode_batch_entry` and defined its state-2
   revert as restoring that signature - a genuine contradiction with
   this spec, not a refinement it could unilaterally declare.
   Resolved in this spec's direction, which is structurally better (no
   signature churn on the shared decode seam; state 2's revert becomes
   a pure section delete): `notes/pipeline-rebuild-spec.md` rev 3 now
   records that `decode_batch_entry`'s signature is PERMANENT - the
   fused batched worker applies the transform at `decode_batch_entry`'s
   call site, in a fused-only worker loop - and its four-state matrix
   is updated to match: state 2 (keep batching, revert fusion) is
   "delete the marked fusion section", with no signature-restoration
   step. States 3/4 delete the whole module, which subsumes the
   section. Fusion's
   transform definitions and command-side code live outside both engine
   modules, always (they live in the four command modules).
4. **Budget accounting under fusion (item 4's budget rule, honored
   verbatim):** the decoded-byte permit is charged
   `decoded_len_hint()` for the decoded block regardless of what the
   transform emits, and is released only at ordered delivery. For the
   three filter commands the transform shrinks the payload (a filtered
   `Vec<OwnedBlock>` is far smaller than the decoded block), so the
   charge over-covers. **altw is the exception (R1 finding 6):** its
   transform ADDS two delta-encoded location arrays per way (up to ~10
   varint bytes per ref per array, typically 2-4), so a way-blob's
   fused output can EXCEED the decoded input, and the decoded budget
   is NOT a ceiling on transformed bytes in flight. The engines remain
   bounded regardless: in-flight transformed payloads are count-capped
   (`decode_ahead` admissions plus the reorder buffer in the default
   engine; `worker_count * 2` channel slots plus the reorder buffer in
   the batched engine), each payload bounded by the decoded block's
   size plus ~20 bytes per way ref. The honest ceiling claim is
   count-times-bounded-payload, not the decoded budget - stated in a
   code comment at both fused engines' budget charge sites - and the
   altw pair's morning read-out adjudicates ACTUAL peak RSS via
   `brokkr sidecar --compare` before drawing any memory conclusion.
   The charge itself stays `decoded_len_hint()` in both engines -
   transform output never enters budget arithmetic. Never release
   early. Both engines.
5. **Gate interactions (plan Mechanics):** the only supported
   combination is `PBFHOGG_BATCHED_PIPELINE=1 PBFHOGG_FUSE_TRANSFORM=1`.
   Dispatch between the two fused engines happens inside
   `for_each_fused_block` on `PipelineConfig::batched` - the same field
   item 4 installed, no second read of that env var. All four gate
   states are well-defined per command: unset/unset = shipped batch
   arm on the default engine; BATCHED only = batch arm on the batched
   engine (via the item-4 dispatch inside `into_blocks_pipelined`);
   FUSE only = fused arm on the default engine; both = fused arm on the
   batched engine.
6. **Shared overnight cells:** the tags-filter `-R` baselines and the
   getid 8k `--add-referenced` baseline serve items 2, 3, and 4; the
   combination cell belongs to item 4's list and is read against
   item 3's same-night 8k baseline. Expression pinned:
   `-R --filter w/highway=primary` (brokkr form), identical for both
   items.

### 1.6 Measurement record and baselines

Per the plan, the authoritative pre-change baseline for every verdict
is the SAME-NIGHT baseline cell measured by overnight.sh at the batch's
HEAD commit on plantasjen (the ADR-0006 landing measured ~45 %
cross-day drift on I/O-bound cells from environment alone). Standing
context numbers, all plantasjen:

- getparents planet-8k FullScan 62.0 s (`595e8d7e`, 2026-07-11) and
  planet-primary walker 19.0 s (`a7c064eb`) - the 8k number is the
  closest proxy for the fused arm's baseline; the same-night cell
  supersedes it.
- getid planet-8k 48.6 s (`ddf6fed4`) is a PLAIN-include run - context
  only; pass 2 never ran in it. No stored planet `--add-referenced`
  baseline exists; the same-night baseline cell is the first.
- tags-filter `-R` planet 51.8 s (`cf116a6b`,
  `reference/performance.md`).
- altw europe-raw decode-all: no baseline exists anywhere; the
  overnight baseline cell is the first measurement of this regime.
  Both cells of the pair are conditional on the brokkr `--force-altw`
  brick (section 3 brick 4); without it the pair does not run and the
  item's verdict reads from the other three signal cells.

Expected effect shape, stated so the morning read-out interprets
correctly: the 8k cells are the WALL signal (the removed seam costs -
block-queue hop, batch materialization, second dispatch - are per-block
and multiply by 1.45 M blobs); the planet-primary cells are expected
wall-neutral with an RSS drop (~235 MB batch materialization gone,
visible in `brokkr sidecar --compare` per-phase RSS); the altw
europe-raw pair may still be partially writer-bound even at zstd:1, so
a neutral altw wall does not by itself argue revert. After the verdict,
numbers settle into `reference/performance.md` (new current) and
`reference/performance-history.md` (arc), per the plan's step 5.

---

## 2. Target artifact

### 2.1 Gate plumbing

One helper in `src/commands/mod.rs`, next to the batch helpers it will
eventually replace:

```rust
/// PBFHOGG_FUSE_TRANSFORM gate (env-gated read-path batch, item 3).
/// Unset -> false; "1" -> true; "0" -> false; anything else is a hard
/// error naming the variable (silent misspellings must not read as off).
pub(crate) fn fuse_transform_from_env() -> Result<bool> {
    Ok(crate::read::pipeline::bool_gate_from_env("PBFHOGG_FUSE_TRANSFORM")?)
}
```

`bool_gate_from_env` is item 4's artifact and already implements those
exact semantics. The helper is called once at each fused command's
public entry (`getid`, `getparents`, `tags_filter`,
`add_locations_to_ways`) - before any pass starts, so an invalid value
fails fast - and the result is plumbed as a plain `fused: bool`
parameter below it. This is the plan Mechanics' pinned mechanism: unit
and equivalence tests drive the bool parameter directly; CLI tests set
the variable on the child process via `CliInvoker::env()`. No
`std::env::set_var` anywhere.

**Execution-proof counters (strengthened per R1 finding 7):** two
counters, because branch selection and transform execution are
different facts:

- `fuse_transform_active` - `crate::debug::emit_counter(
  "fuse_transform_active", 1)`, emitted once when a command's gate-on
  branch is entered, immediately before `for_each_fused_block`. Proves
  branch SELECTION only.
- `fuse_transform_blocks` - emitted from the fused CONSUMER every 64
  consumed transformed items and once more at pass end with the final
  total. Proves transforms actually ran through to ordered delivery.
  All four commands emit it (this replaces rev 1's altw-only
  `altw_pass2_blocks_fused`; the 64-block cadence follows the
  `altw_pass2_batches_dispatched` precedent).

`capture_env` proves a cell ran with the variable set; the counters
prove the fused code executed - the direct defense against the
miswired-cell class rev 1 of the plan contained. Both counters ride
the nonblocking sidecar FIFO (`debug.rs`: `O_NONBLOCK`, silent drop on
a full buffer), so counter ABSENCE is read as CELL INVALID (miswired
or dropped - attended rerun), never as a neutral measurement.
Morning cell-validity rules: a gated signal cell missing both counters
is invalid; the combination cell must additionally show item 4's
batched-engine counters (`pipeline_batches`) - fused counters without
them mean the batched engine never dispatched, a miswire; the
getparents planet-primary gated control must show NEITHER counter
(walker arm; the branch is inside `getparents_pipelined`, which
primary never enters) - absence there is expected, presence is the
miswire.

### 2.2 New reader surface

One `pub(crate)` method on `ElementReader` (reader.rs), beside the
three existing pipelined entries:

```rust
/// Ordered pipelined read with the per-block transform fused into the
/// decode workers. `transform` runs on decode threads (it must not
/// assume the calling thread); `consume` runs on the calling thread in
/// exact file order. Errors from the transform surface in sequence
/// position, after all earlier blocks have been consumed.
pub(crate) fn for_each_fused_block<T, X, F>(
    self,
    transform: X,
    consume: F,
) -> Result<()>
where
    R: Read + Send,
    T: Send,
    X: Fn(PrimitiveBlock) -> std::result::Result<T, String> + Sync,
    F: FnMut(T) -> Result<()>,
{
    if self.pipeline_config.batched {
        super::batched_pipeline::run_batched_pipeline_fused(
            self.blob_iter, self.decode_threads, self.pipeline_config,
            self.blob_filter, &transform, consume)
    } else {
        super::pipeline::run_pipeline_fused(
            self.blob_iter, self.decode_threads, self.pipeline_config,
            self.blob_filter, &transform, consume)
    }
}
```

The `String` error type matches the existing `process_block`
convention in all four commands; the engines convert it to a
`crate::error::Error` (`ErrorKind::Io(other)`) at the erroring
sequence position. Note there is NO `R: 'static` bound: both fused
engines run on the calling thread's scope (no background
pipeline-owner thread, no block queue), which is also why
`PBFHOGG_BLOCK_QUEUE_BYTES` does not apply to fused arms (section 2.8).

Transforms borrow command-local state (`&ElementIds`, `&NodeIndex`,
`&IdSet`), so nothing here is `'static`; both engines must execute the
transform through scoped borrows (sections 2.3, 2.4).

### 2.3 Default-engine fused path: `run_pipeline_fused` (pipeline.rs, additive)

```rust
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn run_pipeline_fused<R, T, X, F>(
    mut blob_reader: BlobReader<R>,
    decode_thread_count: Option<usize>,
    pipeline_config: PipelineConfig,
    blob_filter: Option<BlobFilter>,
    transform: &X,
    mut consume: F,
) -> Result<()>
where
    R: Read + Send,
    T: Send,
    X: Fn(PrimitiveBlock) -> std::result::Result<T, String> + Sync,
    F: FnMut(T) -> Result<()>,
```

A generic sibling of `run_pipeline`, added at the end of the file in a
clearly-marked fusion section. **Duplication decision** (same rationale
as item 4's): the skeleton (~150-180 lines) is duplicated rather than
`run_pipeline` being genericized, because gate-off byte-identity
forbids refactoring the spine the default path runs through, and the
revert path must be "delete the fused section". Shared primitives are
reused as-is: `AdmissionGate`, `Permit`, `ByteBudget`, `BytePermit`,
`should_skip_blob`, `PipelineConfig`, `PIPELINE_METRICS`. On a KEEP
verdict the duplication resolves per the four-state matrix (section 5);
any post-verdict generic unification of the two drains is a named
follow-up, out of scope here.

Stage behavior, with the deltas from `run_pipeline` called out:

- **Entry:** identical - `set_parse_tagdata` iff the filter has a tag
  filter, `set_parse_indexdata` iff any filter is set.
- **Stage 1 (reader thread):** identical, including the optional raw
  `ByteBudget` charged `retained_len()` per blob.
- **Stage 2 (dispatcher thread):** builds the same dedicated pool with
  the same sizing rule, then wraps its dispatch loop in
  `decode_pool.in_place_scope(|task_scope| ...)` and spawns per-blob
  FUSED tasks with `task_scope.spawn`. This is the load-bearing
  mechanism change: `rayon::ThreadPool::spawn` requires `'static`, but
  command transforms borrow command-local state; rayon's scoped spawns
  admit those borrows, and the enclosing `std::thread::scope` makes the
  `&X` reference (from the caller's frame) valid inside the dispatcher
  closure. `in_place_scope` returns only when all spawned tasks have
  finished, which is exactly the EOF-drain semantics the plain
  dispatcher has. Admission is unchanged and stays in sequence order
  BEFORE each spawn: `AdmissionGate::acquire` (cap
  `effective_decode_ahead()`), then the optional decoded `BytePermit`
  charged `decoded_len_hint()` (section 1.5 point 4). Read/framing
  errors are forwarded via a fused `send_direct_error` twin, same
  precedence rule (delivered in sequence position).
- **Fused task (rayon scoped, on a pool thread):** under one
  `catch_unwind`: non-OsmData -> `None` payload; filter check via
  `should_skip_blob` -> `None` + `blobs_skipped_by_filter` bump; else
  decode via `to_primitiveblock_inline_with_scratch` (fn-local
  `thread_local!` scratch Vecs, the shared `DecompressPool`), then
  `transform(block)` - the `PrimitiveBlock` drops inside the worker,
  which is the whole point - mapping `Err(String)` to the Io-flavored
  error at that seq. A panic anywhere inside (decode OR transform)
  converts to the same "decode task panicked" error shape the plain
  engine uses. Send `(seq, item, Some(permit), decoded_byte_permit)` on
  the fused decoded channel; on send failure set the shutdown flag.
- **Stage 3 (calling thread):** `drain_fused<T>` - a generic twin of
  `drain_decoded` (`ReorderBuffer` keyed by blob seq, capacity
  `decode_ahead`); for each ready item: `Some(Ok(t))` ->
  `consume(t)?` then drop permits; `Some(Err(e))` -> return Err;
  `None` -> drop permit. Early consumer exit drops the receiver,
  blocked senders fail, shutdown propagates to stage 1 - identical
  prompt-stop shape (the fused twin of
  `early_exit_does_not_read_whole_file` pins it).
- **Metrics:** `PIPELINE_METRICS.emit()` on every exit including the
  error path; the carried-over counters (`decode_tasks`,
  `blobs_skipped_by_filter`, `decoded_recv_wait_ns`,
  `decode_admit_wait_ns`/`decode_admit_blocked`, raw/decoded send
  waits) are populated with the same meanings. All of this is
  gate-on-only execution, so reusing the existing counter names is safe
  for gate-off sidecar identity. No new `PipelineMetrics` fields.

### 2.4 Batched fused path: `run_batched_pipeline_fused` (batched_pipeline.rs, marked additive section)

Everything payload-independent is REUSED, not duplicated: the pump
(batch assembly, flush-before-blocking admission, both budgets acquired
in batch-seq order by the sole acquirer), `BatchQueue`, `ByteBudget`,
`CancelGuard`, `BatchCharge`, `MIN_BLOB_CHARGE` flooring, and
`decode_batch_entry` with its UNCHANGED signature. If the item-4
implementation inlined the pump into `run_batched_pipeline`, the fusion
implementer extracts it as a behavior-identical `fn` both entries call -
the module executes only gate-on, so internal refactors cannot touch
gate-off behavior, and the plain batched path's semantics stay pinned by
item 4's `batched_*` tests remaining green.

**Pump-sharing design, pinned (R1 finding 5):** the pump has exactly
one payload-dependent act - direct read-error delivery on the consumer
channel (item 4 section 2.5 step 1 sends a concrete `DecodedBatch`).
The extracted pump is therefore generic over the consumer envelope:

```rust
fn run_pump<E: Send>(
    blob_reader: BlobReader<impl Read>,
    raw_budget: &ByteBudget,
    decoded_budget: &ByteBudget,
    queue: &BatchQueue,
    consumer_tx: &std::sync::mpsc::SyncSender<(usize, E)>,
    mk_err: impl Fn(crate::error::Error) -> E,
) -> ...
```

The plain entry passes
`|e| DecodedBatch { entries: vec![Err(e)], decoded_permit: None }`;
the fused entry passes
`|e| FusedDecodedBatch { entries: vec![Err(e)], decoded_permit: None }`.
Everything else the pump touches (`BatchMsg`, `BatchQueue`, both
budgets, the flush-before-blocking admission, batch-seq assignment) is
already payload-independent - `BatchMsg` carries compressed blobs, not
decoded payloads, so the worker-facing side needs no genericity at
all. Ownership flow is unchanged from item 4's section 2.5.

Fusion-section additions:

```rust
struct FusedDecodedBatch<T> {
    entries: Vec<std::result::Result<T, crate::error::Error>>, // file order within the batch
    decoded_permit: Option<DecodedPermit>,
}

pub(crate) fn run_batched_pipeline_fused<R, T, X, F>(...) -> Result<()>
where R: Read + Send, T: Send,
      X: Fn(PrimitiveBlock) -> std::result::Result<T, String> + Sync,
      F: FnMut(T) -> Result<()>,
```

- **Fused worker** (scoped thread, one loop per worker, mirroring the
  plain worker's rules): pop batch; `BatchCharge` owns the compressed
  storage; per blob in order call `decode_batch_entry(...)` and map
  `Some(Ok(block))` through `transform(block)`; `None` (skip) produces
  no entry; the first `Err` (decode or transform) is pushed, the batch
  remainder is discarded, and the worker stays alive - identical to
  item 4's rule 4. The whole per-batch loop runs under `catch_unwind`;
  a panic (decode or transform) converts to the "decode task panicked"
  error at that entry position. Send failure: shut both budgets down,
  close the queue, exit; the worker-scoped `CancelGuard` covers
  escaping unwinds.
- **Consumer** (calling thread): reorder by `batch_seq`; per ready
  batch, `consume(entry?)?` for each entry in order, then drop the
  batch's `decoded_permit` after its last entry - the item-4 budget
  rule verbatim. Early-return and panic paths identical to the plain
  consumer (explicit shutdown + close + guard).
- The decoded charge is still acquired by the pump per batch at flush
  time, from `decoded_len_hint()` sums - the transform's output size
  never enters budget arithmetic (section 1.5 point 4). A code comment
  in the fusion section states this.

### 2.5 Command rewiring (all four, same pattern)

Each command's fused-capable function gains a plain `fused: bool`
parameter, plumbed from the public entry's single
`fuse_transform_from_env()?` read. The false arm is today's code,
verbatim. getid pass 2, in full (the other three are the same pattern):

```rust
// getid_with_refs(..., fused: bool), pass 2, replacing the single
// for_each_primitive_block_batch call with a two-arm branch:
if fused {
    crate::debug::emit_counter("fuse_transform_active", 1);
    reader.for_each_fused_block(
        |block| {
            let mut bb = BlockBuilder::new();
            let mut output: Vec<OwnedBlock> = Vec::new();
            let counts = process_block(
                &block, &mut bb, &mut output, ids, true, dep_ref, strip_tags)?;
            flush_local(&mut bb, &mut output)?;
            Ok((output, counts))
        },
        |(blocks, (nodes, ways, relations))| {
            for OwnedBlock { bytes, index, tagdata, way_members } in blocks {
                writer.write_primitive_block_owned(
                    bytes, index, tagdata.as_deref(), way_members.as_deref())?;
            }
            stats.nodes_written += nodes;
            stats.ways_written += ways;
            stats.relations_written += relations;
            // plus the fuse_transform_blocks emission every 64
            // consumed items + final total (section 2.1), elided here
            Ok(())
        },
    )?;
} else {
    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
        // today's body, verbatim
    })?;
}
```

Per command:

- **getid** (`src/commands/getid/mod.rs`): `getid()` reads the gate;
  `getid_dispatched` and `getid_with_refs` gain `fused: bool`. Only the
  add-referenced pass-2 seam branches; pass 1
  (`parallel_classify_accumulate`) and the include/invert arms are
  untouched. Transform captures `ids`, `dep_ref`, `strip_tags`;
  consumer captures `&mut writer`, `&mut stats`. Existing
  `GETID_PASS1/PASS2` markers and `getid_*` counters unchanged, so
  `brokkr sidecar --compare` aligns phases across gate states.
- **getparents** (`src/commands/getparents/mod.rs`): `getparents()`
  reads the gate; `getparents_dispatched`, `getparents_with_arm`, and
  `getparents_pipelined` gain `fused: bool` (the walker arm ignores
  it). Transform wraps its `process_block`; markers unchanged.
- **tags-filter** (`src/commands/tags_filter/mod.rs`): `tags_filter()`
  reads the gate; `tags_filter_single_pass` gains `fused: bool` (the
  two-pass path ignores it). Transform wraps `filter_block_parallel`;
  the consumer merges the three matched counters exactly as
  `process_filter_batch`'s drain does today.
- **altw** (`src/commands/altw/mod.rs`): `add_locations_to_ways()`
  reads the gate; `write_output_checked` and `write_output_decode_all`
  gain `fused: bool` (the passthrough path ignores it). The fused
  branch replaces the manual `BATCH_SIZE` loop; the transform wraps
  `process_block` with per-call local `refs_buf`/`locations_buf`.
  The uniform `fuse_transform_blocks` (section 2.1) doubles as the
  SIGKILL-forensics fused twin of `altw_pass2_batches_dispatched`
  (which remains on the unfused arm).

**Per-block `BlockBuilder` lifecycle and byte-identity of the output.**
The fused transform constructs a fresh `BlockBuilder` per block (the
getparents walker-arm precedent). The batch arm reuses one builder per
rayon worker via `map_init` - but `BlockBuilder::encode_block()` ends
with `self.reset()` (`src/write/block_builder.rs`, the line before
`take()`/`take_owned()` return), so a builder is stateless across
blocks and the two lifecycles produce identical bytes per input block.
Output-block boundaries are a function of the input block alone (flush
on builder-full plus one final flush per input block, in both shapes);
blocks are written in file order in both shapes; compression is
per-blob and deterministic at a fixed level; stats are u64 sums.
Therefore gate-on output is byte-identical to gate-off, and the
equivalence tests in section 3 are BYTE-compare, the strictest oracle
available.

### 2.6 Semantics preserved (the contract the tests pin)

- Blocks reach the consumer in exact file order under both engines
  (blob-seq reorder in the default engine, batch-seq reorder plus
  in-batch order in the batched engine).
- The first error in file order wins; blocks before the erroring seq
  are consumed first; read/framing errors beat transform/decode errors
  raced in the same window.
- A `consume` error or panic stops both engines promptly without
  reading the rest of the file.
- A transform panic surfaces as the "decode task panicked" error, never
  a hang.
- Filter skips and non-OsmData blobs deliver nothing, exactly as the
  plain engines' `None` payloads.
- `PIPELINE_METRICS.emit()` fires on every exit.

### 2.7 Gate-off byte-identity argument (scope per item 4 section 2.1)

Gate-off means: unset variable, `fused = false` everywhere, every
false arm is today's code verbatim. The edits touching shipped-path
files are (a) one additive helper in `src/commands/mod.rs`, (b) one
additive method in `reader.rs`, (c) an additive marked section in
`pipeline.rs` - `run_pipeline`, `DecodeTask`, `spawn_decode_task`,
`drain_decoded` unedited, no `PipelineMetrics` field changes, (d) per
command: one env read at entry, one bool parameter, one branch whose
false arm is verbatim. `batched_pipeline.rs` is gate-on-only code in
its entirety. New counters (`fuse_transform_active`,
`fuse_transform_blocks`) are emitted only inside gate-on branches, so
the gate-off sidecar counter set is unchanged. The claim is structural
intent, not a pre-change golden-binary comparison (R1 finding 8): the
section-3 CLI tests compare the new binary's gate-off arm against its
gate-on arm, and `brokkr check` is not a pre-change byte oracle. What
carries the identity claim is the edit inventory above - verbatim
false arms, additive-only engine code - confirmed in effect by the
untouched full default suite plus the section-3 gates.

### 2.8 Knob semantics (stated in a code comment on `for_each_fused_block`)

- `PBFHOGG_READ_AHEAD_BYTES` / `PBFHOGG_DECODE_AHEAD_BYTES` (item 2):
  apply identically inside both fused engines (stage-1 raw budget;
  decoded permits per section 1.5 point 4).
- `PBFHOGG_BLOCK_QUEUE_BYTES` (item 2): INERT on fused arms - the fused
  entry never constructs `into_blocks_pipelined`, so there is no block
  queue. Its adjudication cell (getparents 8k, item 2) is a baseline
  cell without FUSE set, so item 2's verdict is unaffected; but if BOTH
  item 2 keeps that knob AND fusion keeps, the knob loses getparents /
  getid / tags-filter / altw-decode-all from its consumer set (the read
  bench's `into_blocks_pipelined` users remain). Morning adjudication
  resolves jointly.
- `PBFHOGG_CMD_BATCH_BYTES` (item 2): INERT on fused arms - no command
  batching exists there. Its only production consumer is
  `for_each_primitive_block_batch`, which a fusion KEEP deletes
  (section 5); a joint keep therefore retires the knob, recorded in
  both items' verdicts.
- `PBFHOGG_FADVISE_BATCH_BYTES` (item 1): below this seam, composes
  identically.
- `PBFHOGG_BATCHED_PIPELINE` (item 4): selects the engine inside
  `for_each_fused_block`; the only verdict-bearing combination is
  both-gates-on (one overnight cell, item 4's list). Other
  combinations are correctness-supported (the CLI combination tests
  pin them at fixture scale) but unmeasured.

---

## 3. Bricks

Each brick lands separately with its gates green before the next
(`brokkr check` at every boundary), one commit per brick - which
realizes the plan's "one command at a time internally, one gate".
Dataset choice per gate: fixture scale for wiring/ordering/equivalence
(the questions are not scale questions), denmark for real-data
cross-validation, europe/planet only in the overnight. No CHANGELOG
entries at these landings - gated default-off scaffolding is not
user-visible; the entry ships with a morning KEEP (plan step 5).

### Brick 0 - both fused engines + reader surface + gate helper

`run_pipeline_fused` (section 2.3), `run_batched_pipeline_fused`
(section 2.4), `for_each_fused_block` (section 2.2),
`fuse_transform_from_env` (section 2.1). No command reaches any of it
yet, so gate-off behavior is trivially untouched. Inline `#[cfg(test)]`
tier-1 unit tests, driving the engines on in-memory PBFs with toy
transforms (the transform PARAMETER is the injection point - no static
hooks, no `test-hooks` feature, no fault binary; per testing.md's
two-hook-shapes picker, neither shape is needed when the seam is
already a caller-supplied closure):

In `pipeline.rs` (fusion section):

- `fused_output_matches_block_pipelined` - identity-shaped transform;
  same blocks, same order as `for_each_block_pipelined`.
- `fused_transform_error_surfaces_after_prior_blocks` - transform errs
  on block k; asserts blocks < k consumed, then Err.
- `fused_transform_panic_reports_error` - panicking transform; asserts
  the "decode task panicked" error, no hang (watchdogged via the
  `assert_completes` pattern from reader.rs's par tests).
- `fused_early_consumer_error_stops_promptly` - `consume` errs on the
  first item over a CountingRead-style reader; asserts the file is not
  read to EOF.
- `fused_tiny_byte_budgets_complete_in_order` -
  `.read_ahead_bytes(1).decode_ahead_bytes(1)` twins; budgets bind,
  order holds.
- `fused_stalled_transform_preserves_order` - transform stalls on a
  chosen seq via a captured atomic until released; asserts completion
  and exact order under skew.
- `fused_thread_count_parity` - `.decode_threads(1)` vs
  `.decode_threads(8)` identical consumed sequence (testing.md's `-j`
  parity leg; the scratch-leak leg is N/A - the engines create no
  files).
- `fused_blobfilter_skips_blobs` - only-ways filter on indexed input;
  transform sees no node blocks.

In `batched_pipeline.rs` (fusion section; die with the module or the
section, which is the point):

- `batched_fused_matches_plain_batched` - same blocks/order as the
  plain batched engine.
- `batched_fused_transform_error_position` - first Err in file order,
  prior blocks consumed, batch remainder discarded.
- `batched_fused_transform_panic_reports_error` - watchdogged.
- `batched_fused_early_consumer_error_stops_promptly` - shutdown wakes
  the pump, scope joins.
- `batched_fused_dispatch_via_builder` - constructs the reader with
  `.batched_pipeline(true)` and calls `for_each_fused_block`, pinning
  the dispatch arm itself.

Gate: `brokkr check`.

### Brick 1 - getid `--add-referenced`

Rewiring per section 2.5; in-module unit test
`fused_with_refs_matches_batched` (mixed fixture with ways + dep
nodes; `getid_with_refs` fused true vs false; byte-compare the two
output files); CLI test `fused_gate_getid_add_referenced_matches_default`
in the new `tests/cli_fused_gate.rs` (root tier-1; runs
`getid --add-referenced` twice via `CliInvoker`, env unset vs
`.env("PBFHOGG_FUSE_TRANSFORM", "1")`, byte-compares outputs), plus
`fused_gate_rejects_invalid_value` (env value `2`; `assert_failure`;
stderr names the variable). Gates:

- `brokkr check`
- `brokkr test fused_gate_getid_add_referenced_matches_default --sweep all`
- `brokkr verify getid-removeid --dataset denmark --variant indexed`
- `scripts/envrun.sh PBFHOGG_FUSE_TRANSFORM=1 brokkr verify getid-removeid --dataset denmark --variant indexed`
  - coverage honesty: the verify suite's getid runs are plain include
  (walker/streaming arms, fusion-inert), so this pair proves gate-on
  does not disturb the NON-fused getid paths; fused correctness is the
  byte-compare tests above.
- `scripts/envrun.sh PBFHOGG_FUSE_TRANSFORM=1 brokkr getid --dataset denmark --variant indexed --add-referenced`
  - real-data execution smoke of the fused arm (pass 2 is pipelined at
  every encoding, so denmark reaches it); denmark cannot show the win
  and is not read for one.

Commit (results.db + dirty markdown ride along).

### Brick 2 - getparents FullScan arm

Rewiring; in-module unit test `fused_full_scan_matches_batched`
(`getparents_with_arm` with `ScanArm::FullScan`, fused true vs false,
byte-compare); CLI test `fused_gate_getparents_full_scan_matches_default`
(uses the item-4 `--full-scan-min-blobs 0` instrument to force the
FullScan arm at fixture scale). Gates:

- `brokkr check`
- `brokkr test fused_gate_getparents_full_scan_matches_default --sweep all`
- `scripts/envrun.sh PBFHOGG_FUSE_TRANSFORM=1 brokkr getparents --dataset denmark --variant indexed`
  - dispatch-undisturbed smoke only: denmark dispatches to the walker,
  so the fused arm is NOT reached here (no brokkr-reachable
  denmark-scale cell exists for this arm; the forced-threshold CLI test
  is the coverage, by design of ADR-0006 and the brick-0 instrument).

Commit.

### Brick 3 - tags-filter `-R`

Rewiring; in-module unit test `fused_single_pass_matches_batched`
(small built PBF with tagged/untagged elements, `tags_filter_single_pass`
fused true vs false, byte-compare); CLI test
`fused_gate_tags_filter_single_pass_matches_default` (`-R` plus a
positional expression). Gates:

- `brokkr check`
- `brokkr test fused_gate_tags_filter_single_pass_matches_default --sweep all`
- `brokkr verify tags-filter --dataset denmark --variant indexed`
- `scripts/envrun.sh PBFHOGG_FUSE_TRANSFORM=1 brokkr verify tags-filter --dataset denmark --variant indexed`
  - this gate GENUINELY exercises the fused path: per the item-4 brick-4
  coverage audit, `verify_tags_filter.rs` runs three `-R` expressions
  through the single-pass seam. Zero diffs, modulo the documented
  parity exceptions in `reference/osmium-parity.md`.

Commit.

### Brick 4 - altw decode-all

Rewiring; CLI test `fused_gate_altw_decode_all_matches_default`
(fixture with indexdata stripped via
`tests/common/adversarial.rs::mutate_blob_header_indexdata`,
`add-locations-to-ways --index-type sparse --force`, gate-off vs
gate-on byte-compare). No separate in-module unit test: the command
needs a built index either way and the CLI test drives the identical
entry at the same cost - the byte-compare through the real binary IS
the tier-1 contract. Gates:

- `brokkr check`
- `brokkr test fused_gate_altw_decode_all_matches_default --sweep all`
- `scripts/envrun.sh PBFHOGG_FUSE_TRANSFORM=1 brokkr verify add-locations-to-ways --dataset denmark --variant indexed --mode sparse`
  - gate-on control on the INDEXED variant (passthrough path,
  fusion-inert): proves gate-on does not disturb the shipped altw
  paths. (`--variant indexed` pinned explicitly per the contract - R1
  finding 8.)
- CONDITIONAL on the brokkr brick below:
  `scripts/envrun.sh PBFHOGG_FUSE_TRANSFORM=1 brokkr add-locations-to-ways --dataset denmark --variant raw --index-type sparse --force-altw`
  - real-data execution smoke of the fused decode-all arm (denmark has
  `pbf.raw` configured). Without the brick this command CANNOT run -
  brokkr never forwards pbfhogg's `--force`, so the child errors on
  raw input. Brick absent: the CliInvoker byte-compare test above
  (which drives the real binary on a raw fixture with pbfhogg
  `--force`) is the standing correctness proof, and this smoke runs
  attended once the brick lands.

**External prerequisite (R1 finding 1, blocker): a brokkr-side brick,
owned by the user. STATUS: LANDED 2026-07-11, brokkr commit `5f6ce56` -
`--force-altw` exists, forwards pbfhogg `--force`, composes with
`--inject-prepass`/`--index-type`/`--compression`, and the exact europe
cell below was confirmed working verbatim by the brokkr dev. The
pre-flight below still runs (cheap insurance); the fallback clause
remains but should not trigger.** Original analysis, kept for the
record: brokkr's `--force` is ONLY its own dirty-tree
override: `AddLocationsToWays` has no command-specific force field and
its argument builder never appends `--force` to the child command
(R1, confirmed by dry-run - the generated child argv contained
`--index-type sparse --compression zstd:1` but no `--force`), so
pbfhogg rejects raw input and every brokkr-routed decode-all
invocation fails. The needed brick, in the brokkr repo: a
command-specific flag on `add-locations-to-ways` that forwards
pbfhogg's `--force`, proposed spelling `--force-altw` - non-colliding
with brokkr's own per-subcommand `--force`, following the existing
`--force-repack` precedent, which disambiguates exactly this collision
on `repack`. This spec cannot land brokkr code; the brick is a named
external prerequisite, and every brokkr-routed raw altw command in
this spec is CONDITIONAL on it.

**Fallback, pinned so the item is adjudicable without the brick:**
altw fusion CORRECTNESS is proven by
`fused_gate_altw_decode_all_matches_default` (real binary, raw
fixture, byte-compare) regardless of brokkr. The overnight altw pair
is conditional: if the brick is absent at overnight-handoff time, the
pair is dropped from overnight.sh and the item's verdict is read from
the three remaining signal cells (getid 8k `--add-referenced`,
getparents 8k, tags-filter `-R` 8k); the altw fused arm then keeps or
reverts with the whole-item verdict, its performance unmeasured -
recorded as such in TODO.md, with the pair queued for a follow-up
night once the brick exists.

**Pre-flight (replaces rev 1's storage probe):** the storage question
is answered from brokkr's source (R1): result storage is gated on
`git.is_clean`, not on the force flag - a clean-tree run stores
normally, forced or not; rev 1's probe command would itself have
failed on raw input before storing anything. What the pre-flight now
checks is the BRICK: run
`brokkr add-locations-to-ways --dataset denmark --variant raw --index-type sparse --force-altw --dry-run`
and inspect the generated child argv for pbfhogg `--force`. Flag
rejected, or `--force` absent from the child argv, means the brick is
absent - activate the fallback (drop the overnight pair). Dry-run exit
status alone is NOT sufficient proof of a runnable cell - R1 confirmed
dry-run reports success even when the child command would fail - which
is why the check is argv inspection, not dry-run success.

Commit.

### Brick 5 - combination coverage + final gate-on cross-validation + handoff

CLI tests `fused_gate_combination_getid_matches_default` and
`fused_gate_combination_getparents_matches_default` (both env vars set
- `PBFHOGG_BATCHED_PIPELINE=1` and `PBFHOGG_FUSE_TRANSFORM=1` - vs
unset; byte-compare; getid `--add-referenced` and getparents
`--full-scan-min-blobs 0` respectively; these pin the batched fused
worker end to end through the real binary). Gates:

- `brokkr check`
- `brokkr test fused_gate_combination_getid_matches_default --sweep all`
- `scripts/envrun.sh PBFHOGG_FUSE_TRANSFORM=1 brokkr verify all --dataset denmark --variant indexed`
  - the whole-suite gate-on regression net (reaches the fused
  tags-filter path directly; every other command must be undisturbed).
- `scripts/envrun.sh PBFHOGG_FUSE_TRANSFORM=1 brokkr test roundtrip_denmark --timeout 120`
  - the contract's ignored real-data roundtrip gate
  (`reference/technical-implementation-spec.md` point 5) - owed
  because this item adds a reader surface and two reader engines (R1
  finding 8; rev 1 wrongly claimed it not owed). Gate-on proves the
  gated binary's standard reader paths are undisturbed; the fused
  surface itself is not on the roundtrip route - its whole-file
  write-and-reread coverage is the getid/altw CLI byte-compares plus
  the denmark verifies.

Commit. Then hand the section-4 cells to the plan's overnight.sh
rewrite (the orchestrator writes the file; `./overnight.sh --dry-run`
must pass - which validates brokkr's own flag surface, including the
altw `--compression zstd:1` axis; if dry-run rejects that flag on the
altw subcommand, the fallback is the same pair without
`--compression`, accepting the zlib:6 writer-bound caveat of section
1.6). Dry-run success does NOT validate the child command - R1
confirmed it reports success while omitting `--force` - so the altw
cells additionally require the brick-4 child-argv inspection before
handoff; brick absent means the pair is dropped per brick 4's
fallback.

### Re-verification obligation mapping

| Obligation | Where satisfied |
|---|---|
| Both gate states equivalence-tested | Parameter-API byte-compare unit tests (bricks 1-3) + CLI env-path byte-compare across all four commands and both combinations (`cli_fused_gate.rs`) |
| Shutdown / early-exit | Brick 0: early-consumer-error tests both engines, watchdogged panic tests |
| Ordering | Brick 0: match/stall/skew tests both engines; the standing Sort.Type_then_ID assertion is above a different entry (`for_each_pipelined`) and does not run on this surface - order is pinned by the byte-compares and the explicit order tests |
| External cross-validation | tags-filter gate-on `brokkr verify tags-filter` (fused path directly); getid/altw gate-on verifies as non-disturbance controls; `verify all` gate-on at brick 5 |
| Real-data roundtrip | Owed (R1 finding 8: this item adds a reader surface and two reader engines): brick-5 gate-on `scripts/envrun.sh PBFHOGG_FUSE_TRANSFORM=1 brokkr test roundtrip_denmark --timeout 120`. Writer internals are unchanged (the writer sees identical `OwnedBlock`s); the fused surface's whole-file write-and-reread coverage is the CLI byte-compares plus the denmark verifies |
| Memory ceiling | Bounded by construction for the three filter commands (engines' admission: gate + byte budgets, permits to ordered delivery; the 64-block batch materialization is REMOVED). altw's transform EXPANDS output past the decoded charge (section 1.5 point 4) - its in-flight bound is count-based, and the altw pair adjudicates measured peak RSS. Command-cell sidecar RSS read in the morning |

---

## 4. Overnight cells (handoff to the plan's overnight.sh rewrite)

Every cell pins `--variant` explicitly. Baseline first, gated twin
immediately after (shared baselines per the plan's layout). Command
cells store ONE UUID per run (unlike `brokkr read`); rows are
identified in the morning by the mode strings in stored `cli_args`
(`--add-referenced`, `-R --filter w/highway=primary`, `--snapshot 8k`,
`--index-type sparse`) plus the `capture_env` metadata
(`PBFHOGG_FUSE_TRANSFORM=1` on the row) - NEVER by position. The
section-2.1 execution-proof counters (`fuse_transform_active` +
`fuse_transform_blocks`, sidecar) are the proof a gated cell executed
the fused arm; a gated signal cell missing them is INVALID and reruns
attended, never read as neutral (section 2.1).

**Bench counts (R1 finding 2, reconciled with
`reference/performance.md`'s reading rules):** every command cell runs
`--bench 3` (best-of-three stored). The reading rules say verdicts
come from `--bench 3` best-of, not single runs; planet's `--bench 1`
habit is a cost exception, and these command cells are minutes-scale -
cheap enough to pay for verdict-grade numbers. Both sides of every
pair use the SAME bench count (a best-of-3 baseline against a
single-run twin would bias the no-regression controls toward false
regression). Cost: roughly +30-60 min across the non-altw pairs; the
altw pair grows to ~36-75 min per cell and is conditional anyway -
still inside the night with the rider last. Item 4's read cells stay
`--bench 1` on cost under a pre-registered aggregation rule, pinned in
`notes/pipeline-rebuild-spec.md` rev 3; fusion has no read cells of
its own. The +/-3 % floor is unchanged.

getid `--add-referenced` (primary pair = RSS observable + control; 8k
pair = wall signal; the 8k baseline is shared with item 2's CMD_BATCH
pair and item 4's isolation + combination cells):

- `run brokkr getid --dataset planet --variant indexed --add-referenced --bench 3`
- `run env PBFHOGG_FUSE_TRANSFORM=1 brokkr getid --dataset planet --variant indexed --add-referenced --bench 3`
- `run brokkr getid --dataset planet --variant indexed --snapshot 8k --add-referenced --bench 3`
- `run env PBFHOGG_FUSE_TRANSFORM=1 brokkr getid --dataset planet --variant indexed --snapshot 8k --add-referenced --bench 3`

getparents (8k = signal, baseline shared with items 2 and 4; primary =
labeled walker control, fused arm expected NOT to run, both
execution-proof counters expected ABSENT on its gated twin):

- `run brokkr getparents --dataset planet --variant indexed --snapshot 8k --bench 3`
- `run env PBFHOGG_FUSE_TRANSFORM=1 brokkr getparents --dataset planet --variant indexed --snapshot 8k --bench 3`
- `run brokkr getparents --dataset planet --variant indexed --bench 3`
- `run env PBFHOGG_FUSE_TRANSFORM=1 brokkr getparents --dataset planet --variant indexed --bench 3`

tags-filter `-R` (baselines shared with item 4's gated twins; one
baseline per dataset serves both items, env metadata distinguishes the
twins):

- `run brokkr tags-filter --dataset planet --variant indexed -R --filter w/highway=primary --bench 3`
- `run env PBFHOGG_FUSE_TRANSFORM=1 brokkr tags-filter --dataset planet --variant indexed -R --filter w/highway=primary --bench 3`
- `run brokkr tags-filter --dataset planet --variant indexed --snapshot 8k -R --filter w/highway=primary --bench 3`
- `run env PBFHOGG_FUSE_TRANSFORM=1 brokkr tags-filter --dataset planet --variant indexed --snapshot 8k -R --filter w/highway=primary --bench 3`

altw decode-all - **the sizing decision this spec owes the plan:**
europe-raw, `zstd:1`. Europe raw is configured on plantasjen (~35 GB,
~522 k blobs at ~8 k elements/blob - a genuinely high-blob-count
encoding, which is the signal regime); denmark-raw is REJECTED as
substitute (seconds-scale walls cannot clear a 3 % floor above run
noise). `zstd:1` is pinned because the altw ledger's compression-CPU
floor finding says pass 2 at zlib:6 is bounded by `frame_blob_into` -
at the default compression the decode-side effect fusion targets would
be masked by the writer; zstd:1 drains the writer queue fast enough to
expose it (and both cells of the pair share the pin, so the comparison
is internally valid). Memory: sparse at europe is a ~29 GB file-backed
scratch mmap plus a ~1-2 GB IdSet, measured surviving a 27 GB host at
~6 minutes on indexed input; decode-all adds only bounded pipeline
memory, and the fused twin strictly less. Wall estimate: 12-25 min per
cell (full-decode passes 0/1/2 plus europe-wide re-encode at zstd:1);
the pair is fusion's heaviest, already counted in the plan's rev-2
budget arithmetic (at `--bench 3` it grows to ~36-75 min per cell).
**The pair is CONDITIONAL on the brokkr `--force-altw` brick (brick
4):** brick absent at handoff means the pair is dropped from
overnight.sh and the verdict reads from the three other signal cells.
Storage is not a concern - clean-tree runs store regardless of force
flags (brick-4 pre-flight).

- `run brokkr add-locations-to-ways --dataset europe --variant raw --index-type sparse --compression zstd:1 --force-altw --bench 3`
- `run env PBFHOGG_FUSE_TRANSFORM=1 brokkr add-locations-to-ways --dataset europe --variant raw --index-type sparse --compression zstd:1 --force-altw --bench 3`

Combination cell (owned by item 4's list, restated for completeness;
read against the getid 8k `--add-referenced` baseline above):

- `run env PBFHOGG_BATCHED_PIPELINE=1 PBFHOGG_FUSE_TRANSFORM=1 brokkr getid --dataset planet --variant indexed --snapshot 8k --add-referenced --bench 3`

**Pre-registered verdict (plan noise floor):** ONE verdict for the
whole item - the gate is one flag; there is no per-command keep. All
walls are `--bench 3` best-of (above). Cell validity precedes reading:
a gated signal cell without its execution-proof counters is INVALID
and reruns attended (section 2.1), never read as neutral. KEEP
requires >= 3 % wall improvement on at least one signal cell: getid 8k
`--add-referenced`, getparents 8k, tags-filter `-R` 8k, or altw
europe-raw (when the pair runs; brick-absent nights adjudicate on the
first three). Planet-primary getid and tags-filter pairs must sit
inside +/-3 % both directions - they are no-regression controls that
DO execute fused code, and either regressing > 3 % is an automatic
revert for the item, as is any executing signal cell; their expected
win is RSS, read via `brokkr sidecar --compare` as supporting evidence
(the batch materialization - ~235 MB at primary decoded sizes - should
vanish from the scan/pass-2 phases). The getparents primary pair is
different in kind (R1 finding 2): it is an INERT control - the fused
code never runs there (counters expected absent) - so its drift cannot
indict code it never executed. If the inert control moves > 3 % in
either direction, the night's environment is suspect and the night is
INVALID for this item (attended rerun of the affected pairs), NOT a
revert. A wall-neutral altw pair does NOT count against keep
(writer-bound caveat, section 1.6) - it counts against keep only if it
regresses.

---

## 5. Morning verdict paths (four-state matrix, shared with item 4)

Item 4 section 5 defines the four states and the standing ordering
rule: **fusion's edits (keep or revert) are applied FIRST, batching's
second, in every state.** This section supplies fusion's half.

**Fusion KEEP edits** (states 1 and 3):

- Delete `fuse_transform_from_env`, the four `fused: bool` parameters,
  and the four branches - the fused arm becomes the only arm in
  `getid_with_refs`, `getparents_pipelined`, `tags_filter_single_pass`,
  and `write_output_decode_all`. Delete the
  `fuse_transform_active` counter (meaningless without the gate).
  `fuse_transform_blocks` is deleted from getid, getparents, and
  tags-filter, and RENAMED `altw_pass2_blocks` in altw - it inherits
  the SIGKILL-forensics role of `altw_pass2_batches_dispatched`, which
  dies with the batch arm.
- Delete the three `for_each_primitive_block_batch` call sites and
  altw's manual batch loop; then delete
  `for_each_primitive_block_batch`,
  `for_each_primitive_block_batch_budgeted`, and
  `BATCH_COUNT_BACKSTOP` from `src/commands/mod.rs` (consumer
  inventory in section 1.1; their unit tests retire with them).
  **`BATCH_SIZE` is NOT deleted (R1 finding 3):**
  `src/commands/extract/simple.rs` imports it for its unsorted pass-2
  batch loop, which is not a fusion target - the constant MOVES to
  `extract/simple.rs` as a module-local `const BATCH_SIZE: usize = 64;`
  (its sole surviving user), and the shared one in `mod.rs` is deleted
  only after that move. This deletes `PBFHOGG_CMD_BATCH_BYTES`'s only consumer: if
  item 2's verdict kept that knob, fusion's keep supersedes it - the
  knob and its env read are deleted and the joint outcome is recorded
  in both items' TODO verdicts.
- State 1 (batching also keeps): item 4's subsequent KEEP edits delete
  the default engine - `run_pipeline_fused` and its fused
  task/drain die with `run_pipeline` in that pass (this line item is an
  addendum to item 4's state-1 inventory, executed from here);
  `for_each_fused_block` collapses to the bare batched-fused call.
- State 3 (batching reverts): item 4's subsequent REVERT deletes
  `batched_pipeline.rs` wholesale, taking the fusion section with it;
  `for_each_fused_block` collapses to the bare `run_pipeline_fused`
  call. End state: `pipeline.rs` carries TWO ordered engines - plain
  (for the read bench, time-filter history, geocode pass 1, i.e. the
  non-fused `into_blocks_pipelined`/`for_each_block_pipelined`
  consumers) and fused (for the four commands). A generic unification
  of their drains is a named follow-up, out of scope.
- Promote `fused_*` tests to unprefixed names; delete
  `tests/cli_fused_gate.rs` (its oracle - the batch arm - no longer
  exists); the in-module fused-vs-walker arm-equivalence tests survive
  as standing cross-arm contracts.
- Author `decisions/0009-fused-command-transforms.md` (or 0008 if item
  4 reverted; section 1.4): worker-side transforms as the standing
  shape, the decoded-charge budget rule, the deleted batch machinery
  and why.
- Docs: rewrite `reference/pipelined-reader-paths.md` (the four callers
  are now fused; the batch-shape description and the `BATCH_SIZE` line
  in `reference/blob-density.md` "Decisions that need revisiting" go
  stale and are reconciled); settle numbers into
  `reference/performance.md` + `reference/performance-history.md`;
  CHANGELOG entry with the headline per-command numbers; close the
  TODO item.
- Gates: `brokkr check` +
  `brokkr verify all --dataset denmark --variant indexed`.

**Fusion REVERT edits** (states 2 and 4), complete inventory:

- Delete `tests/cli_fused_gate.rs`; delete the `fused_*` unit tests
  from the four command modules and from the two engine modules'
  fusion sections.
- Delete the four fused branches and `fused: bool` parameters
  (restoring the batch arms as the unconditional path - they never
  left); delete `fuse_transform_from_env` from `src/commands/mod.rs`;
  delete the `fuse_transform_active` and `fuse_transform_blocks`
  emissions.
- Delete `for_each_fused_block` from `reader.rs`.
- Delete the fusion section from `pipeline.rs` (`run_pipeline_fused`,
  the fused item type, `drain_fused`, the fused direct-error helper).
- Delete the fusion section from `batched_pipeline.rs` (state 4: the
  module deletion by item 4's revert subsumes it; state 2: delete the
  section, leaving the plain batched engine exactly as item 4 shipped
  it - `decode_batch_entry` was never re-signed, so nothing to
  restore).
- Record the measured verdict and numbers in TODO.md; close the item.
- Gates: `brokkr check`; in state 2 additionally item 4's
  `brokkr verify all --dataset denmark --variant indexed` runs per its
  own path.

Either way the end state has zero env vars for this item, restoring
the standing contract.

---

## 6. Stopping rule

In scope: the two engine fusion sections, `for_each_fused_block`, the
gate helper, the four command branches with their bool plumbing, the
two new counters, the brick-0/1-5 tests (`tests/cli_fused_gate.rs`,
in-module `fused_*` tests), the brick-4 pre-flight probe, the section-4
overnight cells.

Out of scope, explicitly:

- **time-filter** (both paths): the history path consumes
  `for_each_pipelined` element-by-element - not one of the four rev-2
  targets and proven overnight-unreachable by the item-4 survey; the
  snapshot path is `parallel_classify_phase`. Untouched.
- **build-geocode-index pass 1** (`for_each_block_pipelined` +
  relation filter): not a target; untouched.
- getid include/invert arms, `removeid`, getid pass 1; getparents
  walker arm; tags-filter two-pass (all three classify phases); altw
  passthrough output, external backend, sparse index build (passes
  0/1), relation scans. All untouched.
- `cat` (raw-frame passthrough, deliberately non-pipelined), and every
  other `parallel_classify_*` consumer.
- ADR-0006 dispatch thresholds, arms, and estimator.
- The batch helpers themselves (`for_each_primitive_block_batch*`,
  `BATCH_SIZE`) BEFORE the verdict - they are the gate-off spine; their
  deletion is a KEEP-path edit only.
- `run_pipeline` / `DecodeTask` / `drain_decoded` internals; item 4's
  plain batched engine outside the marked fusion section;
  `par_map_reduce` / `par_fold_blobs`; `BlobReader`, `Blob` decode
  internals, `BlockBuilder` encoding, writer paths.
- The scan/classify pull engine and any engine unification (report 2
  section 3.2) - future item, own spec.
- the brokkr `--force-altw` forwarding brick itself (named external
  prerequisite, brick 4 - user-owned, brokkr-repo; this spec only
  defines the brick and the fallback that makes the item adjudicable
  without it).
