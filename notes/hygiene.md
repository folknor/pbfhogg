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
  Degrade's copies have diverged from repack's in the sort-preservation guard (repack drains when
  `!bb.is_empty() || !pending.is_empty()`, degrade only on `!bb.is_empty()`;
  the hunter could not construct a failing path, so it is latent and rests on
  an unstated flush-before-add property).
- `extract/common.rs::pread_execute` (whose `ConsumerItem` match is written
  twice verbatim in the same function for the post-drain tail),
  `extract/simple.rs` (node and way classify), `extract/multi.rs` (node write,
  way/relation write), `tags_filter/mod.rs` pass 2.
- `geocode_index/builder/pass1_5.rs`.

`getparents` and `getid --id-osm-file` use the shared helper; the rest do not.
Divergence across copies is already fact:

- Error policy: stage 2 overwrites `*err_ref = Some(e)` (last error wins),
  stage 1's flush slot keeps the first, stage 3 has both a first-wins
  `s3_error` and the router's `abort_error`; repack/degrade latch and keep
  draining, renumber returns immediately (and hangs), sort uses rayon's
  short-circuiting collect.
- Cancellation: stage 2 polls the error mutex per bucket, classify relies on
  channel drop, stage 1 runs until the schedule drains, getparents breaks the
  scan on first error with a comment on why that matters at planet scale.
- Validation: `blob_body_within_file` runs in the classify and getparents
  schedule builders; the external `scan_blob_metadata`, passthrough
  `build_schedule`, extract and tags-filter scans skip it, so a past-EOF
  `data_size` fails late as a bare `UnexpectedEof` with no offset.
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

`diff/parallel.rs` and `diff/derive_parallel.rs` each define `BlobDesc`,
`WalkedFile`, `walk_file`, `Shard`, `plan_shards`, `build_shard`,
`pread_decode`, `Side`, `element_merge`, `kind_type_char`,
`append_and_cleanup` and the driver. The equality predicate is no longer
copied: every diff path, merge-join and `digest` call
`crate::content::content_equal` (CORRECTNESS.md "Tag iterators end early..."
describes it). `plan_shards` comments differ; `emit_merge_stats_counters` is
duplicated between `diff/mod.rs` and `derive.rs`; `extract_delete_info` and
`write_delete_element` between `derive.rs` and `derive_parallel.rs`. Because
the CLI defaults `-j 0` to all cores, the parallel path is the default for
indexed inputs and the sequential paths run only under `--verbose` or `-j 1`.
Fix: one shared shard core; an equivalence test asserting byte-identical
`-j1` and `-j8` output for diff and derive catches drift in any copy.

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
provenance stamping: `BlobReader` stamps path, blob index and offset except in
its two length-prefix I/O-error branches (offset dropped); `HeaderWalker`
stamps path and index but never the offset it holds as `frame_start`;
`raw_frame.rs` returns `BoxResult` with no stamping, so `cat`, `extract`,
`check --index` and `getid` raw paths report frame errors with no path or
offset. Fix: one frame state machine over an abstract byte source (stream and
pread-at-offset as two impls), returning a stamped `FrameHeader`. Enforce with
a conformance test running every truncation shape through every source; the
`blob.rs` test with `read_arm` / `skip_arm` / `walk_arm` is that test in
embryo.

Reported by: read path.

## HYG-015 - Write-side blob framing and dispatch are copied

Anger: 9 - writer framing on a hot path.

- `PbfWriter::write_framed_blob` (sync path) reimplements `frame_blob_into`
  line for line; the test `the_sync_path_refuses_an_oversize_blob_body` records
  that they diverged once (the size guard landed in one copy).
  `reframe_raw_with_index_scratch` is a third framing assembly that skips the
  `MAX_BLOB_DATASIZE` check (arguably right for passthrough, but nobody
  decided). `encode_blob_header_into`'s WayMembers culprit heuristic
  (`len > 1024`) guesses which field overflowed instead of computing sizes.
- `write_primitive_block`, `write_primitive_block_no_indexdata` and
  `write_primitive_block_owned` each carry about 70 identical lines; the test
  `every_primitive_block_variant_stops_once_the_writer_thread_is_dead` exists
  because a fix to one missed the others. The permit acquire duplicates
  `acquire_permit` instead of calling it.
