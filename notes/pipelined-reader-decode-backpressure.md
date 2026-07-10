# Pipelined reader: unbounded decode-in-flight memory

Status: LANDED AND VALIDATED 2026-07-10 (commit a0a2e3b). Implemented with
release-after-deliver permits - a hard bound where admitted minus delivered
stays at or below decode_ahead. Validation kept it: every wall gate is
neutral-to-faster (europe read -13%, japan read -3.9%, getid and tags-filter
europe within noise) and the memory bound is decisive - reorder filled
high-water pinned at 32 versus the pre-change 660, with decode_admit_blocked
at 10216 confirming the gate engaged. Baselines captured at 86a03f2 via
brokkr --commit; V0/V1 rows in .brokkr/results.db.

Origin: an elivagar bug report against pbfhogg's 3-stage pipelined reader,
cross-checked against the pbfhogg tree. The conclusion was that the pathology
was real, documented in our own source, and live in two of our own commands
before the 2026-07-10 implementation - so the fix was owed to pbfhogg
independently of elivagar.

Companion docs:

- [external-join-oom-investigation.md](external-join-oom-investigation.md)
- [cross-pipeline-optimization-plan.md](cross-pipeline-optimization-plan.md)
- [../reference/pbfhogg-techniques-for-elivagar.md](../reference/pbfhogg-techniques-for-elivagar.md)
- [injected-prepass.md](injected-prepass.md) - the other elivagar-driven
  work item; its section 9 records the touch points with this fix
  (Option-3 read loop hosts the field-5 enforcement point; land this fix
  first on the shared bench host).

## 1. Symptom (as reported)

Reported by elivagar chasing planet-on-30GB. Evidence: a North America
locations-on-ways run (19 GB PBF, elivagar commit 899f436, host `plantasjen`,
brokkr run `6a13f306`).

- phase12 peaked at 21.5 GB RSS.
- Anonymous memory ramped 0 -> 20.5 GB in the first ~20 s of a 148 s phase:
  a ~1 GB/s climb that tracks NVMe read throughput, not feature processing.
- `pipeline_reorder_high_water` reported 660 on this run (germany extracts:
  19-51, norway: 163-264).
- Extrapolated to a 90 GB planet PBF, this alone busts any 30 GB-class host.

The high-water spread (tens on small extracts, 660 on NA) is the tell: memory
is a function of completion skew across the decode pool, which grows with file
size, not a fixed working set.

## 2. Root cause

Two coupled unbounded buffers in [`src/read/pipeline.rs`](../src/read/pipeline.rs).
The 3-stage pipeline's nominal bounds (`read_ahead=16` raw channel,
`decode_ahead=32` decoded channel) do not actually bound decode-side memory.

### 2a. Stage-2 dispatcher spawns without backpressure

`run_pipeline` stage 2 (pipeline.rs:186-254) drains the raw channel and calls
`decode_pool.spawn(...)` per blob:

```
for (seq, blob_result) in raw_rx {
    ...
    decode_pool.spawn(move || { /* decompress + parse + tx.send */ });
}
```

`rayon::spawn` never blocks. Every raw blob is moved out of the 16-slot raw
channel and into the rayon task queue immediately, each queued closure owning
its raw `Blob`. The reader thread therefore never sees a full raw channel
(`pipeline_raw_send_wait_ns` is ~0 in every run we have) and streams the whole
file into queued closures at disk rate. Effective read-ahead is the entire
file, not 16 blobs.

### 2b. The reorder buffer admits far-ahead blocks unboundedly

Stage 3 (pipeline.rs:271-301) parks decoded blocks in a
[`ReorderBuffer`](../src/reorder_buffer.rs). `ReorderBuffer::with_capacity(decode_ahead)`
sets only an initial `VecDeque` capacity; `push` (reorder_buffer.rs:29-44)
calls `resize_with(slot_idx + 1, ...)` for any sequence gap, so the window
grows without limit. When a blob's decode task is stalled behind others in the
pool (work-stealing completion order is not submission order across a deep
queue), stage 3 keeps `recv()`-ing far-ahead decoded blocks and parking them.

