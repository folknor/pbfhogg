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
```

Each entry carries an `Anger:` rating from 0 to 10: how likely resolving it
is to make the maintainer angry. 10 is rebuilds or contract changes on
measured paths; 9 is hot-path or contract changes that need benchmarks or
the maintainer's decision; 8 is altw, renumber and apply-changes internals
and observability contracts; 7 is geocode output and format, and CLI
behaviour; below that the work is progressively more contained.

---

## HYG-001 - Worker-count policy is spelled at about twenty sites and the copies have diverged

Anger: 10 - worker counts for every command.

The rule "how many threads by default" is reimplemented wherever a pool is
built:

- `read/pipeline.rs` `run_pipeline` and `run_pipeline_fused`:
  `available_parallelism() - 2`, min 1, fallback 4.
- `read/reader.rs` `par_map_reduce`: `- 1`, min 1, fallback 3.
- `scan/classify.rs::resolve_thread_count` (documented as the owner):
  `- 2`, fallback 4.
- Hand copies of the same formula: `extract/common.rs`, `extract/simple.rs`
  (twice), `extract/multi.rs` (twice), `tags_filter/mod.rs`,
  `geocode_index/builder/pass1_5.rs`, `altw/passthrough.rs`,
  `altw/external/{mod,stage1,stage2,stage3,stage4}.rs` (uncapped, `.min(6)`,
  `.min(4)`, `.min(4)`), `renumber/relations.rs` (capped at 6),
  `apply_changes/rewrite.rs` (`- 2`, min 2, fallback 4, `Some(1)` refused),
  `merge_changes/mod.rs` (no reserve, fallback 8), `diff/derive.rs` gzip
  workers (no reserve, fallback 4).
- Fixed counts: `renumber::PASS1_WORKERS = 4`, `STAGE2D_WORKERS = 6` (its
  comment notes `>= 1` is load-bearing because of `remove(0)`); neither is
  reachable from `-j`.
- Rayon's global pool (all cores) for sort, repack, degrade, `cat --clean`,
  geocode pass 3 and admin assembly.
- The CLI `-j/--jobs 0` resolution: `run_diff` and `run_derive_changes`
  resolve `0` to `available_parallelism().unwrap_or(1)` (two copies, fallback
  1); `tags-filter`, `apply-changes`, `merge-changes` map `0` to `None` (three
  copies) and the library picks; `inspect --nodes` and `inspect tags` pass the
  raw `usize` and `tags_count` does `(jobs > 0).then_some(jobs)`. The library
  `jobs` type is `usize` in some options structs and `Option<usize>` in
  others.

Divergence is fact: reserve 2 vs reserve 1 vs none, fallback 4 vs 3 vs 8 vs 1.
One `tags-filter` run uses two rules in two passes and `--jobs` reaches only
one. On a host where `available_parallelism` fails, commands disagree by 4x.
"Auto" is nproc or nproc-2 depending on the command. A cgroup-limited
container reporting host cores gets more threads than it can run, undetected.
Merge-changes `write_simplified` chunks by `div_ceil(worker_count)`, and chunk
boundaries become extra `<create>` sections, so its output bytes differ
between hosts with different core counts; byte-comparing tests flake across
machines. Tests cannot force single- or many-worker interleavings anywhere.

Fix: a `Jobs` type (`Auto | Fixed(NonZeroUsize)`) parsed by clap and carried in
every options struct, and one `resolve_workers(Jobs, reserve)` in the library
that owns the nproc read and the fallback; `NonZeroUsize` also deletes the
`STAGE2D_WORKERS >= 1` comment. Enforce with `clippy.toml`
`disallowed-methods = ["std::thread::available_parallelism"]` and one allow at
the owner. Every hunter proposed the same lint independently.

Reported by: all nine scopes.

## HYG-002 - No scope has an answer to "what are the tunables"

Anger: 10 - tunables, including POOL_SIZE, which was measured at 16.

Every scope's knobs live wherever they were first needed, mostly private, with
no injection point:

- Read: `DEFAULT_READ_AHEAD`, `DEFAULT_DECODE_AHEAD`, `BLOCK_QUEUE`,
  `PAR_INFLIGHT_BUDGET`, `PAR_BATCH_MAX_BLOBS`, `PAR_BATCH_MAX_BYTES`,
  `MAX_RETAINED_CAPACITY` and `MAX_POOL_SIZE` (decompress), `HEADER_PROBE_SIZE`,
  `SAMPLE_CAP`, `FULL_SCAN_ARM_MIN_BLOBS`, `DEFAULT_MAX_WINDOW`,
  `GRID_MAX_INDEX_BYTES`, across six files. `ElementReader` exposes
  `read_ahead`, `decode_ahead`, `decode_threads`; `par_fold_blobs` takes the
  budget and batch sizes as parameters but the public method passes the
  constants, and `BLOCK_QUEUE` for `into_blocks_pipelined` is fixed.
- Write: `PIPELINE_DISPATCH_PERMITS`, `WRITE_AHEAD`,
  `parallel_writer::POOL_SIZE` (16, "could become a CLI flag later"),
  `PER_WORKER_CAPACITY`, `uring_writer::NUM_BUFS` and `BUF_SIZE`,
  `direct_writer::BUF_CAPACITY`, `parallel_gzip::DEFAULT_CHUNK_SIZE`,
  `QUEUE_SLOTS_PER_WORKER`, the zlib reserve slack. Every parallel-writer test
  spawns 16 OS threads and every uring test pins 16 MB of `RLIMIT_MEMLOCK`.
  The `PER_WORKER_CAPACITY` memory budget doc multiplies two constants from
  two places and nothing checks it. `POOL_SIZE = 16` ignores core count and
  device.
- Apply-changes: `DRAIN_CHANNEL_CAPACITY`, `CANDIDATE_CHANNEL_CAPACITY`, the
  25 ms drain `recv_timeout` (an inline literal), `DESCRIPTOR_OVERHEAD` (two
  files). Merge-changes: the chunking rule and the `inputs.len() <= 1` switch
  between streaming and buffered strategies (different gzip member layouts).
- Altw: `NUM_BUCKETS` (256), `BUCKET_BUF_SIZE`, stage 2 `FLUSH_THRESHOLD`,
  stage 1 `HEADROOM_FDS` (a function-local const), the `1 << 30` RLIMIT clamp
  and 1024 fallback (each twice), `AUTO_STORE_BUDGET_PCT`,
  `AUTO_ANON_HEADROOM_BYTES`, `WILLNEED_WINDOW_BYTES`,
  `WILLNEED_COALESCE_GAP_BYTES`, the worker caps, `BucketLayout`'s width
  ceiling, the `granularity > 1e9` refusal, 8 bytes per slot. Small fixtures
  always run 256 buckets times N workers' shard files, which is why tiny tests
  create thousands of files.
- Re-encoding: `FRAME_BATCH = 32` (twice), `CORRUPT_INDEXDATA_INFLATION`,
  `DEFAULT_BLOCK_CAP`, the worker counts of HYG-001.
- Filtering: the multi-extract 500-extract limit (a literal twice in one line,
  undocumented in `reference/cli-reference.md`), `GRID_REGION_THRESHOLD = 16`
  declared inside a function body, `extract/simple.rs::BATCH_SIZE`, the
  `pool_get_pub(.., data_size * 4)` decompression guess (four times),
  `geo.rs`'s `1e-30` degenerate-segment epsilon (twice in production, again in
  a test that reimplements the formula).
- Geocode: excluded highway classes (`pass2::EXCLUDED_HIGHWAYS`), accepted
  admin levels `2..=10`, per-level vertex multipliers 16/8/4/2/1, hole
  ownership samples 5, interp endpoint tolerance and search scope, bucket
  count, chunk sizes 100k/500k/100k, Stage B window 8, per-segment sample
  budget 2^24. The first group is policy (what counts as a street or an admin
  boundary) and belongs in `BuildConfig` or a `GeocodePolicy`; the second is
  mechanism.
- CLI: clap defaults in `main.rs`, `check`'s ID floors, `debug::PIPE_BUF`.
- Channel depths `sync_channel(16)` / `(32)` and reorder capacities 32 / 64 at
  every hand-rolled pool (HYG-011); stage 4 records why 32 is right, the
  others inherit the number without the reason.

The in-repo precedent for doing it right is `FULL_SCAN_ARM_MIN_BLOBS`: one
owner, injected into `getid` and `getparents` as `min_blobs` so tests drive
both arms. Even that one is half done: `getparents_with_min_blobs` is a
`#[doc(hidden)] pub` "test instrument" that the CLI calls for a hidden
`--full-scan-min-blobs` flag, while other `dispatch_scan_arm` users have no
override.

