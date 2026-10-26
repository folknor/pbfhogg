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

On a measured path, a change argued as a win because it removes an
allocation, a copy, a pass or a reread is unproven, not obvious: at planet
scale memory moves rather than vanishing, and each stage's throughput shifts
with it in ways the code does not show. altw's history has two such "obvious"
wins that measured badly and were reverted (fusing stage 1's passes lost to
re-decompression; a pipelined stage 4 reader was faster but held 27 GB
against 1.5 GB). Before such a change, search `git log`,
`reference/performance-history.md` and deleted `notes/` and `plans/` files
for an earlier attempt at the same idea and its measured outcome; if one was
reverted, say why the new attempt differs. Prefer changes that leave the data
flow and each stage's memory shape untouched. Anything that changes them
goes on "Review once drained" as a bisect candidate, named by commit.

Launch order: every queued entry has run.

Every entry still open in this file and `notes/bugs.md` is owed a resolution
in this drain, except what the maintainer moved to the CLI-surface redesign
in `notes/todo.md` (the `--direct-io` reach and `--json` entries, and the
clippy-exemption entry, which the maintainer wants resolved outside the
drain). BUG-009 is the maintainer's own project.

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
- cat and `cat --dedupe` now call `ProcessActions::fd_allowance` before
  pinning their inputs and refuse the run when the host allows fewer than
  the inputs plus a fixed `CAT_EXTRA_FDS` (the CLI raises its soft limit
  there, as for altw external). A new refusal and a new process-policy
  call on a measured command, from the pin adoption; unmeasured, though it
  is one `getrlimit`, one `setrlimit` and one `/proc/self/fd` listing per
  run.
- `reframe_raw_with_index` (cat, merge and apply-changes passthrough) now
  builds its header through the same `encode_frame_header` as freshly
  encoded blocks, so it gained the `MAX_BLOB_DATASIZE` body refusal. Argued
  unreachable today (the frame decoder refuses the same bound on every
  passthrough body) and kept as framing policy owned by frame construction;
  one compare per reframed blob on planet cat, byte-identical output,
  unmeasured. The `OversizePart::HeaderWayMembers` attribution is now
  computed (the header would fit without the bitmap) instead of
  `len > 1024`.
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
- The text diff's drivers (the sharded block-pair engine, with an inline
  single shard, and the element stream; the sequential block-pair merge the
  commit also touched is gone) take their writer as an `Option`, which now
  lives in `DiffSink`, so `diff_stats` can share them without formatting:
  each write site has a branch on it, including per common element when
  `-c` is off, and the `diff` entry point moved its body into a shared
  `compare`. Argued branch-predictable and below noise on the text path, not
  measured. The count mode takes the byte-equal fast path like `diff -c`
  (`diff_stats` sets `suppress_common`, which the skip follows); see the
  review audit's ruling on `diff -q`.
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
  - Every indexed diff run, `-j 1` included, now starts with an up-front
    `walk_pair` over both files' blob headers (`diff/shard.rs`: a
    `HeaderWalker` per input, a pread per blob, `--direct-io` not
    honoured), which checks canonical order and indexdata before any blob
    is decoded. The per-blob order guard and the trailing walk this
    sub-item first described went with the sequential block-pair merge.
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
  preads costs anything is unmeasured, and the tree does both: about eight
  commands hand the `HeaderWalker`'s `FADV_RANDOM` fd to their pread workers
  through `walker.input()` (classify, tags-filter, extract smart and multi,
  getparents, geocode pass 1.5, diff's shard walk, altw's sparse
  passthrough), while altw external and apply-changes keep separate
  descriptors on an argument (commit `a3f1a1bc`) that was never measured.
  Renumber is not on the shared-walker side: its schedule walk keeps its
  own `FADV_RANDOM` descriptor, and what it shares (commit `db49c929`, its
  comment at the open and `reference/performance.md` say so) is one plain
  body descriptor across its data phases, a different question. `InputFront`
  is not in either list: it is a precondition probe closed before any pass
  runs. The split may track access pattern rather than accident (the
  sharing commands are selective classify-style readers using `FADV_RANDOM`
  plus a `WILLNEED` prefetch over the schedule; altw external stage 4 reads
  every blob), so measure both shapes. One matched `--bench 1` A/B per
  side, for example on altw external stage 4 at Europe scale, would settle
  it.
- The sequential unindexed diff and derive merges (the element stream)
  recycle their decompression buffers through one `DecompressPool` per
  merge, shared by both sides and every phase (`MergeBuffers` in
  `osc/merge_join.rs`), instead of allocating a fresh buffer per block. The
  indexed block-pair merge pooled too when this was written (`126cbce2`)
  and no longer does (`2c12a955`). Judgement calls, none measured:
  - The gain is reasoned only: glibc malloc's dynamic mmap threshold
    probably already serves a freed buffer back after the first few blocks.
    Recycling cannot plausibly allocate more, but it adds an `Arc` owner
    allocation and an uncontended mutex per block.
  - Peak memory gains the pool's idle buffers, bounded by the most decoded
    blocks the merge held at once (a current block per side and a stashed
    one) and by the pool's
    4 MiB-per-buffer and 64-buffer limits. Buffers over 4 MiB are dropped
    and now show in the diff and derive runs' `DECOMPRESS_POOL_METRICS`
    oversize-drop counters, which these paths never fed before.
  - The parallel shard decoders, the measured default path, still
    allocate per block; left alone (see the decode-buffer growth item
    below). Since the one-engine merge (below) the indexed block-pair merge
    no longer uses `MergeBuffers` at all, at any `-j`; only the unindexed
    element stream still pools.
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
- Partially indexed input, ruled 2026-10-26 (`notes/review-audit.md`,
  Slice C): every command refuses it except `cat`, `check --index` and
  `inspect`. The drain's acceptances (getid on every path, getparents on
  both arms, tags-filter on every path, time-filter on both paths) are
  reverted, and the refusal moved from opt-in walks into `read::frame::Frames`,
  armed by default on every command-input read (`Frames::of_input`,
  `BlobReader::open_in`), observed at frame completion; `PartialIndex`,
  `HeaderWalker::refusing_partial_index` and digest's own guard are gone.
  Judgement calls on measured paths, not measured: an armed frame parses
  indexdata (a 42-byte copy and a deserialize) on reads that skipped it
  (the pipelined reader without a blob filter, `par_map_reduce`), one
  compare per data frame everywhere; export, degrade's passthrough and
  geocode pass 1 now refuse at their own reads. Hotpath: cat `--clean`'s
  schedule walk reports as
  `build_classify_schedule_blob_filtered_accepting_partial`.