Each `recv()` frees a slot in the 32-cap decoded channel, so senders keep
sending. Net: the decoded-channel cap bounds in-flight *sends*, not
*accumulated decoded blocks*. A high-water of 660 slots means up to ~660 held
decoded `PrimitiveBlock`s (tens of MB each, decompressed) parked in the window.

### 2c. The two compound

A deep task queue (2a) creates wide completion skew; completion skew inflates
the reorder window (2b). Read rate, not consumer rate, sets the ceiling.

### 2d. We already knew

The retention is documented in `run_pipeline`'s own doc comment
(pipeline.rs:69-92): "~25+ GB of heap retention... measured and verified across
glibc, jemalloc, and multiple `MALLOC_ARENA_MAX` configurations." `cat`'s
migration comment (cat/mod.rs:413) records the concrete failure: an earlier
`into_blocks_pipelined` + budgeted-batch path hit "28.9 GB peak measured
2026-04-26 overnight," and the house response was to move `cat` off the
pipelined reader rather than bound it.

## 3. API surface and internal users

The change lands inside `run_pipeline`; it changes no signatures. But it
changes the runtime memory behaviour - and the effective meaning of the
`decode_ahead` knob - for every consumer of the pipelined reader.

### Public surface (all `pub`, `src/read/reader.rs`)

| Entry point | Line | Shape |
|---|---|---|
| `ElementReader::for_each_pipelined` | 193 | element closure |
| `ElementReader::for_each_block_pipelined` | 232 | block closure |
| `ElementReader::into_blocks_pipelined` -> `PipelinedBlocks` | 260 | iterator |
| `ElementReader::read_ahead(n)` | 92 | builder knob |
| `ElementReader::decode_ahead(n)` | 104 | builder knob |

All three entry points funnel into the single `run_pipeline`.

### Internal callers - production

| Site | Command | Residual filter | Shape |
|---|---|---|---|
| `geocode_index/builder/pass1.rs:34` | `build-geocode-index` | `BlobFilter::only_relations()` | bounded |
| `getid/mod.rs:569` | `getid` | `with_blob_filter(BlobFilter::new(...))` (547) | filtered, selectivity-dependent |
| `tags_filter/mod.rs:396` | `tags-filter` | `with_blob_filter(filter)` (372) | filtered, selectivity-dependent |
| `altw/mod.rs:547` | `add-locations-to-ways` (non-external) | **none** | **unfiltered full scan** |
| `time_filter/mod.rs:165` | `time-filter` | **none** | **unfiltered full scan (344 M elements at Japan)** |

### Internal callers - non-production

- `cli/src/main.rs:2718` - `run_bench_read` "pipelined" arm (`brokkr read`).
- `tests/read_paths.rs` (114, 133, 151, 169, 501, 592, 628) - conformance
  tests (pipelined == sequential; early-drop-no-hang).
- `examples/partition_stats.rs:60` - example.

### Two findings that set the calculus

1. **Nobody overrides the knobs.** `read_ahead(n)` / `decode_ahead(n)` are
   defined but called from zero sites in-tree. Every internal user runs at
   defaults (16 / 32). A cap keyed on `decode_ahead` therefore lands uniformly
   at 32 across all five commands - no caller has tuned around it.

2. **The migration to `parallel_classify_phase` is partial.** All five
   commands above *also* call `scan::classify::parallel_classify_phase` for
   their main phase, but each left a residual pipelined pass behind. Two of
   those residuals (`altw:547`, `time_filter:165`) are unfiltered full-file
   scans - the exact unbounded shape elivagar reported, running inside pbfhogg
   today. So this is not "elivagar is the last consumer"; it is a latent OOM in
   our own `time-filter` and `add-locations-to-ways` residual paths, waiting
   for planet scale.

## 4. Options weighed

### Option 1 - bound in-flight decode tasks (in pbfhogg)

One token counter bounds both buffers at once. "In-flight" = spawned but not
yet delivered past the decoded-channel send (queued + decoding +
blocked-on-send).

- Dispatcher acquires a token (`Mutex<usize>` + `Condvar`, or a semaphore)
  before `decode_pool.spawn`; blocks when `in_flight == cap`.