Fix: per-scope tuning structs (`ReadTuning`, `WriterConfig`, a
`PipelineTuning` on `MergeOptions`, an altw `Tuning`, a `GeocodePolicy`)
defaulted in one place, passed in rather than read as constants, documented in
`reference/`. Enforce structurally (engines take the struct, so a new knob has
nowhere else to live), with const asserts for cross-constant invariants, a test
that renders the defaults against a checked-in snapshot, and a text rule
rejecting `const` items declared inside `fn` bodies under `src/commands/`.

Reported by: all nine scopes.

## HYG-005 - Nothing stops a header feature name being spelled as a literal again

Anger: 0 - constants, output bytes unchanged.

Every header feature name in `src/` and `cli/` now goes through the
`Feature` newtype and `HeaderBlock` constants in `read/block.rs`. The text
rule on quoted feature literals outside `read/block.rs` was not added: the
existing textlint rules use `region = "code"`, which excludes string
literals, so whether brokkr can match strings needs checking first, and
`"DenseNodes"` legitimately appears as a field name in renumber's error
strings and integration-test fixtures, which need an exclusion.

Reported by: write path.

## HYG-007 - "Node, way or relation" has a dozen encodings

Anger: 8 - element kinds, expression semantics.

- Two enums named `ElemKind`: `crate::blob_meta::ElemKind` (apply-changes,
  merge_join) and `crate::osc::xml_parse::ElemKind` (merge-changes, tags-filter
  OSC), same variants, not convertible.
