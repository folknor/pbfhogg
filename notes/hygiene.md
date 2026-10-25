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

23. HYG-171 to HYG-179, then HYG-183 to HYG-189, one per line, in ID
    order
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
- The indexdata trust model (`reference/indexdata-trust.md`) was written by
  folding the trust paragraphs of CORRECTNESS.md, pipeline.md and
  cli-reference.md into one page and correcting what had gone false
  (ADR-0013's geocode allocation account, CORRECTNESS.md's "costs neither
  memory nor a refusal" for the geocode builder). Judgement calls:
  - The ruling extends ADR-0013's belief to the header's
    `Sort.Type_then_ID` claim and names `check --index --ids` as the
    admission test, with the gaps listed on the page; whole-file indexing
    is recorded as an inference, not a claim.
  - altw external's intra-blob order check (a per-blob monotonicity pass
    over the tuples stage 2 already extracts, `degrade --unsort-intra` as
    the fixture) was documented as believed, not added. Candidate to add if
    a measurement shows it free.
  - `check --ids`' type-order check gained the direct node/relation
    comparison (a file with no ways, a relation blob before a node blob,
    used to pass). One comparison over offsets already computed; not
    measured.
- `apply-changes --force` now writes a wholly indexed output, restoring what
  the pre-descriptor-first pipeline did (`coalesce_passthrough` reframed
  unindexed passthroughs; commit `719f306a` lost it). Judgement calls:
  - An untouched unindexed blob is reframed in the worker
    (`reframe_raw_with_index`, `DrainItem::OwnedBytes`) on both routes, so
    the splice route gives up `copy_file_range` for those blobs. Forced path
    only; not measured.
  - The indexed hot path changes only in shape: `take_drain_item`'s
    placeholder-index fallback became `take_copy_range` (indexed only), and
    `handle_owned_passthrough` lost its kind/range override. Argued
    neutral, not measured.
  - A forced blob whose elements span two kinds is now refused; it used to
    be copied or rewritten under its first kind, which duplicated the second
    kind's modifications and put its creates out of type order. A new
    `DrainItem::Omitted` drops a blob with no element instead of copying it
    unindexed (it also used to fake a type transition to Node).
- getid accepts a partially indexed file on every path, a contract choice
  against the uniform refusal the schedule walks introduced: the finding
  asked for refusals where a full walk already happens, and the drain
  decided instead that refusal is owed only where a path cannot do without
  the missing indexdata (`reference/indexdata-trust.md`). Classify builders
  take a `PartialIndex` policy; getid's `--add-referenced` pass 1 and the
  `-I` reader (`parse_ids_from_pbf`, shared with getparents) accept, every
  other caller refuses as before. A reviewer should confirm no getid path
  decides from indexdata a blob lacks, and judge whether the other
  refusals should follow the same rule. The builder change adds one branch
  at walker open; not measured.
- getparents accepts a partially indexed file on both arms by the same
  rule, with no code change: only its stated reason changed (the trust
  page, the `refusing_partial_index` doc comment, the guide). Pinned by a
  unit test comparing output bytes, element order and stats against the
  wholly indexed file, in both orders on both arms, and by three
  first-blob-only contract rows. The arms differ in one diagnostic: the
  full-scan arm's filter note reads the first data blob only, so it is
  silent on an indexed head with an unindexed tail where the walker notes
  it; left as is.