- The decode task releases the token + notifies after its `tx.send((seq, item))`
  completes - on the OsmData path, the skip path, and the error path.
- Cap: `decode_ahead` (default 32) - its name already describes this. With
  cap 32: raw blobs held <= 32, decoded blocks (channel + window) <= ~64.
  Worst case ~2-3 GB at planet blob sizes vs unbounded today. Byte-based
  tokens would be tighter but count-based is simpler and blob sizes are capped
  in practice by the 8000-elements-per-blob convention.

Consumer-side stalls then propagate backpressure to the reader thread, which
is the correct behaviour: read at the rate the consumer absorbs, not at NVMe
rate into RAM.

Cost: adds a coordination point to a primitive pbfhogg has otherwise been
walking away from; a mis-set cap could regress readers that are well-served by
deep decode-ahead (see validation risks, section 6).

### Option 2 - export the classify machinery (pub API commitment)

`scan::classify::{build_classify_schedules_split, parallel_classify_phase}`
are `pub(crate)` today, used at ~17 internal sites (cat, altw, repack, extract,
getid, tags-filter, tags-count, degrade, getparents, geocode pass2, check
refs/verify_ids, time-filter, multi-extract). Making them `pub` lets elivagar
restructure phase12 onto the pattern pbfhogg itself believes in: shared
`File`, pre-built per-kind schedule, workers that pread+decode+process on one
thread (allocation confined per worker, no cross-thread drops), 16/32-cap
channels, small per-push reorder buffer. Bounded by construction; it also
deletes the ordered-consumer send bottleneck at the root.

Cost: the classify surface is still moving (check/extract/repack keep adapting
it). `pub` is a hard-to-walk-back contract. Justify only if elivagar adopts
classify as its permanent primary read path, not a one-phase experiment.

### Option 3 - elivagar owns its read loop on existing public API

