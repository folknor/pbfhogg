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
  - The unindexed element stream now checks element order per converted
    element (one key compare), classifies each block not of the phase's
    kind by iterating its elements, and drops excluded kinds' blocks
    without converting them; it still decompresses every block of both
    files, deliberately, since that is what refuses a misplaced block.
    Expected faster under `--type`, slightly slower per element otherwise;
    not measured.
  - `WireBlobHeader::parse` refuses a known field with the wrong wire type
    under every retention flag, so a header some read paths used to accept
    is refused everywhere.

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
against its base. Fix: one shared shard core.

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
uring, summed across 16 pool threads for parallel; parallel counts
`writer_bytes_written` at dispatch, others after the write; and
`writer_sync_all_calls` / `_ns` count a failed fsync on the publication path
(`WriterMetrics::sync_recorded` records success or failure) but not in the
parallel and uring writers, whose `file.sync_all()?` returns before
`record_sync_all`. Fix: `writer_thread<S: OutputSink>`
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

Reported by: write path.

## HYG-024 - Counter declaration and emission is hand-written boilerplate

Anger: 8 - counter machinery and semantics.

Altw stages each declare 10 to 35 `AtomicU64` locals, a `_ref` binding for
each, and an emit block repeating `emit_counter("x", x.load(Relaxed) as i64)`
under `#[allow(clippy::cast_possible_wrap)]`, about 300 lines;
`passthrough.rs` has a `Pass2Counters` struct and stages 1 to 4 do not. The
compare-exchange "track the max" loops (altw stages 2 to 4, `cas_max` in
`read/pipeline_metrics.rs`, `write/metrics.rs`) were proposed for
`fetch_max` (already used in renumber's pool, altw stage 4's readiness wait
and `parallel_gzip`), but they are probably not a candidate: they load and
store only when the candidate is larger, so in steady state a call is a
plain read and the cache line stays shared across workers, while
`fetch_max` on x86-64 compiles to a locked read-modify-write on every call,
which on these per-blob paths would take the line exclusively once per blob
per worker. Unmeasured (rule 8). Comments at the loops recording why they
are hand-written were proposed and deferred, not declined; whoever picks
this up should settle both the comment and whether the existing `fetch_max`
sites are hot enough to matter.
`write/metrics.rs` spells its field list three times (struct, `new`, `emit`)
with hand-kept name strings; its `include_str!` funnel tests scan six files but
not `framing.rs` (which writes `compress_ns`, `frame_ns`, `bytes_framed`,
`tagdata_dropped_oversize`) or `parallel_gzip.rs`, and cover only the
`sync_all` pair and `recv_wait_ns`, while `write_ns` and `bytes_written` are
hand-rolled at about ten sites. Elapsed nanoseconds have one owner,
`crate::elapsed::elapsed_ns_u64`, except the altw sites that truncate with
`as u64` (stages 1 to 4, `passthrough.rs`, `coord_payloads`), which were left
because truncating and saturating differ. Fix: a derive or
macro generating struct, `new` and `emit` from one list (see above on the
max loops); a `Stopwatch` whose only exit is
`record_into(&AtomicU64)`; the funnel's source scans moved to brokkr.toml
textlint rules selected by glob, so no file escapes a hand-kept `SOURCES`
list.

Reported by: altw and scanning, write path.

## HYG-032 - Record layouts are serialised by hand at both ends

Anger: 8 - record codecs.

