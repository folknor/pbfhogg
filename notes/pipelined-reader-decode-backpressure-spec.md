# Implementation spec: bounded decode admission in the pipelined reader

Status: spec, 2026-07-10. Authored against the tree at commit `6d6e158`.
Not yet implemented; landing is gated on a bench host (section 8 pins the
order: baselines run before the commit lands).

## 0. Standing references

- Contract this spec is written against:
  [reference/technical-implementation-spec.md](../reference/technical-implementation-spec.md)
- Source item naming the problem (analysis, options weighed, decision,
  scoping findings):
  [notes/pipelined-reader-decode-backpressure.md](pipelined-reader-decode-backpressure.md)
  and the TODO.md "Cross-pipeline optimization" entry pointing at it.
- Test placement and tier contract (this spec adds tests):
  [reference/testing.md](../reference/testing.md)
- Measurement record and reading rules:
  [reference/performance.md](../reference/performance.md),
  [reference/performance-history.md](../reference/performance-history.md),
  `.brokkr/results.db`.
- Standing rule honored, not changed:
  [reference/pipelined-reader-paths.md](../reference/pipelined-reader-paths.md)
  ("do not convert any pipelined path to sequential decode"; the
  getparents `c912e4d` 4.7x Denmark regression is the gate). This change
  keeps decode fully parallel; it bounds admission, it does not
  serialize anything.
- Interaction: [notes/injected-prepass.md](injected-prepass.md) section 9
  requires this change to land (and its verdict to be read) before that
  item's format work benches, so the two do not share a baseline.

## 1. What this builds

One coherent commit inside `src/read/pipeline.rs` and its two support
modules that makes the pipelined reader's `decode_ahead` knob honest:

1. An **admission gate** (token counter, cap = `decode_ahead`, default 32)
   between stage 2's raw-channel drain and `decode_pool.spawn`, so
   spawned-but-undrained decode tasks are bounded. Today `rayon::spawn`
   never blocks, so effective read-ahead is the entire file and decoded
   memory is unbounded (21.5 GB RSS / reorder high-water 660 on a 19 GB
   input; the pathology is documented in `run_pipeline`'s own doc comment
   and is live in-tree - see survey).
