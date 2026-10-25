# Hygiene

Values with more than one owner, channels with more than one implementation,
policy reinvented per call site, dead code, and host assumptions, consolidated
from the hygiene hunt across every scope. Each entry gathers every site the
hunters named and what they said could enforce the fixed version. Defects
found on the way are in `notes/bugs.md`.

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.
6. The higher an entry's anger index, the higher the bar for implementing
   anything as a result of it. A high-anger entry should more often than not
   end as a code comment (rule 3) rather than a code change.
7. The higher an entry's anger index, the more care is owed - exponentially -
   against performance regression from any change made for it. Before
   touching code under a higher-index entry, read
   `reference/performance-history.md` and `reference/performance.md`. The
   current code is likely not an accident.
8. Nothing is measured - no benchmark, A/B, planet run or profiling cell -
   until this document and `notes/bugs.md` are both drained. An entry whose
   only remaining work is a measurement is removed, not kept waiting.
```

Each entry carries an `Anger:` rating from 0 to 10: how likely resolving it
is to make the maintainer angry. 10 is rebuilds or contract changes on
measured paths; 9 is hot-path or contract changes that need benchmarks or
the maintainer's decision; 8 is altw, renumber and apply-changes internals
and observability contracts; 7 is geocode output and format, and CLI
behaviour; below that the work is progressively more contained.

---

## Triage of 2026-10-25

Not a finding. The maintainer and an orchestrating session walked the open
entries of this file and `notes/bugs.md` and attached a "Recommendation
(2026-10-25)" paragraph to each entry they discussed. These are
recommendations, not decisions: they rest on a limited reading of the code
and history (the session got facts wrong along the way and corrected some of
them; others are unverified and say so), and the agent that takes an entry is
free to disregard them entirely. Where the maintainer ruled something
themselves, the entry says "Maintainer:".

Entries were split so that each shared API and each command gets its own
entry; the split-off entries took fresh IDs.

Launch order, one agent per line, run serially:

4. HYG-061
5. HYG-152
6. HYG-153
7. HYG-154
8. HYG-155
9. HYG-156
10. HYG-157
11. HYG-017
12. HYG-038
13. HYG-032
14. HYG-161 (after HYG-032)
15. HYG-162
16. HYG-163
17. HYG-034
18. HYG-164
19. HYG-165
20. HYG-166
21. HYG-167 (after the other HYG-034 pieces: it adds the text rule)
22. HYG-151 (confirms the per-command list below)
23. HYG-168 to HYG-179, one per line, in ID order
24. HYG-047
25. HYG-158
26. HYG-159
27. HYG-160
28. HYG-077 (last: it annotates files across the crate)

Discussed and skipped for now: HYG-036, HYG-037, HYG-058, HYG-109. Not
discussed: HYG-002, HYG-010, HYG-011, HYG-012, HYG-013, HYG-014, HYG-015,
HYG-016, HYG-053, HYG-081, BUG-009.

---

## Review once drained

Not a finding: a standing list of changes made while draining this file and
`notes/bugs.md` that need a thorough, independent review once both are
drained (rule 8's measurement gate lifts at the same point). Rule 4's
deletion waits on this list too: it is reviewed, or moved somewhere that
outlives this file, before the file goes. Add to it whenever a drain change
is a judgement call on a measured or deliberate path.

The first drain round (commits `1db71d8a`, `9cce24b3`, `1dd2ea09`) was done
by agents on items picked as "no-brainers". One pick was wrong: removing the
unused `direct_io` parameters treated deliberate plumbing as dead code, and
the one removal that landed (the parallel diff's `_direct_io`) was put back.
That makes every other change in the round suspect of the same mistake -
undoing something deliberate, or costing time or memory on a measured path -
and none of it has been measured. A reviewer with no stake in the round's
conclusions reads those commits against the code and history they changed
and reports, per change, whether it undid a deliberate choice, and whether it
can cost wall time, RAM or memory pressure.

The judgement calls already known, to be checked first:

- altw external's metadata scan refuses indexdata that overstates `max_id`
  (as part of the new order check), reversing the tolerance commit
  `2fabb026` built into stage 2's walk on purpose. Reviewed and kept by the
  maintainer; the reason is in `scan_blob_metadata`'s doc.
- The same scan refuses node blobs whose range holds negative IDs.
- The `blob_body_within_file` checks in the classify builders were deleted
  as unreachable behind `HeaderWalker`'s extent check, as was getparents'
  duplicate.
- The writer framing refactor (`frame_in_scratch`) moved the sync path's
  entry trim inside `compress_ns` and an oversize `compress_buf` free ahead of
  the write; argued allocation-neutral, not measured.
- `write_primitive_block*` now take their permit through `acquire_permit`
  before spawning, and the guard moves into the closure.
- `EPERM` and `EINVAL` joined `EXDEV`, `ENOSYS` and `EOPNOTSUPP` in the
  `copy_file_range` userspace-fallback set, with fstat and fcntl guards on
  the cold path. Kept by the maintainer: targets that used to fail now
  work, slower.
- The io_uring CopyRange loop accepts a short pread as progress instead of
  failing.
- Two public APIs were added (`BlobReader::new_named`, a now-public
  `Blob::locate_error`), and `blob_from_offset` reads after an earlier failed
  read instead of reporting no blob.
- `CliInvoker::child_limits` was deleted with the other unused builders, and
  the docs that cited it reworded.
- Degrade's sort-preservation guard now also checks `pending`, which was
  argued always empty there.
- altw external's `max_node_id_from_blob_meta` lost its reversed-range and
  negative-`max_id` checks (and their tests) as unreachable behind the new
  scan, and `build_node_blob_mapping` lost its reversed-range check.
- The diff and derive delete and merge-stats helpers were merged to one owner
  each across modules, argued inlining-neutral under fat LTO.
- The geocode build no longer runs `create_dir_all` on its output directory,
  argued redundant because `OutputDir` creates the staging directory it
  hands the build; a staging directory that vanished mid-run now fails at
  the first data-file create (naming the file) instead of being recreated.
- `read::input::read_to_string` now reports non-UTF-8 content as
  `ErrorKind::InvalidData` rather than `Io` for every caller (getid ID files,
  extract configs and region files, the tags-filter expressions file), which
  library consumers matching on the kind will see.
- The text diff's three drivers (sequential block-pair, element stream,
  parallel shards) now take their writer as an `Option` so `diff_stats` can
  share them without formatting: each write site gained an `if let Some`
  branch, the shard writer became `Option<BufWriter<File>>`, and the
  `diff` entry point moved its body into a shared `compare`. Argued
  branch-predictable and below noise on the text path, not measured. The
  count mode deliberately keeps the byte-equal fast path off (it follows
  `suppress_common`, which `diff_stats` holds false) so its counts and
  refusals equal an unsuppressed text diff's.
- The CLI's inputs, outputs and scratch site now come from one
  `CommandPaths` (`cli/src/command_paths.rs`), which also made
  `build-geocode-index` refuse an `--output-dir` holding its input (or the
  entry naming it). That refusal is new and runs before the existing-output
  refusal, as the alias check does for every other command; the directory
  rule deliberately allows a path that only passes through a symlinked
  directory inside the output.
- Diff's input checks were made path-independent (the sequential blob
  order and tail fixes). Judgement calls on measured paths:
  - `BlobReader::next` is now `next_frame` plus `RawFrame::into_blob`, on
    every sequential read in the crate; the envelope parse is unchanged but
    page-cache eviction now runs before it rather than after. Argued
    cost-neutral, not measured.
  - `OrderWatermark` (shared by the diff guard, apply-changes' output guard
    and altw external's metadata scan) takes a mixed-sign range's first
    element as `min_id` rather than 0, so it accepts valid mixed-sign
    layouts it used to refuse; it can no longer certify order inside such a
    range.
  - The sequential block-pair merge runs the order guard on every blob it
    reads and, under a filter excluding the trailing kinds, walks the rest
    of both files' headers with a `HeaderWalker` (a pread per remaining
    blob, nothing for unfiltered runs).
  - The unindexed element stream now checks element order per merged
    element (one key compare), classifies each block not of the phase's
    kind by iterating its elements, and drops excluded kinds' blocks
    without merging their elements; it still decompresses every block of both
    files, deliberately, since that is what refuses a misplaced block.
    Expected faster under `--type`, slightly slower per element otherwise;
    not measured.
  - `WireBlobHeader::parse` refuses a known field with the wrong wire type
    under every retention flag, so a header some read paths used to accept
    is refused everywhere.
- O_DIRECT alignment is discovered per open (`src/direct_io.rs`, statx
  `STATX_DIOALIGN`) with a 4096 floor, so every host that reports 4096 or
  less, or nothing, keeps byte-identical I/O; only a larger requirement
  changes buffer alignment, tail padding and skip arithmetic. Judgement calls
  on measured paths, none measured:
  - One `statx` per direct reader or writer open (several per run on cat,
    extract and geocode paths, never per blob or worker).
  - A reported alignment that is not a power of two or exceeds
    `IO_BUF_BYTES` is a new `direct-io` platform refusal (exit 3) at open.
  - The io_uring pool is now allocated after the output open instead of
    before ring creation (binding declared first, so drop order is kept).
  - `DirectReader` tracks its fd offset: `skip` uses it instead of a
    `stream_position` syscall, and a regular file's final fill stops at
    `file_len` instead of issuing a read at the unaligned offset the last
    short read left (block devices keep reading to the device's EOF).
  - The library's `libc` minimum went to 0.2.189 for
    `stx_dio_read_offset_align`; the lockfile was already there.
  - The io_uring path could not be run end to end here (the bench host's
    `RLIMIT_MEMLOCK` refuses it, see ADR-0014); only unit tests cover it.
- The unindexed diff and derive paths (sequential, inputs without
  indexdata) no longer convert every element to owned strings: the
  element-level merge (`osc::merge_join::stream_merge_phase`) compares and
  prints borrowed elements straight from each side's current block, through
  the same action handler as the block-pair path, so the derived OSC now
  carries full metadata on both paths. Judgement calls on measured paths,
  none measured:
  - Per side the decoded block now stays alive while its elements are
    merged, instead of being dropped after conversion to an owned buffer.
    Each side's peak is no higher than before (conversion held block plus
    buffer), but the two sides' peaks can now coincide: a large block on one
    side stays resident while the other side walks many small blocks. Bounded
    by two decoded blocks, the same shape as the indexed block-pair merge.
  - Derive's borrowed OSC writers (every path, the parallel shards included)
    now resolve tags, roles and the user name, and diff's verbose details
    tags and roles, through the checked lookups and refuse a bad string
    index; argued cost-neutral (the lookups were already done, only the
    error is kept).
  - The unindexed text diff no longer errors on a corrupt string index in an
    element it only compares or deletes; it reports it as different, as the
    indexed path always did (contract in `content.rs`).
  - Unindexed verbose diff details switched from the owned linear tag scan
    to the borrowed `HashMap` one the indexed path uses (differs only on
    duplicate keys).
- Fault injection moved from process-global statics (the
  `static-test-hooks` feature, its sweep, check script and one-process
  test targets, all deleted) to one `FaultPlan` per cancellation token
  (`src/test_hooks.rs`), and the fault tests folded into `tests/all.rs`
  and gzip's unit tests. Every hook site is gated on `test-hooks`, so
  product builds compile none of it, except two always-compiled shape
  changes: `ParallelGzipWriter::new` takes the invocation's `&Cancel`
  (unused without the feature), and `Output::pbf_writer` matches on the
  backend inside an `if let` whose condition is a constant `None`
  without the feature. Argued codegen-neutral, not measured. Also:
  `MergeOptions` lost its test-only field and derives `Default`;
  `PbfWriter::test_panic_in_framing_at` became a plan field reached
  through `PbfWriter::test_faults`; the read pipeline's decode gate lost
  its reset/epoch machinery (a plan is fresh per token). Two migrated
  tests had been passing vacuously and were fixed on the way: the altw
  stage 3 test set `force`, which the external index refuses before
  stage 3, and the derive test re-staged the output its clean run had
  just published, which is refused before any shard runs.
- Sidecar counters moved onto `src/counters.rs`: `counter_set!` generates
  each counter struct, `new`, `Default` and a private visitor and emitter
  from one list of `field => "emitted_name"` (optional `/ 1_000_000` for
  nanosecond accumulators emitted as milliseconds); `record_max` is the one
  max loop; `Stopwatch` replaced `crate::elapsed::elapsed_ns_u64` (deleted)
  and the altw `as_millis() as u64` / `as_nanos() as u64` truncations;
  `debug::emit_counter` takes `impl CounterValue` (signed exact, unsigned
  saturating, conversion inside the armed-FIFO closure). Every emitted
  name was checked to appear the same number of times per file before and
  after. Judgement calls on measured paths, none measured:
  - Altw stages 1 to 4, passthrough and sparse pass 1 hold their counters
    in one struct per stage instead of separate stack-local atomics (the
    `_ref` bindings are gone). Same atomic operations, but which counters
    share a cache line changed; argued below noise at per-blob, per-bucket
    and per-flush frequency. No counter in these stages is bumped per way
    or per ref; the finest-grained are stage 4's per-group and
    per-non-way-field reframe timers, which already shared one struct and
    keep their field order.
  - The two production `fetch_max` sites (altw stage 4 readiness-wait max,
    the parallel gzip reorder high-water) now use `record_max`: fewer
    locked operations when the maximum does not move, the same otherwise.
  - Truncating and saturating conversions were treated as value-identical
    (they differ only past about 584 years of nanoseconds, or past
    `i64::MAX` at emission). Per-sample millisecond truncation was kept,
    and the stage 2 `_ms` / `_ns` twins still read the clock twice, in the
    old order.
  - Emission order changed only in altw stage 4 (the worker set first, then
    the consumer-side locals). `PIPELINE_METRICS` stays one process-global
    static; its six pool-drop fields moved to a second static,
    `DECOMPRESS_POOL_METRICS`, still emitted only through
    `emit_decompress_pool_counters` from the exit hook.
- The rest of the crate's counters moved onto the same machinery.
  Apply-changes' `DrainCounters` and `WorkerCounters` are `counter_set!` sets
  (still two sets in two allocations; `counter_set!` gained an unnamed,
  never-emitted counter form so `loc_node_blobs_scanned` keeps its place);
  renumber's `StageCounters` is generated by a local macro that spells
  every phase's name verbatim, the phase passed only at emission so the
  per-worker payload is the same seven atomics; plain emitted values lost
  their `as i64` casts and redundant `i64::try_from` wrappers, timers use
  `Stopwatch`, and `scripts/check_source_invariants.py` now refuses an
  `as i64` inside `emit_counter` arguments. Every emitted name was counted
  per file before and after (renumber's 28 now-literal names checked by a
  unit test against the old `format!` spelling). Judgement calls, none
  measured:
  - Moving the apply-changes atomics into macro-generated structs may
    change field layout and so which counters share a cache line (same
    declaration order, same atomic operations; Rust's default layout
    permits reordering either way). The drain's high-water marks stay a
    plain load and store, the drain being their only writer.
  - Degrade's `degrade_drop_n` and `degrade_drop_seed` keep their old
    wrapped value for inputs from 2^63 up (`cast_signed`), where every
    other unsigned value now saturates at `i64::MAX` instead of wrapping.
  - `merge_changes_decompress_ns` accumulates u64 nanoseconds with a
    saturating add instead of a `Duration`; it would differ from the old
    wrapping cast only past about 292 years of cumulative read time.
- Per-run ownership of `PIPELINE_METRICS` and `WRITER_METRICS` was
  adjudicated against: the sidecar reads both as process totals
  (`--stalls` takes the maximum of each monotonic `*_wait_ns`), so per-run
  emission would undercount multi-pass processes and per-run instances
  folded back into a total would change nothing observable; the reasons
  are in `pipeline_metrics.rs`, beside `WRITER_METRICS`, in
  `reference/pipelined-reader-paths.md` and `reference/testing.md`.
  Changes made instead, none measured:
  - Both read engines emit `PIPELINE_METRICS` after `thread::scope` has
    joined instead of inside it, so waits recorded while stages shut down
    on an error, cancellation or early drop are in the snapshot. One emit
    per pipelined read either way; a stage panic re-raised by the scope now
    emits nothing (it used to emit, then panic).
  - `WriterMetrics::sync_recorded` is now the crate's only output fsync:
    `sync_file` (buffered, also `to_path_sync`'s finalizer),
    `DirectWriter::sync_all`, the `pwrite` pool and io_uring call it, and
    `record_sync_all` became private. The parallel and io_uring durable
    syncs now count a failed fsync (they returned before recording); the
    timed span of `FileWriter::sync` and of `to_path_sync`'s finalizer
    shrank to the fsync itself (the buffered `EINVAL` fstat now falls
    outside it).
  - `with_sync_observer(&Cancel, f)` wraps `FileOutputSink::flush`, the
    `pwrite` pool's durable sync, io_uring's `flush_final`,
    `PbfWriter::flush`'s synchronous finalizer and the three `publish`
    methods. Without `test-hooks` it is an `#[inline(always)]` `f()`;
    argued codegen-neutral. With it, a thread-local selects the token
    whose `FaultPlan::sync_calls` counts the fsyncs, which made the
    writer, file-writer and publication fsync tests exact (they now run
    only under `test-hooks`, i.e. the `all` sweep).