`compact.rs` writes metadata field by field in `arena_append_meta` and
`read_meta` reads back at hand-written offsets `+1, +5, +13, +21, +25`;
`META_LEN = 29` is a hand sum; `NODE_META_OFF = 20` and `WAY_META_OFF = 16`
repeat the record header layout. Altw's `IdRecord::read_from` and
`ResolvedEntry::write_to` have no shared codec and stage 3 decodes
`ResolvedEntry` by slicing `chunk[0..4]`, `[4..8]`, `[8..12]`. The geocode
`pass3.rs` `BUCKET_RECORD_SIZE = 15` with `write_bucket_record` /
`parse_bucket_file` duplicates the pattern `format.rs` exists to own; `interp.rs`
(`read_addr_point_mmap`, `read_node_at`, unchecked `offset + SIZE`) and
`reader.rs` (`decode_addr_point`, `record_at`, `read_record`,
`two_node_distance`, `checked_add`) decode the same records differently;
`U32EntryIter`, `SegmentEntryIter`, `AdminEntryIter` have identical `new`
bodies, `binary_search_cells` and `binary_search_admin_cells` are one function
twice, and `validate_list` is a fourth implementation of the list framing, so a
count-width bump is five edits. Fix: a `FixedRecord` / `Record` trait (`SIZE`,
`write_to`, `read_from`) with sizes derived from it, or `zerocopy` / `bytemuck`
packed structs, so a layout mismatch is a type error; a generic
`ListIter<T: Record>` and `binary_search<T>`.

Reported by: change application, altw and scanning, geocode index.

## HYG-034 - Decode failures are turned into plausible default values

Anger: 9 - decode failures and aborts become errors: new refusals, Results on builder hot paths.

- `elements.rs` (three sites): `WireInfo::parse(data).ok().unwrap_or_default()`,
  so corrupt metadata becomes version 0 / timestamp 0, indistinguishable from
  absent; `time-filter` and history handling can treat a corrupt element as
  timestamp 0. `block.rs::validate_info`'s doc says so.
- `block.rs::PrimitiveGroup::dense_nodes` and its twin: a `WireDenseNodes`
  parse error yields an empty iterator, so a corrupt dense group reads as "no
  nodes". `dense.rs::DenseNodeIter::new` drops all dense metadata on
  corruption.
- `wire.rs` (`read_tag().ok()??`) ends on the first bad tag. The `blob_meta`
  scanners no longer do this, and their callers now propagate the error.

Already adjudicated, not to be re-reported: tag iterators ending early on an
out-of-range string index, and roles collapsed to `""` by
`role().unwrap_or("")` (the OSC writer, `bench_write_loop` and the owned
conversions included), are accepted in CORRECTNESS.md "Tag iterators end
early on an out-of-range string index". A tag array ending mid-varint is
rejected up front by `validate_groups_strict`.
- `owned.rs` `element_metadata` / `dense_node_metadata`: a `user()` string-table
  error becomes `""` and missing timestamps, changesets and uids become `0`,
  re-encoded by sort, `cat --dedupe`, degrade and time-filter. Documented, but
  opposite to decision 0004's defensive-input stance, and invisible.