2. The **two shutdown changes** for consumer early-exit (source note,
   scoping finding A). Only the first is deadlock-critical (R2 finding 3):
   stage 3 owns and drops `decoded_rx` at loop exit so blocked senders
   fail-fast, release their permits, and the dispatcher unblocks - without
   this the gate genuinely deadlocks. The second - a shutdown `AtomicBool`
   set on decoded-channel send failure - is a *promptness* change, not a
   deadlock fix: without it the pipeline still terminates, but the
   dispatcher keeps admitting tasks whose sends fail until stage 1 drains
   the whole file (today's whole-file-read cost), instead of breaking
   within ~`cap` blobs. Both land together; the framing distinction
   matters only for reasoning about what is mandatory.
3. **Counters** that make the new behaviour observable:
   `pipeline_decode_admit_wait_ns` (new), `pipeline_reorder_high_water`
   re-pointed at filled slots (the memory diagnostic),
   `pipeline_reorder_window_high_water` (new; the old window meaning, the
   completion-skew diagnostic).
4. **Tier-1 pressure tests** that exercise the full-channel shutdown path
   the existing 3-block fixture cannot reach, plus inline unit tests for
   the new primitives.
5. **Doc sweep**: `decode_ahead` contract, `run_pipeline` memory warning,
   `pipeline_metrics` doc fix, `reference/pipelined-reader-paths.md`
   invariants paragraph, CHANGELOG entry, source-note and TODO.md status
   flips.

No public signature changes. The public knob `ElementReader::decode_ahead(n)`
keeps its name and gains a tighter guarantee.

## 2. Survey of the ground

Verified by direct source read at `6d6e158` (HEAD at spec-writing time;
the source note's scoping pass was at `119edc5` and every line reference
below was re-verified since).

### 2.1 Current mechanism (`src/read/pipeline.rs`)

- `run_pipeline` (pipeline.rs:93) runs three stages under
  `std::thread::scope`: stage 1 reader thread feeding a
  `sync_channel(read_ahead=16)` of raw blobs; stage 2 dispatcher thread
  draining it and calling `decode_pool.spawn` per OsmData blob
  (pipeline.rs:183-251); stage 3 on the calling thread draining a
  `sync_channel(decode_ahead=32)` of decoded items through a
  `ReorderBuffer` (pipeline.rs:258-300).
- `rayon::spawn` never blocks, so stage 2 moves every raw blob into the
  rayon task queue at disk rate; `pipeline_raw_send_wait_ns` is ~0 in
  every stored run because the raw channel never fills.
- `ReorderBuffer::push` (reorder_buffer.rs:29-44) grows the window via
  `resize_with(slot_idx + 1, ...)` for any sequence gap; the
  `with_capacity(decode_ahead)` argument is only an initial `VecDeque`
  capacity. Each stage-3 `recv()` frees a decoded-channel slot, so the
  channel bounds in-flight sends, not accumulated decoded blocks.
- `decoded_rx` is created before `thread::scope` (pipeline.rs:115) and is
  captured by reference into the scope body; it outlives the scope join.
  Today that is survivable only because `rayon::ThreadPool::drop` does
  not join spawned tasks (verified against rayon-core 1.13.0 in the
  source note): on early exit the entire remaining file is still read and
  decompressed on detached threads, but nothing hangs. With a naive gate
  this exact shape becomes a real deadlock cycle - which is why the
  shutdown changes are part of the same commit, not a follow-up.
- Every DecodedItem is `(usize, Option<Result<PrimitiveBlock>>)`; skipped
  blobs (filtered, headers) still send `(seq, None)`, so every admitted
  seq produces exactly one send. This is load-bearing for the permit
  release accounting in section 3.
- The decode closure already wraps its body in `catch_unwind` and
  converts panics to an `Err` item that is still sent (pipeline.rs:197-234).
- `PIPELINE_METRICS.emit()` runs inside the scope body, after the stage-3
  loop and before the scope's implicit join (pipeline.rs:305). Late
  stragglers can still bump counters after emit today; the gate shrinks
  that window but the emit position is unchanged.

### 2.2 Callers (all funnel through `run_pipeline`)

Public surface (`src/read/reader.rs`): `for_each_pipelined` (:193),
`for_each_block_pipelined` (:236), `into_blocks_pipelined` ->
`PipelinedBlocks` (:264), knobs `read_ahead` (:92) / `decode_ahead`
(:104). `PipelinedBlocks::drop` (reader.rs:574-583) drops its receiver
then **joins** the background pipeline thread - post-change that join
completes within ~cap blobs instead of after a full-file drain.

Internal production callers, verified at `6d6e158`:

| Site | Command | Filter | Shape |
|---|---|---|---|
| `src/geocode_index/builder/pass1.rs:34` | `build-geocode-index` | `only_relations()` | bounded workload |
| `src/commands/getid/mod.rs:569` | `getid --add-referenced` pass 2 | blob filter | filtered, batched |
| `src/commands/tags_filter/mod.rs:396` | `tags-filter -R` single pass | blob filter | filtered, batched |
| `src/commands/altw/mod.rs:547` | `add-locations-to-ways` pass 2a `write_output_decode_all` | none | unfiltered full scan; fires only on non-indexed input (`require_indexdata` at altw/mod.rs:222 errors without pbfhogg `--force`) |
| `src/commands/time_filter/mod.rs:165` | `time-filter` history path | none | unfiltered full scan; history-only |

Non-production: `cli/src/main.rs:2718` (`brokkr read` pipelined arm - an
unfiltered full scan with a trivial consumer, and therefore a direct
bench handle on the changed path), `examples/partition_stats.rs:60`,
`tests/read_paths.rs` conformance tests.

Nobody in-tree overrides `read_ahead`/`decode_ahead`; every caller runs
at defaults 16/32, so the cap lands uniformly.

### 2.3 Two corrections to the source note found by this survey

1. **The time-filter memory bench in the note's validation plan is not
   runnable.** The note proposes a peak-RSS before/after on `time-filter`
   at europe/japan. The residual pipelined call (`time_filter/mod.rs:165`)
   is the *history* path only - the snapshot path migrated to
   `parallel_classify_phase` in `83183fb` - and `brokkr.toml` has no
   history PBF configured (TODO.md, "Blocked on dataset / config"). A
   regular-PBF time-filter bench never enters `run_pipeline`. Section 8
   substitutes gates that exercise the same code path through drivable
   commands.
2. **The altw unfiltered scan needs non-indexed input plus pbfhogg's
   `--force`.** `write_output_decode_all` fires only when
   `indexdata_present == false`, and `require_indexdata` hard-errors on
   such input unless pbfhogg's `--force` is set. Whether brokkr's
   `add-locations-to-ways` subcommand plumbs that flag is unverified
   (brokkr source is outside this repo's sandbox). Section 8 makes that
   gate contingent with a deterministic fallback, and makes the primary
   memory gate one that is proven drivable (`brokkr read`, whose
   invocation shape exists in `.brokkr/results.db`, e.g. europe UUIDs
   `6ff16830`/`da7315ad`).

### 2.4 Failure history and standing decisions

- **Sequential conversion is refuted and not proposed.** `c912e4d`
  (reverted, 4.7x Denmark regression) pinned the rule in
  `reference/pipelined-reader-paths.md:42`. This spec adds backpressure
  to the parallel pipeline; decode stays on the pool.
- **Walking away instead of fixing was the previous house response.**
  `cat` (28.9 GB OOM, migration comment at cat/mod.rs:413), `check --ids`
  (`516129e`), `time-filter` snapshot (`83183fb`), `tags-filter` way-deps
  (`17b116c`) all migrated off the reader. Five residual callers remain
  (2.2); migrating all of them is neither cheap nor desirable
  (`parallel_classify_phase` does not fit cross-blob streaming state, per
  pipelined-reader-paths.md "Adding a new pipelined caller" rule 2).
  Fixing the primitive fixes all residuals and every external
  `into_blocks_pipelined` user at once.
- **The allocator-retention pathology is distinct and out of scope.**
  `DecompressPool` (`8f6999b`) solved decompression-buffer recycling; the
  25+ GB cross-thread `WireStringTable` retention paragraph in
  `run_pipeline`'s doc comment describes allocator behaviour, not
  admission, and stays true. This spec bounds *live* decoded blocks
  (unbounded admission), which is the 0 -> 20.5 GB-in-20-s ramp shape.
- **ADR check.** No `decisions/*` ADR governs pipeline buffering.
  Per `decisions/README.md` ("behavior goes in `reference/`; decisions go
  here") this landing documents behaviour - the bounded-admission
  contract lands in doc comments, `reference/pipelined-reader-paths.md`,
  and CHANGELOG - and establishes no new project-wide policy with
  defensible alternatives left standing (the options survey lives in the
  source note). No new ADR.
- `CORRECTNESS.md` / `DEVIATIONS.md`: untouched - no parser, encoder, or
  osmium-visible behaviour changes. Element output of every command is
  byte-identical; ordering guarantees are unchanged (the reorder buffer
  still delivers in file order).

## 3. Target artifacts

### 3.1 `src/reorder_buffer.rs` - maintained filled count

```rust
pub(crate) struct ReorderBuffer<T> {
    next_seq: usize,
    filled: usize,               // NEW: count of Some slots, O(1) maintained
    pending: VecDeque<Option<T>>,
}
```

- `push`: after `self.pending[slot_idx] = Some(item);` add
  `self.filled += 1;`.
- `pop_ready`: pops only when the front slot is filled (existing guard),
  so on a successful pop add `self.filled -= 1;`.
- New accessor: `pub(crate) fn filled_len(&self) -> usize { self.filled }`.
- `pending_len()` keeps its meaning (window length including gap slots).
- Other `ReorderBuffer` users (altw external stages, apply-changes drain,
  altw passthrough) compile unchanged; the field is private.

### 3.2 `src/read/pipeline.rs` - gate, permit, shutdown, helpers

Move the item aliases to module scope so helpers can name them:

```rust
type RawItem = (usize, crate::error::Result<crate::blob::Blob>);
type DecodedItem = (usize, Option<crate::error::Result<PrimitiveBlock>>);
```

The gate (module-level, with inline tests; lock poisoning handled via
`unwrap_or_else(PoisonError::into_inner)`, matching
`altw/external/stage2.rs` house style - `unwrap_used = deny` holds):

```rust
/// Bounds spawned-but-undrained decode tasks. "In-flight" covers
/// queued + decoding + blocked-on-send; a task's Permit drops only
/// after its decoded-channel send resolves.
///
/// SINGLE-ACQUIRER INVARIANT (R2 finding 4): `acquire` is called only
/// from the one stage-2 dispatcher thread. `release` therefore only ever
/// has to wake that single waiter, so `notify_one` is correct. If a
/// second acquirer is ever added, `notify_one` silently loses wakeups and
/// this must become `notify_all` (or a per-waiter scheme). The invariant
/// is asserted in a comment on the gate, not just here.
struct AdmissionGate {
    count: Mutex<usize>,
    cond: Condvar,
    cap: usize,                  // pipeline_config.decode_ahead.max(1)
}

impl AdmissionGate {
    fn new(cap: usize) -> Self;          // cap.max(1)
    /// Returns whether the caller actually blocked (entered Condvar::wait),
    /// so the dispatcher can bump a *contended-acquisition* counter, not
    /// just cumulative time (R1 finding 3 - see 3.3).
    fn acquire(&self) -> bool;            // lock; blocked=false; while *n >= cap { wait; blocked=true }; *n += 1; blocked
    fn release(&self);                    // lock; *n -= 1; drop lock; notify_one
}

/// RAII permit; Drop = release. Held by each spawned decode task.
struct Permit(Arc<AdmissionGate>);
impl Drop for Permit { fn drop(&mut self) { self.0.release(); } }
```

Stage-2 dispatcher loop shape (replacing pipeline.rs:183-251; the
`cognitive_complexity = deny` lint is kept satisfied by extracting the
spawn into a helper):

```rust
let gate = Arc::new(AdmissionGate::new(pipeline_config.decode_ahead));
let shutdown = Arc::new(AtomicBool::new(false));
// ... inside the stage-2 scope thread, after pool build:
for (seq, blob_result) in raw_rx {
    if shutdown.load(Relaxed) {
        break;                    // drops raw_rx -> stage 1 stops promptly
    }
    match blob_result {
        Ok(blob) => {
            let t_admit = Instant::now();
            let blocked = gate.acquire();   // blocks at cap; returns whether it waited
            PIPELINE_METRICS
                .decode_admit_wait_ns
                .fetch_add(elapsed_ns_u64(t_admit), Relaxed);
            if blocked {
                // Contended-acquisition count is the honest "gate engaged"
                // signal; cumulative ns is near-nonzero even uncontended
                // (R1 finding 3).
                PIPELINE_METRICS.decode_admit_blocked.fetch_add(1, Relaxed);
            }
            let permit = Permit(Arc::clone(&gate));
            PIPELINE_METRICS.decode_tasks.fetch_add(1, Relaxed);
            spawn_decode_task(
                &decode_pool, seq, blob, dispatch_tx.clone(),
                Arc::clone(&buffer_pool), blob_filter.clone(),
                permit, Arc::clone(&shutdown),
            );
        }
        Err(e) => {
            // Direct forward, no permit (bounded by the decoded channel).
            // Timing into decoded_send_wait_ns as today; on send failure
            // the receiver is gone: break.
            if send_direct_error(&dispatch_tx, seq, e).is_err() { break; }
        }
    }
}
```

`spawn_decode_task` contains the existing decode closure body verbatim
(thread-local scratch, `catch_unwind`, filter skip, scratch-capacity
metrics), with two additions at the tail:

```rust
let t_send = Instant::now();
if tx.send((seq, item)).is_err() {
    shutdown.store(true, Relaxed);
}
PIPELINE_METRICS.decoded_send_wait_ns.fetch_add(elapsed_ns_u64(t_send), Relaxed);
drop(permit);   // release AFTER the send resolves - the documented point
```

Load-bearing details (from the source note, restated as contract):

- `drop(permit)` explicit at the closure tail is both the capture and the
  documented release point; the `Drop` impl covers anything that unwinds
  outside the existing `catch_unwind` (belt and braces - the catch
  already converts decode panics into an `Err` item that is still sent).
- **Release-after-send is a deliberate choice with a soft-bound cost, and
  the alternative is weighed here (R1 finding 1, R2 findings 1-2).**
  Releasing after the decoded-channel send bounds *in-flight permits* to
  `cap`, but a sent-but-undelivered block parked in the reorder window
  behind a slow oldest-seq holds **no permit**. So under decode-time skew
  the dispatcher keeps admitting, later seqs keep completing and releasing,
  and the window's filled-slot count has **no cap-derived algebraic
  bound**. This is a genuinely soft bound (see the corrected 3.5).
  - *The hard-bound alternative, release-after-deliver.* Let the `Permit`
    ride inside `DecodedItem` through the channel into the reorder slot and
    drop on `pop_ready` (the channel already carries a tuple; the extra
    plumbing is modest). Then `admitted - delivered <= cap`, so total live
    decoded blocks is hard-capped at `cap` with no window-blowup mode,
    matching what the doc contract implies.
  - *Why release-after-send is nonetheless chosen here.* It lets decode run
    further ahead during a bursty-consumer stall (the pool keeps work),
    which is exactly the batched `getid` / `tags-filter -R` residual shape
    and the section-6 throughput risk. The tradeoff is real: burst
    absorption vs a hard memory bound. **Decision:** ship release-after-send
    with the honest soft-bound wording (3.4, CHANGELOG, known risks) and the
    empirical `<= 64` keep-gate; keep release-after-deliver as the named
    fallback if the memory counter gate fails in practice (section 8), where
    it replaces "revert" as the first mitigation for a *memory* failure the
    way raising the default is for a *wall* failure.
- Release-after-send means a stalled consumer holds at most `cap` items
  in the decoded channel plus `cap` permit-holding tasks plus the reorder
  window (whose filled slots are soft-bounded - see 3.5). Memory bound
  derivation in 3.5.
- Wake-on-shutdown needs no extra plumbing: a failing sender releases its
  permit, release notifies, the dispatcher's next iteration reads the
  flag and breaks.
- Count-based tokens, not byte-based: simpler, and blob sizes are capped
  in practice by the 8000-elements-per-blob convention (source note,
  option 1).

Stage 3 moves into a helper that **consumes** the receiver, so it drops
at helper return, before the scope join:

```rust
fn drain_decoded<F>(
    decoded_rx: Receiver<DecodedItem>,
    decode_ahead: usize,
    block_fn: &mut F,
) -> Result<()>
where
    F: FnMut(PrimitiveBlock) -> Result<()>,
{
    let mut pending: ReorderBuffer<Option<Result<PrimitiveBlock>>> =
        ReorderBuffer::with_capacity(decode_ahead);
    loop {
        // timed recv into decoded_recv_wait_ns (existing shape);
        // Err(_) from recv -> all senders dropped -> break Ok
        pending.push(seq, item);
        PIPELINE_METRICS.record_reorder_levels(
            pending.filled_len(),    // -> reorder_high_water (memory)
            pending.pending_len(),   // -> reorder_window_high_water (skew)
        );
        while let Some(item) = pending.pop_ready() {
            match item {
                Some(Ok(block)) => block_fn(block)?,
                Some(Err(e)) => return Err(e),
                None => {}
            }
        }
    }
    Ok(())
}
```

`run_pipeline` body after the change: build channels, spawn stage 1 and
stage 2 in the scope, `drop(decoded_tx)`, then
`let result = drain_decoded(decoded_rx, pipeline_config.decode_ahead, &mut block_fn);`
followed by `PIPELINE_METRICS.emit(); result` - `decoded_rx` is moved
into the helper and is dead before the scope's implicit join. The
existing comment about `raw_rx` early-drop semantics on the pool-build
error path stays valid and stays put.

### 3.3 `src/read/pipeline_metrics.rs`

- New field `decode_admit_wait_ns: AtomicU64` - cumulative dispatcher
  block time in `acquire()`. This is the new home of read backpressure;
  `raw_send_wait_ns` structurally never captured it (stage 2 never let
  the raw channel fill) - and note that post-change `raw_send_wait_ns`
  itself becomes a live signal for the first time, since a blocked
  dispatcher finally lets the raw channel fill.
- New field `decode_admit_blocked: AtomicU64` - count of `acquire()` calls
  that actually entered `Condvar::wait` (R1 finding 3). This is the
  gate-engaged signal the validation plan reads, *not* `decode_admit_wait_ns
  > 0`: cumulative ns can be near-nonzero even when every acquisition was
  uncontended (lock/branch overhead alone), so `> 0` on the ns counter does
  not prove the cap was ever hit. A nonzero *blocked count* does. Emitted as
  `pipeline_decode_admit_blocked`.
- `reorder_high_water` changes meaning to FILLED slots. Its doc comment
  today claims "Bounded by `decode_ahead`" (pipeline_metrics.rs:31) -
  false today (elivagar's 660 is the counterexample), true after this
  change; the doc fix rides along.
- New field `reorder_window_high_water: AtomicU64` - window length
  including gaps (the old meaning; completion-skew diagnostic). Doc
  comment must state: cross-run comparisons against pre-change UUIDs use
  THIS column against the old `reorder_high_water`, not the new one.
- Replace `record_reorder_high_water(len)` with
  `record_reorder_levels(filled: usize, window: usize)` doing two
  `cas_max` calls (the free fn already exists).
- Emit the new counters in `emit()` as `pipeline_decode_admit_wait_ns`,
  `pipeline_decode_admit_blocked`, and `pipeline_reorder_window_high_water`,
  alongside the existing eight.
- **Module-doc fix (R1 finding 6):** `pipeline_metrics.rs`'s module comment
  (line 8) still names time-filter "the immediate driver" and describes the
  snapshot path's residual planet RSS. The snapshot path migrated off the
  reader in `83183fb`; only the history path remains (survey 2.3). Rewrite
  that paragraph to describe the current driver set (the five callers in
  2.2) rather than the stale time-filter framing. This rides in B7's doc
  sweep since the file is already being edited.

### 3.4 `src/read/reader.rs` - doc contract only

- `decode_ahead(n)`: same name, tighter guarantee - now also bounds
  spawned-but-undrained decode tasks; decoded in-flight memory stays
  **near** `n` blocks (~2x `n` in-flight/channel, plus a reorder window
  that is small for conventionally-framed inputs). The wording is
  deliberately "near", not "at most": release-after-send means the window
  is soft-bounded, not hard-capped, under decode-time skew or heterogeneous
  blob sizes (R1 finding 1, R2 finding 2 - see 3.5). Do not promise a hard
  `n`-block ceiling in the rustdoc.
- `into_blocks_pipelined` / `PipelinedBlocks`: dropping the iterator (or
  a `block_fn` error in `for_each_block_pipelined`) now stops the
  pipeline promptly - stage 1 stops reading within ~`decode_ahead` blobs
  instead of the background threads draining the rest of the file.

### 3.5 Memory bound and shutdown walkthroughs (the derived contract)

Bound with cap = `decode_ahead` = 32, default `read_ahead` = 16:

- raw blobs held: <= 16 (raw channel) + <= 32 (owned by in-flight tasks);
- decoded blocks alive: <= 32 in permit-holding tasks (decoded or
  blocked-on-send) + <= 32 in the decoded channel + the reorder window;
- the window's filled slots have no *algebraic* cap-derived bound (a
  pathologically slow single decode could let arbitrarily many later
  seqs complete and park). With the task queue capped at `cap` the oldest
  in-flight seq starts executing within at most ~cap task completions, so
  **for conventionally-framed inputs** - blobs near the 8000-elements-per-
  blob convention, hence size-homogeneous, plus rayon's roughly-FIFO
  injection - the skew collapses to O(cap + pool width). The keep gate
  reads the empirical counter: post-change `pipeline_reorder_high_water`
  (filled) <= 64 (2x cap) on every validation run.

  **Two corrections to the earlier framing (R1 finding 1, R2 finding 2):**

  1. *The "660" is a window figure, not a filled-slot figure.* The current
     code records `pending.pending_len()` (pipeline.rs:287), which counts
     gap slots too. No pre-change filled-slot measurement exists, so "660
     filled slots observed today" was wrong. Post-change the filled count
     gets its own counter (`reorder_high_water`); the old 660 is comparable
     only against the new `reorder_window_high_water`. The pre-change
     *unbounded* claim is about the window, and stands.
  2. *The homogeneity premise is input-dependent and not enforced on read.*
     The 8000-elements-per-blob convention is a Geofabrik/osmium property,
     not a wire-format guarantee. Official planet blobs average ~228 000
     elements/blob (`reference/blob-density.md`), and an adversarial or
     merely irregular PBF with one oversized blob at the oldest in-flight
     seq reproduces the original pathology - that seq stalls, the window
     balloons - needing only more skew than a well-formed file provides.
     The validation datasets (japan/europe, well-formed OSM) pass the
     `<= 64` gate precisely because they *are* conventionally framed, so
     passing validation does **not** establish the bound universally. The
     contract wording in 3.4 is narrowed to "near `decode_ahead` for
     conventionally-framed inputs" to match; the only way to get a true
     universal hard bound is the release-after-deliver variant (3.2), held
     as the named fallback.

Worst case for conventionally-framed inputs ~2-3 GB at planet blob sizes
versus unbounded today (source note option 1 sizing). Heterogeneous inputs
are soft-bounded, not capped - the honest claim this spec makes.

Shutdown walkthroughs the implementation must preserve (these become the
inline comments at the dispatcher and drain helper):

- **Normal completion**: stage 1 EOF drops `raw_tx`; dispatcher drains
  `raw_rx`, exits, drops `dispatch_tx`; tasks finish sends and release;
  last sender clone drops; `drain_decoded` recv Errs, returns Ok;
  receiver drops; scope joins.
- **Consumer early exit** (`PipelinedBlocks` drop, or `block_fn` Err):
  `drain_decoded` returns and its receiver drops -> blocked senders
  fail-fast, set `shutdown`, release permits -> dispatcher wakes from
  `acquire` (a release always eventually happens because permits are held
  only by tasks and every task terminates), spawns at most a handful of
  tasks whose sends fail instantly, reads the flag at loop top, breaks,
  drops `raw_rx` -> stage 1's send Errs, breaks. Scope joins. Total
  overshoot: ~cap blobs, versus whole-file today.
- **I/O error item**: forwarded on the direct path without a permit;
  a send failure there just breaks the dispatcher.
- **Decode panic**: `catch_unwind` converts to an `Err` item, send still
  happens, permit still drops (and the RAII guard covers the impossible
  path around it).
- **Pool-build failure**: error sent directly, dispatcher returns before
  the gate exists; unchanged from today.

## 4. Bricks

All bricks land as ONE commit (section 8); the per-brick gates below are
the local verification run while constructing it, per the development
contract in `reference/testing.md` (tier 1 on every edit). Placement per
that document: inline unit tests for module internals; stable-API
integration tests in `tests/read_paths.rs` file root (tier 1), imports
confined to the stable allowlist (`ElementReader`, `PbfWriter`,
`BlockBuilder`, `MemberData`, error types via public `From`).

### B1. `ReorderBuffer::filled_len`

Per 3.1. Inline tests in `src/reorder_buffer.rs`:

- `filled_diverges_from_window_across_gap`: `push(0)`, `push(2)` ->
  `filled_len() == 2`, `pending_len() == 3`; fill the gap and drain ->
  `filled_len() == 0`.
- Existing tests untouched.

Gate: `brokkr check`

### B2. Pipeline metrics fields

Per 3.3. Compiles standalone (new counters emit 0 until B4 wires them).

Gate: `brokkr check`

### B3. `AdmissionGate` + `Permit`

Per 3.2, module-level in `src/read/pipeline.rs`. Inline tests:

- `gate_blocks_at_cap_and_release_unblocks`: cap 2, acquire twice on the
  test thread; spawn a thread that acquires then signals a channel.
  **Synchronize deterministically, do not "assert no signal immediately"
  (R1 finding 4):** a bare immediate-poll can pass merely because the child
  has not been scheduled yet, so it proves nothing. Instead assert the
  blocking positively - e.g. `recv_timeout` on the child's signal channel
  returns `Err(Timeout)` for a bounded interval while the child is parked
  (the child provably reached `acquire` because it sent a *pre-acquire*
  breadcrumb on a second channel first), then `release()` and assert the
  post-release signal arrives via a blocking `recv` with a generous cap.
  The positive "child parked in wait" evidence is the assertion, not the
  absence of a signal. The new `acquire -> bool` return also lets the test
  assert the child's acquisition reported `blocked == true`.
- `permit_drop_releases`: acquire via `Permit` at cap 1 in a scope, drop
  it, acquire again succeeds.

Gate: `brokkr check`

### B4. `run_pipeline` restructure

Per 3.2/3.5: module-scope item aliases, `spawn_decode_task` helper,
`send_direct_error` helper (or inline if the lint tolerates it),
`drain_decoded` helper consuming the receiver, gate acquisition +
shutdown flag in the dispatcher, `record_reorder_levels` call sites,
doc-comment updates in-file:

- `run_pipeline`'s memory-warning block keeps the allocator-retention
  paragraph (distinct pathology, still true) and gains: "Decode admission
  is bounded by `decode_ahead`: at most `decode_ahead` decode tasks are
  spawned but undrained, so decoded blocks in flight are ~2x
  `decode_ahead` plus the reorder window. Backpressure from a slow
  consumer propagates through the decoded channel to the admission gate
  and from there to the raw channel and stage 1."
- Shutdown walkthrough comments per 3.5.

Gate: `brokkr check` - this is the brick where the conformance tests
(`pipelined_matches_sequential`, `block_iterator_matches_pipelined`,
existing `block_iterator_early_drop`) prove the restructure did not
change delivery order or error propagation.

### B5. Reader doc contract

Per 3.4. Doc-only; rustdoc examples unchanged.

Gate: `brokkr check` (clippy runs rustdoc lints; no behaviour to test)

### B6. Tier-1 pressure tests in `tests/read_paths.rs`

The existing `block_iterator_early_drop` uses the 3-block fixture against
32-cap channels, so the full-channel shutdown path is never exercised
(source note, scoping finding B). New fixture and three tests at file
root (tier 1; they run in every `brokkr check`):

- Fixture `write_many_block_pbf(path: &Path, node_blocks: usize)`:
  like `write_test_pbf` but emits `node_blocks` node blocks (3 nodes
  each, strictly ascending IDs across blocks), then the existing way and
  relation blocks. Used at `node_blocks = 16` so the file has more blocks
  than any tiny-cap channel.
- `block_iterator_early_drop_under_pressure`:
  `ElementReader::from_path(&path).unwrap().read_ahead(1).decode_ahead(1)`,
  `into_blocks_pipelined()`, `next()` once, drop the iterator. Completing
  at all is the assertion (the naive gate deadlocks here; hang = test
  timeout).
- `block_fn_error_stops_pipeline`: same tiny caps,
  `for_each_block_pipelined(|_| Err(std::io::Error::other("stop").into()))`;
  assert the call returns `Err` (public `From<io::Error> for Error`
  keeps this on the stable surface). Prompt return rather than a
  full-file drain is observable as the test not timing out; correctness
  of the propagated error is the assertion.
- `pipelined_matches_sequential_tiny_caps`: `read_ahead(1).decode_ahead(1)`
  on the 16-block fixture; collected `(type, id)` stream equals
  `collect_sequential` - ordering pinned under maximum backpressure.

The three tests above only demonstrate *eventual completion*; they do not
assert the primary bound or that shutdown was prompt (R1 finding 4). Two of
the following close that gap, and a note on the timeout mechanism replaces
the informal "hang = test timeout":

- `admission_high_water_bounded_under_slow_first_decode`: the direct proof
  of the memory bound. Because the library exposes no decode-delay hook,
  add a **test-only** admission observation seam - e.g. a `#[cfg(test)]`
  (or `cfg(feature = "test-hooks")`) callback on the pipeline config that
  fires with `(pending.filled_len(), pending.pending_len())` at each
  `record_reorder_levels` call, plus a way to make the seq-0 decode block
  on a test-held gate. Drive the 16-block fixture at `decode_ahead(2)`,
  hold seq 0 until several later seqs have completed, then release it;
  assert the observed filled high-water never exceeded the intended cap
  (this test is *expected to be the one that fails* if release-after-send's
  soft bound bites, so it doubles as the regression sentinel for R1/R2
  finding 1 - keep its bound at the honest soft value for well-framed
  fixtures, `<= 2x cap`, and comment that heterogeneous inputs are out of
  its reach). If a decode-delay seam is judged too invasive for tier 1,
  this test moves to `mod tier2` and the seam stays `#[cfg(test)]`.
- `early_exit_does_not_read_whole_file`: prove prompt shutdown positively,
  not by absence of a hang. Wrap the fixture in a counting `Read` (a small
  `struct CountingRead<R>` tracking bytes/`read` calls, constructed via the
  reader's reader-accepting constructor), drop the `into_blocks_pipelined`
  iterator after one `next()` at tiny caps, then assert the counter shows
  substantially fewer bytes read than the full file (bounded overshoot
  ~`cap` blobs, not the whole file). This is the observable assertion the
  early-drop test today lacks.
- **Timeout mechanism.** Rust integration tests have no per-test timeout,
  so "hang = test timeout" is not a real mechanism (R1 finding 4). The
  deadlock-shaped tests (`block_iterator_early_drop_under_pressure`,
  `block_fn_error_stops_pipeline`) must guard themselves: run the pipeline
  work on a spawned thread and assert completion via `recv_timeout` on a
  done-channel with a generous bound (fail the test on timeout rather than
  hanging the suite). The `brokkr test` harness wall does not substitute -
  it kills the whole sweep, not the one test, and gives no per-test verdict.

Gates:

```
brokkr check
brokkr test read_paths block_iterator_early_drop_under_pressure
brokkr test read_paths block_fn_error_stops_pipeline
brokkr test read_paths pipelined_matches_sequential_tiny_caps
brokkr test read_paths admission_high_water_bounded_under_slow_first_decode
brokkr test read_paths early_exit_does_not_read_whole_file
```

### B7. Doc sweep

- `CHANGELOG.md` under "Unreleased", new "### Changed" (behaviour change
  at an existing surface - passes the CLAUDE.md user test):

  > - The pipelined reader (`for_each_pipelined`,
  >   `for_each_block_pipelined`, `into_blocks_pipelined`) now bounds
  >   decode-in-flight memory near the `decode_ahead` knob (default 32
  >   blocks). Previously the decode stage admitted the entire file at
  >   disk rate, so decoded-block memory grew with file size (21.5 GB
  >   peak observed on a 19 GB input). Dropping `PipelinedBlocks` early,
  >   or returning an error from the block closure, now stops the
  >   pipeline within ~`decode_ahead` blobs instead of reading and
  >   decompressing the rest of the file in the background.

- `reference/pipelined-reader-paths.md` "Invariants": new paragraph
  "Admission is bounded" (decode_ahead-capped tokens, release-after-send,
  soft window bound for conventionally-framed inputs, early-exit stops
  promptly), so the reference doc stops describing the unbounded behaviour
  as current.
- `reference/pipelined-reader-paths.md` "Callers": **reconcile the stale
  inventory in the same landing (R1 finding 6)** - it is already being
  edited for the invariant paragraph. Today the list carries callers that
  migrated *off* the pipelined reader (`cat --type`, whose re-encode branch
  moved to a pread passthrough schedule - cat/mod.rs:413; and an
  `add-locations-to-ways` entry pointing at the removed `altw/dense.rs`
  path) and omits a current one (`build-geocode-index` pass1 with
  `only_relations()`, pass1.rs:34). Bring the inventory in line with the
  five production callers surveyed in 2.2 (drop the migrated ones or move
  them to the "uses something else" section, fix the altw path to
  `altw/mod.rs`, add pass1).
- `notes/pipelined-reader-decode-backpressure.md` status line flips to
  "implemented, validation pending" at commit time, then to "landed and
  validated" with the section-8 numbers.
- TODO.md "Cross-pipeline optimization" entry updated the same way.

Gate: `brokkr check` (the gremlin scan covers the markdown)

### Pre-commit gate for the whole unit

Reader semantics changed structurally, so the whole-file roundtrip check
that no smaller test makes is owed (contract item 5; the ignored
`roundtrip_denmark` runbook in TODO.md):

```
brokkr check --profile full
```

Dataset rationale: `--profile full` includes the Denmark roundtrip
(tier 3); denmark is the smallest dataset that exercises a real
multi-thousand-blob pipelined read end to end. No `brokkr verify` gate is
owed: no command's element output changes (delivery order and content are
pinned by the conformance tests and the roundtrip), and verify would read
identical bytes on both sides.

## 5. What is explicitly out of scope (stopping rule)

- **Option 2** (exporting `scan::classify` as `pub`) and **Option 3**
  (elivagar's own read loop) from the source note: separate, deliberate
  decisions, not this landing.
- **Migrating the five residual callers** off the pipelined reader: the
  fix makes the shared primitive safe; per-caller migrations remain
  individually owned TODO items (e.g. the altw sparse latent-risk entry
  in TODO.md keeps its own instrument-first discipline).
- **The allocator-retention pathology** (cross-thread `WireStringTable`
  frees): distinct mechanism, documented, unchanged.
- **`WAIT_DECODE_ADMIT` sidecar stall spans**: the `decode_admit_wait_ns`
  counter suffices for the verdict; marker-pair stall attribution joins
  the instrumentation-layering item (`notes/instrumentation-layering.md`)
  where a try-fast-path gate can be built once for all categories.
- **Byte-based admission tokens**: rejected in the source note;
  count-based is the design.
- **No knob changes**: `DEFAULT_READ_AHEAD = 16`, `DEFAULT_DECODE_AHEAD
  = 32` stay. Raising the default is the named mitigation path (section
  8), a separate landing if and only if a gate fails.
- Other `ReorderBuffer` users and every non-pipelined read path:
  untouched.

## 6. Known risks

- **Throughput on bursty consumers.** The unbounded queue does buy decode
  throughput when the consumer is bursty (the pool always has work). Cap
  32 is exactly where a batched consumer (`getid`, `tags-filter -R`)
  could slow down; section 8's wall gates exist for this, and the
  mitigation is raising the honest knob, not redesigning.
- **Per-blob mutex + condvar on the dispatcher.** ~522k acquire/release
  pairs at europe blob counts, plus a `notify_one` per release even with
  no waiter. Microseconds against per-blob zlib decompression; the wall
  gates price it anyway.
- **Semantics of `reorder_high_water` change.** Any tooling or notes
  comparing that counter across the boundary must use
  `reorder_window_high_water` for the old meaning; the metric doc
  comments carry the warning (3.3).
- **Heterogeneous blob sizes defeat the soft bound (R2 finding 2).** The
  `<= 64` filled-slot bound holds only for conventionally-framed inputs.
  One oversized blob at the oldest in-flight seq (planet-style ~228k-element
  blobs, or an adversarial mix) stalls that seq and lets the window balloon
  behind it - the original pathology, at smaller scale. There is no runtime
  guard; the validation datasets are all well-framed and cannot exercise
  it. If a real input trips this, release-after-deliver (3.2) is the fix,
  not a knob change.
- **`notify_one` rests on the single-acquirer invariant (R2 finding 4).**
  Correct only because the one stage-2 dispatcher is the sole `acquire`
  caller. A second acquirer would silently lose wakeups. Asserted in a
  comment on the gate (3.2); called out here so a future maintainer adding
  a second acquirer sees the constraint.

## 7. Sequencing constraint

`brokkr check` (and `--profile full`) stay green at the only *code*
landing boundary there is: L1 is one commit. (The post-validation record
is a separate concern with its own ordering - see section 8's keep bullet,
R1 finding 5.) Benchmark discipline per the
contract: baselines are measured on a clean tree BEFORE the commit
exists, the commit lands, then post-change numbers are measured against
that commit hash. Concretely: **do not land the implementation commit on
main until section 8's V0 baselines have been captured on the bench
host.** (Stored `.brokkr/results.db` rows cannot substitute for V0: the
only stored europe `getid --add-referenced` run, `c0d364c3` at `7cf002c`,
predates `DecompressPool` and the `parse_and_inline` fix and is
explicitly non-comparable per TODO.md.)

## 8. Validation plan and keep/revert

Bench host: plantasjen (the `reference/performance.md` reference host).
All runs sequential, never in parallel, per the benchmarking rules. Peak
anon RSS from `brokkr sidecar <UUID> --human`; counters from
`brokkr sidecar <UUID> --counters`. All recorded numbers carry commit
hash + hostname.

### V0 - baselines (clean tree at the pre-landing commit)

Dataset choices, reasoned per gate (contract item 5): denmark answers
"did small-scale wall move at all" cheaply; japan is the smallest input
whose multi-second pipelined wall can show a best-of-3 throughput delta
on an unfiltered scan; europe is the smallest input in the
completion-skew regime that produced the pathology (522k blobs; elivagar
saw high-water 163-264 at norway scale, 660 at 19 GB NA). Planet is not
pulled in: no gate here is about the 30 GB ceiling, and europe already
answers the scale question.

```
brokkr read --dataset japan --variant indexed --bench
brokkr read --dataset europe --variant indexed --bench 1
brokkr getid --dataset denmark --variant indexed --add-referenced --bench
brokkr getid --dataset europe --variant indexed --add-referenced --bench
brokkr tags-filter --dataset denmark --variant indexed -R --filter w/highway=primary --bench
brokkr tags-filter --dataset europe --variant indexed -R --filter w/highway=primary --bench
```

(`brokkr read` emits one result row per read mode; the pipelined arm's
row/UUID is the one all read-gate verdicts are read from. The europe read
is `--bench 1` on cost - four modes, sequential arm alone ~230 s - so its
wall bound below is single-shot-noise-widened, and its RSS reading is
corroborating-only, not a keep/revert gate (R1 finding 2; verdict table).
**If a real bench-3 europe memory gate is wanted**, drive only the
pipelined arm so the cost objection disappears - R1 proposes
`brokkr read --dataset europe --variant indexed --modes pipelined --bench`.
That single-mode selector is a brokkr-side feature not documented in
CLAUDE.md and unverified from this repo's sandbox; confirm brokkr accepts
`--modes` before relying on it. If it does, promote the europe RSS row to
verdict-bearing at best-of-3; if not, the counter gate remains the sole
verdict-bearing memory check and the europe RSS stays corroborating.
Invocation shapes are all
proven in `.brokkr/results.db` / `brokkr history`: `read` europe
`6ff16830`, `getid --add-referenced` planet 2026-04-28 history entry,
`tags-filter -R --filter w/highway=primary` denmark `cdab2760`.)

Record from the europe read pipelined arm: wall, peak anon RSS, and
`pipeline_reorder_high_water` (window semantics pre-change). **Escalation
rule**: if that high-water is already <= 64 at europe, europe cannot
demonstrate the cap and the memory verdict rests on the structural bound
plus the japan/europe counter readings post-change; note it and proceed
(do not reach for planet - the wall gates still hold, and the counter
bound is checked on every run regardless).

Contingent slow-consumer RSS demonstration (desirable, not verdict-bearing):

```
brokkr add-locations-to-ways --dataset japan --variant raw --index-type sparse --bench 1
```

If this errors because brokkr does not plumb pbfhogg's `--force` for
non-indexed input (survey 2.3), skip it on both sides of the boundary and
note the skip; the verdict-bearing memory gates above do not depend on it.

### L1 - the commit

All bricks B1-B7, one commit, after `brokkr check` and
`brokkr check --profile full` are green. Bundle dirty `*.md` and
`.brokkr/results.db` per the repo's git rules. Suggested subject:
`read: bound pipelined decode admission at decode_ahead; prompt shutdown on early exit`.

### V1 - post-change (same commands, at the L1 commit)

Re-run the exact V0 command list (plus the contingent altw run if V0 ran
it).

### Verdict

| Gate | Reading | Keep bound |
|---|---|---|
| Wall: `read` japan pipelined arm | best-of-3 vs V0 | <= V0 x 1.03 |
| Wall: `getid` europe `--add-referenced` | best-of-3 vs V0 | <= V0 x 1.03 |
| Wall: `tags-filter -R` europe | best-of-3 vs V0 | <= V0 x 1.03 |
| Wall: `read` europe pipelined arm | single-shot vs V0 | <= V0 x 1.05 |
| Wall: denmark smokes | best-of-3 | no pathological blowup (sub-second walls are noise; smoke only) |
| Memory: `pipeline_reorder_high_water` (filled) | every V1 run | <= 64 (2x `decode_ahead`) |
| Memory: europe read pipelined arm peak anon RSS | V1 vs V0 | corroborating, **not verdict-bearing** (R1 finding 2): single-shot at `--bench 1`, so a decrease is recorded and expected but cannot on its own decide keep/revert per the best-of-3 rule (performance.md:46). The verdict-bearing memory gate is the counter row above, which holds on every run regardless of bench count. If a bench-3 europe pipelined RSS is wanted as a real gate, drive the pipelined arm alone at `--bench` (see note). |
| Backpressure sanity | V1 counters | `pipeline_decode_admit_blocked` > 0 on at least the europe read - a nonzero *contended-acquisition count* (R1 finding 3). `pipeline_decode_admit_wait_ns > 0` is **not** sufficient: cumulative ns is near-nonzero even when the cap was never hit. |

- **All bounds met**: keep. The post-validation record - flip the source
  note and TODO.md to "landed and validated" with UUIDs; walls that moved
  beyond noise update their `reference/performance.md` rows with the
  superseded numbers and arc narrative settling into
  `reference/performance-history.md`; walls within noise leave
  `performance.md` untouched (this change claims neutrality on wall, a win
  on memory) - **is not independently committable and this must be
  resolved before landing (R1 finding 5).** Section 7 declares L1 the only
  commit, but the V1 record is markdown + `.brokkr/results.db` with no
  code, and the CLAUDE.md rule bars committing "markdown changes and/or
  results.db alone" - it must ride *upcoming code commits*. So there is no
  in-plan commit to carry it. Two orderings are legitimate; pick one
  explicitly rather than leaving the keep path unorderable:
  1. **Defer the record.** Leave the source note / TODO / performance edits
     dirty in-tree after V1 and let them tag along the *next* unrelated code
     commit (the standing "tag along dirty md + results.db" rule). The
     stored results.db rows preserve the numbers meanwhile.
  2. **Ask the user for an explicit exemption** to land the V1 record as its
     own commit. The user grants such exemptions; the spec should not
     assume one silently.
  L1 stays the only *code* commit either way; this bullet just names how
  the record lands instead of pretending it commits itself.
- **A wall bound fails**: one permitted mitigation landing - raise
  `DEFAULT_DECODE_AHEAD` (first candidate: 64), a one-line diff with its
  own commit, then re-run the failed gate plus the two memory gates. If
  the memory gates still hold and the wall recovers: keep, and record the
  new default in the same doc set. If not: `git revert` L1 (and the
  mitigation), record the numbers and the failure in the source note's
  own "failed attempts" ledger style, and re-open the note at option 2/3.
- **The memory counter bound fails** (filled high-water well above 2x
  cap): the release-after-send soft bound bit in practice - decode-time
  skew or blob heterogeneity let the window balloon (R1/R2 finding 1). One
  permitted mitigation landing before revert: switch to **release-after-
  deliver** (permit rides `DecodedItem` into the reorder slot, drops on
  `pop_ready`; 3.2), which hard-caps `admitted - delivered <= cap`, then
  re-run the memory gate plus the three wall gates. If the counter now
  holds and the walls stay within bound: keep with the hard-bound variant,
  and update 3.4 / CHANGELOG wording from "near" to "at most". If the
  counter still fails, or the walls regress past bound: `git revert` L1,
  record the numbers in the source note's failed-attempts ledger, re-open
  at option 2/3. Do not tune the cap around an unexplained number.
- **External confirmation (non-gating)**: elivagar can re-run their NA
  locations bench against `into_blocks_pipelined` and report peak RSS +
  high-water (expected 21.5 GB -> ~6-8 GB, high-water near the cap,
  compared via `reorder_window_high_water` against their old 660).

## 9. Review disposition ledger (R1 codex, R2 opus)

Two reviews of this spec were consolidated. Every source claim they made
was re-verified by direct read at the current tree before folding. Findings
folded into the sections above:

| # | Source | Severity | Verified against | Folded into |
|---|---|---|---|---|
| 1 | R1-1 + R2-1/2 | high | pipeline.rs:287 records `pending_len()`; blob-density.md (planet ~228k elems/blob) | 3.2 (release-after-send vs -deliver, decision + tradeoff), 3.4 (soft "near" wording), 3.5 (two corrections: 660 is window not filled; homogeneity is input-dependent), 6 (heterogeneity risk), 8 (memory-fail mitigation = release-after-deliver) |
| 2 | R1-2 | high | performance.md:46 (verdicts are best-of-3) | 8 V0 note + verdict table (europe RSS demoted to corroborating; `--modes pipelined` offered but flagged brokkr-unverified) |
| 3 | R1-3 | medium | spec's own `acquire()` timing | 3.2 (`acquire -> bool`), 3.3 (new `decode_admit_blocked` counter), 8 (backpressure-sanity gate now reads blocked count) |
| 4 | R1-4 | medium | Rust has no per-test timeout | B3 (deterministic gate-test sync, positive parked-evidence), B6 (slow-first-decode bound test, counting-`Read` early-exit test, `recv_timeout` guard replacing "hang = timeout") |
| 5 | R1-5 | medium | CLAUDE.md git rules | 7 (L1 = code-only boundary), 8 keep-bullet (post-validation record is not independently committable; defer-or-exemption named) |
| 6 | R1-6 | minor | pipeline_metrics.rs:8 (stale time-filter driver); pipelined-reader-paths.md Callers (stale `cat`/`altw/dense.rs`, missing pass1) | 3.3 (module-doc fix), B7 (caller-inventory reconciliation) |
| 7 | R2-3 | minor | reader.rs:281/579 early-exit chain | 1 item 2 (only the first shutdown change is deadlock-critical; the AtomicBool is promptness) |
| 8 | R2-4 | minor | single stage-2 dispatcher acquires | 3.2 (gate `notify_one` single-acquirer comment), 6 (risk note) |

Findings **not** turned into a spec change, and why:

- **R2-5 (late stragglers can bump counters after `emit()`).** Already
  disclosed in the spec at 2.1 ("Late stragglers can still bump counters
  after emit today; the gate shrinks that window but the emit position is
  unchanged"), and R2 itself agrees it is fine. No new action; the existing
  acknowledgement stands.
- **R2-5 second half (the `record_reorder_levels` swap is clean).** Not a
  defect - it endorses the existing 3.3 design. Nothing to change.
- **R1-2's specific remedy `brokkr read --modes pipelined`** is *recorded
  but not adopted as fact*: the single-mode selector is a brokkr-side flag
  not documented in CLAUDE.md and unverifiable from this repo's sandbox. The
  underlying problem (single-run verdict) is folded; the remedy is folded as
  contingent-on-brokkr-support, not asserted.

Nothing was rejected as factually wrong - both reviews checked out on every
verifiable claim.