- `getid::ElemKindTag` (public), now also the kind of the shared
  `getid::ElementRef`, whose `FromStr` accepts both `n123` and `node/123`
  forms and is a ready seed for the single `ElementKind` `FromStr`.
- `time_filter::ElementKind` (private), and `TypeFilter` used as an element
  kind in export.
- CLI enums `ShowElementType`, `DefaultType`, `DefaultTypeArg`,
  `ExportTypeArg`, each with a hand-written conversion in `main.rs`.
- `char` 'n'/'w'/'r' in `getid::parse_id_spec` (then `unreachable!()` twice)
  and `tags_filter/osc.rs::matches_any` (with a `_ => false` arm);
  `merge_join::kind_type_char`.
- Bool triples in `tags_filter::element_matches` and `tags_count`.
- Kind to string: `xml_parse::ElemKind::tag`, `drain::kind_name`,
  `merge_join::kind_name`, inline matches in
  `merge_changes::check_version_advance` and `write_change_element`, string
  literals passed as `kind: &str` to `CompactDiffOverlay::push_*` and
  `delete_*`, `compact.rs::arena_append_*`, `osc/write.rs::member_type_str` and
  the `BytesStart::new("node")` writers; reverse direction in
  `xml_parse::kind_for` and `parse_member_type`.
- Kind to ordinal: `drain::kind_ordinal`, `merge_join::kind_order`,
  `ChangeElement::key` (`(0, id)`...), `compact.rs` member-type bytes 0/1/2.

The drain comment says its kind deliberately does not derive `Ord` so the
order cannot be silently reordered; a unit test pinning the order does that
better than three hand-written copies. Fix: one public `ElementKind` with
`as_str`, `ordinal`, `FromStr` and a CLI-side `ValueEnum` newtype; delete
`osc::xml_parse::ElemKind`. The type removes the `unreachable!()`s and
`_ => false` arms.

Reported by: change application, filtering and spatial, CLI and plumbing.

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

## HYG-027 - OSC and apply-changes element writers are duplicated

Anger: 7 - OSC output bytes.