- `to_path_uring` and `to_path_parallel` carry the same spawn plus `init_rx`
  match (messages differ by one word), all four constructors build the same
  `WritePipeline` literal, and a test helper builds a fifth.
  `to_path_uring`'s unbounded `init_rx.recv()` is documented and left.

Fix: one `frame_parts` every path calls; one `dispatch_framed(FrameRequest)`
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
`writer_bytes_written` at dispatch, others after the write. Fix: `writer_thread<S: OutputSink>`
as the only loop with an `on_idle()` hook; a source-scan test that only
`pipeline.rs` calls `pop_ready()` under `src/write/`.

Reported by: write path.

## HYG-017 - `copy_file_range` and pread loops are written five times with three EINTR rules

Anger: 8 - I/O fallbacks, gating, alignment.

`copy_range::copy_range` (implicit out offset), `parallel_writer::copy_range_to_fd`
(explicit), fallbacks `copy_range_fallback` and `copy_range_fallback_pwrite`,
and `uring_writer::handle_copy_range_uring`'s own pread loop. Diverged:
`copy_range_fallback_pwrite` retries EINTR on pread, `copy_range_fallback` and
the uring loop do not, neither `copy_file_range` loop does, and
`DirectWriter::flush_buf` retries on write citing the pwrite fallback as
precedent; copy_range loops on short pread, uring treats any short pread as a
hard error; both fall back only on `EXDEV`, so `EINVAL`, `EOPNOTSUPP` and
`ENOSYS` (FUSE, some NFS, older kernels) are hard errors; `copy_range` does
`len as usize`, `copy_range_to_fd` uses `try_from(..).unwrap_or(usize::MAX)`.
Fallback to userspace on `EXDEV` (a planet-scale performance cliff) is silent
and uncounted. Fix: one module owning the syscall, retry rule and fallback set;
`disallowed-methods` for `libc::pread`, `libc::copy_file_range`, `libc::write`
elsewhere.

Reported by: write path.

## HYG-018 - Output backend choice is two bools, decided in four functions and re-derived by commands

Anger: 9 - output backend enum: API, plus copy-range behaviour.