- cat's `--clean` path (`cat_filtered`, with or without `--type`) was
  rewritten from three per-kind phases (`build_classify_schedules_split`,
  `parallel_classify_phase` plus a `ReorderBuffer` per phase) into one pass
  per input: one header walk (`build_classify_schedule_blob_filtered`,
  `PartialIndex::Accept`, a kind-only `BlobFilter` when a kind is
  unselected) and one `parallel_classify_phase_ordered` run with an
  admission window of `POOL_REORDER_CAPACITY_WIDE`, the merge writing
  straight to the writer. A planet production command (5m34s planet in
  `reference/performance.md`); not measured, per rule 8. On wholly indexed
  sorted input the decoded set and output are argued the same as before,
  minus two phase barriers; a reviewer should run a matched planet A/B of
  `cat --clean version` against the parent commit, wall and peak anon, and
  read whether the ordered pool's window (64, sized for reorder buffers,
  not tuned here) leaves workers stalled behind a slow head blob. Behaviour
  changes, all deliberate and sparred: output order is input order (an
  unsorted input is no longer regrouped by kind); a partially indexed input
  is accepted; `--clean` without `--type` (or with all three kinds) no
  longer asks for indexdata and rejects `--force`, since it decodes every
  blob once either way; an indexed blob whose kind claim is false loses no
  selected element any more (elements are kept by actual kind); and the
  summary's "Decoded N blobs" now counts input blobs decoded, where it
  counted framed output blobs. The old per-phase reorder buffer was the
  unbounded shape HYG-053 describes; the ordered pool bounds it.
- tags-filter accepts a partially indexed file on every path, by the same
  rule, reversing the refusal its two-pass walks had: pass 1 and pass 2
  walkers no longer opt into `refusing_partial_index`, and the relation
  closure and way-dependency walks pass `PartialIndex::Accept`. `-R` already
  ran on such a file. Sparred with codex, which found no path deciding from
  indexdata a blob lacks; the cost is the extra decode of unindexed blobs in
  every closure iteration. On a wholly indexed planet run only a per-header
  guard branch goes; not measured. Pinned by a unit test (both orders,
  every kind unindexed, -R, two-pass, closure over two iterations, invert,
  remove-tags, distrust; bytes and stats equal to the wholly indexed run)
  and two first-blob-only contract rows. Lateral: tags-count shares the
  tag-key skip and still refuses through
  `build_classify_schedule_blob_filtered`; whether its workers count by
  actual kind was not checked, so a reviewer should judge it under the same
  rule.
- time-filter accepts a partially indexed file on both paths (HYG-157).
  The entry's premise was wrong: the history path reads no indexdata at
  all (the pipelined reader parses none without a blob filter, and the
  path is picked by `HistoricalInformation`), so there was no first-blob
  decision to record. The snapshot path's classify walk, with no kind or
  blob filter, now passes `PartialIndex::Accept`: the refusal protected
  nothing, and it was the odd one out against the wholly unindexed file
  the same path already accepted. The trust page's kind-claim row no
  longer lists time-filter. Sparred with codex, which found no
  counterexample and asked for false-claim and straddling-history
  coverage: a unit test pins byte-identical output and stats for both
  paths under partial (both orders), alternating and parseable-but-false
  indexdata, with history versions straddling the blob boundaries; two
  first-blob-only contract rows (snapshot and history). On a wholly
  indexed planet run only a per-header guard branch goes; not measured.
- The five passthrough copy loops are two, both in `write/copy_range.rs`
  (HYG-017): `copy_range` (the one `copy_file_range`, `CopyOut::Cursor` for
  the buffered sink and unpipelined `write_raw_copy`, `CopyOut::At` for the
  pwrite pool) and `read_range` over a `ReadSink` (the userspace fallback's
  bounce buffer, and the io_uring writer, which reads straight into its
  registered buffers as before). clippy.toml bans `libc::copy_file_range`,
  `pread*`, `pwrite*`, `read`, `write`, `sendfile*` and `splice` library-wide;
  the copy module's one `copy_file_range` is the only allow (verified: the
  lint fires on it with the allow removed). Sparred with codex. Judgement
  calls on measured paths, none measured:
  - The kernel copy still asks for the whole remaining range per call; the
    pool's copy gained an `Option` branch per call (one call per 2 GiB).
  - The io_uring sink is a trait impl with checked `space` / `filled`
    instead of the inline loop. Same pread sizes, no bounce, full-buffer
    submits only; it could not be run end to end here (memlock, ADR-0014).
  - `DirectWriter::flush_buf` and `DirectReader`'s fill now use std `File`
    `write_all` / `read` (the same single syscall each); `DirectReader`'s
    buffer is now allocated zeroed (once per reader) so it can be a slice.
  - The pool's fallback is now the shared bounce sink (`write_all_at`), and
    the copy errors and EOF messages are one wording across backends.
  - New once-per-run Note (`writer.copy_range_fallback`) from the fallback
    counters plus a first-refusal errno kept in `WriterMetrics`; the public
    `report_writer_index_loss` is renamed `report_writer_degradations` and
    emits it. The Note says the userspace copy was selected, never that it
    finished.