`osc/write.rs` has owned writers (`write_node_xml`, `write_way_xml`,
`write_relation_xml`) and borrowed ones (`write_dense_node_xml`,
`write_borrowed_node_xml`, `write_borrowed_way_xml`,
`write_borrowed_relation_xml`), each spelling the same element shape;
`write_tags_xml` and `write_borrowed_tags_xml` differ only in iterator type;
metadata is emitted by both `OwnedMetadata::push_attrs` (strings) and
`push_info_attrs` (typed). Comments show the "presence, not truthiness" fix was
repeated once already. In apply-changes `element_writes.rs`, `write_osc_way`
and `write_osc_way_local` differ only in the capacity call; `write_osc_relation`
and the inline relation code in `rewrite_block.rs` (twice) are the same;
`stream_output::emit_create_for_output` and `rewrite_block::emit_create_local`
are the same match; node creation is spelled four times and the `(0, 0)`
missing-loc fallback twice. Fix: write each OSC element once over a small trait
both owned and borrowed types implement; a `BlockSink` trait for
`&mut PbfWriter` and `&mut Vec<OwnedBlock>`.

Reported by: change application.

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

## HYG-033 - The CLI's arena cap is process-global and depends on which command ran

Anger: 7 - RSS.

The library no longer calls `mallopt`: `check_refs`, `verify_ids`, `cat`
(`cat_filtered`), `repack`, `degrade` (`degrade_decode_path`), `renumber`,
`time_filter` and the geocode builder call the host's
`ProcessActions::cross_thread_alloc_pool` hook, and only the CLI's hook
(`process::cap_malloc_arenas`, `mallopt(M_ARENA_MAX, 2)`) does anything
(ADR-0016). The glibc calls live in `src/process.rs` and `src/debug.rs`, gated
on `target_os = "linux"` plus `target_env = "gnu"` with a no-op fallback, and a
textlint rule in `brokkr.toml` keeps raw `libc::mallopt` and friends out of
every other file, so musl builds.
What remains: `mallopt` is process-global and never restored, so in the CLI the
setting depends on which command ran (an embedding caller that installs the same
hook keeps two arenas for life). `time_filter`'s comment documents a path that
regresses under it. With jemalloc/mimalloc as the global allocator it silently
does nothing and the documented RSS difference reappears. `brokkr.toml`
captures `MALLOC*` env vars, suggesting `MALLOC_ARENA_MAX` was once the knob.
Fix: allocator tuning belongs to `cli/src/main.rs`, per subcommand if needed,
rather than a hook at command level. A `--target x86_64-unknown-linux-musl`
build in a sweep would pin the musl half, which is currently held only by the
textlint rule.

Reported by: re-encoding, geocode index, CLI and plumbing.

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
(`Warning!` from `run_*`): `tags-filter -R -t`, `getid -t` without `-r`, and
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

## HYG-040 - Unix-only APIs are used without the crate saying it is Unix-only

Anger: 7 - Unix-only decision.

`std::os::unix::fs::FileExt` is imported unconditionally in
`read/header_walker.rs`, `write/parallel_writer.rs` (compiled unconditionally,
`to_path_parallel` is public API), `apply_changes/streaming.rs`, renumber, both
parallel diff modules, extract's `pread_execute` and multi workers, altw
`external/*` and geocode `pass1_5.rs`. Altw also calls `libc::getrlimit` /
`setrlimit` ungated and assumes `rlim_t == u64`. AGENTS.md says `apply_changes`
compiles in a no-features build and the `minimal` sweep checks it, but only on
Linux; on a non-Unix target the whole crate fails to build. `sparse.rs` does
gate `statvfs` / `posix_fallocate` and falls back to `set_len`, which skips the
free-space check, so a full disk on macOS surfaces as SIGBUS on the mmap store.
Fix: decide. Declare the crate Unix-only in `Cargo.toml` and README (or a
`compile_error!`), or put pread behind a small platform shim; if non-Unix is
meant to work, a `cargo check --target x86_64-pc-windows-gnu` sweep.

Reported by: read path, write path, change application, re-encoding, filtering
and spatial, altw and scanning, geocode index.

## HYG-041 - Whether missing indexdata is an error, a warning or nothing is decided per command

Anger: 7 - indexdata policy.

