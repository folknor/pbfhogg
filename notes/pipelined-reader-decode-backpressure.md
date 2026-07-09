# Pipelined reader: unbounded decode-in-flight memory

Status: 2026-07-08. Analysis and decision recorded; implementation and
validation deferred (no benchmark host available today).

Origin: an elivagar bug report against pbfhogg's 3-stage pipelined reader,
cross-checked against the pbfhogg tree. The conclusion is that the pathology
is real, is documented in our own source, and is still live in two of our own
commands - so the fix is owed to pbfhogg independently of elivagar.

Companion docs:

- [external-join-oom-investigation.md](external-join-oom-investigation.md)
- [cross-pipeline-optimization-plan.md](cross-pipeline-optimization-plan.md)
- [../reference/pbfhogg-techniques-for-elivagar.md](../reference/pbfhogg-techniques-for-elivagar.md)

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

In [`src/read/pipeline.rs`](../src/read/pipeline.rs):

1. Introduce an admission counter shared between stage 2's dispatcher and the
   decode tasks. Semaphore-like: `Arc<(Mutex<usize>, Condvar)>` or a small
   permit type. Cap = `pipeline_config.decode_ahead`.
2. Dispatcher acquires before each `decode_pool.spawn`; blocks while
   `in_flight == cap`.
3. Every decode task releases + notifies exactly once after its `tx.send`
   returns - guard all three exit paths (OsmData, filtered-skip returns `None`,
   panic/error). An abort-on-drop guard should release the permit if a task
   panics before its manual release, so the dispatcher cannot deadlock.
4. Instrumentation (this is where the current archaeology cost came from):
   - `decode_admit_wait_ns` counter in
     [`pipeline_metrics.rs`](../src/read/pipeline_metrics.rs) - time the
     dispatcher spends blocked on admission (this is the new home of the read
     backpressure that `raw_send_wait_ns` used to fail to capture).
   - A **filled-slot** high-water on `ReorderBuffer` distinct from
     `pending_len()`. Today `pending_len()` (reorder_buffer.rs:58) returns the
     window length including empty gaps, which overstates memory. A count of
     `Some` slots would have made this diagnosis one counter read instead of an
     archaeology session; `pipeline_reorder_high_water` should track that.

### Semantics note (public knob)

`decode_ahead(n)` is `pub`. Its contract changes: today it bounds the decoded
channel + initial `VecDeque` capacity; after, it also bounds spawned-but-not-
drained decode tasks. Same name, tighter guarantee. Update the doc comment on
`ElementReader::decode_ahead` and the `run_pipeline` memory-warning block, and
add a CHANGELOG entry (behaviour change at an existing surface: the pipelined
reader now bounds decode-in-flight memory near `decode_ahead`).

## 7. Validation plan (deferred - needs a bench host)

Cannot run today. When a host is available:

- **No-regression bench** on the two filtered batched consumers, `getid` and
  `tags-filter` - these are the throughput-sensitive residual paths, and a hard
  cap of 32 is exactly where a bursty consumer that wanted deep decode-ahead
  could slow down. Compare wall before/after at denmark + europe.
- **Memory win** on `time-filter` and `add-locations-to-ways` (sparse) at
  europe/japan: peak RSS and `pipeline_reorder_high_water` before/after. Expect
  high-water to cap near `decode_ahead` and peak RSS to fall to working set.
- **Elivagar re-run**: they can re-run the NA locations bench from their side
  once a fix lands (against `into_blocks_pipelined`) and report peak RSS +
  high-water back. Expected: 21.5 GB -> ~6-8 GB (their other stocks), high-water
  capped near `decode_ahead`.
- Record all numbers with commit hash + hostname per repo convention; they land
  in `.brokkr/results.db` automatically for the pbfhogg-side benches.

### Known risk to watch

The unbounded queue does buy decode throughput when the consumer is bursty (the
pool always has work). If cap 32 measurably hurts a fully-parallel consumer,
`decode_ahead` is already a `PipelineConfig` field - the fix makes an existing
knob honest rather than adding a new one, so the mitigation is to raise the
default or let that one caller set it, not to redesign.

## 8. Today's blocker

No benchmark or verification host available on 2026-07-08. The analysis,
option weighting, and decision are final; the code change is small and
self-contained but must not be considered done until the section-7 benches run.
Do not commit performance claims to markdown without the commit hash + hostname
the convention requires.