- `osc/write.rs`: `info.user().ok()`.
- `StringInterner::resolve` and `CompactTagIter` return `""` on an
  out-of-range id or invalid UTF-8 ("keeps a read-side accessor from
  panicking"), turning arena corruption into silent empty tags.

Whether the read-path cases are safe depends on whether
`PrimitiveBlock::from_vec*` validated the same bytes; `block.rs` has a
validation pass, so some are "cannot happen after construction", but that is an
invariant held by a different function in a different file. Fix: accessors
return `Result`, or construction produces a validated type whose lazy accessors
are infallible by construction; for crate-written arenas a `debug_assert!` plus
an error-returning iterator; for `owned.rs`, propagate or count and report
(its existing fault test would flip to assert the error). Enforce with
`disallowed-methods` on `Result::ok` inside `src/read/` and `src/blob_meta/`
with per-site `#[allow(reason = ...)]`.

Reported by: read path, CLI and plumbing, change application.

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

Fix: remove the parameters, and have the CLI refuse or say so where a command
honours the flag on some passes only, so `--direct-io` help can say what it
affects; regenerate the matrix from a per-command table in code (read passes
honoured, write honoured) rather than by hand. Enforce with one contract test
per command that a no-feature build refuses `--direct-io` before opening any
input, and a unit test of the table against the capability matrix.

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

Reported by: write path, read path, altw and scanning, re-encoding, change
application.

## HYG-039 - O_DIRECT alignment is a hardcoded 4096 and the O_DIRECT open is spelled three times

Anger: 8 - I/O fallbacks, gating, alignment.

`PAGE_SIZE = 4096` is "universally safe across Linux filesystems" on both the
read and write sides. The real requirement is the device and filesystem DIO
alignment that Linux reports via `statx(STATX_DIOALIGN)` (6.1+), which can be
512 or larger than 4096; on a mismatching host the open succeeds and the first
write fails with an unexplained `EINVAL`, and `DirectReader::skip` arithmetic
assumes 4096 too. `DirectWriter::create`, `uring_init_and_run` and
`read/direct_reader.rs` each build the same
`OpenOptions::new()...custom_flags(libc::O_DIRECT)`. Fix: detect via `statx`
with 4096 as fallback; one `open_direct_for_write`; a text rule on
`custom_flags(libc::O_DIRECT)` outside it.

Reported by: read path, write path.

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

Reported by: write path, read path, CLI and plumbing, filtering and spatial,
re-encoding, altw and scanning.

## HYG-051 - Per-run metrics live in process-global statics

Anger: 8 - counter machinery and semantics.

`PIPELINE_METRICS` is a `static` never reset, and `emit()` runs at the end of
every `run_pipeline` / `run_pipeline_fused`: a command running two pipelines
sequentially emits cumulative counters twice (the second includes the first
pass's waits and counts, high-water marks are max over the process); two
concurrent pipelines (`into_blocks_pipelined` on two files, `diff`) add into
the same atomics; `par_map_reduce` and sequential `for_each` emit nothing, so
the sidecar has no read-side counters for commands built on them. An early
`PipelinedBlocks` drop returns "pipeline consumer dropped" as an `Err`, so a
normal early exit runs the error path and emits like a failed run.
`WRITER_METRICS` is written from every layer and emitted once by `main` after
dispatch (per-flush emission double-counted), so a library consumer or a test
running two commands gets cumulative totals; the writer tests
`end_of_stream_finalization_still_fsyncs` and
`flush_pushes_bytes_onward_without_fsyncing` assert on deltas of a global that
concurrent tests also bump, with bounds "loose enough to tolerate" it.
`Instant::now()` is called directly at about 25
write-path and 60 altw sites for these, which is part of why none can be
asserted deterministically. Fix: a metrics value owned by each run or writer
(`Arc` handed to stages), emitted with a pass label or returned in stats.
Enforce with a test running two pipelines and asserting the second emit is not
cumulative.

Reported by: read path, write path, CLI and plumbing, altw and scanning.

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

Reported by: re-encoding, read path, filtering and spatial, altw and scanning.

## HYG-058 - Sort and dedupe memory on adversarial input

Anger: 8 - memory bounds change algorithms and output.

Sort holds every overlap run's re-encoded output (`overlap_outputs`) before the
write loop; the comment says for pathologically unsorted input it can approach
the input size, which is planet-sized RSS on exactly the input sort exists for.
Process runs in a sliding window of K in-flight runs written in order. Dedupe's
sweep heap is bounded by overlap-run width, which one blob spanning the whole ID
range makes the whole kind.

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

Reported by: change application, re-encoding, altw and scanning, geocode index.

## HYG-062 - Fault-injection hooks are process-global statics

Anger: 8 - fault plans in options structs.

Hooks live as statics in `read/pipeline.rs`,
`read/region_grid.rs`, `write/parallel_writer.rs`, `write/parallel_gzip.rs`,
`write/uring_writer.rs`, `commands/diff/parallel.rs`,
`commands/diff/derive_parallel.rs`, `commands/altw/external/stage3.rs` and
`geocode_index/builder/pass3.rs` (the hooks in `geocode_index/reader.rs`,
`write/writer.rs` and `commands/apply_changes/` are per-instance),
re-exported through several `mod.rs` files under differing naming schemes (`read::pipeline_test_hooks`,
`read::region_grid_test_hooks`, a `write::` re-export, an `altw::` re-export),
so nothing answers "what can a test inject". Reset discipline differs:
`parallel_writer` says reset at start and end, `region_grid`
resets both sides, stage 3's `PANIC_AT_BUCKET_IDX` doc says reset "at the start
AND end of any test". The statics now sit behind their own
`static-test-hooks` feature, which keeps them out of the shared `all` and
lib unit-test harnesses by construction (reference/testing.md, "Test
binaries"), and `scripts/check_static_hook_targets.py` enforces one test
per target. The
`read/pipeline.rs` test now arms through a guard
that releases, joins and resets on every exit path; the other hooks still rely
on manual reset at start and end. Apply-changes is the one seam with an injection
point (`MergeOptions::panic_at_blob_seq`), and that field forces every struct
literal to carry a `#[cfg(feature = "test-hooks")]` line and a hand-written
`Default` with `#[allow(clippy::derivable_impls)]`. Fix: one `FaultPlan` type
(zero-sized without `test-hooks`) carried in each command's options, cloned to
workers as an `Arc`, behind a single `pbfhogg::test_hooks` facade; until then, a
`HookGuard` returned by arming that resets on `Drop`. Enforce with a text rule
banning `static` inside `mod test_hooks`.

A concrete untested contract waiting on such a seam: altw external's stage 4
builds the output writer on a worker thread, and a `--direct-io` /
`--io-uring` setup refusal there must reach the caller as the typed
`PlatformUnsupported` (exit 3, not 2). Nothing can force that refusal today
without a host assumption: the CLI's `--direct-io` also makes reads direct, so
an input-side refusal fires first on the main thread, and
`external_backend_output_refusal_stays_typed_across_stage_join` in
`cli/tests/cli_add_locations_to_ways.rs` passes without exercising the join on any
host whose filesystem accepts O_DIRECT output. An injection point that makes
stage 4's writer construction fail would make it deterministic.

Reported by: test harness, read path, write path, altw and scanning, geocode
index, change application, CLI and plumbing.

## HYG-077 - The enforcement levers most of these findings need are unused

Anger: 8 - clippy.toml with expect_used denied means hundreds of edits.

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

## HYG-078 - Inputs are opened and their headers read several times per run

Anger: 8 - open the input once in altw and apply-changes.

Altw checks sorted and indexed preconditions through four separate opens
(`auto_select_index_type` opens an `ElementReader` and a `BlobReader`, the
explicit-sparse hint another `ElementReader`, `external_join` calls
`require_indexdata` and opens another for `is_sorted`, stage 4 a `BlobReader`
for the header), each with its own failure message. Apply-changes opens the
base PBF five times by path (`require_indexdata`, `read_header`, `File::open`
for copy, `HeaderWalker::open`, `run_workers`), so a file replaced mid-run gives
mixed reads. Renumber opens and parses the header twice in a row. `multi.rs`
reads the header via `BlobReader` and again via `HeaderWalker`, and the first
decides sortedness. `inspect`, `bench-write`, `renumber` and `diff` also open
a reader (or run a probe) whose only job is to make a header-less file fail
with the missing-header error before the scan proper, which would otherwise
read it as empty. Fix: open once and hand the `File` and parsed header down
(for altw, a plan that opens the input once, checks every option combination
that depends on it and the scratch filesystem, and chooses the backend; the
input-free combinations are already refused by `AltwOptions::validate`).

Reported by: altw and scanning, change application, re-encoding, filtering and
spatial.

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

Reported by: split out of the diagnostic-wording item.

## HYG-121 - apply-changes carries the base's replication state onto a newer output

Anger: 8 - apply-changes output contract; nidhogg reads replication state
from PBF headers.

Output headers now come from one policy (`src/commands/output_header.rs`,
`reference/cli-reference.md` "Output header"). What remains: apply-changes
states `Replication::Carry`, so its output declares the base's
`osmosis_replication_timestamp` and sequence number although it holds the base
plus the OSC, a later state. osmium apply-changes writes a fresh header with
no replication fields. Claiming the older state is the safe direction for an
updater (it re-applies a diff, which is idempotent at equal versions), but it
is not what the output contains. The OSC carries no sequence number, so the
honest options are to drop the timestamp and sequence number (osmium) or keep
them and document them as the base's; which one depends on how nidhogg's
`update.rs` reads them, which the maintainer has to check. bench-merge shares
the path.

What nidhogg does today (nidhogg may be changed freely): only `update::init`
reads the header, to bootstrap its state file; afterwards the state file is
authoritative. With no replication fields, `init` falls back to Geofabrik's
current state, which silently skips diffs if the PBF is not current - so
today's carry is the safe direction for it (a re-init replays, which is
idempotent). `merge.rs` passes a `HeaderOverrides` with every replication
field `None`. Proposed, not decided: pbfhogg drops the fields unless the
caller supplies them, nidhogg's merge stamps the sequence and timestamp of
the last diff it applied, and `init`'s fallback stops being silent.

Needs research before a ruling:

- What osmium `apply-changes` writes, exactly, and whether osmium or
  osmosis ever advance replication fields across a merge (and from what).
- What other consumers of pbfhogg output read these fields (osm2pgsql's
  replication init, pyosmium's `pyosmium-up-to-date`, Overpass, imposm),
  and what each does with a stale value versus a missing one.
- Whether an OSC's companion `state.txt`, or the OSC's own content (newest
  element timestamp), is a sound source pbfhogg could derive the true state
  from.
- Which other pbfhogg commands carry replication state (`Replication::Carry`
  in `output_header.rs`) onto outputs whose content no longer matches it,
  so one rule covers them all.

HYG-128 (no flag clears a header field) waits on this outcome.

Reported by: the output-header policy work.

## HYG-128 - No flag clears a header field

Anger: 3 - CLI behaviour, small.

`--generator` and `--output-header` can only set a value; an empty value
writes a present, empty field. Nothing removes `source`, the replication base
URL or the writing program from an output header (`degrade --strip-bbox` is
the only removal, and only of the bbox). Whether removal is wanted, and with
what spelling, is the maintainer's call; no syntax is reserved today.

Needs research, together with HYG-121: the one concrete use seen so far is
stripping stale replication state, which HYG-121 is deciding the default
for. If its outcome drops the fields from apply-changes output unless the
caller supplies them, the main reason for a removal flag goes away. Options
on the table for whatever remains: an empty value meaning absent (changes
today's present-but-empty behaviour), or an explicit repeatable
`--clear-header KEY` with a conflict rule against `--output-header KEY=...`.

Reported by: the scratch-lifecycle review fixes.

## HYG-137 - `cat` could split unclassifiable blobs losslessly

Anger: 4 - cold path only; speculative.

Speculative: nothing is known to need it. Plain `cat` re-encodes a blob
the index scan cannot classify (no elements, a group carrying more than
one element field, kinds changing across groups) through the block builder
(`reencode_unclassifiable` in `src/commands/cat/mod.rs`), so those blobs
take the builder's normalisation, the same `cat --type` and `--clean`
apply: every node comes out dense, coordinates finer than the default
granularity are rounded to decimicrodegrees, timestamps are truncated to
whole seconds, an `Info` with a version gains explicit defaults for its
absent fields, an `Info` without a version loses its metadata, and
unknown protobuf fields are dropped. Every other blob `cat` writes is
byte-preserved or reframed without re-encoding, so these are the only
blobs plain `cat` alters.

A lossless split is possible at the byte level: copy the block's string
table (and granularity, offsets and date granularity) verbatim into each
output block, and regroup the raw element messages by kind, splitting at
each change of kind and at the element cap. Element bytes would then pass
through untouched, and each output block would be scanned and indexed as
the ordinary reindex arm does. Costs: a protobuf-level regrouper the crate
does not have today (the builder works from decoded elements), dense-node
groups that must be split by re-delta-encoding their packed arrays when a
cap split falls inside one, and string tables carried whole into every
piece (wasted bytes, unless pruned, which is a re-encode of its own).

Worth doing only if a producer whose blocks fall in these shapes turns up
and the normalisation matters to someone; otherwise it stays parked.

Reported by: the BUG-113 implementation.