`getid` and `removeid` refuse without indexdata unless `--force`, with two
different hand-written messages; `getparents` calls `has_indexdata` only to
pick an arm; `getid --add-referenced` calls `require_indexdata` and ignores the
result; extract refuses for complete/smart only; renumber's schedule scan
returns its own differently worded error and has no `--force` escape at all;
sort, dedupe, cat (two variants) and repack each write their own "input PBF has
no blob-level indexdata..." sentence (sort and dedupe identical); geocode has
its own `--allow-unindexed` escape, with the legacy `--force` still meaning
both that and `--overwrite`. The AGENTS.md
list of commands requiring indexdata is a hand-maintained copy of these
decisions. `has_indexdata` and `has_tagdata` in `commands/mod.rs` are the same
header walk returning different fields, and both trust the first OsmData blob
only, so `inspect --indexed` says yes for a file whose head is indexed and tail
is not (the doc says so; the CLI help does not). Fix: a `NeedsIndexdata` error
kind owned by `require_indexdata` with a per-command reason enum; a declarative
per-command policy table read by commands and docs; one
`probe_first_data_blob`.

Reported by: filtering and spatial, re-encoding, CLI and plumbing.

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

## HYG-054 - Read-path growth with no guard

Anger: 8 - memory bounds change algorithms and output.

`IndexedReader` keeps a `Vec<BlobInfo>` for the whole file and
`read_ways_and_deps` collects referenced node IDs into a `BTreeSet<i64>`
(documented as potentially billions), with no guard; at planet the failure is
an OOM kill. `BatchQueue`'s `ByteBudget::acquire` admits unconditionally when
`used == 0` and `release` uses `saturating_sub`, so an accounting bug degrades
silently to "no budget"; a `debug_assert!(used >= n)` in `release` would catch
it. `DecompressPool` dropping oversize buffers and refusing returns past
`MAX_POOL_SIZE` is uncounted, so the pool's effectiveness is invisible to the
sidecar.

Reported by: read path.

## HYG-055 - Write-path growth with no guard

Anger: 8 - memory bounds change algorithms and output.

`FrameScratch::take_with_capacity` reserves every replacement buffer to the
high-water mark ever seen on that thread, so one oversize blob pins that
capacity in every rayon thread's scratch for the process lifetime, bounded by
`MAX_BLOB_DATASIZE` times thread count (gigabytes at planet core counts).
`parallel_gzip::writer_loop`'s reorder `BTreeMap` is bounded only indirectly by
channel capacity; a gap `DeathNotice` does not cover would grow it to stream
end (not reachable today).

Reported by: write path.

## HYG-056 - Change-application memory grows with the whole input

Anger: 8 - memory bounds change algorithms and output.

Merge-changes N>1 streaming holds every input's fully re-emitted compressed XML
(`Vec<(Vec<u8>, u64)>`) before writing any; at planet `--osc-range` that is the
whole compressed output. An ordered drain writing chunk i as soon as it and its
predecessors are done caps memory at about `jobs` chunks. `--simplify` holds
the whole `ChangeStream` with owned strings, a `BTreeMap` of survivors and three
grouped `Vec`s; a key-to-position index would cut it to O(objects).
`CompactDiffOverlay` arenas are append-only (a superseding change appends and
repoints, interned strings including every distinct `user` are never freed),
and `heap_size_estimate` counts the garbage as live. `MergeStats::blob_sizes` is
a `Vec<u32>` per blob, cloned in `print_summary` to sort, with `frame_len as u32`
unchecked; a fixed-bucket histogram does the same job. Tags-filter OSC holds the
whole filtered change set in nine `Vec`s before writing; three section spools
would bound it.

Reported by: change application, filtering and spatial.

## HYG-057 - Multi-GB scratch writes have no free-space precheck

Anger: 7 - new refusals.

Sparse altw checks `statvfs` before preallocating; external altw writes hundreds
of GB into the output directory with no precheck and finds ENOSPC whenever stage
1 or 2 hits it, minutes to hours in. Diff and derive scratch is the full output
size, written to shard files and again to the output (derive: shard, outer,
gzip, about 45 GB of XML at planet per its comment), with no check. Fix: check
in the up-front plan (HYG-035) against the scratch filesystem.

Reported by: altw and scanning, re-encoding.

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

## HYG-064 - Page-fault counters still read a failed `/proc` sample as zero

Anger: 7 - CLI behaviour.