- Input preconditions read through one front read
  (`src/commands/input_front.rs`, `InputFront`): one `FileReader` open
  (direct I/O honoured) reads the header frame, decodes the header only when
  asked and probes to the first data blob's header only when asked, so each
  caller keeps its refusal order. Apply-changes lost its second open
  (`require_indexdata` plus `read_header`), altw its `auto_select_backend`
  `ElementReader` and `BlobReader`, the explicit-sparse hint reader, the
  external path's `require_indexdata` and `is_sorted` reader and stage 4's
  header `BlobReader` (the header is handed down), diff, derive and
  `check --ids` their second open each; inspect and bench-write dropped the
  probe opens whose only job was the missing-header error, and each scan
  refuses a header-less file itself (`BlobReader::leading_header` became
  public for bench-write; an `altw auto` row joined the input contract).
  Judgement calls, none measured:
  - Argued below noise, not nil: each removed front open re-read the
    file's prefix (the header frame and the frame headers up to the first
    data blob) from a description closed straight after, mostly right after
    another probe had read it; stage 4's removed header read came after
    stages 1 to 3, so its pages were not necessarily cached, and it no
    longer DONTNEEDs them under the eviction policy.
  - Auto selection now judges indexed from the first OsmData blob (the hint
    every backend's own check reads) instead of the frame right after the
    header, so a header-only file is "indexed=true" in its note and an
    extension frame before the data no longer decides; it no longer reads
    or validates that blob's payload.
  - Apply-changes and altw read the header payload into memory before the
    indexdata verdict (the old probe skipped it); `merge_header_read_ms` now
    times the whole front read, probe included.
  - Inspect's index-only path no longer refuses `direct_io = true` in a
    library build without `linux-direct-io` (the CLI refuses the flag at
    parse time there), and its `inspect.direct_io_ignored` note now follows
    the header decode. Bench-write reports a header-less file whose first
    data blob's PrimitiveBlock is malformed as missing header, not the
    decode error; a truncated payload or malformed Blob envelope in that
    first frame still wins, as it did through the old probe (which read and
    decoded the first blob before judging its type). Inspect's in-scan
    guards judge the frame type before parsing the Blob envelope, so a
    header-less file whose first data frame has a malformed envelope, bad
    compressed data or a malformed PrimitiveBlock is now missing header in
    both scans, where the old probe reported that error; a truncated first
    payload still reports truncation. So for a malformed first envelope
    bench-write and inspect now differ. The missing-header contract
    supports the new verdicts.
  - Separate descriptions stay where they were (walker `FADV_RANDOM`,
    worker pools, the splice source): sharing one would merge advice and
    readahead state. Comments at apply-changes' opens, renumber's schedule
    walk and multi-extract's walk say so.
- Whether sharing one input descriptor between a header walk and body
  preads costs anything is unmeasured, and the tree does both: renumber
  shares one fd across all phases (commit `db49c929`, its comment at the
  open and `reference/performance.md` say so), and about eight commands
  hand the `HeaderWalker`'s `FADV_RANDOM` fd to their pread workers through
  `walker.input()` (classify, tags-filter, extract smart and multi,
  getparents, geocode pass 1.5, diff's shard walk, altw's sparse
  passthrough), while altw external, apply-changes and `InputFront` keep
  separate descriptors on an argument (commit `a3f1a1bc`) that was never
  measured. One matched `--bench 1` A/B per side, for example on altw
  external stage 4 at Europe scale, would settle it.
- The sequential diff and derive merges (indexed block-pair under `-j 1`
  or `--verbose`, the unindexed element stream always) recycle their
  decompression buffers through one `DecompressPool` per merge, shared by
  both sides and every phase (`MergeBuffers` in `osc/merge_join.rs`),
  instead of allocating a fresh buffer per block. Judgement calls, none
  measured:
  - The gain is reasoned only: glibc malloc's dynamic mmap threshold
    probably already serves a freed buffer back after the first few blocks.
    Recycling cannot plausibly allocate more, but it adds an `Arc` owner
    allocation and an uncontended mutex per block.
  - Peak memory gains the pool's idle buffers, bounded by the most decoded
    blocks the merge held at once (current per side, a stash, the block
    alive while `drain_remaining` decodes the next) and by the pool's
    4 MiB-per-buffer and 64-buffer limits. Buffers over 4 MiB are dropped
    and now show in the diff and derive runs' `DECOMPRESS_POOL_METRICS`
    oversize-drop counters, which these paths never fed before.
  - The parallel shard decoders (`diff/parallel.rs` and
    `diff/derive_parallel.rs` `pread_decode`), the measured default path,
    still allocate per block; left alone (HYG-181).

---

## HYG-002 - Tunables are centralized but not injectable

Anger: 10 - tunables, including POOL_SIZE, which was measured at 16.

Every tunable now lives in `src/tunables.rs`, grouped by scope with its
rationale, values unchanged; format and protocol constants (field numbers,
record layouts, spec limits) stay where they are. What remains is the second
half: the engines still read the constants directly, so nothing but a code
edit changes a knob, and a test cannot drive a pool at one worker or a tiny
bucket count (small fixtures still create 256 buckets times N workers' shard
files). `ElementReader` exposes `read_ahead`, `decode_ahead` and
`decode_threads`; `par_fold_blobs` takes the budget and batch sizes as
parameters but the public method passes the constants, and `BLOCK_QUEUE` for
`into_blocks_pipelined` is fixed. The one injected knob,
`FULL_SCAN_ARM_MIN_BLOBS`, reaches `getparents` through a `#[doc(hidden)] pub`
"test instrument" the CLI calls for a hidden `--full-scan-min-blobs` flag,
while other `dispatch_scan_arm` users have no override.

Fix: per-scope tuning structs (`ReadTuning`, `WriterConfig`, a
`PipelineTuning` on `MergeOptions`, an altw `Tuning`, a `GeocodePolicy`)
defaulted from `src/tunables.rs`, passed in rather than read as constants,
documented in `reference/`.

Reported by: all nine scopes.

## HYG-010 - The read engine's ordered pipeline exists twice and has drifted

Anger: 10 - unifies the read engine and the blob pools; the batched-pipeline rebuild was already measured and reverted.

`read/pipeline.rs` has `run_pipeline` (admission gate, permit riding with the
item, shutdown flag, panic capture, reorder buffer, metrics) and
`run_pipeline_fused`, a near-verbatim copy the comment says to "keep
additive". Drift: the error-forwarding branch records `decoded_send_wait_ns` in
`send_direct_error` even on failure, the fused copy only on success; the plain
engine spawns with `ThreadPool::spawn`, the fused one with `in_place_scope`
(different lifetime and drain semantics); the `raw_rx` drop-ordering comment
exists in one copy. The plain engine is the fused one with `transform = Ok`.
The fused transform signature `Fn(PrimitiveBlock) -> Result<T, String>` also
blocks typed errors (HYG-021). Fix: one engine generic over the transform,
frame source and work item, so the command-level pools of HYG-011 can use it
too; afterwards make `ReorderBuffer` private to the engine module.

Reported by: read path.

## HYG-011 - The pread-decompress-dispatch-reorder pool is hand-built about fifteen times

Anger: 10 - unifies the read engine and the blob pools; the batched-pipeline rebuild was already measured and reverted.

The same machine (a dispatcher feeding `Arc<Mutex<Receiver>>`, N workers doing
pread into a reused buffer, `decompress_blob_raw`, the per-blob work, a bounded
result channel, a consumer with or without a `ReorderBuffer`, a first-error
slot) is built in:

- `scan/classify.rs`: `parallel_classify_phase_abortable`,
  `parallel_scan_blobs_raw`, `parallel_classify_accumulate`, three near-verbatim
  copies in one file; only `_abortable` supports early exit, only the classify
  ones use a `DecompressPool` and a WILLNEED window.
- Altw: `external/stage1.rs` pass A (atomic-index dispatch, own reorder
  buffer), `stage2.rs`, `stage3.rs`, `stage4.rs` and `passthrough.rs`
  (`passthrough.rs`'s doc says it was "lifted from external/stage4.rs").
- Renumber: now one pool in `renumber/pool.rs` with cancellation and a
  claim window ahead of the write head, the properties the shared engine
  needs; a candidate to generalise rather than another copy.
- `repack` and `degrade`: each defines `FRAME_BATCH`, `KindPayload`,
  `WorkerOutput`, `PhaseStats`, `run_kind_phase`, `frame_owned`,
  `frame_and_write_batch`; `cat/mod.rs::run_kind_phase` is a third member.
  The sort-preservation guards now agree; repack fires its guard only when
  the worker produced full frames, degrade on every input blob (it mirrors
  input blobs one to one).
- `extract/common.rs::pread_execute` (whose `ConsumerItem` match is written
  twice verbatim in the same function for the post-drain tail),
  `extract/simple.rs` (node and way classify), `extract/multi.rs` (node write,
  way/relation write), `tags_filter/mod.rs` pass 2.
- `geocode_index/builder/pass1_5.rs`.

`getparents` and `getid --id-osm-file` use the shared helper; the rest do not.
Divergence across copies is already fact:

- Error policy: stage 1's flush slot and stage 2 keep the first error, stage 3
  has both a first-wins `s3_error` and the router's `abort_error`;
  repack/degrade latch and keep draining, renumber returns immediately (and
  hangs), sort uses rayon's short-circuiting collect.
- Cancellation: every pool now observes `host.cancel`, but each copy wires it
  by hand - ordered pools in the consumer (`recv_checked` in classify,
  renumber's `run_ordered`, the altw and extract consumers), unordered ones
  in the worker (accumulate, stage 2 and 3 buckets, diff and derive shards,
  pass1_5) - and each copy's early return has to drop its receiver before
  the scope joins, which two copies (altw passthrough and stage 4) got wrong
  until the cancellation work moved the receiver into the consumer closure.
  Internal early exit still differs: stage 2 polls the error mutex per
  bucket, classify relies on channel drop, stage 1 runs until the schedule
  drains, getparents breaks the scan on first error with a comment on why
  that matters at planet scale.
- Prefetch: only classify gets the sliding WILLNEED window.
- Stall counters: no hand pool emits `*_wait_ns`, so `--stalls` is blind to
  extract, tags-filter pass 2 and most altw stages.
- The `Mutex<Receiver>` lock is held across a blocking `recv()`, so idle
  workers queue on the mutex, and it poisons silently
  (`unwrap_or_else(PoisonError::into_inner)`).

Fix: one `BlobPipeline` (schedule of `Decode | Passthrough` items, worker
state factory, per-blob fn returning `crate::Result`, ordered or unordered
consumer returning `ControlFlow`, cancellation token, one `FirstError` type,
prefetch policy, pread error context, stall counters). `parallel_classify_phase`
is already most of it. The external stages differ only in what they dispatch
(blobs vs buckets). The `passthrough` / `stage4` fork becomes one assembly
pipeline parameterised by a `CoordSource` trait (sparse `NodeIndex`, external
router payloads), which also merges the two `reframe_way_blob_with_locations`
(same name, different signatures). Enforce by dependency rule:
`std::thread::scope`, `sync_channel` and `Arc::new(Mutex::new(` allowed only in
the pipeline module.

Reported by: altw and scanning, re-encoding, filtering and spatial, read path,
geocode index.

## HYG-012 - Sort and `cat --dedupe` are one algorithm in two copies

Anger: 10 - merging sort and dedupe could undo sort's deliberate pread-walker trade-off.

`sort/mod.rs` and `cat/dedupe.rs` each define `BlobEntry`, `HasId`,
`build_blob_index`, `type_order`, `detect_overlaps`, `write_passthrough_blob`,
`read_frame_into`, `extract_blob_bytes`, `count_entry`, a sweep merge and
`flush_heap_below`. Divergence: sort reads headers through `HeaderWalker`
(pread plus `fadvise(RANDOM)`, justified by a planet measurement), dedupe
through `FileReader` plus `read_frame_header`; sort has `intra_unsorted`
detection, the counted-vs-claimed split, parallel overlap rewrite and the
`copy_file_range` coalescer, and dedupe has only gained the counted-vs-claimed
split; the kind-boundary run-splitting fix exists in both as a comment citing
the other. The header reconciliation and claimed-count split were each fixed
in one copy and later ported to the other by hand. `HasId::version` maps absent to 0 in dedupe while `derive.rs`
says "absent is not zero". Fix: one module parameterised on input set and
merge policy.

Reported by: re-encoding.

## HYG-013 - Diff and derive-changes share a parallel core in two copies

Anger: 9 - diff core on a hot path.

Shard planning (`BlobDesc`, `WalkedFile`, `walk_file`, `Shard`,
`plan_shards`, `build_shard`) lives once in `diff/shard.rs`, and the delete
and merge-stats helpers have one owner each. `diff/parallel.rs` and
`diff/derive_parallel.rs` still each carry `pread_decode`, `Side`,
`element_merge`, `append_and_cleanup` and the driver loop, and
`parallel.rs`'s `emit_element` / `emit_modified` copy `mod.rs`'s
`write_compact_line` / `write_modified_line` (identical formatting, per
element). The equality predicate is shared (`crate::content::content_equal`).
Because the CLI defaults `-j 0` to all cores, the parallel path is the default
for indexed inputs and the sequential paths run only under `--verbose` or
`-j 1`. `cli_diff`'s `diff_sequential_and_sharded_output_byte_identical` and
`derive_sequential_and_sharded_output_byte_identical` pin `-j1` against `-j8`
(8 shards per kind) and fail on a shard-boundary drift. One known divergence
they cannot see: the sequential merge (`osc/merge_join.rs`) decides a blob's
leftover tail from what its iterator saw, the parallel copies from indexdata
`max_id`, so an understated `max_id` could drop a tail on the parallel path
only (within `shard.rs`'s "index claims are trusted" contract). A second:
the parallel copies have no byte-equal fast path, so they decode every
selected blob where the sequential merge skips a byte-identical pair under
`-c` and `--format osc`. That is a refusal difference (a corrupt body inside
an identical pair passes `-j 1` and fails `-j N`; `diff`'s input-checks
paragraph in both CLI references states it) and a likely cost on the
default path for inputs that share blobs, such as an `apply-changes` output
against its base. Fix: one shared shard core. The two parallel files also
carry about twenty sound `.expect("checked")` / `.expect("checked peek")`
calls (a peek, then a next) that HYG-047's triage left here: they go when the
copies merge, or become `match (peek, peek)` with `next_if`.

Reported by: re-encoding.

## HYG-014 - Three frame decoders for one frame format

Anger: 9 - frame decoders on a hot path.

`BlobReader::read_blob_header` (streaming, stamps offsets),
`frame::read_frame_header` (streaming, used by `raw_frame.rs`) and
`HeaderWalker::next_header_inner` (pread). `frame.rs` is called the one owner
of frame policy but owns only the length-prefix guard and the header-position
rule (`check_header_position`, which every decoder applies). Each decoder
reimplements the EOF policy (1-3 trailing bytes tolerated), the datasize guard
(`BlobReader` carries "the inline twin of the check in
`parse_blob_header_with_index`"), the truncation messages (the "shape 3" /
"shape 4" text is worded differently in `BlobReader` and `HeaderWalker`, and is
jargon from `reference/truncation-handling.md` an operator cannot decode), and
provenance stamping. `src/read/frame_conformance_tests.rs` now pins every
truncation shape through five sources (`BlobReader` read and skip,
`read_raw_frame`, `read_blob_header_only` plus `FileReader::skip`, and
`HeaderWalker`), and the disagreements it pins are: `BlobReader` locates
header-level refusals at frame start + 4 where every other source uses the
frame start, and its skip path names the last declared payload byte where
its read path names the first missing one; `read_raw_frame` and
`read_blob_header_only` never give a blob index; the caller-side
`FileReader::skip` raises a payload truncation with no path, blob or offset;
only the `BlobReader` read path parses the Blob envelope, so a garbage
payload is refused there and passed by the other four; only `BlobReader`
says "(shape N)". `reference/truncation-handling.md` states these
disagreements. Fix: one frame state machine over an abstract byte source
(stream and pread-at-offset as two impls), returning a stamped
`FrameHeader`; the conformance test's disagreement rows then collapse to one
expectation per shape.

Reported by: read path.

## HYG-015 - Write-side blob framing and dispatch are copied

Anger: 9 - writer framing on a hot path.

- The sync and pipelined paths share `frame_in_scratch`;
  `reframe_raw_with_index_scratch` is a second framing assembly that skips the
  `MAX_BLOB_DATASIZE` check (arguably right for passthrough, but nobody
  decided). `encode_blob_header_into`'s WayMembers culprit heuristic
  (`len > 1024`) guesses which field overflowed instead of computing sizes.
- `write_primitive_block`, `write_primitive_block_no_indexdata` and
  `write_primitive_block_owned` each carry about 70 identical lines; the test
  `every_primitive_block_variant_stops_once_the_writer_thread_is_dead` exists
  because a fix to one missed the others. They now differ only in where the
  index comes from (scanned in the closure, precomputed, none).
- `to_path_uring` and `to_path_parallel` carry the same spawn plus `init_rx`
  match (messages differ by one word), all four constructors build the same
  `WritePipeline` literal, and a test helper builds a fifth.
  `to_path_uring`'s unbounded `init_rx.recv()` is documented and left.

Fix: one `dispatch_framed(FrameRequest)`
with `IndexSource::{Scan, Precomputed, None}`; one `WritePipeline::spawn`.
Structural.

Reported by: write path.

## HYG-016 - Writer loops are copied per backend and their metrics mean different things

Anger: 9 - writer loops on a hot path.

`pipeline::writer_thread`, `uring_writer::uring_main_loop` and
`parallel_writer::dispatch_loop` each reimplement recv, `record_recv_wait`,
push, high-water, `pop_ready` and `check_stream_complete`, and every backend
reimplements the `OutputChunk` match with `write_ns` and `bytes_written`
bookkeeping. `parallel_gzip::writer_loop` has its own completeness check with a
`BTreeMap` reorder buffer instead of `ReorderBuffer` and no announced total.
ADR `0014-output-backends-stay-separate` argues against unifying backends, but
the loop is not backend logic; uring could be an `OutputSink` whose
`write_chunk` does the opportunistic `reap_cqes(false)`. Meanings have
diverged: `writer_write_ns` is wall time on the writer thread for buffered and
uring, summed across 16 pool threads for parallel; and parallel counts
`writer_bytes_written` at dispatch, others after the write. Fix: `writer_thread<S: OutputSink>`
as the only loop with an `on_idle()` hook; a source-scan test that only
`pipeline.rs` calls `pop_ready()` under `src/write/`.

Reported by: write path.

## HYG-017 - `copy_file_range` and pread loops are written five times

Anger: 8 - I/O fallbacks, gating, alignment.

The rules are now shared from `write/copy_range.rs` (`retry_eintr`,
`pread_retrying`, `len_to_usize`, the one fallback decision
`kernel_copy_refused`, the `writer_copy_range_fallback_*` counters), but the
five loops are still five: `copy_range` (implicit out offset),
`parallel_writer::copy_range_to_fd` (explicit), the fallbacks
`copy_range_fallback` and `copy_range_fallback_pwrite`, and
`uring_writer::handle_copy_range_uring`'s pread loop. Nothing stops a new raw
loop bypassing the helpers. A fallback to userspace is counted only in
sidecar counters, so outside brokkr it is silent; proposed, not decided: a
once-per-run `Note` from the fallback counters, called beside
`report_writer_index_loss` after dispatch (the same pattern), naming the
range and byte counts (the errno would need one more cold-path counter). Fix: one module owning the loops too;
`disallowed-methods` for `libc::pread`, `libc::copy_file_range`, `libc::write`
elsewhere.

Maintainer (2026-10-25): queued; no recommendation is recorded for this
entry.

Reported by: write path.

## HYG-032 - Geocode index records are decoded by hand, differently per reader

Anger: 8 - record codecs; `reader.rs` serves lookups.

Split on 2026-10-25: this entry keeps the geocode format and reader; the
builder side is HYG-161, the OSC arena HYG-162, altw external's records
HYG-163.

`reader.rs` (`decode_addr_point`, `record_at`, `read_record`,
`two_node_distance`, `checked_add`) decodes records by hand at offsets that
`format.rs` should own; `U32EntryIter`, `SegmentEntryIter`, `AdminEntryIter`
have identical `new` bodies, `binary_search_cells` and
`binary_search_admin_cells` are one function twice, and `validate_list` is a
fourth implementation of the list framing, so a count-width bump is five
edits.

Recommendation (2026-10-25): a hand-written `Record` trait (`SIZE`,
`write_to`, `read_from` over `[u8; SIZE]`) in `geocode_index/format.rs`,
implemented by every record type; a generic `ListIter<T: Record>`, one
`binary_search<T>`, and `validate_list` on the same framing; `reader.rs`
decodes through it with checked offsets. On-disk bytes stay identical: an
existing index reads the same and the format version does not move. No new
dependency (`zerocopy` / `bytemuck` would be the maintainer's call and are
not needed). Lookup decode paths change, so the change goes on "Review once
drained" as unmeasured.

Reported by: geocode index.

## HYG-161 - The geocode builder decodes its own records by hand

Anger: 7 - geocode build.

Split out of HYG-032; runs after it. `pass3.rs`'s `BUCKET_RECORD_SIZE = 15`
with `write_bucket_record` / `parse_bucket_file` duplicates the pattern
`format.rs` exists to own, and `interp.rs` (`read_addr_point_mmap`,
`read_node_at`) decodes the same records as `reader.rs` differently, with
unchecked `offset + SIZE` where `reader.rs` uses `checked_add`.

Recommendation (2026-10-25): move the bucket record and `interp.rs` onto
HYG-032's `Record` trait, with checked offsets, so the builder-side reader
stops being the unchecked one. Index bytes stay identical.

Reported by: geocode index.

## HYG-162 - The OSC arena's metadata layout is a hand sum

Anger: 8 - apply-changes, per OSC element.

Split out of HYG-032. `compact.rs` writes metadata field by field in
`arena_append_meta` and `read_meta` reads back at hand-written offsets
`+1, +5, +13, +21, +25`; `META_LEN = 29` is a hand sum; `NODE_META_OFF = 20`
and `WAY_META_OFF = 16` repeat the record header layout.

Recommendation (2026-10-25): constants only. Derive `META_LEN` and the
offsets from per-field width constants, with `const` asserts, so a layout
change is one edit and a mismatch fails to compile. Encode and decode logic
stay as written: zero runtime change on a measured path. Moving onto a
record trait can wait until rule 8 lifts.

Reported by: change application.

## HYG-163 - altw external's records have no shared codec

Anger: 8 - altw external stage 3, per entry.

Split out of HYG-032. `IdRecord::read_from` and `ResolvedEntry::write_to`
have no shared codec, and stage 3 decodes `ResolvedEntry` by slicing
`chunk[0..4]`, `[4..8]`, `[8..12]`.

Recommendation (2026-10-25): constants only, as HYG-162: record sizes and
field offsets derived from width constants with `const` asserts, logic
unchanged.

Reported by: altw and scanning.

## HYG-034 - Element metadata accessors turn a decode failure into defaults

Anger: 9 - read hot path, per element.

Split on 2026-10-25, one entry per API: this entry keeps the element
metadata accessors; dense nodes are HYG-164, the owned conversions HYG-165,
the OSC writer HYG-166, the OSC arena and the enforcement rule HYG-167.

`elements.rs` (three sites): `WireInfo::parse(data).ok().unwrap_or_default()`,
so corrupt metadata becomes version 0 / timestamp 0, indistinguishable from
absent; `time-filter` and history handling can treat a corrupt element as
timestamp 0. `block.rs::validate_info`'s doc says so. (`wire.rs`'s
`read_tag().ok()??` ending on the first bad tag is the same pattern; the
`blob_meta` scanners no longer do it and their callers propagate.)

Already adjudicated, not to be re-reported: tag iterators ending early on an
out-of-range string index, and roles collapsed to `""` by
`role().unwrap_or("")` (the OSC writer, `bench_write_loop` and the owned
conversions included), are accepted in CORRECTNESS.md "Tag iterators end
early on an out-of-range string index". A tag array ending mid-varint is
rejected up front by `validate_groups_strict`.

The crux for this entry and HYG-164: whether a swallow can fire at all
depends on whether `PrimitiveBlock::from_vec*` validated the same bytes.
`block.rs` has a validation pass, so some sites may be "cannot happen after
construction", but that invariant is held by a different function in a
different file, and nobody has confirmed it runs on every construction path
(strict and lazy).

Recommendation (2026-10-25), per site, never as a blanket accessor change:
where construction provably validates the bytes on every path, the swallow
becomes a `debug_assert!` plus a comment naming the validator (same cost per
element, invariant written where it is relied on); where it does not, the
error must reach the caller without turning public accessors into `Result`
(an API break and a per-element cost) - the internal re-encoding callers use
checked lookups, as HYG-165 and HYG-166 do. Even a same-cost change here goes
on "Review once drained".

Reported by: read path, CLI and plumbing, change application.

## HYG-164 - A corrupt dense group reads as no nodes

Anger: 9 - read hot path.

Split out of HYG-034 (see its crux paragraph and recommendation, which
apply here unchanged). `block.rs::PrimitiveGroup::dense_nodes` and its twin:
a `WireDenseNodes` parse error yields an empty iterator, so a corrupt dense
group reads as "no nodes". `dense.rs::DenseNodeIter::new` drops all dense
metadata on corruption.

Reported by: as HYG-034.

## HYG-165 - The owned conversions turn a bad user string into an empty one

Anger: 8 - sort, `cat --dedupe`, degrade and time-filter re-encode through it.

Split out of HYG-034. `owned.rs` `element_metadata` / `dense_node_metadata`:
a `user()` string-table error becomes `""` and missing timestamps,
changesets and uids become `0`, re-encoded by sort, `cat --dedupe`, degrade
and time-filter. Documented, but opposite to decision 0004's defensive-input
stance, and invisible.

Recommendation (2026-10-25): the user lookup uses the checked accessor and
refuses, as the borrowed OSC writers already do since the unindexed-diff
change; the four commands pick the refusal up through this one API (new
refusal, CHANGELOG line; the existing fault test flips to assert the error).
Absent fields becoming `0` stay: that is the block builder's documented
normalisation (spelled out in `reencode_unclassifiable`'s doc in
`src/commands/cat/mod.rs`), so it gets a comment, not a change.

Reported by: as HYG-034.

## HYG-166 - The OSC writer swallows a bad user string

Anger: 7 - OSC output.

Split out of HYG-034. `osc/write.rs`: `info.user().ok()`.

Recommendation (2026-10-25): the checked lookup and a refusal, matching the
borrowed OSC writers.

Reported by: as HYG-034.

## HYG-167 - The OSC arena resolves a bad id to an empty string

Anger: 6 - crate-written arenas.

Split out of HYG-034; runs after HYG-034, HYG-164, HYG-165 and HYG-166.
`StringInterner::resolve` and `CompactTagIter` return `""` on an
out-of-range id or invalid UTF-8 ("keeps a read-side accessor from
panicking"), turning arena corruption into silent empty tags.

Recommendation (2026-10-25): corruption here is a pbfhogg bug, not hostile
input, so a `debug_assert!` at each site. This entry also brings the
enforcement for the whole HYG-034 family: a `brokkr.toml` text rule banning
`.ok()` in `src/read/` and `src/blob_meta/` except on lines carrying a
marker comment with the reason (`disallowed-methods` cannot be scoped to
modules, and `Result::ok` is legitimate elsewhere).

Reported by: as HYG-034.

## HYG-036 - `too_many_arguments` is denied and then allowed locally all over

Anger: 9 - validate flags earlier, refuse no-op flags.

The flag-refusal policy is done (`reference/cli-reference.md`, "Flag rules").
What remains is the lint side: the library takes one `WriteOptions`
(compression, `OutputBackend`, direct reads, force) instead of positional
bools, but `too_many_arguments` is still `#[allow]`ed on the extract entry
points, `writer_from_header`, `getid_dispatched`, `filter_by_id`, the
getparents dispatchers, altw's `write_output_*`, `tags_filter_*`,
apply-changes' `worker_loop` and `handle_candidate`, several CLI `run_*`
functions, and more; `too_many_lines` likewise. A denied lint with that many
local exemptions does not enforce anything. Fix: group the parameters into
option structs and drop the exemptions, or stop denying the lint.

Maintainer (2026-10-25): not worth the effort now; skipped. For whoever
picks it up: about 90 `too_many_arguments` and 50 `too_many_lines`
exemptions, many on hot inner helpers (`write/encode.rs`, apply-changes'
worker and drain, the altw stages, the diff and derive parallel files). The
session's suggestion was to group parameters only on once-per-run entry
points, turn the rest into `#[expect(..., reason = "...")]` (which also
fails the build when an exemption goes stale), and ban
`allow(clippy::too_many_` with a `brokkr.toml` text rule.

Reported by: CLI and plumbing, filtering and spatial, change application, altw
and scanning, re-encoding.

## HYG-037 - `--direct-io` is accepted and ignored on many paths

Anger: 9 - validate flags earlier, refuse no-op flags.

The parameter is taken and discarded in altw `stage1_way_pass(_direct_io)`,
`build_node_index_sparse(_direct_io)`, `collect_way_referenced_node_ids`,
`collect_relation_member_ids` (every pread path, so the flag does nothing to
reads in passes 0 and 1, stage 1 or stage 2), `sort::build_blob_index`
(kept "for signature stability" in a private pre-1.0 function),
`diff::parallel::diff_block_pair_parallel` and the parallel derive path
(both pread through `HeaderWalker`, so `diff --direct-io` on indexed inputs
with `-j` above 1 reads buffered; derive never receives the flag),
`getid::parse_ids_from_pbf(_direct_io)` (the CLI threads `write.direct_io`
into it), and `check_refs` (`let _ = direct_io;`). Extract's pread passes open
buffered regardless (a comment explains alignment), `multi.rs` writes through
`PbfWriter::new(BufWriter)` regardless, `export` has no I/O flag. `inspect`
prints that it ignored the flag. An operator who chose O_DIRECT to protect the
page cache still fills it, with nothing logged.

Where the flag is honoured it is often only for the header read and the
first-blob indexdata probe: renumber (header only), repack and degrade's decode
path (probe and header, the classify pools pread through a buffered shared
fd), time-filter's snapshot path, tags-filter's two-pass scans, and
getparents' dispatch probe all read their bulk data buffered. The `sort`
reads and `check --refs` (`let _ = direct_io`, with a comment that it is
"accepted silently for now") discard it. The writes are honoured by every
PBF writer that goes through `Output::pbf_writer`, except multi-extract's
single-pass path (a synchronous `BufWriter`).

Two consequences are visible to a user. The "I/O capability matrix" in
`reference/cli-reference.md` marks sort, renumber, repack, degrade, extract,
tags-filter, getid, getparents, time-filter, add-locations-to-ways, apply-changes
and `check --refs` as "Yes (R+W)" or "Yes", and has no row for `digest`: it
describes the flag's presence, not its reach. And in a build without the
`linux-direct-io` feature, whether `--direct-io` is refused ("requires the
linux-direct-io feature", exit 2) depends on whether the command happens to
open a reader with the flag set: `check --ids --direct-io` is refused,
`check --refs --direct-io` runs and ignores it.

Even an honoured open can be silently buffered: `statx(STATX_DIOALIGN)`
reports zero alignment for a file whose filesystem falls back to buffered
I/O (ext4 `data=journal`, inline-data or verity files; f2fs files whose
writes are forced buffered). `src/direct_io.rs` treats that as "nothing
reported" and keeps the 4096 floor, since the run stays correct; whether
`--direct-io` should say so, or refuse, belongs with this entry.

Fix: remove the parameters, and have the CLI refuse or say so where a command
honours the flag on some passes only, so `--direct-io` help can say what it
affects; regenerate the matrix from a per-command table in code (read passes
honoured, write honoured) rather than by hand. Enforce with one contract test
per command that a no-feature build refuses `--direct-io` before opening any
input, and a unit test of the table against the capability matrix.

Maintainer (2026-10-25): skipped for now. The session's notes for whoever
picks it up: do not remove the ignored `direct_io` parameters - the first
drain round did that to the parallel diff and it was put back as deliberate
plumbing; they are where the flag reaches once those passes honour it. The
suggestion was a per-command reach table in the CLI that generates the
capability matrix and the help text, refusing the flag where it reaches
nothing (`check --refs`, under the existing no-op-flag rule), parse-time
refusal in every no-feature build, and one `Note:` per run when statx
reports zero alignment.

Reported by: altw and scanning, re-encoding, filtering and spatial, CLI and
plumbing.

## HYG-038 - Linux behaviours are gated on features that have nothing to do with them

Anger: 8 - I/O fallbacks, gating, alignment.

- `copy_file_range`, `OutputChunk::CopyRange`, `write_raw_copy` and the
  parallel backend's `WriteOp::CopyRange` compile only with `linux-direct-io`,
  though kernel-space splicing has nothing to do with O_DIRECT; a default build
  silently loses passthrough splicing. `sort` notices via `cfg!`,
  `apply_changes/rewrite.rs` does not (HYG-018). `linux-direct-io = []` does not
  gate `libc`, which is unconditional.
- `FileReader::buffered` issues `POSIX_FADV_SEQUENTIAL` only under
  `any(feature = "linux-direct-io", feature = "linux-io-uring")`, while
  `blob.rs` issues `DONTNEED` (when eviction is on: the CLI's
  `PageCachePolicy::Evict` or an explicit opt-in) under plain
  `target_os = "linux"`; so the default library build reads without the
  readahead hint and the CLI with it, and any benchmark comparing a library
  consumer to the CLI compares different I/O.
- Altw stage 2 and 3 `posix_fadvise(DONTNEED)` on slot buckets and id shards,
  and stage 2's dropped `sync_data`, run whenever the `linux-direct-io` feature
  is compiled, whether or not `--direct-io` was passed; the `consumer` sweep
  and the release build behave differently with no flag in sight, and the
  `s2_fadvise_*` counters read zero on other builds with no "not compiled"
  marker.
- `posix_fadvise` return codes are discarded at three read-path sites with
  their own comments; correct for hints, but three statements of it.

Fix: gate on `target_os = "linux"` (or a `linux-copy-range` feature), decide
scratch eviction at run time or always; one `advise(fd, off, len, advice)` that
counts failures. Enforce the SEQUENTIAL hint with a test in the `minimal` sweep
using the `evict_calls` call-count trick.

Checked 2026-10-25: still current. `write_raw_copy` (`write/writer.rs`) is
under `#[cfg(feature = "linux-direct-io")]`, sort's and apply-changes'
drain copy-run flushes have `#[cfg(not(...))]` fallback twins, the
SEQUENTIAL hint in `FileReader::buffered` is under
`all(target_os = "linux", any(linux-direct-io, linux-io-uring))`, and altw
stages 2 and 3 gate their scratch `DONTNEED` on the feature. Why it matters
more than it looks: `pbfhogg-cli`'s default features are empty, while
`brokkr.toml` builds the benchmarked binary with both Linux features, so a
plain `cargo install pbfhogg-cli` (and every default library consumer) runs
without the splicing, readahead hint and scratch eviction that every
published number assumes. The `Host` work (ADR-0016) does not cover this:
it moved process-wide state (`mallopt`, `malloc_trim`) behind the CLI's
process toolbox and input-page eviction behind `PageCachePolicy`, whose doc
excludes the library's own scratch files; the three behaviours here touch
only the library's own descriptors and files.

Recommendation (2026-10-25): gate all three on `target_os = "linux"`, so the
features mean only O_DIRECT and io_uring; the measured binary is unchanged
and default builds gain what is measured (CHANGELOG: default Linux builds
now splice passthrough blobs). altw's scratch eviction becomes always-on
under Linux, today's measured behaviour, rather than a new run-time choice.
One `advise` helper with one comment and a failure counter; the
minimal-sweep test as above. Each new comment cites ADR-0016 for why the
library may do this.

Reported by: write path, read path, altw and scanning, re-encoding, change
application.

## HYG-047 - Code aborts on caller-controlled input where a refusal was owed

Anger: 9 - decode failures and aborts become errors: new refusals, Results on builder hot paths.

- `BlockBuilder::add_node`, `add_way`, `add_relation`,
  `add_way_with_locations` and the `_raw` variants `assert!(self.can_add_*())`,
  aborting on a wrong element kind or full block, while the builder already has
  a non-panicking `poison` mechanism for other bad input.
  `BlockBuilder::with_element_cap_prealloc(0, _)` panics.
- `PbfWriter::into_inner` panics on a pipelined writer, while `writer_mut` was
  changed to return an error for the same state.
- `ReorderBuffer::push` asserts on stale, duplicate and window-exceeded seqs,
  documented as programming errors because seqs come from `enumerate()`. That
  holds for the read pipeline, not structurally: about fifteen callers feed it,
  some from worker-computed seqs, and a panicking consumer inside
  `thread::scope` re-raises at join.
- `check/verify_ids.rs`: `slot.expect("parallel_classify_phase must deliver
  every blob")` twice, on the path whose job is to survive hostile files.
- `geo::assemble_rings` `.expect("nonempty ring")` and `.expect("search is
  nonempty")`, true by construction today.
- `getid::parse_id_spec` `unreachable!()` twice and `parse_element_spec`
  `unreachable!()`, held only by a `char`-typed prefix (HYG-007).
- About 20 `.expect("checked")` / `.expect("checked peek")` in
  `diff/parallel.rs` and `derive_parallel.rs`, three in degrade, removable by
  `match (peek, peek)` with `next_if`.
- `SparseArrayIndex::get` and `SharedSparseWriter::insert` assert on a rank
  outside the store; a real internal invariant, but the messages are
  paragraph-length.
- `new_permit_pool` uses `.expect` soundly; nothing distinguishes sound uses
  from the others.

`unwrap_used` is denied workspace-wide but `expect_used` is not, so every one
of these slips through. Fix: `add_*` returning `Result` or poisoning; a
typestate `PbfWriter<Sync>` / `PbfWriter<Pipelined>`; `ReorderBuffer::push`
returning `Err`; `let Some(..) else`. Enforce with
`clippy::expect_used = "deny"` and `allow-expect-in-tests` in a `clippy.toml`.
The input-driven panics are in `notes/bugs.md` (IdSet, BucketLayout, renumber
deltas).

Split on 2026-10-25, one entry per API: this entry keeps `BlockBuilder`;
`PbfWriter::into_inner` is HYG-158, `check --ids` HYG-159, getid HYG-160, and
the `expect_used` lever moved to HYG-077, which runs last. Left with the
work that owns them: `ReorderBuffer::push` (HYG-053), the diff and derive
parallel peeks (HYG-013), and the sound invariants (`assemble_rings`,
`SparseArrayIndex` / `SharedSparseWriter`, `new_permit_pool`), which get
`#[expect]` reasons under HYG-077 and shorter assert messages.

Recommendation (2026-10-25) for `BlockBuilder`: the `add_*` and `_raw`
asserts become the builder's existing poison, surfacing when the block is
built (same branch cost per element, signature unchanged; a panic becomes an
error for library users, CHANGELOG line), and `with_element_cap_prealloc(0,
_)` refuses a zero cap with an error.

Reported by: write path, read path, CLI and plumbing, filtering and spatial,
re-encoding, altw and scanning.

## HYG-158 - `PbfWriter::into_inner` panics on a pipelined writer

Anger: 6 - public API.

Split out of HYG-047. `into_inner` panics on a pipelined writer, while
`writer_mut` returns an error for the same state.

Recommendation (2026-10-25): return a `Result`, as `writer_mut` does; a
breaking API change before 1.0, CHANGELOG line. HYG-047's typestate
alternative (`PbfWriter<Sync>` / `PbfWriter<Pipelined>`) is not needed for
this.

Reported by: as HYG-047.

## HYG-159 - `check --ids` expects every classify slot filled

Anger: 5 - the hostile-input path.

Split out of HYG-047. `check/verify_ids.rs`:
`slot.expect("parallel_classify_phase must deliver every blob")` twice, on
the path whose job is to survive hostile files.

Recommendation (2026-10-25): let-else returning an internal error.

Reported by: as HYG-047.

## HYG-160 - getid's `unreachable!()`s rest on a char prefix

Anger: 4 - contained.

Split out of HYG-047. `getid::parse_id_spec` has two `unreachable!()` and
`parse_element_spec` one, held only by a `char`-typed prefix.

Recommendation (2026-10-25): let-else returning an error.

Reported by: as HYG-047.

## HYG-053 - Reorder buffers are bounded only statistically

Anger: 10 - a credit scheme inside the read engine and blob pools.

The channel bounds queued messages, but consumers drain the channel into the
`ReorderBuffer` whether or not the head seq has arrived, so one slow blob lets
the buffer grow by however many blobs the other workers finish meanwhile.
`cat::run_kind_phase`'s doc claims in-flight is capped "to roughly the
decode-thread count"; the same holds for the three renumber loops,
repack/degrade, and the read pipeline's decode window, which uses
`decode_ahead` for both admission and the decoded channel depth (one number for
two meanings). `ReorderBuffer::with_capacity(capacity)` sets
`max_window = DEFAULT_MAX_WINDOW.max(capacity)`; `with_max_window` has no
production caller, so every buffer runs at 65 536 slots, and
`altw/passthrough.rs` passes `passthrough_items.len() + decode_threads`, so its
window is as large as the input. Stage 4 and `pread_execute` pre-insert every
passthrough item before any worker result arrives, so ordering correctness
rests on the buffer accepting an arbitrary number of out-of-window items; if
the explicit-ceiling variant were used, inputs with more than 32 passthrough
blobs would break. Fix: a credit scheme (a worker may not claim seq > head + K)
in the shared engine of HYG-011. A working precedent now exists:
`scan::classify::parallel_classify_phase_ordered` (used by the geocode
builder's Pass 2) hands out schedule positions only within a fixed window
ahead of the merged head.

`ReorderBuffer::push` asserts on stale, duplicate and window-exceeded seqs,
documented as programming errors because seqs come from `enumerate()`; that
holds for the read pipeline, not structurally, since about fifteen callers
feed it, some from worker-computed seqs, and a panicking consumer inside
`thread::scope` re-raises at join. HYG-047's triage left this here: with a
credit scheme in one engine, `push` can return `Err` instead.

Reported by: re-encoding, read path, filtering and spatial, altw and scanning.

## HYG-058 - Sort and dedupe memory on adversarial input

Anger: 8 - memory bounds change algorithms and output.

Sort holds every overlap run's re-encoded output (`overlap_outputs`) before the
write loop; the comment says for pathologically unsorted input it can approach
the input size, which is planet-sized RSS on exactly the input sort exists for.
Process runs in a sliding window of K in-flight runs written in order. Dedupe's
sweep heap is bounded by overlap-run width, which one blob spanning the whole ID
range makes the whole kind.

Maintainer (2026-10-25): skipped for now. The session's suggestion for
whoever picks it up: sort gets the sliding window (K a tunable around twice
the worker count, output byte-identical, realistic inputs have fewer runs
than K so nothing changes for them; the pattern is
`parallel_classify_phase_ordered`'s); dedupe gets a rule-3 comment and a
docs sentence, since a sweep merge must hold whatever overlaps the current
position and bounding it means an external merge.

Reported by: re-encoding.

## HYG-061 - The indexdata trust model is decided per command

Anger: 9 - indexdata trust model.

Sort trusts indexdata only under a sorted header and otherwise decodes; dedupe
trusts it whenever present; renumber trusts `count` but cross-checks per blob,
and trusts `max_id` for preallocation but checks per ID; repack and degrade
trust `parallel_classify_phase`'s schedule; apply-changes trusts `count` for
stats and `id_range` for routing, using `infer_kind_and_range` only when
indexdata is absent, so a forged or stale `min/max` routes a blob as "no
overlap" and splices it unchanged past a delete (silent data loss); altw scan
filtering relies on indexdata `kind` being honest; altw external's metadata
scan refuses ranges that break the sorted order, but element order inside one
blob is unchecked, so a node blob holding `[2, 1]` with honest indexdata
`[1, 2]` makes stage 2 drop present nodes (its `debug_assert` sees only the
tuples the inner loop steps over; a per-blob monotonicity pass over the
extracted tuples would catch it, `degrade --unsort-intra` is the fixture);
the geocode builder trusts indexdata kind to schedule node blobs, so a node
in a blob labelled way or relation is never seen by Pass 2a. `CORRECTNESS.md`
partly states this for sort. `has_indexdata` decides whether a whole file is
indexed from its first data blob alone, so a file with indexdata on the first
blob and not on a later one passes every gate; the output-header policy had to
add its own check for that case on the raw-copy paths, and the schedule walks
now refuse it (below).

The second half of the model is what a command does when indexdata is absent,
and that is decided per command too. It is now pinned, as observed, by two
tables in `cli/tests/cli_input_contract.rs`
(`what_each_command_does_with_unindexed_input_is_pinned` and
`..._with_input_indexed_on_its_first_blob_only_is_pinned`); the tables are
the current statement of who refuses, who escapes with which flag, who falls
back with which note, and who verifies. Pinned oddities worth a ruling:

- renumber has no `--force`; altw `--index-type external` refuses `--force`
  at parse time.
- A partially indexed file is refused, with the typed missing-indexdata
  error at its first unindexed data blob, by every command whose schedule
  comes from a walk over every header (`HeaderWalker::refusing_partial_index`):
  sort, apply-changes, digest, the classify-based schedule builders and the
  bespoke schedule walks of tags-filter, extract, altw and the geocode
  builder. diff, renumber and altw external refuse any unindexed blob. What
  is left:
  - Still trusting the first blob: getid and getparents (their walker and
    full-scan arms are chosen by input size, so neither walk opts in),
    `cat --type`, `cat --dedupe`, `tags-filter -R`, time-filter's history
    path, and `getid --add-referenced` unless way IDs are asked for (its
    classify walk runs only then). `inspect --indexed` says yes while plain
    `inspect` says no.
  - `apply-changes --force` on an unindexed base writes a partially indexed
    output: a blob the diff does not touch is copied without indexdata, a
    rewritten one gets it. Every walk above then refuses that output.
  - check, inspect and time-filter now refuse a partial file through their
    classify walks (`check --refs`, `check --ids`, `inspect --nodes`,
    `inspect tags`, non-history time-filter), though check answers "is this
    file right?" and `check --index` reports the same file as a violation
    (exit 1). time-filter and `extract --simple` never needed indexdata.
  - The walks see headers only, so they cannot tell an unindexable blob
    (empty, or mixed kinds) without indexdata from an unindexed one, where
    `check --index` exempts the former; such a blob among indexed ones is
    refused. Since `cat` indexes those blobs, only a hand-made file has one.
  - The geocode builder refuses at its Pass 1.5 schedule walk, after Pass 1
    has decoded the whole file once.
- Only tags-filter and plain inspect have `--distrust-metadata`; getid, cat
  `--type`, apply-changes and altw trust the metadata with no way to turn
  that off.

The sortedness the same commands require is read from the header flag alone
(renumber and digest additionally check order while reading), and a
header-only input counts as indexed (no blob lacks indexdata). No sentence
anywhere says which of refuse, fall back or verify a command is meant to be.

Decided (the first ruling, a uniform forced-run Note, is implemented in
`require_indexdata`, keyed `reader.filter_without_indexdata` so the reader's
own filter notes fold into it):

- No action: renumber's missing `--force` and altw external's parse-time
  `--force` refusal are guards; `--distrust-metadata` stays where it is.

Fix: state the trust model once in `reference/` (indexdata is a hint; these
properties are verified, these are believed, this is the consequence, and this
is what a missing index does), have each command cite it, and move the
contract tables' rows when a ruling changes one.

Recommendation (2026-10-25), agreed with the maintainer. Split: this entry
keeps the trust model page and the BUG-005 / BUG-081 comments; the forced
apply-changes output is HYG-152; the first-blob trusters are HYG-153 to
HYG-157, one per command.

- Proposed ruling: indexdata and the `Sort.Type_then_ID` flag are believed,
  not verified. pbfhogg's writer and `cat` produce them; a file whose claims
  lie is outside the contract; `check --index` is the verifier for a file of
  unknown provenance. Verifying everywhere costs a per-blob decode, which
  the splice and passthrough fast paths exist to avoid, so the cheap checks
  that exist stay (renumber's per-ID check, the order watermark, the
  schedule walks' partial-index refusal) and nothing more expensive is
  added.
- This entry writes that model once in `reference/`: which properties are
  verified and which believed; what a lie costs per command, stating the
  worst ones plainly (apply-changes splicing a forged-range blob past a
  delete, altw external dropping present nodes when a blob's elements are
  out of order under a sorted header, the geocode builder never seeing a
  node in a blob labelled way or relation); and what each command does
  without indexdata, which the contract tables already pin. Each command's
  docs cite the page. BUG-005 and BUG-081 then close under rule 3, with
  comments at their sites pointing to the page.
- altw external's intra-blob order check (a per-blob monotonicity pass over
  the tuples stage 2 already extracts, `degrade --unsort-intra` as the
  fixture) is recommended as documented-as-believed, not added; it goes on
  "Review once drained" as a candidate if a measurement shows it free.
- The pinned oddities above under "check, inspect and time-filter", "The
  walks see headers only" and "The geocode builder refuses at its Pass 1.5"
  are recorded on the page as current behaviour, not changed.

Reported by: change application, re-encoding, altw and scanning, geocode index.

## HYG-152 - `apply-changes --force` on an unindexed base writes a partially indexed output

Anger: 8 - apply-changes internals.

Split out of HYG-061. A blob the diff does not touch is copied without
indexdata and a rewritten one gets fresh indexdata
(`frame_blob_pipelined(..., Some(&indexdata), ...)` in `streaming.rs`), so the
output is partially indexed and every walk that refuses partial indexdata
(diff, sort, the schedule walks) refuses pbfhogg's own output; the contract
table in `cli/tests/cli_input_contract.rs` pins it. Under `--force` the
scanner routes every unindexed blob to the workers (`scanner.rs`: no
indexdata, no fast path), which already decompress and scan it.

Recommendation (2026-10-25): index every blob the forced path writes, so the
output is wholly indexed: for an untouched blob that is a header reframe on
a cold path, not new decode work (unverified: how the worker emits an
untouched unindexed blob today). Move the contract table row.

Reported by: the 2026-10-25 triage, while closing brokkr#25.

## HYG-153 - getid trusts the first blob's indexdata for the whole file

Anger: 8 - indexdata trust model, one command.

Split out of HYG-061. getid's walker and full-scan arms are chosen by input
size, and neither walk opts into `HeaderWalker::refusing_partial_index`, so
a file indexed on its first blob only passes. `getid --add-referenced` is
covered only when way IDs are asked for, since its classify walk runs only
then.

Recommendation (2026-10-25): opt into the partial-index refusal only where a
full header walk already happens; add no new walk. Where none happens, the
trust model page (HYG-061) records that getid decides from the first blob.
Move the contract table rows that change.

Reported by: as HYG-061.

## HYG-154 - getparents trusts the first blob's indexdata for the whole file

Anger: 8 - indexdata trust model, one command.

Split out of HYG-061. As HYG-153: the walker and full-scan arms are chosen
by input size and neither opts into the partial-index refusal.

Recommendation (2026-10-25): as HYG-153.

Reported by: as HYG-061.

## HYG-155 - `cat --type` and `cat --dedupe` trust the first blob's indexdata

Anger: 8 - indexdata trust model, one command.

Split out of HYG-061. Both modes decide from the first data blob whether the
whole file is indexed.

Recommendation (2026-10-25): as HYG-153.

Reported by: as HYG-061.

## HYG-156 - `tags-filter -R` trusts the first blob's indexdata

Anger: 8 - indexdata trust model, one command.

Split out of HYG-061. `tags-filter -R` decides from the first data blob,
while tags-filter's other schedule walks already refuse a partial file.

Recommendation (2026-10-25): as HYG-153.

Reported by: as HYG-061.

## HYG-157 - time-filter's history path trusts the first blob's indexdata

Anger: 8 - indexdata trust model, one command.

Split out of HYG-061. time-filter's history path decides from the first
data blob, while its non-history classify walk refuses a partial file.

Recommendation (2026-10-25): as HYG-153.

Reported by: as HYG-061.

## HYG-077 - The enforcement levers most of these findings need are unused

Anger: 8 - clippy.toml with expect_used denied means hundreds of edits.

Recommendation (2026-10-25), agreed with the maintainer; runs last of the
queue, since it annotates files across the crate. Record the
levers-per-entry ruling below as a comment in `clippy.toml`, and bring
HYG-047's lever: deny `clippy::expect_used` workspace-wide with
`allow-expect-in-tests = true` in `clippy.toml`; every remaining production
`expect` gets `#[expect(clippy::expect_used, reason = "...")]` saying why it
is sound (the sound invariants HYG-047 lists included, with their assert
messages shortened). Most of the roughly 700 `.expect(` hits are in test
modules, which the setting exempts. `panic`, `unreachable` and
`arithmetic_side_effects` stay off. The entry is then resolved: the other
levers arrive with their own entries.

`clippy.toml` exists but carries only the filesystem choke points for
output publication, run scratch and path-naming input opens (`disallowed-methods`, allowed
workspace-wide and denied across the library and the CLI);
`disallowed-types` and `disallowed-macros` are unused; `expect_used`,
`arithmetic_side_effects`, `panic`, `unreachable` and `exit` are not enabled;
`allow_attributes_without_reason` is not enabled, so `#[allow(dead_code)]` and
the many `too_many_*` exemptions (HYG-036) need no justification; the existing
`include_str!` source-scan tests in `write/metrics.rs` are the in-repo template
for text rules. Every hunter independently pointed most of its "enforceable
by" answers at one of these.

`clippy::print_stdout` and `clippy::print_stderr` are denied in the library
(a crate attribute in `src/lib.rs`, outside `cfg(test)`). Decided: every
other lever waits for the entry that needs it and arrives with that work,
not as a sweep.

Reported by: all nine scopes.

## HYG-081 - The apply-changes node-to-way barrier is a three-party protocol held together by polling

Anger: 10 - rewrites apply-changes' barrier.

Scanner, workers and drain coordinate through `last_node_seq_tx`, `barrier_tx`,
an `OnceLock` loc map and per-worker `Arc<Mutex<FxHashMap>>` coord slots.
Deadlock freedom rests on the drain's 25 ms `recv_timeout` re-polling, the
"idle barrier fire" block that `scanner.rs` warns not to remove, workers
flushing `local_coords` before sending each node item, and the scanner holding
back way descriptors; scanner and drain know of each other only through comments
citing each other. The `-j 1` refusal exists because one deadlock case is
unhandled. The drain's final read of the slots happens while a worker's
end-of-loop flush may still land after the merge and be lost; it is always
empty today because each candidate flushes before sending. The two barrier
triggers (idle fire vs type transition) are indistinguishable in the markers.
`candidate_rx` is behind a `Mutex` held across a blocking `recv()`. Channel-
closed handling is decided per site (`scanner::send_err`,
`streaming::send_drain`, the drain, `barrier_tx`) with comments contradicting
the code. Poison is ignored at five sites, each continuing after a worker
panicked holding a coord slot. Fix: make the node phase a separate pipeline run
(scan nodes, join, merge, publish, then scan ways and relations), so the barrier
is a function return, the timeout and idle fire disappear and `-j 1` stops
deadlocking; return coordinates inside the node `DrainItem` instead of the side
channel; or at least a crossbeam `select` over both channels. Until then, a
`test-hooks` stress test delaying the scanner's `last_node_seq` send past the
drain's last node item.

Reported by: change application.

## HYG-109 - No `--json` diagnostics mode

Anger: 7 - CLI behaviour.

The global `-q, --quiet` landed (`reference/cli-reference.md` "Quiet": a kind
filter in `StderrSink::event`, `cli/src/host.rs`). Still open: `--json`, a
second sink serializing each event's kind, key and arguments, which needs no
library changes. Facts it has to cope with:

- Raw `cli_eprintln!` lines that are not events: getid's `-v` requested and
  missing ID lists (refused with `-q`, so quiet never had to cover them), the
  `bench-read`, `bench-write` and `bench-merge` key=value blocks (brokkr may
  parse them; the quiet ruling left them as they are), and the final `Error:`
  line. A `--json` covering them routes them through the sink first.
- Summary lines have no common shape: some lead with the command name
  (`time-filter:`, `add-locations-to-ways:`), some with a verb (`Wrote`,
  `Sorted`, `Renumbered`, `Merged`), apply-changes prints a multi-line block,
  each `extract --config` region's line carries its own `  [i] path - `
  label, `export` keys itself `export_to.summary`, and getid and
  `getid --invert` share the key `getid.summary` while `cat --dedupe` has its
  own. A `--json` sink would inherit all of it.
- `Developer` events (`[altw external] ...`, the geocode hotpath counters)
  carry `key=value` text or a bracketed prefix of their own.

Maintainer (2026-10-25): skipped for now. The session's view: this is a new
capability with a public format to design, not hygiene; if it comes back,
normalising the summary-line shapes and keys is the first step.

Reported by: split out of the diagnostic-wording item.

## HYG-181 - The parallel diff and derive shard decoders allocate a fresh buffer per block

Anger: 9 - the measured default path of diff and derive.

`pread_decode` in `src/commands/diff/parallel.rs` and
`src/commands/diff/derive_parallel.rs` decompresses every blob into a fresh
`Vec` and hands it to `PrimitiveBlock::from_vec_with_scratch`, so each shard
worker allocates a new, often megabyte-sized, buffer per block. The
sequential merges now recycle theirs through one `DecompressPool` per merge
(`MergeBuffers` in `osc/merge_join.rs`); the classify workers keep one pool
per worker. The same change fits here, one pool per shard worker, but this
is the headline planet path (`diff-snapshots -j 16` in
`reference/performance.md`), and the sequential change itself is unmeasured
("Review once drained"). Whether glibc already recycles these frees well
enough is unknown (rule 8).

Reported by: the HYG-149 fix.

## HYG-180 - Two counter families still build their names with `format!`

Anger: 6 - sidecar counter plumbing on per-blob command paths.

Repack's `repack_{kind}_*` and derive-parallel's `derivepar_{tag}_*` counter
names are built with `format!` from a prefix, so they cannot be grepped
verbatim - the problem renumber had until commit `13dd02aa` gave it literal
names per phase (a local `stage_counters!` macro, a `Phase` value picking
the name column at emission, a unit test pinning every name against the old
spelling). The same treatment fits both; no emitted name or value may
change.

Reported by: the HYG-150 implementation.

## HYG-151 - Every pass reopens its input by path, so a replaced input gives mixed reads

Anger: 8 - an input-handle contract across every multi-pass command.

Every multi-pass command takes `&Path` and opens the input once per pass or
per access pattern: apply-changes four times (front read, scanner walker,
worker pool, splice source), altw once per pass and stage, and likewise
sort, extract, tags-filter, getid, getparents, renumber, cat and geocode.
Each open resolves the path again, so a file renamed over the input mid-run
(an atomic replace by a downloader, say) is read partly from each inode:
the header and schedule from one, blob bodies or `copy_file_range` splices
from the other, with no error. Sharing one descriptor is not the fix:
`posix_fadvise` advice and readahead state belong to the open file
description, and the separations are deliberate (the header walker's
`FADV_RANDOM` must not reach worker body preads; see the comments at
apply-changes' opens). The fix that keeps them is an identity anchor: the
first open records `(st_dev, st_ino)`, and every later open of the same
input is verified against it with `fstat` after opening (a check of the path
before opening races), refusing a different inode with a typed error. That
needs a per-run input handle passed down in place of `&Path`; the `Host` is
the wrong owner, since a library caller may reuse one across runs between
which replacing the file is legitimate. An identity check does not catch an
in-place rewrite of the same inode (truncate and write), which nothing short
of a snapshot does; the entry is about replacement consistency only.

Research done 2026-10-25 (a history search of altw external, prompted by the
maintainer recalling two reverted attempts at something like this):

- No attempt to share or hand down the input descriptor in altw was ever
  committed or reverted. "Open once and hand the File down" was only a plan
  (HYG-078), dropped before implementation in commit `a3f1a1bc` on an
  argument ("would regress: fadvise and readahead state belong to each
  open"), not a measurement.
- What was tried twice and reverted twice in altw external were pass-fusion
  experiments: fusing stage 1's pass A and pass B by spooling node IDs to
  scratch (`44913a5a` reverted by `ba62fb1d`; v1 and v2 reverted together
  by `568a3bc5`: Europe stage 1 42.6 s became 48.6 s and 49.8 s, because
  zlib-rs at about 1 GB/s beats re-reading 23 GB of scratch on a 25 GB-RAM
  host), and epoch-spill fused stages 2 and 3 (`3ae10521`, then `b160c414`:
  planet 741 s against 661 s, 25 GB RSS). Both were lost on measured time or
  memory; neither applies to an identity check that changes no pass
  structure or data flow.
- The "sharing a descriptor would regress" premise in this entry is
  overstated: renumber shares one fd across all phases (`db49c929`, never
  reverted), and about eight commands hand the walker's `FADV_RANDOM` fd to
  their pread workers through `walker.input()`. Only altw external,
  apply-changes and `InputFront` keep separate descriptors. See "Review once
  drained". The entry's conclusion (anchor, not shared fd) stands on its
  own: a held-open anchor descriptor would also cost fds against altw's
  fd budget (`ca15f441`, `5c36fed5`).
- The plumbing as first proposed does not work as written:
  `crate::read::input::open` also opens scratch files (altw stage 3's
  buckets, stage 4's ref-count sidecar, `coord_payloads.rs`), so the check
  cannot live inside it; and a public `InputFile { file, path }` already
  exists in `src/read/input.rs`, used for scratch worker files too through
  `from_parts`, so a new type of that name would clash.

Recommendation (2026-10-25), split one entry per command: this entry keeps
the API - a small `(st_dev, st_ino)` anchor value recorded at
`InputFront::open` (the first open; its descriptor is dropped afterwards, so
the value travels next to the header, as stage 4's header handoff already
does), a separate anchored-open entry point beside `input::open` (scratch
keeps the plain one), the typed refusal, a `test-hooks` fault point that
replaces the input between passes, and the `reference/` sentence on what a
replaced input does and does not catch (in-place rewrites, replacement
before the first open). Public library signatures keep `&Path`. This entry
also confirms the list of multi-pass commands below; entries for commands
that turn out to open their input once are removed under rule 3. HYG-168
to HYG-179 then thread the anchor through one command each, with a test
from the fault point. New refusal: CHANGELOG line.

Reported by: the HYG-078 resolution.

## HYG-168 - apply-changes reopens its input by path per access pattern

Anger: 8 - apply-changes internals.

Split out of HYG-151; runs after it. apply-changes opens its base four
times: the front read, the scanner walker, the worker pool and the splice
source. The comments at those opens state that separate descriptors are
required; per HYG-151's research that is an unmeasured argument
(renumber and the `walker.input()` sites share). Recommendation
(2026-10-25): thread the anchor through all four opens, and reword the
comments to "separate descriptors are kept here; whether sharing would cost
anything is unmeasured".

Reported by: as HYG-151.

## HYG-169 - altw reopens its input by path per pass and stage

Anger: 8 - altw internals.

Split out of HYG-151; runs after it. Sparse and external together, one
command. Input opens found on 2026-10-25: `InputFront::open` in
`external/mod.rs` and `altw/mod.rs` (auto-select), `HeaderWalker::open` in
`external/blob_meta.rs`, stage 1's shared `Arc<File>`, `relation_scan.rs`,
`InputFile::open` for stage 2, stage 4's shared file, and on the sparse path
`altw/mod.rs` and `passthrough.rs`. Workers clone an already-opened `Arc`,
so the extra `fstat`s are a handful per run. Signatures such as
`stage4_assembly(input: &Path, ...)` and `scan_blob_metadata(input: &Path,
...)` change. Recommendation (2026-10-25): thread the anchor through every
input open; scratch opens stay plain.

Reported by: as HYG-151.

## HYG-170 - sort reopens its input by path per pass

Anger: 8 - planet-scale command.

Split out of HYG-151; runs after it. Recommendation (2026-10-25): thread the
anchor through every input open.

Reported by: as HYG-151.

## HYG-171 - extract reopens its input by path per pass

Anger: 8 - planet-scale command.

Split out of HYG-151; runs after it. multi-extract included if it is the
same command. Recommendation (2026-10-25): as HYG-170.

Reported by: as HYG-151.

## HYG-172 - tags-filter reopens its input by path per pass

Anger: 8 - planet-scale command.

Split out of HYG-151; runs after it. Recommendation (2026-10-25): as
HYG-170.

Reported by: as HYG-151.

## HYG-173 - getid reopens its input by path per pass

Anger: 7 - one command.

Split out of HYG-151; runs after it. Recommendation (2026-10-25): as
HYG-170.

Reported by: as HYG-151.

## HYG-174 - getparents reopens its input by path per pass

Anger: 7 - one command.

Split out of HYG-151; runs after it. Recommendation (2026-10-25): as
HYG-170.

Reported by: as HYG-151.

## HYG-175 - renumber reopens its input by path

Anger: 8 - planet-scale command.

Split out of HYG-151; runs after it. Renumber already shares one fd across
its phases (`src/commands/renumber/mod.rs`, from `db49c929`;
`reference/performance.md` says so too), while commit `a3f1a1bc`'s message
and the comment it added describe renumber's schedule walk as a separate
description, and HYG-151 lists renumber among the per-pass openers: which
is true needs checking first. Recommendation (2026-10-25): anchor whatever
opens remain beyond the first.

Reported by: as HYG-151.

## HYG-176 - cat reopens its input by path per pass

Anger: 7 - one command.

Split out of HYG-151; runs after it. Recommendation (2026-10-25): as
HYG-170.

Reported by: as HYG-151.

## HYG-177 - build-geocode-index reopens its input by path per pass

Anger: 7 - geocode build.

Split out of HYG-151; runs after it. Recommendation (2026-10-25): as
HYG-170.

Reported by: as HYG-151.

## HYG-178 - diff reopens its inputs by path

Anger: 8 - diff on a hot path.

Split out of HYG-151; runs after it. Not in HYG-151's original list: the
parallel walk and the shard workers open each input separately (unverified
in detail). Recommendation (2026-10-25): as HYG-170, for both inputs.

Reported by: the 2026-10-25 triage.

## HYG-179 - derive-changes reopens its inputs by path

Anger: 8 - derive on a hot path.

Split out of HYG-151; runs after it. As HYG-178. Recommendation
(2026-10-25): as HYG-170, for both inputs.

Reported by: the 2026-10-25 triage.