`commands/mod.rs::writer_from_header_bytes` and
`writer_from_header_bytes_parallel` are the same 30 lines except the default
branch, each with its own copy of the "--io-uring requires the linux-io-uring
feature" / "--direct-io requires..." strings; `writer_from_header` and
`_parallel` wrap them again. Backend is `(direct_io: bool, io_uring: bool)`, so
`true, true` is representable and silently means uring. Commands re-derive
capability: `sort` computes
`use_copy_range = io_uring || (!direct_io && cfg!(feature = "linux-direct-io"))`,
`apply_changes/rewrite.rs` computes `io_uring || !direct_io` without the
`cfg!`, which has diverged. The sort comment justifying it ("io_uring uses
linked ReadFixed+WriteFixed for CopyRange") is stale; the uring module doc
records that chain was removed for producing mid-file zero gaps. The parallel
backend cannot be chosen by an operator; it is picked per command. In
apply-changes, `ScannerChannels` holds `drain_tx: Option<_>` and
`use_copy_range: bool`, two values that must agree, which is why "drain:
received CopyRange item but use_copy_range is false" and the non-feature
`flush_copy_range` stub exist. Fix: `enum OutputBackend { Buffered, Parallel,
Direct, Uring }` parsed once with conflicts refused, `PbfWriter::open(path,
backend, compression, header)`, `PbfWriter::supports_copy_range()` as the only
capability query, and a `Route::Splice { drain_tx } | Route::Owned` enum in
apply-changes.

Reported by: write path, change application, re-encoding, CLI and plumbing.

## HYG-020 - Library error text names CLI flags and commands

Anger: 9 - error kinds and texts: API and output.

`require_indexdata` tells the user to run `pbfhogg cat input.osm.pbf -o
indexed.osm.pbf` and to pass `--force`; `require_sorted*` says `pbfhogg sort`;
`verify_ids` repeats the cat recipe; renumber suggests `pbfhogg cat --clean`
and its schedule scan says "run `pbfhogg cat` to add indexdata"; the writer
constructors say `--io-uring requires ...`; `inspect`'s scan says
`--direct-io not used...`; `repack` says "--elements-per-blob must be > 0" and
`BlockBuilder::with_element_cap_prealloc` asserts the same thing with the same
flag name; framing errors and the WayMembers message name
`--elements-per-blob`, which altw, apply-changes and extract never had;
degrade names `--block-cap` and `--unsort`. `error.rs` explicitly refuses to
name CLI flags ("made the library a dependency of the CLI's argument surface")
and `export_error` moved one hint to the CLI; the rule is applied in one place
and broken in about ten. Fix: typed errors (`MissingIndexdata { path }`,
`Unsorted { path, role }`, `FeatureNotCompiled(&'static str)`,
`BlobTooLarge { field, size, limit }`) with advice mapped in the CLI;
`NonZeroUsize` for caps removes the second validation. Enforce with a text rule
rejecting `pbfhogg ` and `--` flag names in string literals under `src/`, tests
exempt. The alternative is to accept CLI-first wording and say so in
`LIBRARY.md`.

Reported by: CLI and plumbing, write path, re-encoding.

## HYG-021 - Error kinds are stringified away at thread boundaries and reported as I/O

Anger: 9 - error kinds and texts: API and output.

- `streaming::io_err`, `drain::io_err` ("Mirrors the io_err helper in
  streaming.rs") and about eight inline
  `new_error(ErrorKind::Io(std::io::Error::other(e.to_string())))` in the
  apply-changes drain: decompress failures, protobuf parse failures, the
  overlay's typed errors, protocol bugs ("barrier ran twice?", "coord_slots is
  None", "scanner: fast-path descriptor missing indexdata") and the
  unsorted-output refusal all reach the caller as `Io`.
- Merge-changes maps every error to `String` across the rayon boundary
  (`do_work`, `build_simplify_stream`, per-chunk closures) because
  `BoxResult` is not `Send`.
- Altw: stage 4 wraps router aborts, reframe and assembly failures as
  `Io(Other)`; sparse `SharedSparseWriter::insert` does the same for an
  out-of-domain coordinate (input data, not I/O); `.map_err(|e| e.to_string())`
  appears more than ten times, with `external_join` explaining the `Send`
  problem.
- Renumber worker results are `Result<_, String>`.
- `extract::pread_execute`'s `block_fn` and `getparents`' `ClassifyResult` are
  `Result<_, String>`, re-wrapped as `Io(Other)`; `ExportOptions::new` wraps
  tag expression and bbox parse errors as `io::Error(InvalidInput)`, so user
  input errors are classed as I/O for exit-code mapping.
- Read path: "decode task panicked", "failed to build decode pool", the fused
  transform's `String` error, "pipeline consumer dropped", "pipeline thread
  terminated without completing", "IndexedReader requires a seekable
  BlobReader" are internal faults reported as `Io(Other)`. Each site spells the
  full `new_error(ErrorKind::Io(std::io::Error::other(...)))` with its own
  wording and `io::ErrorKind` (`pipeline.rs` x5, `reader.rs` x3, `indexed.rs`,
  `header_walker.rs` x5); that is how the walker once lost the open error's
`io::ErrorKind`.
- The geocode builder and reader use `Result<T, Box<dyn Error>>` from
  `format!(...).into()` throughout and never use `error.rs`; pass 1.5 and 2b
  stringify worker errors.

A caller matching on `ErrorKind::Io` to decide "retry or bad disk" misfires,
and the `source()` chain is gone. Fix: make the crate error `Send + Sync`, so
thread closures return `crate::Result` and the `String` round trips go; add
`ErrorKind::Internal` and a `PipelineError { seq, frame_start, stage, source }`;
a small set of constructors in `error.rs`. Enforce by deleting both `io_err`
helpers and a `disallowed-methods` / text rule on `std::io::Error::other`
outside `error.rs`.

Reported by: change application, altw and scanning, re-encoding, filtering and
spatial, read path, geocode index.

## HYG-022 - Errors that reach the operator name no file, offset or subject

Anger: 7 - message content.

- No write-path error names the output file: `check_stream_complete` names a
  backend but not the path; ENOSPC reaches the operator as "No space left on
  device"; `parallel_writer` attaches `create {path}` to the copy on `init_tx`
  but returns the bare `e` from the thread, so init and join results disagree.
  `multi-extract` is the one caller that wraps with the path, by hand.
- Open and create failures: `FileReader::open`, `File::create(out.path())` in
  `run_export`, `canonicalize` in the alias check (which also aborts on a
  permission error that has nothing to do with aliasing), `parse_ids_from_file`,
  `parse_geojson` and `parse_extract_config` (a config with 30 polygon files
  says "expected value at line 1 column 1"; `read_expressions_file` does name
  its path), diff and derive scratch creates and `append_and_cleanup` (a bare
  "Permission denied" for a scratch file the user did not choose), and in
  geocode `run_stage_b`, `write_admin_data`, every `File::create(...)?`, and
  `Reader::open`'s header read (the other 18 opens add the path; the header, the
  most likely missing one, does not; "geocode header too short" likewise).
- Pread and decode errors: altw `pass A pread: {e}`, `stage2 node pread: {e}`,
  `relation scan pread: {e}`, `router pread worker {id} blob {idx}`, `open id
  shard`, `read id shard`, `stat id shard`, `open slot bucket` (though
  `prepare_bucket` holds the path), `write id shard W{worker_id}` (no bucket),
  the external metadata scan's "OsmData blob missing indexdata" (no offset);
  geocode pass 1.5 "pread at {offset}" (no path) and decompression errors (no
  offset); renumber stage 2d and R2d errors (pass 1 does name `task.seq`);
  dedupe's "failed to scan block IDs"; the read-path decode-panic error, which
  holds `blob.offset()` and the path and names neither, on a 2.5 M-blob
  planet; geocode reader validation errors (no record index or directory).
- `main` prints `Error: {e}` without the `source()` chain.

Fix: the writer owns its `PathBuf` and wraps every escaping error once
(`WriteError { path, backend, source }`); `open_input(path)` /
`create_output(path)` wrappers; workers wrap at the boundary with
`BlobError { path, seq, offset, kind }`; `main` prints the whole chain. Enforce
with `disallowed-methods` on `File::open`, `File::create` and
`fs::read_to_string` in `src/` outside the wrappers, and an ENOSPC or forced
error test asserting the path appears.

Reported by: write path, CLI and plumbing, altw and scanning, geocode index,
re-encoding, filtering and spatial, read path.

## HYG-024 - Counter declaration and emission is hand-written boilerplate

Anger: 8 - counter machinery and semantics.

Altw stages each declare 10 to 35 `AtomicU64` locals, a `_ref` binding for
each, and an emit block repeating `emit_counter("x", x.load(Relaxed) as i64)`
under `#[allow(clippy::cast_possible_wrap)]`, about 300 lines; the
compare-exchange "track the max" loop is open-coded at least five times while
`AtomicU64::fetch_max` exists and stage 4 already uses it once;
`passthrough.rs` has a `Pass2Counters` struct and stages 1 to 4 do not.
`write/metrics.rs` spells its field list three times (struct, `new`, `emit`)
with hand-kept name strings; its `include_str!` funnel tests scan six files but
not `framing.rs` (which writes `compress_ns`, `frame_ns`, `bytes_framed`,
`tagdata_dropped_oversize`) or `parallel_gzip.rs`, and cover only the
`sync_all` pair and `recv_wait_ns`, while `write_ns` and `bytes_written` are
hand-rolled at about ten sites. `elapsed_ns_u64` exists three times
(`write/pipeline.rs`, `uring_writer.rs`, `read/pipeline_metrics.rs`) plus
`apply_changes/streaming.rs::elapsed_ns` and two open-coded
`u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)`. Fix: a derive or
macro generating struct, `new` and `emit` from one list; `fetch_max`
everywhere; a `Stopwatch` whose only exit is `record_into(&AtomicU64)`; a test
that fails when a `.rs` file under `src/write/` is missing from the funnel's
`SOURCES`.

Reported by: altw and scanning, write path.

## HYG-028 - Where scratch goes is decided per command, with no override

Anger: 9 - scratch location, a new --tmp-dir, reaping old scratch.

- Altw sparse (`referenced * 8`, 29 GB at europe) and external (hundreds of GB
  at planet) put scratch in `output.parent()`, with no `--tmp-dir`, `TMPDIR`
  or library option; a small or slow output volume leaves the user no choice.
- `diff -j` puts shard files next to the old input
  (`old_path.parent().unwrap_or(".")`), claiming "the same assumption the
  `--format osc` sibling driver makes", which it is not. Inputs often live in
  a read-only or shared dataset directory (brokkr's data dir, a mirror), so
  this fails with an unattributed error or drops multi-GB `.txt.tmp` files into
  another tool's directory that `brokkr clean` does not know about.
- `derive-changes` puts temp files next to the output ("same as brokkr
  scratch").
- The geocode builder creates `.buckets-level{N}` inside `output_dir`, the
  user's index directory (possibly live, BUG-048), and `remove_dir_all`s any
  pre-existing directory of that name without checking it is its own.

Fix: one `scratch_dir` resolved once by the CLI (explicit `--tmp-dir`, else the
output's directory with `""` mapped to `.`), validated up front, passed into
the library as a required `&ScratchDir` whose only constructor is in the CLI;
`BuildConfig.scratch_dir` for geocode. If the index file set were enumerated
(it is not; `format.rs` records why), geocode `validate_structure` could
reject unexpected entries in the index dir.

Reported by: altw and scanning, re-encoding, geocode index.

## HYG-029 - Scratch naming and cleanup use a different mechanism at every site, and nothing reaps crashed runs

Anger: 9 - scratch location, a new --tmp-dir, reaping old scratch.

Naming: `diff-par-{pid}{run}-{kind}-{shard}.txt.tmp`;
`derive-{creates,modifies,deletes}-{pid}{run}.xml.tmp`, where `{run}` is the
per-run token suffix (`scratch::RunToken`, empty for a process's first run), so
runs inside one process no longer collide and what remains here is the
cross-process side; derive-parallel shards add
a process-lifetime nanosecond tag "because PID alone collides across
containers", while derive-parallel's outer files in the same function omit it;
`process_scratch_tag` derives the tag from the low 32 bits of `SystemTime`
nanoseconds, so two processes in one tick or on a coarse clock collide; altw's
`format!("id-W{worker_id}-{bucket_idx:03}")` is spelled in `stage1.rs`
(create), `stage2.rs` (open) and `external/mod.rs` (cleanup) while
`ScratchDir::bucket_path` exists for `slot-NNN`, and if one copy changes stage 2
skips missing shards (`NotFound => continue`), surfacing as missing locations.
`std::process::id()` plus the run token is the identity in `ScratchDir` and the
sparse store, and the pid plus a counter in `OutputFile`; none of them guards
against a pid reused by a later process.

Cleanup: sequential derive fragments now use `PathGuard`, including partial
sink construction and unwinding. Derive-parallel outer files use `PathGuard` (citing
`decisions/0003`), then `commit()` and `remove_file` anyway; shard files are
deleted by a sweep that recomputes every possible path on error and by
`append_and_cleanup`, which since the output-write-failure fix (parallel diff
and `diff --format osc`) removes its temp file and every remaining shard's even
when the write to the output fails, but still has no guard over a panic
mid-append; altw deletes id shards and slot buckets mid-run with
`drop(std::fs::remove_file(...))`, so a failed cleanup leaves 80+ GB while
stages 3 and 4 allocate more, unlogged; `ScratchDir::drop` ignores
`remove_dir_all` failure ("user can clean") without saying where; altw has a
`DestinationGuard` as a third output-guard idea beside `OutputFile` and
`PathGuard`.

Stale scratch: a crashed external run leaves `.pbfhogg-external-join-<pid>/`
(hundreds of GB) in the output directory forever; sparse's "removed stale
sparse index temp file" check looks only for its own pid's name, so it almost
never finds anything; a SIGKILLed geocode build at levels 18/15 followed by one
at 17/14 leaves `.buckets-level18` and `.buckets-level15` (tens of GB) because
the sweep only removes the current levels; `OutputFile`'s
`.pbfhogg-tmp-<pid>-<n>.<name>` survives a SIGKILL or OOM kill as a hidden
multi-GB file.

Fix: one per-run scratch directory created with `O_EXCL` / a random name,
everything inside it, one `remove_dir_all` guard on `Drop`; this removes the
need for tags. At startup, report (one hunter says report, another says reap)
`.pbfhogg-*` entries whose pid is dead. Enforce with `disallowed-methods` on
`std::fs::File::create` inside `src/commands/diff/` pointing at the helper, and
a `ScratchDir::id_shard_path` that owns the format.

Reported by: re-encoding, altw and scanning, geocode index, CLI and plumbing.

## HYG-030 - Output publication policy is implemented four ways

Anger: 9 - output .partial and rename for every command.

- Every write backend creates the final path up front (`File::create` /
  `O_TRUNC`) and none removes, renames into place or marks it on failure.
  `pipeline::writer_thread`'s own comment says a short file "ends on a blob
  boundary and reads as a valid shorter PBF"; `DirectWriter::Drop` pads and
  `set_len`s it into shape; uring deliberately does not `set_len`; parallel
  leaves holes up to the highest written offset. A caller that ignores exit
  status, or a killed run, leaves a plausible truncated planet.
- `cli/src/main.rs::guarded_output` claims "Every user-named `--output` goes
  through here". `run_export` hand-rolls the `OutputFile` dance and repeats
  the alias check with a different message; `run_extract_config` builds a guard
  vector with preflight; `run_diff` calls `File::create(path)` directly;
  `build-geocode-index` writes
  the index directory in place, so a failed build leaves a partial index under
  the user's name, though `PathGuard::dir` exists.
- Merge-changes builds its own `OutputFile`-or-`PathGuard` choice from
  `output.exists()` (a TOCTOU with no consequence today); apply-changes'
  library entry refuses an output that is the base file, through
  `path_guard::same_file`, the one owner of file identity, but has no
  publication guard; `tags_filter/osc.rs::write_filtered_osc`
  and `multi.rs` `File::create` whatever they are given, so library callers get
  truncate-then-fail.
- Altw's `DestinationGuard` cites the merge-changes precedent.
- `OutputFile::commit` replacing an existing file is silent, and because it
  renames over the destination, a user `--output` that is a symlink has the
  link replaced by a regular file (the geocode builder now refuses a
  symlinked output directory up front; no `OutputFile` user does).

Fix: one policy in `PbfWriter` (write `<path>.partial` in the same directory,
rename after `flush` succeeds, unlink on `Drop`) and an `Outputs` type owning
file, multi-file and directory publication that every `run_*` and library
entry takes instead of a `&Path`. Enforce with `disallowed-methods` on
`std::fs::File::create` under `src/commands/` (writer layer exempt) and a
forced-error test asserting the destination does not exist.

Reported by: write path, change application, filtering and spatial, CLI and
plumbing, altw and scanning.

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

## HYG-035 - Configuration is validated at the moment of use

Anger: 9 - validate flags earlier, refuse no-op flags.

- `--io-uring` / `--direct-io` without the feature are refused in
  `writer_from_header_bytes*`, after the input is opened, headers read and
  sometimes a full first pass done; on altw or apply-changes the refusal can
  come minutes in.
- Apply-changes refuses `-j 1` after parsing the whole OSC, building
  `DiffRanges` and creating the output writer; the `--force
  --locations-on-ways` refusal is up front, as it should be. `MergeOptions`
  wants a `validate()` called first.
- Altw refuses `inject_prepass && !indexdata_present` inside
  `write_output_checked`, after pass 0 and pass 1 have built the whole sparse
  store.
- Zstd compressor construction happens on the first blob inside a rayon
  closure, after the header is written and the output created, surfacing via
  `WriterGone` lag; `copy_file_range` incompatibility with `--direct-io` is
  discovered when the first `CopyRange` chunk reaches the sink.
- `diff -j` scratch writability is discovered after both header walks;
  derive-parallel creates its outer files after `walk_file` on both inputs.
- `--output-header` and `--generator` are parsed per dispatch arm
  (`HeaderOverrides::parse` in fourteen arms); `--drop-ids`, `--start-id`,
  time-filter's timestamp, `--bbox` and `--show` are parsed in `run_*` or the
  library. The `main.rs` comment "Every free-form flag is parsed exactly once,
  here at the argument boundary" is not true for these. `tags-filter` with no
  expressions and `getid` with no IDs are refused after the fact.
- The external altw free-space need is never checked (HYG-057).

Fix: `value_parser`s for every free-form flag; feature-availability checks at
argument time; a `validate()` per options struct; an altw `AltwPlan::resolve`
that opens the input once, validates every combination and the scratch
filesystem, and chooses the backend. Enforce with a test per flag that the
refusal happens with no input file present.

Reported by: CLI and plumbing, change application, altw and scanning, write
path, re-encoding.

## HYG-036 - "This flag has no effect here" is refused, warned or ignored depending on the command

Anger: 9 - validate flags earlier, refuse no-op flags.

Refused: `-j` with `-R`, `--distrust-metadata` outside the scanning report,
`-v` with `-j` on diff, `inspect -j` / `--force` outside `--nodes`. Warned
(a `Warning:` line from `run_*`): `tags-filter -R -t`, `getid -t` without `-r`, and
`--remove-tags` without `--add-referenced` (which the library accepts
silently). Ignored: `--direct-io` on `check`, `getid -I` and many pipeline
passes (HYG-037), `--jobs` with `-R` in the tags-filter library. Validation is
split between clap attributes and about sixteen hand-written
`if x { return Err(...) }` lines in `main`, which exit 2 with a different
message format from clap's. Fourteen dispatch arms build `WriteOpts` and
`HeaderOverrides` with the same four lines; the library functions (`cat::cat`,
`repack::repack`, `degrade::degrade`, `getid::getid`, `extract::extract`...)
still take `compression, direct_io, io_uring, force` positionally, so
`WriteOpts` is unpacked back into adjacent bools one hop later. The CLI builds
`MergeOptions` field by field twice instead of `..Default::default()`.
`too_many_arguments` is denied workspace-wide and `#[allow]`ed on roughly
fifteen functions in filtering, nine in apply-changes (`worker_loop` has 15
parameters, `handle_candidate` 20), nearly every big altw function and 28 times
in re-encoding; `too_many_lines` likewise. A denied lint with that many local
exemptions does not enforce anything. Fix: one policy (refuse) expressed in
clap (`conflicts_with`, `requires`, `ArgGroup`) or one validation function per
command; consider splitting modal commands (`diff text` / `diff osc`,
`tags-filter pbf` / `osc`, `cat` / `cat dedupe`) so invalid combinations stop
existing; a `#[command(flatten)] WriteArgs` producing a library-level
`WriteOptions` or `RunContext` taken by every command, then remove the
`allow`s and let the lint hold.

Reported by: CLI and plumbing, filtering and spatial, change application, altw
and scanning, re-encoding.

## HYG-037 - `--direct-io` is accepted and ignored on many paths

Anger: 9 - validate flags earlier, refuse no-op flags.

The parameter is taken and discarded in altw `stage1_way_pass(_direct_io)`,
`build_node_index_sparse(_direct_io)`, `collect_way_referenced_node_ids`,
`collect_relation_member_ids` (every pread path, so the flag does nothing to
reads in passes 0 and 1, stage 1 or stage 2), `sort::build_blob_index`
(kept "for signature stability" in a private pre-1.0 function),
`diff::parallel::diff_block_pair_parallel`,
`getid::parse_ids_from_pbf(_direct_io)` (the CLI threads `write.direct_io`
into it), and `check_refs` (`let _ = direct_io;`). Extract's pread passes open
buffered regardless (a comment explains alignment), `multi.rs` writes through
`PbfWriter::new(BufWriter)` regardless, `export` has no I/O flag. `inspect`
prints that it ignored the flag. An operator who chose O_DIRECT to protect the
page cache still fills it, with nothing logged. Fix: remove the parameters, and
have the CLI refuse or say so where a command honours the flag on some passes
only, so `--direct-io` help can say what it affects.

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

## HYG-042 - `IdSet`'s rank index is a runtime state rather than a type

Anger: 8 - IdSet typestate.

`rank`, `resolve` and `total_count` `expect` a built rank index, while
`rank_if_set` returns `None` when it is absent, which sparse altw reads as
"node missing"; forgetting `build_rank_index()` shows up as every coordinate
missing and the missing-locations refusal, not a panic naming the cause.
Renumber's shared `IdSet` written with `set_atomic` is correct for reads only
because `build_rank_index` runs after the scope joins, which code order
guarantees and the type does not; stage 2d's per-worker merge relies on
`remove(0)` on a vec a comment says is non-empty. The `rank` doc comments
disagree with each other ("Max 7 words scanned" vs "max 31 words"). Fix: an
`IdSetBuilder` (atomic, no rank) frozen into a `RankedIdSet` via
`into_ranked()`, so a rank query on an unranked set cannot compile and
`drop_rank_index` and the `Option` fields go away.

Reported by: altw and scanning, re-encoding.

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
filtering relies on indexdata `kind` being honest, and stage 2's merge walk
assumes node blobs are sorted and non-overlapping, checking monotonicity only
under `debug_assertions`, so an unsorted file flagged sorted produces a
misleading "missing locations" error instead of "input not sorted" (stage 1
could check `prev.max_id < next.min_id` in `build_node_blob_mapping` for free);
the geocode builder trusts indexdata kind to schedule node blobs, so a node
in a blob labelled way or relation is never seen by Pass 2a. `CORRECTNESS.md`
partly states this for sort. Fix: state the trust model once in `reference/`
(indexdata is a hint; these properties are verified, these are believed, and
this is the consequence) and have each command cite it.

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

Reported by: test harness, read path, write path, altw and scanning, geocode
index, change application, CLI and plumbing.

## HYG-070 - CLI-driving tests live in the library crate, so binary resolution is restated and guessed

Anger: 8 - moves every CLI test.

`tests/common/cli.rs::pbfhogg_bin()` resolves `BROKKR_TEST_BIN_DIR`, else
`CARGO_TARGET_DIR`, else `CARGO_MANIFEST_DIR/target`, plus a
`cfg!(debug_assertions)` profile guess its own doc admits can pick the wrong
subdir, and joins `"pbfhogg"` with no `EXE_SUFFIX`. The module doc at the top of
the same file states a different rule that never mentions
`BROKKR_TEST_BIN_DIR`, so the two copies disagree. `BROKKR_TEST_BIN_DIR=""`
resolves to `./pbfhogg` relative to the test's cwd and then asserts with a
message blaming the sweep config. `cli/tests/cli.rs` uses
`env!("CARGO_BIN_EXE_pbfhogg")`, cargo's own and always correct, but only
inside the `pbfhogg-cli` package (its tests now run under the `cli` sweep); that
is the reason for the forced duplication. Fix: move every `cli_*.rs` into `cli/tests/`, where cargo builds
the binary with the test's features and hands its path, which removes
`BROKKR_TEST_BIN_DIR`, the profile heuristic, the `build_packages`
stale-binary class and the `minimal` sweep's cfg gates on CLI tests;
`tests/common` becomes a shared path or dev-only
crate. If the split stays, keep the rule in one place and treat empty as
unset.

Reported by: test harness, CLI and plumbing.

## HYG-077 - The enforcement levers most of these findings need are unused

Anger: 8 - clippy.toml with expect_used denied means hundreds of edits.

There is no `clippy.toml`, so `disallowed-methods`, `disallowed-types` and
`disallowed-macros` are unused; `expect_used`, `print_stderr`, `print_stdout`,
`arithmetic_side_effects`, `panic`, `unreachable` and `exit` are not enabled;
`allow_attributes_without_reason` is not enabled, so `#[allow(dead_code)]` and
the many `too_many_*` exemptions (HYG-036) need no justification; the existing
`include_str!` source-scan tests in `write/metrics.rs` are the in-repo template
for text rules. Every hunter independently pointed most of its "enforceable
by" answers at one of these. One related coupling: those `metrics.rs` tests
`include_str!("../../cli/src/main.rs")` from the library crate, which breaks
when the library is built alone from a published tarball.

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
decides sortedness. Fix: open once and hand the `File` and parsed header down
(the altw `AltwPlan` of HYG-035).

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

## HYG-109 - No general `--quiet`

Anger: 7 - CLI behaviour.

Only `diff -q` and `check --quiet` exist. Library diagnostics are kinded,
keyed events whose text carries no prefix, and `StderrSink::event` in
`cli/src/host.rs` is the one place they become text, so a global `--quiet`
is a kind filter there and `--json` a second sink serializing kind, key and
arguments; neither needs library changes. Not yet events: the CLI's own raw
`cli_eprintln!` lines (getid's requested and missing ID lists, the
`elapsed_ms=` and `nodes=` key=value blocks, `Multi-extract: ...`,
`distinct tag values`, the final `Error:` line), which a `--quiet` or
`--json` covering them must route through the sink first; the key=value
blocks may be parsed by brokkr. Which kinds `--quiet` drops (Note and
Progress, presumably; never Warning) and how it interacts with the two
existing flags is the maintainer's call.

Reported by: split out of the diagnostic-wording item.