The `/proc` readers now live in `src/process.rs` and return `Option`; the
CLI and the debug layer call them. `debug::read_page_faults` still maps a
missing sample to zero for its six altw call sites, which publish the
numbers for brokkr, so one failed sample distorts a delta. Fix: carry the
`Option` through those call sites and skip the counter when absent.

Reported by: CLI and plumbing, altw and scanning.

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

## HYG-072 - Real-data tests hard-code dataset paths that `brokkr.toml` owns, and they have diverged

Anger: 6 - needs brokkr changes.

`data/denmark-20260220-seq4704.osm.pbf` appears in `tests/roundtrip_real.rs`
and `tests/cli_apply_changes.rs`, `...-with-indexdata.osm.pbf` in
`tests/geocode_index.rs`, `data/denmark-20260221-seq4705.osc.gz` in
`cli_apply_changes.rs`. The `dm6` host's denmark is
`denmark-20260415.osm.pbf`, so on dm6 every one of these tests prints
"Skipping: not found" and passes. Override variables exist for two of four
sites under two names (`PBFHOGG_TEST_PBF`, `PBFHOGG_TEST_PBF_INDEXED`), none
for the OSC. `tests/geocode_index.rs::output_dir()` probes
`data/scratch/geocode-denmark` "built by brokkr run", but every listed host
sets `scratch = "data/bench-tmp"` (only unknown hosts fall back to
`data/scratch`), so the probe never hits and the test always rebuilds. Tests
assume `CARGO_MANIFEST_DIR/data` is the data root, while `brokkr.toml` allows a
per-host `data =`. `capture_env = ["PBFHOGG*", ...]` captures variables no
production code reads; only these two test overrides read any. Fix: brokkr
resolves datasets and scratch for `brokkr test` / `check` and exports one set
of variables; a missing variable under a profile that names the test is a
failure, not a skip. Enforce with a text rule banning `"data/` literals in
`tests/`.

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

## HYG-079 - The altw inject-prepass pin bit is stolen from latitude

Anger: 8 - altw pin bit.

Stage 2 writes `lat = (tuple.lat << 1) | pin` into `ResolvedEntry` when
`inject_prepass` is set, and stages 3 and 4 decode based on the same flag passed
separately: a second coordinate encoding beside `coord_pack`, whose doc says
nobody re-derives the layout. It is safe (`9e8 << 1` fits in i32), but a stored
latitude's meaning depends on a bool carried through three stages, and stage
3's zero-filled scatter buffer stands for absence (all-zero is Null Island,
unpinned), the convention `coord_pack` was written to retire. Fix: a separate
field (the record has room at 16 bytes, or the high bit of `local_slot_pos`), or
a `coord_pack`-style codec with a type per encoding. Sparse pass 0 with
`inject_prepass` also builds a second full `IdSet` the size of `referenced`,
roughly doubling pass-0 memory, absent from the memory table.

Reported by: altw and scanning.

## HYG-080 - Unsafe shared state whose safety rests on lexical co-location

Anger: 8 - unsafe sparse writer.

`SharedSparseWriter` is a raw pointer with hand-written `Send + Sync`; soundness
rests on the mmap outliving the writer lexically in `build_node_index_sparse`.
Borrow the mmap as `&[AtomicU64]` with a lifetime and drop the `unsafe impl`s.
`BlobReader` stores `evict_fd: Option<RawFd>` copied from `self.reader`; it is
valid because `reader` is never replaced, and a future `set_reader`,
`into_inner` or `mem::replace` would make `posix_fadvise(DONTNEED)` evict some
other file's cache through a reused fd. Make eviction a method on `FileReader`
and store a bool. `UringState.logical_size` is updated by two protocols
(`write()` adds before copying, `handle_copy_range_uring` adds the whole range
up front and may fail midway), safe only because no `set_len` runs on error, an
invariant held by a field comment.

Reported by: altw and scanning, read path, write path.

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

## HYG-091 - Significant fallbacks and events happen with no line or counter

Anger: 8 - new diagnostic lines and wording.

- `diff` and `derive-changes` fall back from parallel to sequential when either
  input lacks indexdata or `--verbose` is set (derive gates on
  `both_indexed && jobs > 1`): a user who passed `-j 16` gets a single-threaded
  run with nothing saying why.