- cat's `--clean` path (`cat_filtered`, with or without `--type`) was
  rewritten from three per-kind phases (`build_classify_schedules_split`,
  `parallel_classify_phase` plus a `ReorderBuffer` per phase) into one pass
  per input: one header walk (`build_classify_schedule_blob_filtered`,
  exempt from the partial-index refusal, a kind-only `BlobFilter` when a kind is
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
  - Pinning commands refuse FIFOs, sockets, character devices and block
    devices up front (`b8acda7c`: a block device's `st_size` is 0, which
    the walker would read as an empty file), including arms that would have read once (the
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
    walker. Every other open became one derived open. Sort now opens three
    times: the front read, the walker in `blob_merge.rs` and one pass-2
    `InputFile` (still `dup`ed into the writer's copy source, one shared
    description as before). The per-overlap-run-worker opens are gone with
    the blob-merge engine (`79d2e923`); overlap sweeps share that
    description. The
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
    derived open, descriptions unchanged. The commit also removed the
    ignored `_direct_io` parameters of `collect_relation_member_closure`
    and `collect_way_node_dependencies`, the mistake the preamble names;
    they were put back after the audit. Not done: the closure, way-deps
    and pass-2 schedules each walk the headers again where pass 1's walk
    could feed them all - repeated work on a measured planet path, not an
    identity hole, so it was left for a measured change of its own.
  - getid adopted it with the CLI pinning, not the library: the first
    design pinned inside `getid` and left the `-I` PBF read whole before
    the main input was pinned, which codex held breaks "pin each before
    probing any", and a FIFO `-I` blocks in its open (no ESPIPE first).
    So the public `getid`, `removeid` and `parse_ids_from_pbf` take
    `impl Into<InputSource>`, the new public `InputSource::pin` pins a
    path or shares a caller's pin, and the CLI pins the input after
    reserving the output and before `IdSources::resolve`, which pins the
    `-I` PBF before reading either ID file. `resolve` is shared, so
    getparents' `-I` is pinned too.
    The same path as input and `-I` is two pins, taken back to back:
    sharing one would need lexical path equality, which misses `./x`
    against `x`. `dispatch_scan_arm` and `estimate_blob_count` take
    `impl Into<InputSource>`. Every open became one derived open,
    descriptions unchanged. The streaming full-scan arm is unreachable
    from a fixture through the public entry, so a test-hooks unit test
    drives `getid_dispatched` with the threshold at one. Found on the
    way: `cli_input_contract.rs` claimed its forced-unindexed getid case
    reached the streaming arm; unindexed input always takes the walker,
    so the comment and test name now say so.
  - getparents adopted it in getid's shape: the CLI pins the input after
    reserving the output and before `IdSources::resolve`, and the public
    `getparents` and `getparents_with_min_blobs` take
    `impl Into<InputSource>`, pinned in `getparents_dispatched` before the
    probe. Opens: the probe, the arm estimate (indexed input only), then
    the walker (its descriptor still shared with the classify pool) or the
    full scan's reader; every one became one derived open. The public
    `has_indexdata` widened to `impl Into<InputSource>` and probes through
    the pin (it never pins a path itself), rather than getparents reaching
    for the private probe, since inspect's entry (HYG-187) needs the same;
    cat's `&&Path` caller gained a deref. Both arms share an open count
    (three on indexed input), so `tests/fault_getparents_input.rs` tells
    them apart through the token's `reorder_filled_high_water`, which only
    the full scan's fused read pipeline records: codex showed that a
    wrapper dropping `min_blobs` would otherwise run the walker twice
    unnoticed. Mutations checked: routing the walker or the full-scan
    reader back to a path open fails every route, and forwarding the
    default threshold from `getparents_with_min_blobs` fails the full-scan
    route. getid's CLI FIFO test now covers getparents too.
  - renumber adopted it in `renumber_external_to`, after the start-ID
    check (which touches no file). Unlike sort, the public
    `renumber_external` takes `impl Into<InputSource>` and pins through
    `InputSource::pin`, as that method's doc asks of an entry that opens
    its input more than once; the CLI still passes the path, since
    nothing on the output side (reservation stats only the destination)
    reads the input first. Its three opens each became one derived open,
    all on the calling thread before any worker: the header reader
    (`O_DIRECT` under direct reads), the one description every data phase
    preads, and the schedule walk's own `FADV_RANDOM` description. The
    file length now comes from the pin's `fstat`. The replacement fixture
    differs in coordinates, tags and counts, since a replacement that
    differed only in IDs would renumber to the same bytes, and the
    unreplaced output is checked by content before it serves as the byte
    reference (codex). Mutation checked: routing the shared data open back
    to a path open fails every route.
  - tags-count adopted it in renumber's shape: the public `tags_count`
    takes `impl Into<InputSource>` and pins through `InputSource::pin`
    after the expressions parse (a malformed one is still refused before
    the input is touched); the CLI still passes the path. Its two opens,
    the `require_indexdata` probe and the schedule walk whose descriptor
    the classify pool preads, each became one derived open, on every
    route (type filter, a tag index skipping some or every blob, forced
    unindexed, direct). Codex confirmed the inventory, including that
    `WillneedPrefetch` and `decided` neither reopen nor stat. Mutation
    checked: routing the walk back to a path open fails every route.
  - repack adopted it in the same shape: the public `repack` takes
    `impl Into<InputSource>`, `repack_to` pins first; the CLI still passes
    the path. Its three opens (the `require_indexdata` probe, the header
    `ElementReader`, the schedule walk whose descriptor every per-kind pool
    preads) each became one derived open, on every route (indexed, forced
    unindexed, LocationsOnWays, an empty relation schedule, direct). The
    `BlockBuilder` pre-allocation bound now reads the pin's metadata, and a
    failed stat now fails the run where it used to fall back to an
    unbounded `u64::MAX` ceiling (codex agreed). No test observes that
    metadata source: the bound changes capacity, not output bytes, so it
    rests on reading the code. Mutation checked: routing each of the three
    opens back to a path open fails the tests.
  - time-filter adopted it in the same shape: the public `time_filter`
    takes `impl Into<InputSource>`, `time_filter_to` pins first; the CLI
    still passes the path. Its opens, the header `ElementReader` (which
    picks the path from `HistoricalInformation`) and, on the snapshot path
    only, the schedule walk whose descriptor the classify pool preads, each
    became one derived open, descriptions unchanged. The history path reads
    once but pins too, since the path is only known after the header read,
    so it now refuses a FIFO. Codex agreed after being pressed on two
    alternatives that would keep a history FIFO working: anchoring a pin on
    the header reader's already open descriptor (a new constructor), or a
    streaming snapshot path (undoing the classify-pool migration that fixed
    its planet memory ceiling); neither gives a correctness gain, only the
    FIFO. Mutation checked: routing either open back to a path open fails
    the tests.
  - inspect adopted it per library entry, with the CLI pinning only where
    it composes calls. `inspect::inspect` takes `impl Into<InputSource>`
    and pins only when the index-only scan may run, since whether it falls
    back to a full decode (a second open) is learned from the walk; a
    decode-only run (`--extended`, `--locations`, `--distrust-metadata`)
    opens once, takes no pin and still reads a FIFO. The full decode's
    file size now comes from `fstat` of its own reader, not a path stat
    (new `pub(crate)` `FileReader::metadata`). `node_stats` takes
    `impl Into<InputSource>` and pins before its probe and walk. The
    `--get` header route moved into a new public
    `inspect::inspect_header`, one `ElementReader::open_in` whose
    descriptor also gives `file.size`, replacing the CLI's
    `ElementReader::open` plus a path stat; it now reads under the host's
    page-cache policy, which codex flagged as an observable change under
    `Evict` (an advisory `DONTNEED` over the header prefix), accepted as
    that policy working as specified. The CLI's scanning report pins once
    when `--indexed` or `--nodes` joins the scan and passes the pin to all
    three calls, and gained cancellation checks before the pin and after
    every computation, so a cancel during the indexdata probe, which has no
    checkpoint, no longer yields an answer. Codex preferred moving the whole
    composition into the library as one operation but found no case the
    split pins wrongly; kept, since mode selection stays in the CLI either
    way. `--indexed` alone and `--show` open once and are unchanged.
    Mutation checked: a path stat for the decode's size, the fallback
    decode or the node walk opened by path, and dropping the CLI's
    composition pin each fail the tests.
  - digest did not adopt it: its envelope walk was folded into its
    decoding pass instead, so it opens once, takes no pin and now reads a
    FIFO (sparred with codex, which agreed after checking that the
    sequential `BlobReader` enforces everything the walk did - header
    length, truncation, payload extent, leading header - and parses the
    same index with the same presence semantics). This touched the shared
    fused read engine, a measured path (ADR-0009): `run_pipeline_fused`
    now takes a `FusedTransform` whose `apply` sees every blob after the
    leading header plus a deferred decode, and `for_each_fused_block`
    wraps its closure in a `BlockOnly` adapter that skips non-data blobs
    as before, monomorphized. Judgement calls, none measured: the scratch
    borrows now end before the getid, getparents, tags-filter and altw
    transforms run instead of spanning them, and `apply` is called (and
    returns `None`) for non-data blobs. `PartialIndexGuard` became generic
    over its location and is shared by the header walker and digest's
    consumer. A multi-fault file can now report a different refusal first,
    since partial indexdata is no longer refused before any decode.
- `BlockBuilder`'s eight `add_*` paths (four public, four raw) no longer
  `assert!(can_add_*)`: a wrong kind or full block poisons the builder
  through a `#[cold] #[inline(never)]` `refuse`, the element is dropped and
  the next `take` / `take_owned` errors. Per-element write hot path on every
  re-encoding command; argued cost-neutral (the same predicted branch, the
  cold body out of line where the assert's panic path was), not measured.
  Sparred with codex: `add_*` returning `Result` plus poison was its first
  preference and was withdrawn once finalization was accepted as the
  builder's error boundary; it confirmed no production caller can emit a
  block or exit zero after a refusal (a kind or cap refusal leaves the
  builder non-empty, so `is_empty`-gated flushes cannot skip it). Later
  adds on a poisoned builder are deliberately not checked, so they still
  mutate its buffers; `can_add_*` answers kind and capacity only. The
  zero-cap panic the finding also named was already gone (`NonZeroUsize`).
- The raw node scanner's dense loop (`scan/node.rs::scan_dense_coords`,
  the per-node hot loop of altw sparse pass 1, altw external stage 2,
  extract simple and apply-changes' node coords) now reads `id`, `lat` and
  `lon` with three `protohoggr::Cursor::read_varint` calls in lockstep and
  propagates their errors, then requires the `lat` and `lon` cursors empty,
  instead of zipping three `PackedSint64Iter`s that stopped silently at the
  shortest array or first bad varint. Same decoder, no extra pass, errors
  built out of line; argued cost-neutral, not measured. Codex accepted the
  mechanism but not "identical performance follows from identical
  branches": codegen of three sequential fallible reads versus the old
  tuple evaluation is the open question. Measure at planet on both altw
  index types. The per-ref loops of the raw way scanner
  (`scan/way.rs::accumulate_refs`: altw, extract simple, geocode passes
  1.5 and 2) and altw's way rewriter (`altw/reframe.rs`) got the same
  change, for the same silent stop on a bad refs varint; same argument,
  not measured.
- The block validator's `packed_varint_count` (every packed sint64 and
  int32 array on every `PrimitiveBlock` construction) and
  `packed_uint32_count` (`keys` / `vals`) now track the byte position within
  each varint and refuse a tenth byte above `1`, matching
  `Cursor::read_varint`, which previously made a ten-byte overflowing varint
  count as one value and decode as none. Still one byte scan, but the
  former's loop body grew from one compare to a position counter; a
  construction-time cost on every read path, not measured.
- The ordered read engine is one `run_pipeline` generic over a
  `DecodeTransform`; `run_pipeline_fused` and `drain_fused` are gone, and
  the plain surfaces (`for_each_block_pipelined`, `into_blocks_pipelined`,
  and through them the `read` bench, time-filter history and geocode pass 1)
  pass the identity transform `Blocks`. Same channels, depths, threads,
  admission and drop order; argued codegen-equivalent under
  monomorphization, not measured. Behaviour that did move: a reader-error
  item's `decoded_send_wait_ns` is now recorded on failed sends in the
  command-transform runs too, the test-hooks decode gate now parks command
  transforms as well (it matches `seq`, not a pass), and hotpath attributes
  the command runs to `run_pipeline` instead of `run_pipeline_fused`.
  Sparred with codex, which confirmed the equivalence point by point but
  held "identical codegen" as plausible, not established.
- `sort` and `cat --dedupe` are one engine, `src/commands/blob_merge.rs`
  (sparred with codex to consensus on the mechanism). Bisect candidate on
  three axes, none measured:
  - `cat --dedupe` pass 1 moved from a sequential `FileReader` walk
    (`fadvise(SEQUENTIAL)`, payload skipped by seek) to `HeaderWalker`
    preads, sort's trade: a planet win and a dense-europe pass-1
    regression when sort made it (performance-history "Sort", #4).
  - `cat --dedupe` passthrough went from read plus `write_raw` per frame
    to sort's coalesced `copy_file_range`, and the overlap runs of both
    commands from (sort) a `par_iter` collect of every run's output before
    the write loop and (dedupe) one serial sweep writing straight to the
    writer, to dedicated worker threads streaming each run's blocks
    through a bounded channel, admitted two runs per worker ahead of the
    write head. Dedupe's memory shape changes most: up to
    `min(window, workers)` sweep heaps at once where it had one, plus
    `window x 4` re-encoded blocks. The planet bench (planet merged with
    itself, every blob in a two-blob run, 133 min single-threaded) is the
    cell to re-run. A run is now one overlap component (or one lone
    unsorted blob); it used to be a maximal stretch of adjacent rewrite
    blobs of one kind, which on a self-merge is the whole kind, so the old
    sort `par_iter` had at most three runs there and a first cut of this
    engine swept the denmark self-merge serially (one unstored smoke run,
    30.0 s against the 29.8 s `214361bb` row). The split adds a builder
    flush at every component end, so the output's block boundaries change
    and adjacent components stop sharing `BlockBuilder`'s backfill of
    default node metadata when one carries metadata and the other does
    not; element sequence and duplicate survivors are unchanged (the old
    sweep emptied its heap at each component boundary anyway).
  - Sort on already-sorted input should be untouched (no runs, no threads,
    the same single coalesced copy), but its pass 2 now reads through one
    shared positional source and the per-entry loop is the shared one.
  Behaviour that moved: dedupe now repairs an unindexed internally
  unsorted blob under its sorted claim (and drops exact duplicates inside
  it) instead of copying it through; an input whose first frame is not
  the OSMHeader is refused at that frame instead of after sort's full
  pass-1 scan; the sort fault test's open count dropped from three plus
  one per overlap run to three; a zero-byte `copy_file_range` is now
  located in the input (path and offset), which dedupe's between-pass
  truncation test needs now that its passthrough splices.
- Diff and derive over indexed inputs are one block-pair engine at every
  `-j` (the HYG-013 drain, sparred with codex): the BlobReader-based
  sequential merge (`MergeSide`, `skip_phase`, `verify_rest`) and the two
  per-command shard copies (`diff/parallel.rs`, `diff/derive_parallel.rs`)
  are gone; `-j 1` is the header walk plus one shard run on the calling
  thread, negative IDs force one unbounded shard, and each shard runs the
  former sequential control flow (byte-equal fast path on fresh pairs,
  observed-tail residuals with the claimed-max refusal) over pread sides,
  clipping only blobs whose claim crosses the shard window. One sink per
  command (`DiffSink`, `ChangeSink`) at every shard count; verbose now
  shards. Judgement calls on the measured default path, none measured:
  - Per shard step the merge now reads both fresh blobs raw before
    deciding, and holds one undecoded when only the other is emitted; the
    old shard loop decoded eagerly. Same decode count, one held body per
    side at most.
  - The byte-equal probe runs on the default path for `--format osc` (and
    `-c`): an index-field compare per fresh overlapping pair, plus an
    envelope parse and memcmp only when min, max and count all match. On
    independent snapshots (planet diff-snapshots) such pairs are expected
    to be rare; on an apply-changes output against its base the sharded
    path now skips decoding unchanged blobs, which it never did.
  - `ElementEqual` carries the element, so derive no longer computes a
    version for every common element (the old sequential engine did; the
    shard copies did not), and the per-element sink call is generic, not
    `dyn`.
  - The merge loop computes each head's ID once per step (the old copies
    twice) and uses `osm_id_cmp` for the in-range test where the shard
    copies used a raw compare.
  - The shard driver opens every shard's sink on the calling thread before
    spawning, where the copies created their files in the workers.
  - Shadow `mergejoin_shadow_*` counters are now emitted on sharded runs
    too, and derive's `derivepar_*` plan counters on `-j 1` runs.
  - `-j 1` on indexed inputs changed I/O shape: a whole-file header walk
    first (as every sharded run did), then pread per blob from the walker's
    descriptor re-advised `SEQUENTIAL` (it was a streaming `BlobReader`);
    it lost the CLI host's page-cache eviction, `--direct-io`, and
    `MergeBuffers` recycling. The text output still goes straight to the
    caller's writer, derive's straight to its final fragments.
  - Single-shard phases of a multi-shard run (a kind with one old blob)
    run inline on the calling thread and write the final destination
    directly instead of a scratch file plus copy.
- The three frame decoders (`BlobReader`'s, the streaming helper behind the
  raw-frame and front-probe reads, `HeaderWalker`'s) became one state
  machine, `read::frame::Frames`, over a byte source (commit `307982d7`,
  the bisect candidate for any read-path delta). Error locations and
  truncation messages changed by design (every frame error at its frame
  start with its blob index; `BlobReader` used to name the exact byte).
  Judgement calls on the hottest read paths, none measured:
  - `BlobReader` iteration, under every `ElementReader` and pipeline: the
    prefix is one `fill` instead of two `read_exact`s, and each frame passes
    through the walk's pending and poison bookkeeping. Header reads (clear +
    bounded `read_to_end`) and payload reads (`Vec::with_capacity(datasize)`
    + bounded `read_to_end`) keep their shape.
  - `HeaderWalker`: same syscalls (one probe pread per frame, one remainder
    pread for a header past the window, none for a payload), but every
    header is now copied from the probe window into the walk's reusable
    header buffer (the whole header, unknown fields included) where the old
    walk parsed it in place. Warm-cache dense inputs are where it would show.
  - The raw-frame walks (cat passthrough, getid streaming scan, inspect,
    check --index, degrade) read each frame into a capacity-sized `Vec`
    with a bounded `read_to_end` instead of a zero-filled `vec!` and
    `read_exact`, and reuse one header buffer instead of allocating one per
    frame.
  - The front probes (`has_indexdata_in`, `InputFront`, `require_indexdata`)
    now check the run's cancellation token before each frame.
- `PbfWriter`'s three `write_primitive_block*` methods are one dispatch,
  `write_osm_data` with a `BlobMeta::{Scan, Bare, Given}`, and every
  pipelined write (framed and the four raw forms) takes its slot through one
  `reserve` (commit "PbfWriter's per-block writes are one dispatch", the
  bisect candidate for any write-side delta on every command that encodes
  blocks). Judgement calls on the per-block hot path, none measured:
  - `write_primitive_block_owned` takes the `OwnedBlock` itself, so its
    tagdata and WayMembers `Vec`s move into the framing closure instead of
    being copied on the producer thread after the permit. In-flight jobs
    now carry the original allocation's capacity slack (bounded by the
    dispatch permits) instead of a length-sized copy, and the producer no
    longer holds the original while the worker holds the copy.
  - The owned path's 42-byte index serialization moved from the producer,
    before the permit, to the framing worker.
  - The Scan and Bare closures capture the whole `BlobMeta` enum (sized for
    `Given`), so their rayon job box grew by roughly a `BlobIndex` and two
    `Option<Vec>`s; no added allocation.
  - The raw writes (`write_raw`, `write_raw_owned`, `write_raw_chunks`,
    `write_raw_copy`) now check `writer_gone` before taking a permit, so
    after a framing closure saw the writer thread die they return its error
    without copying or sending. They used to reach the same error at the
    failed send.
- The writer counters were given one meaning per name across backends
  (commit "Writer counters mean one thing on every backend"):
  `writer_write_ns` was retired for `writer_handoff_ns` (timed once per
  chunk at the writer loop, `time_handoff`) and `writer_pool_write_ns`.
  Judgement calls on the write hot path, none measured: the `pwrite` pool's
  dispatcher gained a stopwatch per chunk; the synchronous writer's raw
  writes (`multi-extract`'s passthrough) gained a stopwatch and the
  payload and byte atomics per write, on a process-global shared by every
  region writer; `writer_bytes_written` is now bumped per constituent write
  (each `RawChunks` chunk, each of a synchronous frame's three parts)
  instead of once per chunk.
- The three pipelined writer loops became one, `pipeline::deliver_in_order`,
  with each backend an `OutputSink` (commit "The pipelined writer backends
  share one ordered-delivery loop"; ADR-0014 amended). Sparred to
  equivalence, not measured: io_uring's non-blocking reap and the pool's
  stopped check run in `after_drain` (after each received item, never before
  the first, so a large header's failed write cannot preempt the stream's
  own first error); the pool loses its vacuous pre-first-receive stopped
  check; the uring `uring_panic_at_dispatch` hook now counts per handoff
  operation, not per item. Every chunk now reaches the sink as owned
  buffers through one generic `hand_off`, monomorphized per backend; the
  data flow, channel and buffer shapes are unchanged. The io_uring sink
  could not run on the bench host (`RLIMIT_MEMLOCK` 8 MB hard) and is
  compile-checked only.
- The OSC gzip writer (`write/parallel_gzip.rs`, planet `diff --format osc`
  assembly) swapped its `BTreeMap` reorder map for
  `ReorderBuffer::with_max_window(permit window, permit window)` and
  inserts with the new fallible `try_push`; it was deliberately NOT fitted
  to `deliver_in_order` (barriers, keep-draining after failure, own
  counters). Sparred, not measured. Same threads, queues, chunk size and
  credit window; the map's per-insert tree nodes became one up-front
  window-sized slot array (tens of small slots). Behaviour changes: a seq
  breaching the window is now a recorded failure in release builds (was a
  debug assert); `finish` refuses a delivered-count mismatch against the
  producer's sent count (argued unreachable today); failure-path permit
  returns are `try_send`; `parallel_gzip_reorder_high_water` counts filled
  slots, as the map's length did. `ReorderBuffer::push` is now a panicking
  wrapper over `try_push` on every read and write pipeline's per-blob path:
  no allocation on success, machine code not compared.
- `clippy::expect_used` is denied workspace-wide (sparred, not measured).
  Most production `expect`s were annotated; some were removed by
  restructuring on measured or output-defining paths: altw external stage
  1's id-shard writers are built all-or-nothing and installed whole (a
  `Vec<BufWriter>`, the `Option` wrapper gone; same 256 writers and buffers
  per worker), fixing a creation error that could turn into an index panic
  on the worker's next blob; the per-blob zstd and zlib compressor caches in
  `write/framing.rs` use a guarded match with `Option::insert`;
  `geo::assemble_rings_search` takes the new ring end from the appended
  segment and loops `while let` over the search stack (argued
  traversal-identical; geocode admin polygons depend on it); degrade's
  unsort hold takes the held element before writing the arriving one.
- Every pool that delivers results in schedule order now runs on one
  engine, `src/ordered_pool.rs` (the reorder-buffer bound drain; design
  sparred with codex to consensus, `reference/pipeline.md` "Ordered pread
  pool"): renumber's pool generalised, classify's `OrderedCredit` dispatcher
  replaced, and the hand-built ordered pools converted (repack, degrade,
  getparents, time-filter snapshot, tags-filter pass 2, extract
  `pread_execute`, both multi-extract writers, altw sparse passthrough,
  external stages 1 and 4); `blob_merge` shares only the gate. A bisect
  candidate on several axes, none measured:
  - Windows where there were none: every converted pool now admits at most
    `default_window(threads)` tasks (8 per worker, floor 32) past the
    consumed head (cat `--clean` and geocode pass 2 keep their own wider
    windows). Before, a 32-slot result channel bounded ordinary run-ahead
    and only an effectively unbounded reorder buffer bounded the slow-head
    case. Workers can now park behind a slow head blob; altw stage 4's way
    workers can park behind an unpublished router slot. Window sizes were
    not tuned. As landed, the result channel was as deep as the window,
    which no run can fill: send-side counters went blind and consumer-bound
    pools held up to a window of results. Fixed later (review-audit Slice F
    item 2): the channel is capped at `POOL_RESULT_CHANNEL_DEPTH` again.
  - Claim-based workers replace the dispatcher thread and
    `Mutex<Receiver>` fan-out. The sliding WILLNEED advice (classify's
    callers, tags-filter pass 2) moved from the dispatcher to the consumer,
    advised for the window's edge before that credit is published.
  - Credit returns after `consume` instead of before (renumber): the window
    now includes the item being written, so renumber's effective window is
    one smaller.
  - Failures surface in schedule order (lowest failing position, or the
    consumer's own) where classify's pools surfaced arrival order;
    repack and degrade lost their write-before-classify latch; cat
    `--clean`, geocode pass 2 and time-filter now stop at a write error
    instead of decoding the rest of the input.
  - Passthrough frames (extract, multi-extract nodes, altw passthrough,
    stage 4) are copied by the consumer between decode results
    (`Interleaved`), so a passthrough prefix is copied after decode
    result 0 arrives rather than before any; each frame checks
    cancellation; multi-extract's all-passthrough fast path is now the
    same consumer with zero decode tasks.
  - Altw stage 1's id-shard writers are flushed after the join, one thread
    per shard, where each worker flushed its own on exit (overlapping
    other workers' scans); a flush is still attempted after a pool error.
  - Sidecar counters keep their names and units (`Timer::Ms` for the
    inherited per-sample-truncated `_ms` counters); `s4_channel_high_water`
    is now measured by the engine with the same workers-up, consumer-down
    accounting. Their meaning held only once the channel cap was restored
    (above); every ordered pool also records `ordered_pool_*_wait_ns` now.
  - Multi-extract's region writer gained the sliding WILLNEED advice
    (through the ordered classify phase); its old dispatcher had none, and
    the advice was measured a win on europe only.
- Decode-buffer growth, a measurement candidate rather than a change
  (resolving the diff shard-decoder pooling entry, sparred with codex; no
  code changed, only the comments in `WireBlock::parse_and_inline_with_scratch`
  and `BlockPairSide::decode`). A diagnostic test (not kept) showed that
  `decompress_blob_raw` leaves a buffer with truthful `raw_size` exactly
  full for zlib and zstd, and that std's `reserve` on a full 1.4 MB Vec
  doubles it to 2.8 MB. So on every decode, `parse_and_inline_with_scratch`'s
  inline-metadata reserve typically doubles a fresh buffer's capacity and
  may move it (one payload copy, unless glibc grows it in place or, for an
  mmapped chunk, remaps it: `__libc_realloc` / `_int_realloc` in
  `research/glibc/malloc/malloc.c`). Its contribution to any command's
  wall is unmeasured. Three things to compare once measurement is allowed:
  - The fresh-buffer paths (the diff and derive shards, apply-changes'
    streaming workers, getid, cat `--clean`, inspect, the sequential
    reader, altw's relation scan) pay the growth per block.
  - Pooled paths pay it on a buffer's first use and keep the doubled
    capacity, which later decodes reuse; a final capacity past the pool's
    4 MiB retain cap (common for payloads above about 2 MiB) is dropped
    and re-grown each time. The 886.6 MB `parse_and_inline_with_scratch`
    allocation share in the planet getparents alloc profile (`f83c8cf1`)
    is probably this growth.
  - A per-shard `DecompressPool` in `run_shard` shared by both sides would
    hold at most two idle buffers per shard (up to 128 MiB of retained
    capacity at `-j 16`). Rejected without measurement: `reserve_exact`
    in the parse (it also hits pooled first use and changes which buffers
    the pool retains) and a guessed headroom at decompression (changes
    every read path's memory shape on a heuristic).
- `parallel_classify_accumulate` workers now stop at their next blob when
  a peer fails or panics (a run-local `AtomicBool`, raised before the
  failing worker's send and by an unwind guard), where they used to drain
  their whole share of the schedule first; a stopped worker sends no
  state. Found sparring HYG-011. Success path cost: one relaxed load per
  blob on extract simple, smart and multi, tags-filter's relation closure,
  getid and `inspect --nodes`; argued below noise, not measured.
- `parallel_classify_phase_abortable` (and `parallel_classify_phase` on it)
  and `parallel_scan_blobs_raw` are thin wrappers over one private driver,
  `run_unordered_per_blob` in `scan/classify.rs`. Same dispatcher thread,
  `Mutex<Receiver>`, channel depths, `WILLNEED` placement, worker-thread
  init and consumer loop; the per-blob work is a monomorphized closure over
  per-worker local state. Hotpath names stay on the wrappers. Reviewed by a
  fresh codex session; codegen argued equivalent, not measured. Every
  caller of the two functions runs through it.
- The two altw way reframes share their block-level walk,
  `altw::reframe::parse_way_block_top` (StringTable range, group ranges,
  the framing scalars and their default-framing refusal); the per-way
  coordinate steps stay separate by design (random-access `NodeIndex`
  lookups against stage 3's pre-encoded positional payload), and a
  `CoordSource` trait was rejected after a codex spar. Stage 4's
  `parse_block_ns` stopwatch still spans exactly that walk, now a call
  across modules; once per way blob, argued neutral, not measured. A
  malformed block tag in stage 4 now reads "reframe block tag", as sparse
  did. `brokkr verify add-locations-to-ways --dataset denmark` passes.
- The unordered classify pools (`run_unordered_per_blob`, hence
  `parallel_classify_phase`, `_abortable` and `parallel_scan_blobs_raw`, and
  `parallel_classify_accumulate`) gained `*_wait_ns` stall counters, one
  process-global set emitted (process totals so far) after every run, success
  or failure: `classify_unordered_dispatch_send_wait_ns`,
  `_desc_lock_wait_ns`, `_desc_recv_wait_ns`, `_result_send_wait_ns` and
  `_result_recv_wait_ns`. Altw stage 2 gained `s2_slot_flush_lock_wait_ns`
  beside the unchanged `s2_slot_flush_lock_wait_ms`. Stage 3 and geocode
  pass 1.5 have no blocking point and say so at the pool. Every blocking
  operation goes through new gated helpers in `src/counters.rs`
  (`lock_recording_wait`, `send_recording_wait`, `recv_recording_wait`):
  the `try_*` form first, so an unblocked operation costs the attempt in
  place of the blocking call's own fast path (no clock, no counter), and a
  blocked one adds two clock reads and one relaxed `fetch_add`. The changed
  sites run on every blob of every caller of those pools (dispatcher send,
  worker lock, receive and send, consumer receive) and on every altw stage 2
  slot flush; argued free, not measured. The accumulate pool's result send
  (one slot per worker, one send each) and its consumer's receive (the scan
  itself) are deliberately not counted. Once measurement is allowed, these
  counters are how to settle the `Mutex<Receiver>` question in
  `reference/performance-history.md`.
- apply-changes' node-to-way barrier (the barrier entry's drain): the scanner's
  node boundary travels on the drain channel as
  `DrainMessage::NodeBoundary { node_end_seq }` (exclusive end, no
  `u64::MAX` sentinel) instead of its own polled channel, and the drain's
  three publication sites became one readiness check after each receive and
  contiguous advance; `process_item` refuses a way or relation before the
  barrier. Judgement calls on the planet `-l` path, none measured:
  - Publication can now happen after a contiguous run of node items rather
    than right after the last of them; argued equivalent because, before
    publication, only node items exist in the drain.
  - A late boundary publishes on its own receive, where it used to wait for
    the next 25 ms timeout and the idle re-poll: up to one timeout earlier.
  - The boundary shares the bounded drain channel with every item (256
    slots), where it had a one-slot channel of its own; the scanner can
    block sending it while the drain is behind, which it already could for
    every splice item. Sparred with codex and reviewed by a fresh session,
    which found no regression.
- The merge engine behind `sort` and `cat --dedupe` (`blob_merge.rs`)
  indexes as plain `cat` does (the dedupe-indexing entry's drain, sparred
  with codex to consensus, then reviewed by a fresh session): pass 1 keeps
  the scan's outcomes apart (a malformed payload is its own located error),
  omits an element-less block, and gives a block no single index describes
  one forced-rewrite entry per kind, which the kind-gated sweep merges into
  type order; an unindexed passthrough is reframed with scanned tagdata
  beside its scanned indexdata. Pass 1 now records physical `Frame`s with
  per-kind `BlobEntry`s pointing at them, and every count is per frame.
  Found on the way and fixed: `sort` under an unclaimed-order header
  planned on scanned indices but copied each blob's original indexdata even
  when the scan contradicted it; such a blob is now reframed. Judgement
  calls on the planet-measured sort path, none measured:
  - Production sort (indexed, sorted claim) scans nothing either way, but
    `plan` now allocates four per-frame `Vec<bool>`s and walks the entries
    twice more, and `BlobEntry` shrank (offsets and tagdata moved to
    `Frame`, which no longer keeps an indexed blob's tagdata, never read
    on a raw copy). Pass 1's retained memory per indexed blob falls by its
    tagdata; argued below noise in wall, not measured.
  - On forced unindexed input (`sort --force`, `cat --dedupe --force`)
    every scanned blob now collects its tag keys (the combined scan), and
    the serialized key set is held from pass 1 to pass 2 for every
    passthrough candidate: memory proportional to unindexed blob count
    times key-set size, chosen over a second decompress in the serial
    write loop.
  - `sort` on an unclaimed-order indexed input gives up `copy_file_range`
    for each blob whose claim is false (reframed through the write loop).
- The last pools without stall counters (bz `pbfhogg#16`, sparred with
  codex to consensus). Judgement calls on measured paths, none measured:
  - The merge engine's pass 2 (`blob_merge::write_plan`, planet `sort`
    and `cat --dedupe`) runs on `ordered_pool::run_ordered_streams`: the
    write loop issues each overlap run itself (its own channel, the sender
    queued FIFO), so the dispatcher thread, the shared admission gate and
    the order channel are gone. Same window (`OVERLAP_RUN_WINDOW_PER_WORKER`
    per worker) and per-run depth, so the same block bound. What moved: one
    thread fewer; a run is issued when the write loop finishes the one a
    window back, where the dispatcher admitted it at the gate's wakeup. As
    before, a failing run does not stop later runs from starting (up to a
    window of them may start before the write loop reaches the failure,
    which still surfaces in run order). A sweep now also checks
    cancellation and abandonment every `SWEEP_CHECKPOINT_REMOVALS` heap
    removals, duplicate discards included (two atomic loads per check), so
    a run's final heap
    drain and a long duplicate stretch are bounded; before, an abandoned
    sweep ran its heap out to the first failed send. Production `sort` on
    sorted input has no runs and starts no thread, as before.
  - Gated stall counters on every blocking point of that pass:
    `ordered_pool_result_send_wait_ns`, `_result_recv_wait_ns`, and the new
    `ordered_pool_job_lock_wait_ns` and `_job_recv_wait_ns`. The job pair
    reads together: the holder keeps the lock across its receive, so the
    lock wait is about the receive wait times the idle peers (seen on a
    forced Denmark run).
  - `cat --dedupe` gained nested markers (`MERGEPBF_PASS1`, `_PASS2`,
    `_WRITE_LOOP`, `_FLUSH` inside the kept `MERGEPBF` pair) and
    `cat_dedupe_*` counters.
  - Extract simple's node and way classify phases left their two
    hand-built `Mutex<Receiver>` pools for `run_unordered_per_blob`
    (through `parallel_scan_blobs_raw`, whose work now receives the
    schedule position). They record the `classify_unordered_*` set and now
    get the dispatcher's sliding `WILLNEED` advice over their schedules,
    which they never had (planet `extract --simple` is a recorded row),
    and the driver's cancellation checks before each wait, not only after
    each result. The unordered pools now emit their counters from a drop
    guard, so a panicking run still emits them.
- apply-changes accepts `-j 1` (`9744a9fc`): `Parallelism::Fixed(1)` and the
  CLI's `-j 1` were refused before and are now accepted. A CLI and library
  contract change, in the CHANGELOG, accepted by the maintainer 2026-10-26.
  Unmeasured: what a one-worker run costs in wall and memory is not known.

No findings remain open; the file stays until the list above is reviewed
(rule 4).