- Passthrough splicing, the `FileReader::buffered` SEQUENTIAL hint and altw
  external's scratch eviction were un-gated from `linux-direct-io` /
  `linux-io-uring` (history: the gates were proxies for a once-optional
  `libc`). The measured binary built with both features, so its behaviour
  is meant to be unchanged; default builds now get all three. Sparred with
  codex. Judgement calls, none measured:
  - The splice path compiles in every build; off Linux `copy_range` goes
    straight to the userspace bounce copy (uncounted, no Note) and refuses
    a pair `bounce_safe` rejects, so a non-Linux default build now uses
    coalesced copy runs where it used the per-blob owned route. A whole
    coalesced run lands on one parallel-writer worker there. No sweep
    builds a non-Linux target.
  - Every `posix_fadvise` goes through `crate::fadvise::advise` (clippy
    bans the raw call): failures are counted under a cold-path Mutex and
    emitted as `fadvise_failures`; one extra match and range check per call
    on the per-blob eviction path. An empty consumed prefix is no longer
    advised (length zero would have meant the whole file; not reachable,
    eviction runs after a frame).
  - altw stage 2's slot-bucket `sync_data` failure now fails the run
    ("failed to sync slot bucket") before the DONTNEED instead of being
    dropped. Success path unchanged.
- The geocode index records are defined once in `geocode_index/format.rs`:
  a `record!` field list derives each record's `SIZE`, offsets,
  `to_bytes` and `from_bytes`; `AdminEntry` packs the interior hint behind a
  checked constructor; `Table` owns whole-file, fixed-count span and
  length-prefixed list access, with `ListCount` / `list_byte_len` the one
  list framing (the builder's list writers use it). The reader holds typed
  `Records<T>` / `Lists<T>` mappings and decodes only through `Table`.
  Index bytes unchanged (wire-offset tests written out independently).
  Sparred with codex. Judgement calls on the lookup path, none measured:
  - Binary search is `Table::find_by_key` with a generic key closure, same
    lo/hi/mid algorithm; relies on inlining for the per-probe decode to
    load only `cell_id`, as the hand-written version did.
  - Entry lists, polygon vertices and interpolation node runs iterate
    `chunks_exact` over a slice cut once, instead of per-entry offset math;
    a segment reads its two nodes from the owning way's span (re-derived
    per query, a few checked adds).
  - A list whose count overruns the file reads as empty rather than as its
    in-bounds prefix, and a way or polygon whose span does not fit reads as
    no geometry rather than a partial one. Both are unreachable on an index
    that opens: open validates every list and span.
- The geocode builder reads its own outputs through the same codec:
  `Records<T>` moved to `format.rs`, pass 2 hands back typed mappings, and
  pass 3 and interpolation resolve read through `Table` (`get`, and the new
  `span_at` for byte-addressed geometry) instead of hand offset math. The
  pass 3 bucket scratch record is a `record!` (`BucketRecord`, crate-private,
  same 15-byte layout, still 16 bytes decoded). Index bytes unchanged.
  Sparred with codex. Judgement calls on the planet-measured pass 3 and
  interp-resolve paths, none measured:
  - Stage A streets and interpolation ways validate a way's whole node span
    once, then walk its consecutive node pairs (`cover_way_segments`), where
    the old loop bounds-checked two nodes per segment. A missing node fails
    the build as before, but no longer after partially covering the way
    (the partial output was discarded anyway).
  - Stage B decodes a bucket file with `Table::iter().collect()` (one
    exact-size allocation) after the unchanged partial-tail refusal.
  - The interp CSR build is driven by `(0..len).into_par_iter()` with a
    checked `get` per point instead of `par_chunks_exact`; still an indexed
    source, so `collect` still allocates once.