- `try_extract_multi_single_pass` returns `Ok(None)`, and the run falls back to
  N sequential full passes, on unsorted input, a missing header, and any
  `BlobReader` error on the first blob (`_ => return Ok(None)`), which is also
  a swallow: sequential then fails differently, later.
- `RegionGrid::build(..)` returning `None` (over budget) is `.flatten()`ed into
  the linear classifier.
- `sort --force` past `require_indexdata` says nothing.
- A blob filter that skips nothing because the file has no indexdata is silent
  (the common "why is my extract slow" case, detectable at the first OsmData
  blob).
- `tagdata` dropped for oversize headers is counted only under the sidecar, so
  a CLI user never learns `tags-filter` speedups were lost.
- Apply-changes' forced rewrite of a false-positive blob because of inline
  upserts is counted nowhere; merge-changes' N>1 vs N<=1 strategy choice is
  unrecorded.

Fix: route each through the `host::Diagnostics` channel (an event with its own
key) and a counter.

Reported by: re-encoding, filtering and spatial, read path, write path, change
application.

## HYG-095 - Multi-extract keeps 3N ID sets with no estimate or refusal

Anger: 8 - memory bounds change algorithms and output.

`inspect tags` documents its exact-count memory growth, and getid's
exhaustive `--verbose-ids` output is documented as deliberate. Still open:
`multi.rs` keeps `3 * N` `IdSet`s for up to 500 regions with no estimate or
refusal. `parse_geojson`, configs and ID files are read with
`read_to_string` of any size (acceptable for operator inputs, noted for
completeness).

Reported by: CLI and plumbing, filtering and spatial.

## HYG-096 - Altw holds the per-way sidecar in RAM, and the straddler bound is unchecked

Anger: 8 - altw internals.

The geocode builder's `StringPool` no longer stores strings twice, and why
its other intermediates stay RAM-resident is recorded in `builder/mod.rs`.
Left, in altw: `PerWayRcs` and `load_ref_count_sidecar` hold the whole
per-way sidecar in RAM (encoded bytes plus offsets), proportional to way
count and absent from the memory accounting the docs quote; stage 4's
straddler payloads are bounded by the two-piece straddler invariant, which
`slot_bucket_count` arithmetic maintains and nothing checks at run time
(`classify_blobs_in_bucket` errors only after the fact).

Reported by: geocode index, altw and scanning.

## HYG-097 - Geocode query behaviour depends on the cell level rather than distance

Anger: 7 - geocode output.

Interpolation endpoint matching scans the street-level cell and its 8
neighbours: about 450 m at level 17, about 100 m at `--street-level 19`, where
endpoints silently stop resolving; the search should be a radius.
`interpolation_resolvable` walks the whole way per candidate inside the radius
and the winner does it again in `interpolated_number`; resolvability could be
precomputed. The building centroid is a vertex mean including the duplicated
closing vertex, biased toward the first vertex and possibly outside concave
buildings, the issue the admin seed fix addressed. Antimeridian handling was
checked and is consistent across flood fill, reader and `hole_inside_outer`;
recorded so the next pass does not re-check it.

Reported by: geocode index.

## HYG-105 - Geocode `BuildStats` repeats the header counts

Anger: 4 - BuildStats API.

`BuildStats` repeats the header counts; it could be derived from the header
plus `dropped_admin_relations`.

Reported by: geocode index.

## HYG-107 - Library-use survey follow-ups

Anger: 3 - mostly observations.

- `common::write_indexed_pbf` says the sync writer emits no indexdata,
  but `write_primitive_block` now emits it on the sync path too. Fixtures
  requiring its absence must remove it explicitly.
- ltrace cannot observe pbfhogg's calls: BIND_NOW/full RELRO leaves no PLT
  breakpoints. Use strace for syscall-backed calls and LD_PRELOAD for mallopt.
- `WRITER_METRICS` and `PIPELINE_METRICS` mix counters across concurrent
  runs. `process_scratch_tag` is also process-global;
  the per-run scratch tokens make sharing that tag harmless.
- `diff/parallel.rs` puts scratch beside the old input unless
  `Host::scratch_parent` says otherwise. Derive's gzip workers ignore `-j`.