`BlobReader` (sequential raw-blob iteration, no decode) + `Blob::to_primitiveblock()`
+ a bounded channel of elivagar's choosing. No pbfhogg change. Classification
stays post-decode via `block_type()`. The one wrinkle - raw-mode's sorted node
store needs in-order puts - is solved by keeping the raw path on the old reader
(legacy-adequate per elivagar's roadmap) and building the new loop for
locations-on-ways only, which is the production and record shape.

## 5. Decision

The three are not mutually exclusive. Chosen split:

1. **Ship Option 1 in pbfhogg** on pbfhogg's own merits. It closes the latent
   `time-filter` / `altw` full-scan OOM and protects any external
   `into_blocks_pipelined` user (elivagar included) for free. This is the
   in-tree work item.
2. **Let elivagar take Option 3** short-term for phase12 - bespoke structure,
   raw-mode nuance, no coupling to a still-moving pbfhogg surface.
3. **Defer Option 2** as a separate, deliberate decision, taken only once
   elivagar has proven classify is its long-term read path and the
   `pub(crate)` surface is worth freezing into `pub`.

The key reframing: Option 1 is justified without elivagar in the room. We are
fixing our own documented pathology in our own commands.

## 6. Implementation plan (Option 1)

Detailed to execution-readiness 2026-07-09 (source-verified against the
tree at `119edc5`; rationale for the shutdown items is in the "Scoping
findings" subsection below). One coherent commit: gate + shutdown +
counters + tests + docs; `brokkr check` green at the boundary; bench-host
validation per section 7 before any recorded claim; keep/revert on the
section-7 gates.

### 6.1 `src/reorder_buffer.rs` - O(1) filled-slot count

- Add a `filled: usize` field, incremented in `push`, decremented in
  `pop_ready` on a successful pop; expose `pub(crate) fn filled_len()`.
  A maintained counter, not a scan.
- `pending_len()` keeps its current meaning (window length including
  empty gap slots).
- Inline tests: filled vs window diverge across a gap (`push(0)`,
  `push(2)` -> filled 2, window 3); drain returns filled to 0.
- Other `ReorderBuffer` users (external stage 1/3/4, apply-changes
  drain, altw passthrough) compile unchanged; the field is private.

### 6.2 `src/read/pipeline.rs` - AdmissionGate, Permit, shutdown

Module-level, with inline tests:

```rust
struct AdmissionGate {
    count: Mutex<usize>,
    cond: Condvar,
    cap: usize,          // pipeline_config.decode_ahead.max(1)
}
// acquire(): lock; while *n >= cap { wait }; *n += 1
// release(): lock; *n -= 1; drop lock; notify_one
struct Permit(Arc<AdmissionGate>);   // Drop = release
```

Lock poisoning: `unwrap_or_else(PoisonError::into_inner)`, matching the
house style in `altw/external/stage2.rs`.

Dispatcher (stage 2) loop shape:

```rust
let shutdown = Arc::new(AtomicBool::new(false));
for (seq, blob_result) in raw_rx {
    if shutdown.load(Relaxed) { break; }      // drops raw_rx -> stage 1 stops
    match blob_result {
        Ok(blob) => {
            let t = Instant::now();
            gate.acquire();                    // blocks at cap
            METRICS.decode_admit_wait_ns += elapsed(t);
            let permit = Permit(Arc::clone(&gate));
            decode_pool.spawn(move || {
                /* existing catch_unwind decode body, unchanged */
                if tx.send((seq, item)).is_err() {
                    shutdown_flag.store(true, Relaxed);
                }
                /* decoded_send_wait_ns timing as today */
                drop(permit);   // release AFTER the send resolves
            });
        }
        Err(e) => { /* direct send, unchanged - no permit needed;
                       bounded by the decoded channel */ }
    }
}
```

Details that are load-bearing:

- `drop(permit)` explicit at the closure tail: move closures capture
  only referenced variables, so the drop is both the capture and the
  documented release point. The `Drop` impl covers the panic path (the
  existing `catch_unwind` converts decode panics to `Err` items and
  still sends; the guard is belt-and-braces for anything outside it).
- Release-after-send means "in-flight" includes channel-blocked
  senders: a stalled consumer holds at most `cap` items in the decoded
  channel + `cap` permit-holding tasks + the reorder window (~2x
  `decode_ahead` decoded blocks). That is the memory bound.
- Wake-on-shutdown needs no extra plumbing: failing senders release
  permits, release notifies, the dispatcher's next iteration reads the
  flag and breaks.
- Stage 3 must own `decoded_rx` and drop it when its receive loop
  exits, BEFORE the scope join - today it is created outside
  `thread::scope` and outlives the join, which is exactly what turns
  the gate into a deadlock on early exit (Scoping findings, item A).
  Concretely: move the loop body into a helper
  `fn drain_decoded<F>(decoded_rx: Receiver<DecodedItem>, config, block_fn: &mut F) -> Result<()>`
  that consumes the receiver; `run_pipeline` calls it and the receiver
  drops at helper return. The helper split also keeps
  `cognitive_complexity = deny` satisfied as the dispatcher grows; a
  `spawn_decode_task` helper may be needed for the same reason.
- In the helper, record `filled_len()` into `reorder_high_water` and
  `pending_len()` into the new `reorder_window_high_water` after each
  push.
- Doc updates in-file: `run_pipeline`'s memory-warning block keeps the
  allocator-retention paragraph (that is a distinct pathology) and
  gains a paragraph stating decode admission is now bounded by
  `decode_ahead` (~2x decode_ahead decoded blocks + window in flight);
  the stage-2 comment about `raw_rx` early-drop semantics stays valid.

**Shipped design revises this plan.** The landed code
(`src/read/pipeline.rs`) does not release the permit after `tx.send`
returns, as sketched above. The `Permit` rides inside `DecodedItem`
through the decoded channel and into the reorder buffer slot, and is
dropped when `drain_decoded` pops the item out via `pop_ready` -
release-after-*deliver*, not release-after-*send*. That makes the
bound `admitted - delivered <= cap`, a hard cap on live decoded
blocks rather than the ~2x-decode_ahead soft bound this section
describes. The pseudocode and the "~2x `decode_ahead`" bullet above
are kept as the historical record of the plan as scoped 2026-07-09;
they do not describe what shipped. `run_pipeline`'s in-file doc
comment and `ElementReader::decode_ahead`'s rustdoc use "at most
`decode_ahead`" language accordingly.

### 6.3 `src/read/pipeline_metrics.rs`

- New `decode_admit_wait_ns` - time the dispatcher spends blocked on
  admission. This is the new home of the read backpressure that
  `raw_send_wait_ns` structurally failed to capture (stage 2 never let
  the raw channel fill).
- `reorder_high_water` changes meaning to FILLED slots (the memory
  diagnostic) and its doc comment - which today claims "Bounded by
  `decode_ahead`", false until this change - becomes true.
- New `reorder_window_high_water` - window length including gaps (the
  completion-skew diagnostic; the old meaning). Cross-run comparisons
  against pre-change UUIDs (e.g. elivagar's 660) must use this column,
  not `reorder_high_water`.
- Emit all three alongside the existing counters.

### 6.4 `src/read/reader.rs` - doc contract

- `decode_ahead(n)`: same name, tighter guarantee - now also bounds
  spawned-but-undrained decode tasks; decoded in-flight memory is ~2x
  `n` blocks plus the reorder window.
- `into_blocks_pipelined`: early drop now stops the pipeline promptly
  (no full-file drain on detached threads).

### 6.5 Tests (tier 1, no bench host needed)

- Inline: `AdmissionGate` blocks at cap / release unblocks (two
  threads, generous timeout); `ReorderBuffer::filled_len` semantics.
- `tests/read_paths.rs` (fixture: parameterized `write_test_pbf`
  variant emitting ~16 node blocks, since the existing 3-block fixture
  cannot fill any channel):
  - `block_iterator_early_drop_under_pressure`: `.read_ahead(1)
    .decode_ahead(1)`, `next()` once, drop. Completing at all is the
    assertion (deadlock = hang = test timeout).
  - `block_fn_error_stops_pipeline`: `for_each_block_pipelined` whose
    closure errors on the first block, same tiny caps; asserts the Err
    propagates (and returns promptly rather than draining the file).
  - `pipelined_matches_sequential_tiny_caps`: conformance at
    `decode_ahead(1)` - ordering under maximum backpressure.

### 6.6 CHANGELOG + doc sweep

- CHANGELOG entry (behaviour change at an existing surface): the
  pipelined reader now bounds decode-in-flight memory near
  `decode_ahead`; early drop / error exit stops promptly instead of
  draining the rest of the file.
- This note's status line flips to "implemented, validation pending";
  TODO.md entry updated the same way.

(Historical aside on why the filled-slot counter exists: `pending_len()`
returns window length including empty gaps, which overstates memory. A
count of `Some` slots would have made the original diagnosis one counter
read instead of an archaeology session.)

### Scoping findings (2026-07-09, code-verified)

Source-level scoping pass before implementation; the 6.x subsections
above already incorporate the consequences. Recorded separately because
the WHY is not obvious from the plan items alone.

**A. The naive gate deadlocks on early exit; today's code does not.**
Verified in rayon-core 1.13.0: `ThreadPool::drop` only calls
`registry.terminate()` (registry.rs:594) - it does NOT join spawned
tasks; workers drain their queues on detached threads. That is why
today's unbounded spawn survives consumer early-exit (`PipelinedBlocks`
drop, or `block_fn` returning `Err`): stage 2 drains the entire raw
channel, spawns everything, drops the pool without blocking, the scope
joins, `run_pipeline` returns, `decoded_rx` finally drops, and the
up-to-32 senders blocked on the full decoded channel fail-fast. Cost
today: an early exit still READS the whole file and decompresses every
queued blob on detached threads that can outlive `run_pipeline`'s
return. Not a hang - but with a naive admission gate it becomes one:
dispatcher blocked in `acquire()` (a scope thread), permits held by
tasks blocked in `tx.send`, `decoded_rx` alive until after the scope
join, which waits on the dispatcher. Real deadlock cycle.

Two required shutdown changes ship with the gate:

1. **Stage 3 drops `decoded_rx` when its loop exits** (move it into the
   stage-3 flow; explicit drop before the scope join). Blocked senders
   then fail-fast, permits release, the dispatcher unblocks.
2. **Shutdown fast-path**: an `AtomicBool` set by any task whose
   `tx.send` fails; the dispatcher checks it each iteration and breaks,
   dropping `raw_rx` so stage 1 stops reading promptly. This upgrades
   early-exit behaviour from "read + decompress the rest of the file"
   to "stop within ~cap blobs" - a real win for early-exit/zip
   consumers, and it makes error returns from `block_fn` fast on large
   files.

**B. The existing early-drop test cannot catch any of this.**
`tests/read_paths.rs::block_iterator_early_drop` uses a 3-block
fixture against a 32-cap channel; the full-channel shutdown path is
never exercised. The implementation must add a deterministic test:
`decode_ahead(1)` + `read_ahead(1)` against a fixture with more blocks
than the caps (e.g. ~16 node blocks), early-dropped after the first
item, plus a `for_each_block_pipelined` variant whose `block_fn`
errors on the first block. Also re-run the pipelined-equals-sequential
conformance shape at `decode_ahead(1)` to pin ordering under maximum
backpressure.

**C. Permit release point confirms the ~2x bound.** Releasing after
`tx.send` returns means "in-flight" includes channel-blocked senders,
so a stalled consumer holds at most `cap` items in the decoded channel
plus `cap` permit-holding tasks plus the reorder window - the ~64-block
worst case in Option 1's sizing, now derived rather than asserted.

Also verified: `pipeline_metrics.rs:31` currently documents
`reorder_high_water` as "Bounded by `decode_ahead`" - false today (the
660 measurement is the counterexample); becomes true once the gate
lands. The doc fix rides along. `ReorderBuffer` gains an O(1)
`filled_len()` (maintained counter, not a scan); `reorder_high_water`
switches to filled slots per the instrumentation plan above, and the
old window-length meaning moves to a new
`pipeline_reorder_window_high_water` counter so completion-skew
visibility is not lost.

### Semantics note (public knob)

`decode_ahead(n)` is `pub`. Its contract changes: today it bounds the decoded
channel + initial `VecDeque` capacity; after, it also bounds spawned-but-not-
drained decode tasks. Same name, tighter guarantee. Update the doc comment on
`ElementReader::decode_ahead` and the `run_pipeline` memory-warning block, and
add a CHANGELOG entry (behaviour change at an existing surface: the pipelined
reader now bounds decode-in-flight memory near `decode_ahead`).

## 7. Validation plan (implemented, pending - needs a bench host)

The code landed 2026-07-10 without running this plan; no benchmark host
was available. Nothing below has run yet. When a host is available:

- **No-regression bench** on the two filtered batched consumers, `getid` and
  `tags-filter` - these are the throughput-sensitive residual paths, and a hard
  cap of 32 is exactly where a bursty consumer that wanted deep decode-ahead
  could slow down. Compare wall before/after at denmark + europe.
- **Memory win** on `time-filter` and `add-locations-to-ways` (sparse) at
  europe/japan: peak RSS and `pipeline_reorder_high_water` before/after. Expect
  high-water to cap at `decode_ahead` (the shipped design hard-caps filled
  slots, per the 6.2 revision note above - not merely "near" it) and peak RSS
  to fall to working set.
- **Elivagar re-run**: they can re-run the NA locations bench from their side
  once a fix lands (against `into_blocks_pipelined`) and report peak RSS +
  high-water back. Expected: 21.5 GB -> ~6-8 GB (their other stocks), high-water
  capped at `decode_ahead`.
- Record all numbers with commit hash + hostname per repo convention; they land
  in `.brokkr/results.db` automatically for the pbfhogg-side benches.

### Known risk to watch

The unbounded queue does buy decode throughput when the consumer is bursty (the
pool always has work). If cap 32 measurably hurts a fully-parallel consumer,
`decode_ahead` is already a `PipelineConfig` field - the fix makes an existing
knob honest rather than adding a new one, so the mitigation is to raise the
default or let that one caller set it, not to redesign.

## 8. Status

No benchmark or verification host was available through 2026-07-10. The
analysis, option weighing, and decision were final before that date, and
the implementation (section 6, revised per the 6.2 note above) has since
landed in-tree. Section 7's validation plan has not run - do not consider
this fix proven correct on wall-clock or memory grounds, and do not commit
performance claims to markdown, until the section-7 benches run on a real
host.