- The element metadata accessors (`Node::info`, `Way::info`,
  `Relation::info`) no longer fall back to default metadata on an `Info`
  parse failure: one helper (`validated_info` in `elements.rs`) panics, the
  failure being unreachable after `validate_groups_strict`. To make that true
  for the element's only metadata, the element parsers (`WireNode`,
  `WireWay`, `WireRelation`, and `WireDenseNodes` for `denseinfo`) refuse a
  second data-bearing occurrence through a new `take_message`, where they
  used to keep the last. Sparred with codex. Judgement calls on the read hot
  path, per element, none measured:
  - Field 4 / 5 parsing goes through `take_message` (an `Option` match)
    instead of a plain assignment, in every element parse on every read path
    and in the raw scanners (`scan/node.rs`, `scan/way.rs`,
    `blob_meta/scan.rs`), which now refuse the repeat too. Renumber's
    byte-level rewriter does not use these parsers and still copies a
    repeated `Info` verbatim.
  - The release-mode panic replaces a silent default; inside the pipeline's
    `catch_unwind` it surfaces as an internal error, elsewhere it unwinds
    or aborts per the caller.
  - `Node::new`, `Way::new` and `Relation::new` narrowed to `pub(super)`.
- The same rule now covers every re-parse a validated block's consumers
  run: one helper (`validated` in `block.rs`, which `validated_info` now
  calls) panics with a cold, never-inlined call naming what failed. Sites:
  `PrimitiveGroup::dense_nodes`, `BlockElementsIter::step` (dense group and
  the per-element `Node` / `Way` / `Relation` arms), the three group element
  iterators, `decode_dense_columns`, `DenseNodeIter::new` (`DenseInfo`), the
  per-node `keys_vals` walk in `DenseNodeIter::next`, and the varint reads in
  `DenseTagIter` / `DenseRawTagIter`. Each used to read corruption as fewer
  nodes, absent metadata, shorter tags or a skipped element. Sparred with
  codex, which agreed the arms are unreachable and that a validated-block
  wrapper type would restate the existing private construction boundary
  rather than add a proof. Judgement calls on the read hot path, none
  measured:
  - Per element and per dense tag varint, an `Err` branch to a cold panic
    call replaces an `Err` branch to a skip, an early end or a loop
    back-edge. Argued cost-neutral; codex cautioned that equal branch counts
    do not guarantee equal codegen (inlining of the generic helper, layout).
  - `WireMessageIter::next` is deliberately unchanged: shared framing
    plumbing, driven by the validation pass and the raw node scanner too,
    whose doc says why its `.ok()` arms are unreachable.
  - `DenseNodeIter::new`, `new_skip_metadata` and `empty` narrowed to
    `pub(super)`.
- The re-encoding metadata and role lookups refuse a bad string index
  instead of writing `""`: `element_metadata` (now taking the element's kind
  and id) and `dense_node_metadata` return `Result`, the owned `read_*`
  conversions derive their metadata from them, and one
  `relation_members_into` replaces the per-command role loops. Sparred with
  codex. Judgement calls, none measured:
  - Every re-encoding command picked up the refusal, not only the four the
    finding named (altw both backends, extract, multi-extract, tags-filter,
    getid, getparents, repack, cat `--clean` too). Per kept element on those
    hot paths the lookup already ran; what is new is a `Result` return and
    its branch, which codex cautioned is not guaranteed free.
  - Roles were widened in on the same reasoning (the lookup already ran);
    the role loop is now a plain `for` with an enumerate and a push where it
    was an `extend` over a `map`. Tags stay the documented CORRECTNESS.md
    gap.
  - `cat --clean user` refuses a bad user although it would drop it: the rule
    is "refuse a bad user the conversion resolves", not "refuse only what is
    printed" (the OSC writers' rule). A versionless `Info` is still dropped
    whole without resolving its user.
  - `Info::user` and `DenseNodeInfo::user` (public) report a negative index
    as a wire error, where they reported out-of-bounds index 0.
  - The sort overlap and cat dedupe extract callbacks became fallible; sort
    and dedupe locate the error at the run's frame, time-filter history
    routes it through its existing `flush_error` channel.