- The panic-hook-swapping reader test can silence concurrent panic output.
- Raising RLIMIT_NOFILE is inherited by children and can break select(2).

The identity instrument blesses the unchanged product build itself: brokkr
owns BROKKR_TEST_BIN_DIR and cannot be redirected to a baseline. Test filters
match substrings, so bless and compare names must not contain each other.

Reported by: library-use implementation survey.

## HYG-108 - Diagnostic wording is inconsistent across commands

Anger: 8 - new diagnostic lines and wording.

The library now reports through keyed `host::Diagnostics` events and the CLI
prints each as one stderr line, byte for byte as before; the wording itself
was deliberately left alone. What remains is a CLI output change:

- Prefixes for one class of event: `Warning:`, `warning:`, `Warning!`,
  `hint:`, `[tags-count]`, `[getid --invert]`, `[inspect]`, `time-filter:`,
  `add-locations-to-ways:`, `[altw external]`, `[coord_payloads]`, bare text.
  The event kind (`Warning`, `Note`, `Progress`, `Developer`) now carries the
  class, so the CLI sink could render the prefix uniformly.
- The apply-changes `-j 1` refusal is ten lines and the version-regression
  errors about five each.
- `warn_locations_on_ways_loss` reports once per input in `cat` with no
  filename; the unrecognised-indexdata warning names no file or offset.
- sort's "verifying intra-blob order" line has no warning prefix though it
  means pass 1 decodes every blob.
- `[uring_writer] error: {e}` and `[parallel_writer] thread exit: {e}` are
  also returned as the error, so the operator sees each twice in different
  words; the CLI prints the geocode counts after the library's own `done`
  line.
- Events that matter to an operator (sort overlaps, repack's cap never
  firing, renumber orphan refs) go out as prose and as counters, spelled
  independently.
- The CLI has no `--quiet` beyond `diff -q` and `check --quiet`; with keyed
  events it could filter by kind.

Reported by: all nine scopes (split out of the library stderr item when the
library stopped writing stderr).

## HYG-068 - Nothing verifies the workspace MSRV

Anger: 5 - toolchain install.

`rust-version` is inherited from `[workspace.package]`, but no sweep checks
the workspace builds on it. `cargo-msrv` is not installed and only a nightly
toolchain is, so a sweep needs the pinned toolchain installed and either
`cargo-msrv` or brokkr support for a toolchain-pinned sweep.

Reported by: CLI and plumbing.

## HYG-069 - Code comments and help text still hard-code drifting figures

Anger: 5 - CLI help text, and the "recommended" question.

The durable docs are reworded and point at `reference/pipeline.md` (the altw
auto rule) and `reference/performance.md` (measurements). Still hard-coded:
the altw `--index-type` help in `cli/src/main.rs` (europe time, host RAM,
8.7 GB, 224 GB, the auto threshold); the `Sparse`, `External` and `Auto` docs
in `altw/mod.rs`; `<1 GB at every stage` and `~256 GB` in
`altw/external/mod.rs`; "19 files" and "~19 GB at planet" in
`geocode_index/builder/mod.rs`; README's claim that every command processes
the planet on a 32 GB machine. The guides and CLI help call `auto`
"recommended" while the default is `sparse`: whether that is intended needs
the user's answer. `docs/.vitepress/dist/` is generated and still says 19
files. The geocode builder's comment on planet-scale limits contradicts the
planet builds on record in `reference/performance.md`.

Reported by: altw and scanning, geocode index, CLI and plumbing, test harness.

## HYG-073 - Test skips outside the converted files still pass silently

Anger: 6 - needs brokkr changes.

`common::skip()` prints `PBFHOGG_TEST_SKIP:` and fails when
`PBFHOGG_TEST_MUST_RUN` is set. Unconverted skip sites remain in
`cli_digest.rs`, `cli_add_locations_to_ways.rs`, `cli_sort.rs`,
`fault_uring_writer.rs`, `roundtrip.rs`, `cli_cat.rs` and
`cli_host_identity.rs`. Brokkr does not yet count the markers or set
`PBFHOGG_TEST_MUST_RUN` for profiles that must run.

Reported by: test harness.