- The OSC overlay's arena reads panic on a broken invariant instead of
  answering `""`: `StringInterner::resolve` (tag keys, member roles, user
  names) on an id it never handed out, `CompactTagIter` on a tag value
  that does not decode, and `byte_to_member_type` on a type byte other
  than 0/1/2 (it used to decode as `MemberType::Unknown`, which the block
  builder then refused as bad input). One cold helper,
  `arena_invariant_broken` in `osc/interner.rs`. Two `brokkr.toml`
  text rules now cover the family (`.ok()` and `.unwrap_or("")` over
  `src/read`, `src/blob_meta`, `src/osc` and `src/owned.rs`, a deliberate
  `.ok()` carrying `// ok-deliberate: <reason>`); the indexdata decoders'
  infallible slice-to-array `.ok()?` became `le_at` (`first_chunk`).
  Sparred with codex. Judgement calls on apply-changes' per-OSC-element
  write path, none measured:
  - The interner's buffer is a `String` and `resolve` slices it with
    `str::get` instead of running `from_utf8` on every key, role and user;
    argued cheaper. Tag values keep the `from_utf8` check, with a cold
    panic in place of the `""`.
  - Chose the panic over the triage's `debug_assert!`, as `validated` in
    `block.rs` did: a release fallback is the silent wrong answer.
- Input pinning (`read::input::PinnedInput`, `InputSource`): went past the
  triage's identity-anchor recommendation after a codex spar. The pin is a
  held `O_PATH` descriptor on Linux, and later opens reopen
  `/proc/self/fd/<pin>` (a fresh description of the pinned file, so a
  replaced input is harmless); only where that probe fails do opens go
  through the path and refuse a different inode (`InputReplaced`). Every
  derived descriptor is `fstat`-verified and opened `O_NONBLOCK`, then
  cleared with `F_SETFL`. Judgement calls, none measured:
  - Pinning commands refuse FIFOs, sockets and character devices up front
    (block devices pass), including arms that would have read once (the
    time-filter history arm).
  - Per later open: one `open` through procfs, one `fstat`, two `fcntl`;
    per pin, one `O_PATH` open held for the run and one probe open. A
    handful per run, never per blob.
  - `FileReader::open` (public) and `InputFile::open` widened from `&Path`
    to `impl Into<InputSource>`; `&&Path` callers lost deref coercion.
  - Each command adopts it in its own entry below.
  - apply-changes adopted it keeping its four separate descriptions (front
    read, splice source, header walker, worker pool), and now opens the
    splice source only on the splice route: it used to be opened and
    dropped unused under a `--direct-io` output. `PinnedInput::
    reopens_through_procfs` became public under `test-hooks` so
    replacement tests can tell which outcome the host owes.
  - add-locations-to-ways adopted it on every route (sparse, decode-all,
    external, auto's estimate walk), pinning in `add_locations_to_ways_to`
    and separately in the public `external_join`; both public signatures
    stay `&Path`, since the CLI calls one of them once. Every open became
    one derived open, descriptions unchanged (external stage 1 and the
    relation scan still open separately, concurrently). The external
    path holds one more descriptor for the run, which stage 1's fd
    allowance counts as already open. The classify schedule builders
    (`build_classify_schedule`, `_blob_filtered`, `_split`) now take
    `impl Into<InputSource>`, ready for the other commands' entries.
  - sort adopted it in `sort_to`; the public `sort` stays `&Path`. Its
    indexdata precondition moved from `require_indexdata` (a `FileReader`
    probe that skipped the OSMHeader payload) to an `InputFront`, which
    reads that payload (a few hundred bytes) without decoding it, so a
    payload read failure now surfaces at the front rather than at pass 1's
    walker. Every other open became one derived open: pass 1's walker, pass
    2's source (still `dup`ed into the writer's copy source, one shared
    description as before) and one per overlap-run worker. The
    `sort_total_bytes_in` counter now reads the pin's `fstat`; no test
    observes that conversion.
  - tags-filter adopted it in `tags_filter_to`, after the option and
    expression refusals, so a bad expression is still refused before the
    input is touched; the public `tags_filter` stays `&Path`. It pins in
    every mode, including `-R` with `--invert-match` or
    `--distrust-metadata`, which open the input once and used to accept a
    FIFO: the options pick the mode, and one input contract per command
    was preferred over one per option set (sparred with codex). Unlike
    sort, its probe stayed `require_indexdata`, which now takes
    `impl Into<InputSource>` and probes through the pin directly (not via
    the public `has_indexdata`), skipping the OSMHeader payload as before;
    `cat` and `cat --dedupe` gained a `&&Path` deref. Every open became one
    derived open, descriptions unchanged. Not done: the closure, way-deps
    and pass-2 schedules each walk the headers again where pass 1's walk
    could feed them all - repeated work on a measured planet path, not an
    identity hole, so it was left for a measured change of its own.

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
  `frame_and_write_batch`. (cat's `--clean` path left this family when it
  became one ordered pass.) The sort-preservation guards now agree; repack
  fires its guard only when the worker produced full frames, degrade on
  every input blob (it mirrors input blobs one to one).
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
This holds for the three renumber loops,
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
builder's Pass 2 and cat's `--clean` pass) hands out schedule positions
only within a fixed window ahead of the merged head. Its `merge` callback
cannot fail, though, so a caller whose merge writes output (cat `--clean`)
records a write error and lets the pool decode the rest of the input before
returning it; an engine that lets the merge stop dispatch would fail fast.

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

## HYG-182 - `cat --dedupe` indexes an unindexed blob differently from plain `cat`

Anger: 5 - a forced or partially indexed input to one cat mode.

Plain `cat` indexes a blob without indexdata through `scan_block_ids_and_tags`
and re-encodes one no single index describes (`reencode_unclassifiable`).
`cat --dedupe`'s pass 1 (`index_input` in `src/commands/cat/dedupe.rs`) runs
its own copy: `scan_block_ids` only, so a passthrough blob it reframes gets
indexdata but no tagdata (it keeps the input's, usually none), and an empty
or kind-mixed block fails the run with "failed to scan block IDs", which
names neither the cause nor plain `cat` as the way out. One indexing rule
for both, or at least the scan with tags and a refusal that says what the
block is, would fit. Not a partial-index matter: a wholly unindexed input
under `--force` meets the same.

Reported by: the HYG-155 resolution (sparred with codex).

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

## Adopting `PinnedInput`, one command per entry

Not a finding: the shared brief for HYG-171 to HYG-179 and HYG-183 to
HYG-189. Each of those commands opens its input more than once per run (a
precondition probe, a header reader, a schedule walk, worker and splice
descriptors), each open resolving the path again, so a file renamed over
the input mid-run is read partly from each file. The API that fixes it
exists: `read::input::PinnedInput` and `InputSource` (`src/read/input.rs`;
contract in `reference/pipeline.md`, "One input, one file, across passes").
`HeaderWalker::open`, `FileReader::open`, `InputFile::open`,
`InputFront::open` and `BlobReader` / `ElementReader::open_in` already take
`impl Into<InputSource>`, so adopting it is mostly passing the pin where the
path went. What each entry owes, settled in a codex spar on 2026-10-25:

- Pin every input once, at the command's start, before any other open of
  it (`PinnedInput::pin(path, host)`), and pass the pin down through every
  later input open, including dispatch and fallback arms (auto-select, a
  parallel attempt falling back to streaming, multi-extract's engines).
  Scratch opens stay plain `read::input::open`.
- Keep the description topology exactly: each existing independent open
  becomes one derived open, and a descriptor shared today (`walker.input()`
  handed to a classify pool, renumber's shared worker file) stays shared.
  Never `try_clone` a pinned descriptor.
- Size and type reads of the input go through the pin
  (`InputSource::metadata` / `PinnedInput::metadata`), not
  `read::input::metadata(path)`.
- Where the CLI composes several library calls on one input (inspect runs
  its scan, `has_indexdata`, then node statistics), the public entry points
  have to accept a pin (an `impl Into<InputSource>` or a `&PinnedInput`
  variant), so one run is one pin; independent pins per call do not help.
- With several inputs, pin each before probing any. The same path given
  twice (`cat a a`, `diff a a`) needs a decision in that command's entry:
  one pin or two.
- Test from the fault point: arm `FaultPlan::arm_input_replacement` after
  an open in the middle of the run, then assert the replacement fired, that
  opens followed it (`attempts_after_fired`), and the outcome in both modes:
  with `force_pin_path_reopen` the run fails `InputReplaced` and publishes
  nothing; without it (procfs) the output equals the unreplaced run's. A
  test that never reaches a later open proves nothing. Where opens race
  across threads, arming after the last open before they start makes every
  racing open follow the replacement, so the counts stay deterministic and
  every racing failure path runs. A probe `PinnedInput::pin` under the same
  host answers `reopens_through_procfs()` (public under `test-hooks`), so
  the non-forced mode asserts the outcome the host owes instead of
  skipping. `tests/fault_apply_changes.rs` is the worked example for one
  arming point; `tests/fault_altw_input.rs` arms after each open of each
  route but the last in turn, which is cheap on small fixtures and runs
  the refusal path of every open after the first. Where a command has a public library entry besides the
  one the CLI calls, test that entry's pin too.
- Pinning refuses FIFOs, sockets and character devices up front; say so in
  the command's CHANGELOG line, which every adopting entry owes (a
  replacement mid-run is now harmless on Linux with procfs, refused on
  other Unix hosts; off Unix a pin verifies nothing, so do not claim
  "elsewhere").
- The classify schedule builders already take `impl Into<InputSource>`;
  a `&&Path` caller needs a deref. So does `require_indexdata` (since
  tags-filter's entry): hand it the pin and the probe reads through it.

Commands confirmed to open their input once per run, not needing a pin:
export, check refs, check index (one open each). inspect's index-only scan
and its full-decode fallback each read the whole report from one open, but
the full decode takes its file size from a separate path `stat`, which
HYG-187 covers.

## HYG-173 - getid reopens its input by path per pass

Anger: 7 - one command.

See "Adopting `PinnedInput`" above. Recommendation (2026-10-25): follow
the brief.

Reported by: the HYG-078 resolution.

## HYG-174 - getparents reopens its input by path per pass

Anger: 7 - one command.

See "Adopting `PinnedInput`" above. Recommendation (2026-10-25): follow
the brief.

Reported by: the HYG-078 resolution.

## HYG-175 - renumber reopens its input by path

Anger: 8 - planet-scale command.

See "Adopting `PinnedInput`" above. Checked 2026-10-25: both are true.
Renumber opens three descriptions - the header reader
(`ElementReader::open_in` in `renumber/mod.rs`), the schedule walker
(`renumber/schedule.rs`) and the one worker file every data phase shares
(`db49c929`). "One fd across all phases" describes the worker phases only.
Recommendation (2026-10-25): pin, and pass the pin to all three; the
worker file stays one shared description.

Reported by: the HYG-078 resolution.

## HYG-176 - cat reopens its input by path per pass

Anger: 7 - one command.

See "Adopting `PinnedInput`" above. Every mode is in scope: even plain cat
reads each input's header (`read_headers`) before its body reader, and the
indexdata probe opens it once more; `cat --dedupe` (`cat/dedupe.rs`)
probes, indexes (`index_input`) and opens its sources again
(`open_sources`). Many inputs mean one held pin descriptor per input for
the whole run, where cat used to hold one at a time. Recommendation
(2026-10-25): follow the brief.

Reported by: the HYG-078 resolution.

## HYG-177 - build-geocode-index reopens its input by path per pass

Anger: 7 - geocode build.

See "Adopting `PinnedInput`" above. Recommendation (2026-10-25): follow
the brief.

Reported by: the HYG-078 resolution.

## HYG-178 - diff reopens its inputs by path

Anger: 8 - diff on a hot path.

See "Adopting `PinnedInput`" above. Checked 2026-10-25: the shard workers
do not reopen (they keep the walker's shared file, `diff/shard.rs`), but
each input is opened by the `sorted_and_indexed` front read and again by
the parallel walk or the sequential readers. Recommendation (2026-10-25):
follow the brief, for both inputs.

Reported by: the 2026-10-25 triage.

## HYG-179 - derive-changes reopens its inputs by path

Anger: 8 - derive on a hot path.

See "Adopting `PinnedInput`" above. As HYG-178. Recommendation
(2026-10-25): follow the brief, for both inputs.

Reported by: the 2026-10-25 triage.

## HYG-183 - tags-count reopens its input by path

Anger: 7 - one command.

See "Adopting `PinnedInput`" above. `require_indexdata` probes the input,
then `build_classify_schedule_blob_filtered` walks it again
(`tags_count/mod.rs`). Recommendation (2026-10-25): follow the brief.

Reported by: the HYG-151 resolution.

## HYG-184 - repack reopens its input by path

Anger: 7 - one command.

See "Adopting `PinnedInput`" above. The indexdata probe, the header
`ElementReader` and `build_classify_schedules_split` each open the input
(`repack/mod.rs`). Recommendation (2026-10-25): follow the brief.

Reported by: the HYG-151 resolution.

## HYG-185 - degrade reopens its input by path

Anger: 7 - one command.

See "Adopting `PinnedInput`" above. The decode path probes indexdata,
reads the header and walks the schedules (`degrade/mod.rs`); the
header-only passthrough reads the header and then the frames through two
`FileReader` opens. Modes differ, so check each before pinning.
Recommendation (2026-10-25): follow the brief.

Reported by: the HYG-151 resolution.

## HYG-186 - time-filter reopens its input by path

Anger: 7 - one command.

See "Adopting `PinnedInput`" above. The header `ElementReader` decides
history against snapshot; the snapshot arm drops it and walks the schedule
through a fresh open (`time_filter/mod.rs`). The history arm reads once.
Pinning at entry also refuses a FIFO on the history arm; that is the
chosen contract (a command that may reopen pins up front), say so in the
CHANGELOG line. Recommendation (2026-10-25): follow the brief.

Reported by: the HYG-151 resolution.

## HYG-187 - inspect reads one input through several unpinned opens

Anger: 7 - CLI behaviour.

See "Adopting `PinnedInput`" above. `inspect --nodes` probes indexdata and
then walks the schedule (`inspect/node_stats.rs`); the full-decode scan
takes the file size from a path `stat` beside its reader
(`inspect/scan.rs`); and the CLI's inspect runs the scan, `has_indexdata`
and node statistics as separate library calls on one path
(`cli/src/lib.rs`), each opening it afresh. The public entry points need
to accept a pin so the CLI pins once. Recommendation (2026-10-25): follow
the brief, with the CLI composition.

Reported by: the HYG-151 resolution.

## HYG-188 - check ids reopens its input by path

Anger: 7 - CLI behaviour.

See "Adopting `PinnedInput`" above. Both engines read `sorted_and_indexed`
and then build the split schedules from a second open, and the parallel
engine stats the path for its length (`check/verify_ids.rs`).
Recommendation (2026-10-25): follow the brief.

Reported by: the HYG-151 resolution.

## HYG-189 - digest reopens its input by path

Anger: 7 - one command.

See "Adopting `PinnedInput`" above. The envelope walk (`HeaderWalker`)
collects each blob's index, then an `ElementReader` decodes the blocks
those indexes are matched against (`commands/digest.rs`): a replacement
between the two digests one file's blocks against the other's indexes.
Recommendation (2026-10-25): follow the brief.

Reported by: the HYG-151 resolution.
