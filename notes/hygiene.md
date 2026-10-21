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
```

---

## HYG-001 - Worker-count policy is spelled at about twenty sites and the copies have diverged

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

## HYG-003 - The 256 KiB I/O buffer size has about ten owners

`FileWriter::buffered` (`BufWriter::with_capacity(256 * 1024, ..)`),
`direct_writer::BUF_CAPACITY`, `uring_writer::BUF_SIZE`,
`copy_range::copy_range_fallback` and
`parallel_writer::copy_range_fallback_pwrite` (`vec![0u8; 256 * 1024]`), the
`PbfWriter::new` doc that tells callers to use `256 * 1024`,
`read/file_reader.rs`, `read/direct_reader.rs::BUF_CAPACITY` (whose doc says it
"Matches the 256 KB BufReader capacity used elsewhere"), `read/blob.rs`,
`altw/external/radix.rs::BUCKET_BUF_SIZE` plus a private shadowing
`BUCKET_BUF_SIZE` and `FLUSH_THRESHOLD` in `stage2.rs`, and stage 1's
`64 * 1024` / `256 * 1024` sidecar literals. The "matches" claims are comments.
Not yet diverged. Fix: one `IO_BUF_BYTES` beside `PAGE_SIZE`, a const assert
that it is a multiple of `PAGE_SIZE`, and a source-scan test (the `metrics.rs`
pattern) rejecting the literal under `src/write/` and `src/read/`.

Reported by: write path, read path, altw and scanning.

## HYG-004 - The 8000-element block cap has three owners plus prose

`block_builder::MAX_ENTITIES_PER_BLOCK = 8000` (private),
`degrade::DEFAULT_BLOCK_CAP = 8000` (pub), repack's
`#[arg(default_value_t = 8000)]` and "Default 8000" help text, plus "8000" as a
load-bearing number in about ten doc comments (string table, classify, check,
tags_filter memory bounds). Changing the builder leaves repack and degrade on
the old number. Fix: one pub const in `block_builder` read by both CLI defaults
and degrade.

Reported by: write path, re-encoding, CLI and plumbing.

## HYG-005 - Header feature-name strings are spelled as literals beside their constants

`HeaderBlock::LOCATIONS_ON_WAYS` exists in `read/block.rs`, yet
`"LocationsOnWays"` is spelled in `altw/external/stage4.rs`, `altw/mod.rs`,
`altw/passthrough.rs`, `apply_changes/rewrite.rs`, `degrade/mod.rs`,
`repack/mod.rs`, `export/mod.rs`, `error.rs` and `cli/src/main.rs`.
`HeaderBuilder::build` spells `"OsmSchema-V0.6"` and `"DenseNodes"` as byte
literals while using constants for `SORT_TYPE_THEN_ID` and
`HISTORICAL_INFORMATION`; `inspect/report.rs` repeats both required-feature
literals to filter them out. A typo produces a header advertising nothing.
Fix: `HeaderBuilder::optional_feature` takes a `Feature` enum or a newtype with
associated consts; add `OSM_SCHEMA_V06` and `DENSE_NODES`; text rule on the
quoted literals outside `read/block.rs`.

Reported by: write path.

## HYG-006 - Protobuf field numbers are bare integers, spelled once per direction

`encode_blob_body` (Blob fields 1, 2, 3, 7), `encode_blob_header_fields`
(BlobHeader 1-5), `HeaderBuilder::build` (HeaderBlock 1, 4, 5, 16, 17, 32-34,
HeaderBBox 1-4) and the string table spell the schema on the write side;
`read/blob_wire.rs` has its own spelling. The degrade strippers
(`strip_blob_header_fields`) take field numbers from callers as `&[u32]`. Fix:
a shared `wire_schema` module of named consts; `strip_*` taking a typed field
enum. The round-trip proptest partly holds this today.

Reported by: write path.

## HYG-007 - "Node, way or relation" has a dozen encodings

- Two enums named `ElemKind`: `crate::blob_meta::ElemKind` (apply-changes,
  merge_join) and `crate::osc::xml_parse::ElemKind` (merge-changes, tags-filter
  OSC), same variants, not convertible.
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

## HYG-008 - Decimicrodegree conversion has two arithmetic conventions and about thirty inline sites

`commands/spatial.rs` defines a private `const E7: f64 = 1e-7` with a long
explanation that every boundary conversion must match the forward expression
exactly, because `/ 1e7` and `* 1e-7` are not the same value in binary
floating point. `extract/mod.rs` (`contains_decimicro`, twice), `export/mod.rs`
(Node and DenseNode arms), `geo.rs::e7_to_rad` and the geocode builder (a dozen
sites) spell `as f64 * 1e-7` inline. `coord_fmt::from_decimicro`, the module
whose job is to own this, divides by `1e7`; diff text output and the OSC writer
go through it, export goes through `* 1e-7`. At `{:.7}` the two usually print
the same digits, so this is latent. `BboxInt::inside_bbox`'s exactness holds
only while every caller's conversion is bit-identical to `to_degrees`. Fix: one
`e7_to_deg` / `deg_to_e7` pair matching `spatial.rs`'s convention; a newtype
`E7(i32)` with `degrees()` makes the bad spelling unrepresentable; a text rule
in `brokkr check` rejecting `1e-7` / `1e7` outside the owner.

Reported by: filtering and spatial, CLI and plumbing.

## HYG-009 - Coordinate domain validation is written four times

`coord_pack::pack` owns the +/-90 / +/-180 refusal (`LAT_BIAS`, `LON_LIMIT`).
`altw/external/stage2.rs` restates it with literals
(`-900_000_000..=900_000_000`) and its own error text; its comment says the
backends "must agree on refusal", and nothing keeps them in step.
`extract::parse_bbox`, `extract::parse_ring` and
`xml_parse::coord_to_decimicro` are three more, with messages that have
diverged ("min_lat (x) must be within..." vs "polygon latitude (x) must be
within..."); only the bbox variant names the field. Fix: export
`coord_pack::in_domain` and `validate_lat(name, v)` / `validate_lon`; a shared
test feeding an out-of-domain node to both altw backends.

Reported by: altw and scanning, filtering and spatial.

## HYG-010 - The read engine's ordered pipeline exists twice and has drifted

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
- Renumber: `pass1.rs`, `stage2.rs`, `relations.rs`, each with
  `sync_channel(32)`, `ReorderBuffer::with_capacity(64)`, an `AtomicUsize`
  work-stealing index; the hang of BUG-001 is in all three.
- `repack` and `degrade`: each defines `FRAME_BATCH`, `KindPayload`,
  `WorkerOutput`, `PhaseStats`, `run_kind_phase`, `frame_owned`,
  `frame_and_write_batch`; `cat/mod.rs::run_kind_phase` is a third member.
  Degrade's copies have diverged from repack's in the prealloc bound (BUG-006)
  and in the sort-preservation guard (repack drains when
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

`sort/mod.rs` and `cat/dedupe.rs` each define `BlobEntry`, `HasId`,
`build_blob_index`, `type_order`, `detect_overlaps`, `write_passthrough_blob`,
`read_frame_into`, `extract_blob_bytes`, `count_entry`, a sweep merge and
`flush_heap_below`. Divergence: sort reads headers through `HeaderWalker`
(pread plus `fadvise(RANDOM)`, justified by a planet measurement), dedupe
through `FileReader` plus `read_frame_header`; sort has `intra_unsorted`
detection, the counted-vs-claimed split, parallel overlap rewrite and the
`copy_file_range` coalescer, dedupe has none; the kind-boundary run-splitting
fix exists in both as a comment citing the other. BUG-004 and BUG-005 are
missed ports. `HasId::version` maps absent to 0 in dedupe while `derive.rs`
says "absent is not zero". Fix: one module parameterised on input set and
merge policy.

Reported by: re-encoding.

## HYG-013 - Diff and derive-changes share a parallel core in two copies, and "is modified" has three owners

`diff/parallel.rs` and `diff/derive_parallel.rs` each define `BlobDesc`,
`WalkedFile`, `walk_file`, `Shard`, `plan_shards`, `build_shard`,
`pread_decode`, `Side`, `element_merge`, `borrowed_elements_equal`,
`borrowed_nodes_equal`, `borrowed_relations_equal`, `kind_type_char`,
`append_and_cleanup` and the driver. The `borrowed_*_equal` trio is a third
copy of `osc::merge_join`'s private functions (`parallel.rs` carries a TODO to
lift them). The copies have drifted textually (merge_join uses
`iter_tags_equal` for the dense/non-dense cases, the copies use `.eq`), look
equivalent today, but the one predicate deciding what `diff` and
`derive-changes` output has three owners and the sequential path uses a
different one. `plan_shards` comments differ; `emit_merge_stats_counters` is
duplicated between `diff/mod.rs` and `derive.rs`; `extract_delete_info` and
`write_delete_element` between `derive.rs` and `derive_parallel.rs`. Because
the CLI defaults `-j 0` to all cores, the parallel path is the default for
indexed inputs and the sequential paths run only under `--verbose` or `-j 1`.
Fix: make `merge_join`'s equality `pub(crate)` and delete the copies; an
equivalence test asserting byte-identical `-j1` and `-j8` output for diff and
derive catches drift in any copy.

Reported by: re-encoding.

## HYG-014 - Three frame decoders for one frame format

`BlobReader::read_blob_header` (streaming, stamps offsets),
`frame::read_frame_header` (streaming, used by `raw_frame.rs`) and
`HeaderWalker::next_header_inner` (pread). `frame.rs` is called the one owner
of frame policy but owns only the length-prefix guard. Each decoder
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
`writer_bytes_written` at dispatch, others after the write;
`uring_cq_wait_ns` is incremented with exactly the value of
`uring_submit_and_wait_ns` at its only site. Fix: `writer_thread<S: OutputSink>`
as the only loop with an `on_idle()` hook; a source-scan test that only
`pipeline.rs` calls `pop_ready()` under `src/write/`.

Reported by: write path.

## HYG-017 - `copy_file_range` and pread loops are written five times with three EINTR rules

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

## HYG-019 - The library writes operator text to stderr and stdout directly

About 40 library files call `eprintln!`. The stdout half is closed: the
library no longer owns stdout (the `inspect` and `check` report types render
into a caller-supplied `io::Write`, the CLI writes through `cli/src/output.rs`,
and `no-print-macro` / `no-stdout-handle` textlint rules in `brokkr.toml` keep
it that way). Stderr is not: an `eprintln!` panics when stderr's reader goes
away (`pbfhogg sort ... 2>&1 | head` exits 101), which is the same defect class
the stdout rules closed; the library-use audit of stderr output is tracked in bz
as pbfhogg#1. What remains:

- Summaries: `print_summary` on every `*Stats` (`ExtractStats`,
  `TagsFilterStats`, `TagsFilterOscStats`, `GetidStats`, `GetparentsStats`,
  `TimeFilterStats`, `ExportStats`, `RenumberStats`, altw `Stats`, apply-changes
  `MergeStats`). Apply-changes `merge` calls its own `print_summary`;
  merge-changes returns stats and leaves printing to the CLI, so the two
  sibling commands follow opposite conventions.
- Progress: sort's "Pass 1: indexing blobs...", `merge_pbf`, `osc/parse.rs`
  "[1/3] Parsing diff..." and per-file lines from `load_all_diffs`,
  apply-changes "Parsing OSC diff" / "Diff ID ranges", `[multi-extract]`,
  `[getid]`, `[tags-filter]`, `[N/M] Extracting to ...`, about 15 lines from
  `build_geocode_index` (plus a `hotpath` block printing `key=value` lines, a
  third format on the stream; the CLI then prints the geocode counts again).
- Warnings and hints: the `warn_*_loss` helpers (and an inline copy of the
  way-level-metadata warning text in `repack/mod.rs`), repack's "never fired",
  sort's sortedness note, the altw explicit-sparse "hint:" and three
  "auto-selected --index-type ..." lines, `blob_wire.rs`'s
  `warn_unrecognised_indexdata` (once per process via a global `Once`, no file
  or offset, so on multi-input commands the degraded input is unnamed and a
  second bad file is never reported).
- Developer diagnostics on every run: `[coord_payloads] streaming router ... MB
  in worker tmps`, `[altw external] num_workers = ...` (twice), `[altw external]
  RLIMIT_NOFILE...`, `[uring_writer] error: {e}` and `[parallel_writer] thread
  exit: {e}` (which also return the error, so the operator sees it twice in
  different words, with no output path).

A library consumer cannot silence, redirect or capture any of it; the CLI has
no `--quiet` beyond `diff -q` and `check --quiet`. Prefixes for one class of
event: `Warning:`, `warning:`, `Warning!`, `hint:`, `[tags-count]`,
`[getid --invert]`, `[inspect]`, `time-filter:`, `add-locations-to-ways:`,
`[altw external]`, `[coord_payloads]`, bare text. Lines: the apply-changes
"Blobs:" summary is about 150 characters with two always-zero sub-counts; the
`-j 1` refusal is ten lines and the version-regression errors about five each;
`warn_locations_on_ways_loss` prints once per input in `cat` with no filename;
sort's "verifying intra-blob order" line has no warning prefix though it means
pass 1 decodes every blob. Events that matter to an operator (sort overlaps,
repack's cap never firing, renumber orphan refs) go out as prose and as
counters, spelled independently. The CLI already states the principle:
`export_error` says "a Display impl in a library has no idea which front end is
reading it".

Fix: the library returns stats and warnings as data (`warnings: Vec<Warning>`
or a `Reporter` / diagnostics sink in the options) and never prints; the CLI
owns formatting, levels and `--quiet`; developer diagnostics move to the
counter channel, where the same numbers already go. Enforce with
`clippy::print_stderr = "deny"` on the library crate, allowed in `cli/` and the
sink (the stdout counterpart is already a textlint rule). Every hunter named
that lint.

Reported by: all nine scopes.

## HYG-020 - Library error text names CLI flags and commands

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
  `header_walker.rs` x5); that is how BUG-031 happened.
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

## HYG-023 - Thread panic payloads are discarded

`parallel_writer_thread_inner` formats a worker join error with `{e:?}` on a
`Box<dyn Any + Send>`, printing `Any { .. }`; the uring and gzip paths say
"panicked" with no payload; diff and derive shard joins do
`unwrap_or_else(|_| Err(io::Error::other("shard worker panicked")))`; the read
pipeline's "decode task panicked" drops it; `PipelinedBlocks::drop` does
`drop(h.join())`. Fix: one `panic_message(&Box<dyn Any>)` helper (downcast
`&str` / `String`) used at every join, or `resume_unwind`.

Reported by: write path, re-encoding, read path.

## HYG-024 - Counter declaration and emission is hand-written boilerplate

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

## HYG-025 - The output sortedness watermark is implemented twice

`drain::check_output_order` and `merge_join::BlobOrderGuard::observe` are the
same transitive-watermark rule; `BlobOrderGuard` says "Same shape as the
drain's output guard". They differ in what they keep (BUG-018) and in error
wording, and the drain comment says `sort` and `cat --dedupe` "already use" the
same rule. Fix: one `OrderWatermark` type in `osm_id.rs`.

Reported by: change application.

## HYG-026 - OSC document framing and gzip output policy are repeated per writer

The `<osmChange version="0.6">` prelude and postlude are written at six sites
(`merge_changes` single-input path, `build_prelude_bytes`,
`build_postlude_bytes`, `tags_filter/osc.rs`, `diff/derive.rs`, the
`osc/write.rs` tests); the parse side has `xml_parse::ROOT`, the write side has
no owner, and `"0.6"` and the XML declaration are repeated. `emit_change`'s
section open/close logic is something every OSC writer needs.
`flate2::Compression::fast()` appears in `merge_changes` (twice),
`tags_filter/osc.rs` and `write/parallel_gzip.rs` (wrapped in a private
`compression_level()` with a doc explaining why, the only owner);
`Writer::new_with_indent(_, b' ', 2)` is repeated; `.gz` detection via
`to_str().is_some_and(|s| s.ends_with(".gz"))` appears three times in
`merge_changes`, so a non-UTF-8 path silently writes plain XML into a `.gz`
name. Merge-changes builds its own multi-member gzip (per-worker `GzEncoder`
buffers concatenated) beside `write/parallel_gzip.rs`, whose doc says its only
in-crate consumer is `diff --format osc`. Fix: an `OscDocumentWriter` and
`OscOutput::create(path)` in `osc/write.rs`; text rule on
`BytesStart::new("osmChange")` and `disallowed-methods` on
`flate2::Compression::fast` outside it.

Reported by: change application.

## HYG-027 - OSC and apply-changes element writers are duplicated

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
- The `unwrap_or(Path::new("."))` idiom misses the empty-parent case at every
  site (BUG-023).

Fix: one `scratch_dir` resolved once by the CLI (explicit `--tmp-dir`, else the
output's directory with `""` mapped to `.`), validated up front, passed into
the library as a required `&ScratchDir` whose only constructor is in the CLI;
`BuildConfig.scratch_dir` for geocode. With a fixed file set
(HYG-084), geocode `validate_structure` could reject unexpected entries in the
index dir.

Reported by: altw and scanning, re-encoding, geocode index.

## HYG-029 - Scratch naming and cleanup use a different mechanism at every site, and nothing reaps crashed runs

Naming: `diff-par-{pid}-{kind}-{shard}.txt.tmp`;
`derive-{creates,modifies,deletes}-{pid}.xml.tmp`; derive-parallel shards add
a process-lifetime nanosecond tag "because PID alone collides across
containers", while derive-parallel's outer files in the same function omit it;
`process_scratch_tag` derives the tag from the low 32 bits of `SystemTime`
nanoseconds, so two processes in one tick or on a coarse clock collide; altw's
`format!("id-W{worker_id}-{bucket_idx:03}")` is spelled in `stage1.rs`
(create), `stage2.rs` (open) and `external/mod.rs` (cleanup) while
`ScratchDir::bucket_path` exists for `slot-NNN`, and if one copy changes stage 2
skips missing shards (`NotFound => continue`), surfacing as missing locations.
`std::process::id()` is the identity in `ScratchDir`, the sparse store and
`OutputFile`.

Cleanup: `derive.rs` calls `sink.cleanup()` after a closure, no `Drop`, so a
panic leaks three files; derive-parallel outer files use `PathGuard` (citing
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
  `bench-merge` deletes the output first (BUG-056); `build-geocode-index` writes
  the index directory in place, so a failed build leaves a partial index under
  the user's name, though `PathGuard::dir` exists.
- Merge-changes builds its own `OutputFile`-or-`PathGuard` choice from
  `output.exists()` (a TOCTOU with no consequence today); apply-changes'
  library entry does nothing (BUG-021); `tags_filter/osc.rs::write_filtered_osc`
  and `multi.rs` `File::create` whatever they are given, so library callers get
  truncate-then-fail.
- Altw's `DestinationGuard` cites the merge-changes precedent.
- `OutputFile::commit` replacing an existing file is silent.

Fix: one policy in `PbfWriter` (write `<path>.partial` in the same directory,
rename after `flush` succeeds, unlink on `Drop`) and an `Outputs` type owning
file, multi-file and directory publication that every `run_*` and library
entry takes instead of a `&Path`. Enforce with `disallowed-methods` on
`std::fs::File::create` under `src/commands/` (writer layer exempt) and a
forced-error test asserting the destination does not exist.

Reported by: write path, change application, filtering and spatial, CLI and
plumbing, altw and scanning.

## HYG-031 - The two OSC sinks validate metadata and roles differently

`parse_owned_metadata` (merge-changes, tags-filter OSC) parses only `version`
and keeps `timestamp`, `changeset`, `uid`, `visible` as raw strings.
`arena_meta` (apply-changes) parses `timestamp` as RFC 3339, `changeset` as
i64, `uid` as i32, and treats `visible` as "exactly `false`". So `uid="abc"` or
`timestamp="yesterday"` is a hard error in apply-changes and passes through
merge-changes, which re-emits it; `visible="False"` is visible to apply-changes
and re-emitted verbatim. The `scan_meta_attrs` doc claims "ONE leniency
decision, shared by every sink", which covers presence, not validity. Roles:
`CompactSink::member` uses `parse_str_attr(e, "role").unwrap_or_default()`,
turning every error into `""`; `OwnedSink::member` and tags-filter OSC use
`parse_str_attr_optional(...).unwrap_or_default()`, and
`parse_str_attr_optional` itself `.flatten()`s attribute errors and `.ok()`s
normalisation errors, so neither tells an absent role from an unreadable one.
`OwnedSink::member`'s `MemberType::Unknown` arm can never run (`parse_member_type`
errors on unknown strings) and its message differs from the live one. Fix: one
typed `OscMeta` parsed once (`osc/write.rs::push_info_attrs` already takes those
typed options), `parse_str_attr_opt -> ParseResult<Option<String>>`, delete
`parse_str_attr_optional`, an OSC-specific three-variant member type.

Reported by: change application.

## HYG-032 - Record layouts are serialised by hand at both ends

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

## HYG-033 - The library mutates the process allocator

`crate::debug::cap_malloc_arenas()` (`mallopt(M_ARENA_MAX, 2)`) is called from
`check_refs`, `verify_ids`, `cat` (`cat_filtered`), `repack`, `degrade`
(`degrade_decode_path`), `renumber`, `time_filter` and the geocode builder, each
with a comment saying it is scoped to this command. The call itself is now
centralized in `src/debug.rs`, gated on `target_os = "linux"` plus
`target_env = "gnu"` with a no-op fallback, and a textlint rule in `brokkr.toml`
keeps raw `libc::mallopt` and friends out of every other file, so musl builds.
What remains: `mallopt` is process-global and never restored, so a library
consumer that calls one of these once (a server that builds a geocode index then
serves) keeps two arenas for life; in the CLI the setting depends on which
command ran. `time_filter`'s comment documents a path that regresses under it.
With jemalloc/mimalloc as the global allocator it silently does nothing and the
documented RSS difference reappears. `brokkr.toml` captures `MALLOC*` env vars,
suggesting `MALLOC_ARENA_MAX` was once the knob. Fix: allocator tuning belongs
to `cli/src/main.rs`, per subcommand if needed, or to an explicit opt-in option
rather than a library call. A `--target x86_64-unknown-linux-musl` build in a
sweep would pin the musl half, which is currently held only by the textlint
rule.

Reported by: re-encoding, geocode index, CLI and plumbing.

## HYG-034 - Decode failures are turned into plausible default values

- `elements.rs` (three sites): `WireInfo::parse(data).ok().unwrap_or_default()`,
  so corrupt metadata becomes version 0 / timestamp 0, indistinguishable from
  absent; `time-filter` and history handling can treat a corrupt element as
  timestamp 0. `block.rs::validate_info`'s doc says so.
- `block.rs::PrimitiveGroup::dense_nodes` and its twin: a `WireDenseNodes`
  parse error yields an empty iterator, so a corrupt dense group reads as "no
  nodes". `dense.rs::DenseNodeIter::new` drops all dense metadata on
  corruption; the tag iterators end early on a bad varint.
- `wire.rs` (`read_tag().ok()??`) ends on the first bad tag; `blob_meta` is in
  BUG-036.
- `owned.rs` `element_metadata` / `dense_node_metadata`: a `user()` string-table
  error becomes `""` and missing timestamps, changesets and uids become `0`,
  re-encoded by sort, `cat --dedupe`, degrade and time-filter. Documented, but
  opposite to decision 0004's defensive-input stance, and invisible.
- `osc/write.rs`: `info.user().ok()` and `m.role().unwrap_or("")`, so
  `diff --format osc` emits `role=""` for a broken role; `bench_write_loop`
  does the same with `m.role().ok().unwrap_or_default()`.
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

- `copy_file_range`, `OutputChunk::CopyRange`, `write_raw_copy` and the
  parallel backend's `WriteOp::CopyRange` compile only with `linux-direct-io`,
  though kernel-space splicing has nothing to do with O_DIRECT; a default build
  silently loses passthrough splicing. `sort` notices via `cfg!`,
  `apply_changes/rewrite.rs` does not (HYG-018). `linux-direct-io = []` does not
  gate `libc`, which is unconditional.
- `FileReader::buffered` issues `POSIX_FADV_SEQUENTIAL` only under
  `any(feature = "linux-direct-io", feature = "linux-io-uring")`, while
  `blob.rs` issues `DONTNEED` under plain `target_os = "linux"`; so the default
  library build reads without the readahead hint and the CLI with it, and any
  benchmark comparing a library consumer to the CLI compares different I/O.
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

`getid` and `removeid` refuse without indexdata unless `--force`, with two
different hand-written messages; `getparents` calls `has_indexdata` only to
pick an arm; `getid --add-referenced` calls `require_indexdata` and ignores the
result; extract refuses for complete/smart only; renumber's schedule scan
returns its own differently worded error and has no `--force` escape at all;
sort, dedupe, cat (two variants) and repack each write their own "input PBF has
no blob-level indexdata..." sentence (sort and dedupe identical); geocode's
`--force` both skips the check and allows overwrite (HYG-087). The AGENTS.md
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

## HYG-043 - Altw raises `RLIMIT_NOFILE` from inside a library call and never restores it

`raise_nofile_to_hard_cap` permanently raises the process soft limit as a side
effect of a library function, with no opt-out; a host application relying on
its own limit (some do, to bound sockets) finds it changed. It is narrated as a
summary line but the library caller is never told. The fd arithmetic cannot be
exercised without changing the test process's limits. The stage 1 fallback
treats a failed `getrlimit` as 1024, and a second `getrlimit` failure after a
successful `setrlimit` also reports `ProbeFailed` with 1024, under-reporting
the limit it just raised. Stage 1's math assumes 256 shards per worker open at
once by design of pass A; a per-bucket shared appender (stage 2's slot-bucket
shape) removes that. Fix: raise it in the CLI, or restore it on exit from
`external_join`, behind an option.

Reported by: altw and scanning.

## HYG-044 - Altw auto routing reads the host directly and misreads containers

`page_cache_budget_bytes` reads `/proc/meminfo` inside the decision; the pure
`route_sorted_indexed` is tested but the end-to-end decision can only be tested
by owning a host with a given RAM. Under a cgroup memory limit `MemAvailable`
is the host's figure, so a container with 8 GB on a 256 GB host routes europe
to sparse and thrashes, the failure auto exists to prevent. The 3 GiB anon
headroom was measured at europe on one host and should grow with the IdSet
estimate. Off Linux the function returns `None`, so auto routes external for
every sorted indexed input there, which is wrong for small inputs; the auto
message should say the reason was "no /proc/meminfo". The `has_index` probe in
`auto_select_index_type` turns every open, read or parse error into `false`, so
a corrupt first blob routes sparse with "indexed=false" instead of being
refused. `/proc/self/stat` is also read unconditionally even without a sidecar.
Fix: a `HostFacts { mem_available, nofile, cpus, scratch_free }` handed to the
plan (HYG-035), reading cgroup v2 `memory.max` / `memory.current` and taking the
minimum; propagate probe errors.

Reported by: altw and scanning.

## HYG-045 - Altw stage 3's worker count must match a tmp-file count held by a second literal

`stage3.rs` says its worker count "Must match the worker tmp file count
allocated by mod.rs", and `router.worker_file(worker_id)` indexes
`worker_files[worker_id]`; both sides are `available_parallelism()...min(4)`
spelled separately, so changing either copy makes that index panic. Fix:
compute the count once in `external_join`, pass it into `stage3_slot_reorder`,
size `worker_files` from it, or have the router own it.

Reported by: altw and scanning.

## HYG-046 - Altw restates the same rules once per backend

- `altw/mod.rs::collect_relation_member_ids` and
  `external/relation_scan.rs::collect_relation_member_ids_indexed` both encode
  "a member way counts only for `type=multipolygon|boundary`, negative IDs
  skipped, want_nodes / want_ways gating"; if the type set changes in one, the
  WayMembers payloads diverge. The `relation_scan.rs` doc justifies the second
  copy by a sequential reader the generic version no longer uses.
- The header feature closure (`LocationsOnWays`, plus `WAY_MEMBERS_V1` and
  `SHARED_NODE_PINS_V1` under inject) is in `passthrough.rs::build_schedule`,
  `stage4.rs::stage4_assembly` and, without inject, `write_output_decode_all`.
- `passthrough.rs` and `stage4.rs` each define a `BlobDescriptor` with the same
  fields and the same passthrough rule (relations always, nodes only under
  `keep_untagged_nodes`, ways never).
- `missing_locations` has two producers on the external path: stage 4's
  reframe returns a per-blob value that is always 0, then `external_join`
  overwrites `stats.missing_locations = total_slots - resolved_count`.

Fix: one `relation_member_ids`, one `altw_header_features(hb, inject)`, the
shared pipeline of HYG-011, and stage 4's `missing` removed from the signature.

Reported by: altw and scanning.

## HYG-047 - Code aborts on caller-controlled input where a refusal was owed

- `BlockBuilder::add_node`, `add_way`, `add_relation`,
  `add_way_with_locations` and the `_raw` variants `assert!(self.can_add_*())`,
  aborting on a wrong element kind or full block, while the builder already has
  a non-panicking `poison` mechanism for other bad input.
  `BlockBuilder::with_element_cap(0)` panics.
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

## HYG-048 - Writer `Drop` discards errors and nothing enforces `flush`

`PbfWriter::Drop`, `ParallelGzipWriter::Drop` and `DirectWriter::Drop` are
best-effort and say callers MUST call `flush` / `finish`. There is no
`#[must_use]` and no debug check that a pipelined writer reached `flush` on a
non-panicking path; a `--bench` run that hits `Drop` on an error path loses the
writer error by design with no record. Fix:
`debug_assert!(std::thread::panicking() || self.finished)` in `Drop`, which
turns every forgotten `flush` in tests into a failure.

Reported by: write path.

## HYG-049 - Durability and buffering differ between the sync and pipelined writers

Pipelined backends fsync at end of stream; `PbfWriter::new` (sync) never does,
so multi-extract, every command on the sync writer and the library examples
produce un-synced output while single-output commands produce synced output.
`PbfWriter::new`'s doc says to wrap in a 256 KiB `BufWriter`; multi-extract
wraps in a default 8 KiB `BufWriter::new`, as do the test helpers in getid,
getparents, tags_filter and header_walker. `FileWriter` is a public enum with
public variants, so library users can construct
`FileWriter::Buffered(BufWriter::new(..))` with any capacity. Fix: decide
durability once in the writer; a `PbfWriter::to_path_sync(path)` constructor
that owns the buffer; private variants.

Reported by: write path.

## HYG-050 - Compression levels are validated in one place and defaulted in several

The `Zstd` doc says "1-22"; `FromStr` accepts `-7..=22`. `Zlib(u32)` is
validated to `0..=9` only by `FromStr`, so a library caller's
`Compression::Zlib(12)` goes straight to `flate2::Compression::new(12)`
(backend-defined behaviour). An out-of-range `Zstd` level surfaces at the first
blob on a rayon worker as an opaque `io::Error::other`. `parallel_gzip`
ignores `Compression` and hardcodes `flate2::Compression::fast()`. Renumber
does `if compression == Compression::default() { Zlib(1) }`, so a caller who
explicitly asks for the default level cannot get it, though the comment says it
respects explicit overrides. Every command defaults to `zlib` (library default
level) while `bench-write` defaults to `zlib:6`, so the benchmark does not
measure what commands do unless the library default is 6. Fix: private-field
`ZlibLevel` / `ZstdLevel` newtypes with checked constructors (the enum is
already `#[non_exhaustive]`), docs pulled from the same consts;
`Option<Compression>` where `None` means the command's own default.

Reported by: write path, re-encoding, CLI and plumbing.

## HYG-051 - Per-run metrics live in process-global statics

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
concurrent tests also bump, with bounds "loose enough to tolerate" it. Altw's
`inject_metrics` statics accumulate across calls ("matches brokkr's
one-process-per-run model"). `Instant::now()` is called directly at about 25
write-path and 60 altw sites for these, which is part of why none can be
asserted deterministically. Fix: a metrics value owned by each run or writer
(`Arc` handed to stages), emitted with a pass label or returned in stats.
Enforce with a test running two pipelines and asserting the second emit is not
cumulative.

Reported by: read path, write path, CLI and plumbing, altw and scanning.

## HYG-052 - The brokkr marker channel is a contract nothing pins, read from ambient state

`debug.rs` spells `BROKKR_MARKER_FIFO` once, but the name and line format
(`<us> <name>\n`, `<us> @<name>=<value>\n`) are a contract with brokkr;
`line_formats_marker_exactly` covers markers only, not the counter shape. The
variable is read lazily on first marker, from whichever thread emits first, and
cached in a process-global `OnceLock`: tests cannot inject a sink, a test cannot
point two runs at different FIFOs, and anything emitting before setup caches
`None` forever. The open is `O_NONBLOCK` and `.ok()?`, so a set variable with a
typo or no reader silently disables all instrumentation with no diagnostic.
`pub mod debug` makes the FIFO protocol, `/proc` readers and allocator pokes
public API; decision 0012 keeps CLI bridges `doc(hidden)`, and `debug` is not
even hidden. Fix: a golden test for both line shapes, mirrored by a brokkr
parse test; a marker sink handed in by `main`; a one-line warning when the
variable is set and the open fails; hide or narrow the module.

Reported by: CLI and plumbing, test harness.

## HYG-053 - Reorder buffers are bounded only statistically

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
in the shared engine of HYG-011.

Reported by: re-encoding, read path, filtering and spatial, altw and scanning.

## HYG-054 - Read-path growth with no guard

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

`FrameScratch::take_with_capacity` reserves every replacement buffer to the
high-water mark ever seen on that thread, so one oversize blob pins that
capacity in every rayon thread's scratch for the process lifetime, bounded by
`MAX_BLOB_DATASIZE` times thread count (gigabytes at planet core counts).
`parallel_gzip::writer_loop`'s reorder `BTreeMap` is bounded only indirectly by
channel capacity; a gap `DeathNotice` does not cover would grow it to stream
end (not reachable today).

Reported by: write path.

## HYG-056 - Change-application memory grows with the whole input

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

Sparse altw checks `statvfs` before preallocating; external altw writes hundreds
of GB into the output directory with no precheck and finds ENOSPC whenever stage
1 or 2 hits it, minutes to hours in. Diff and derive scratch is the full output
size, written to shard files and again to the output (derive: shard, outer,
gzip, about 45 GB of XML at planet per its comment), with no check. Fix: check
in the up-front plan (HYG-035) against the scratch filesystem.

Reported by: altw and scanning, re-encoding.

## HYG-058 - Sort and dedupe memory on adversarial input

Sort holds every overlap run's re-encoded output (`overlap_outputs`) before the
write loop; the comment says for pathologically unsorted input it can approach
the input size, which is planet-sized RSS on exactly the input sort exists for.
Process runs in a sliding window of K in-flight runs written in order. Dedupe's
sweep heap is bounded by overlap-run width, which one blob spanning the whole ID
range makes the whole kind.

Reported by: re-encoding.

## HYG-059 - `geo::assemble_rings` is an unbounded backtracking search

Each ring start runs a DFS over all endpoint candidates with full backtracking
(`used[segment_idx] = false; ring.truncate(..)`), cloning a candidate `Vec` per
frame (`endpoint_map.get(..).cloned()`). On a relation with many segments
sharing endpoints (a grid-like admin boundary or adversarial data) the work is
exponential in the branching endpoints. The input is OSM relations in the
geocode builder, not under the operator's control. No step budget, counter or
warning. Fix: a step cap with a logged give-up, or a deterministic
non-backtracking assembler (osmium's, in `research/libosmium/`).

Reported by: filtering and spatial.

## HYG-060 - Spatial primitives live in three homes, and region properties are recomputed per query

`extract::Bbox` lives in the extract command, `commands/spatial.rs` (the
declared shared owner) imports it from there, and `geo.rs` owns ring math.
`extract::polygon_rings_contains` and `geo::point_in_polygon` are the same
function with different hole-slice types, the copy existing to avoid a
`&[&[..]]` allocation. `polygon_rings_contains` calls
`point_in_ring_with_antimeridian` per candidate node, re-running
`ring_crosses_antimeridian` (O(ring)) and, for crossing rings, allocating an
unwrapped `Vec` per point; its comment says it avoids per-point allocation,
true only for non-crossing rings. `bbox_from_polygons` widens to -180..180 for
an antimeridian ring, which silently defeats blob-level spatial skip; a split
pair of boxes keeps it. `parse_extract_geometry` reads four `f64`s, formats them
into `"a,b,c,d"` and re-parses through `parse_bbox`, working only because float
`Display` is shortest-round-trip and relabelling errors through the string.
Fix: `Bbox`, `BboxInt`, the E7 conversions and a `PreparedRing { points,
crosses, unwrapped }` built once at parse time, in `geo.rs` below the commands;
`point_in_polygon` taking `impl IntoIterator<Item = &[(f64, f64)]>`;
`Bbox::try_new` beside `parse_bbox_str`. The geocode reader benefits from the
prepared rings too.

Reported by: filtering and spatial.

## HYG-061 - The indexdata trust model is decided per command

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
the geocode builder's `CoordMmapShared` rests on it (BUG-047). `CORRECTNESS.md`
partly states this for sort. Fix: state the trust model once in `reference/`
(indexdata is a hint; these properties are verified, these are believed, and
this is the consequence) and have each command cite it.

Reported by: change application, re-encoding, altw and scanning, geocode index.

## HYG-062 - Fault-injection hooks are process-global statics

Hooks live as statics in about twelve modules (`read/pipeline.rs`,
`read/region_grid.rs`, `write/parallel_writer.rs`, `write/parallel_gzip.rs`,
`write/uring_writer.rs`, `commands/diff/parallel.rs`,
`commands/diff/derive_parallel.rs`, `commands/altw/external/stage3.rs`,
`geocode_index/builder/pass3.rs`, `geocode_index/reader.rs`,
`commands/apply_changes/{rewrite,streaming}.rs`), re-exported through four
`mod.rs` files under four naming schemes (`read::pipeline_test_hooks`,
`read::region_grid_test_hooks`, a `write::` re-export, an `altw::` re-export),
so nothing answers "what can a test inject". Reset discipline differs:
`parallel_writer` says reset at start and end, `region_grid`
resets both sides, stage 3's `PANIC_AT_BUCKET_IDX` doc says reset "at the start
AND end of any test". The comments in `parallel_writer.rs`, `parallel_gzip.rs`,
`uring_writer.rs` and `diff/parallel.rs` say tests serialise via
`--test-threads=1` enforced by `brokkr test`; that has not been true since the
per-binary split, and the sweeps run in parallel with no thread limit. The
real invariant (each static-hook test owns its process) is now stated in
`Cargo.toml`, where each static-hook `fault_*` file has its own `[[test]]`
target with the hook it arms and `required-features = ["test-hooks"]`, but
nothing enforces "one test per target" and several comments still describe the
old serialisation story. The `read/pipeline.rs` test now arms through a guard
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

## HYG-063 - "Element matches any expression" is implemented four times

`tags_filter/mod.rs::element_matches` (bool triple), `tags_count` (same body),
`tags_filter/osc.rs::matches_any` (a `char` with `_ => false`) and
`export/mod.rs::matches_expressions` (a `TypeFilter` used as a kind, with
"admitted" written as `!kind.x || expr.x`, a different formula that agrees only
because callers pass single-kind filters). `tag_expr.rs` owns parsing and
`tag_matches` but not the any-expression loop, so each consumer re-decides the
type-gate semantics. Fix: `Expression::admits(ElementKind)` and
`any_matches(exprs, kind, tags)` in `tag_expr.rs`; make `type_filter` and
`matcher` private so no consumer can reimplement the loop.

Reported by: filtering and spatial.

## HYG-064 - Three hand-written `/proc` parsers that return zero on failure

`read_page_faults` (`debug.rs`), `read_peak_rss_kb` (`main.rs`) and
`micros_since_process_start`, in two crates, each parse `/proc` themselves and
silently return zero or `None` on failure; the page-fault one also splits
`/proc/self/stat` on whitespace (BUG-028). Fix: one module, parsing from the
last `)`, returning `Option` that callers handle.

Reported by: CLI and plumbing, altw and scanning.

## HYG-065 - CLI defaults, help text and docs restate library constants

- Geocode build defaults `17 / 14 / 10 / 500 / 75 / 1000` live in
  `BuildConfig::default()`, clap `default_value = "..."` strings,
  `reference/cli-reference.md`, `docs/cli/commands.md` and the generated
  `docs/.vitepress/dist/` copies (checked in; if it is a build artifact it is a
  fifth copy only regeneration keeps in step). The CLI never uses
  `BuildConfig::default()`; it builds the struct field by field via a
  `GeocodeLevels` struct that mirrors `BuildConfig`, so library and CLI
  defaults agree by coincidence.
- The level range "0-19" in clap help and `cli-reference.md` restates
  `MAX_BUILD_S2_LEVEL`.
- `inspect --anomalies` help says "<50% or >150% of median"; the numbers live in
  the report code.
- `--output-header` keys are spelled in `HeaderOverrides::parse`, the help text
  and the error message.
- `inspect --get` keys have three owners: the CLI's `HeaderGet` arm hard-codes
  `file.*`, `header.*` keys, `InspectReport::get_value` owns the rest, and
  `InspectMode::resolve` routes by prefix (`data.`, `metadata.`, `elements.`,
  `blocks.`, `indexed`), so a report key outside those prefixes is routed to
  the header path and errors as unknown; `run_inspect` adds a fourth check
  (`data.` / `metadata.` forces `--extended`).
- The "dense was removed" paragraph is in `IndexType::from_str` and
  `IndexTypeArg::resolve` and has diverged ("Use 'sparse'" vs "Use --index-type
  sparse"); nothing in-tree calls the library `FromStr`.

Fix: `pub const` defaults read by clap `default_value_t`, or clap `Args`
derived on the config struct; help strings built from the consts; a
`ValueEnum` for header override keys; the inspect key table in `inspect` with
per-key needs; a unit test asserting `BuildConfig::default()` equals the
clap-parsed empty-args config.

Reported by: geocode index, CLI and plumbing, altw and scanning.

## HYG-066 - Two element-ID spec parsers for one syntax, already diverged

`inspect --show` parses `n123` and `node/123` via `parse_element_spec` in
`cli/src/main.rs`; `getid` and `getparents` parse via
`getid::parse_id_spec_inner`. `node/123` works for `--show` and is rejected by
getid; negative IDs are rejected by getid (ADR-0002) and accepted by `--show`.
Fix: one `ElementRef` `FromStr` in the library beside `TypeFilter`, used by
both through a clap `value_parser`.

Reported by: CLI and plumbing.

## HYG-067 - The negative-answer exit travels as an error

`ExitWithCode` is modelled as an `Err` downcast in `main` and `guarded_output`.
Any wrapper that re-boxes or maps the error would turn "diff found differences"
into exit 2 with `Error: exit with code 1`. Its `Display` exists only to
satisfy `Error`. Fix: `run_*` returns `Result<Outcome, Error>` with
`Outcome::{Positive, Negative}`.

Reported by: CLI and plumbing.

## HYG-068 - Workspace manifest metadata is duplicated and the CLI MSRV is false

Root `rust-version = "1.99"`, CLI `rust-version = "1.96"`; the CLI depends on
the library by path, so it cannot build on 1.96. The CLI restates `authors`.
`serde_json` with `float_roundtrip` is declared in both manifests (deliberate
and commented), with nothing keeping versions in step. Fix: `rust-version` and
`authors` in `[workspace.package]`, `serde_json` in
`[workspace.dependencies]`, inherited; `cargo msrv verify` in a sweep for the
value.

Reported by: CLI and plumbing.

## HYG-069 - Durable docs and comments hard-code figures that have drifted

- Altw temp disk at planet: `IndexType::External`'s doc and the CLI help say
  about 224 GB; `external_join`'s doc, AGENTS.md, `reference/pipeline.md`,
  `reference/cli-reference.md`, `docs/guide/*` and `docs/cli/commands.md` say
  about 256 GB.
- Altw memory: the `external/mod.rs` doc says "<1 GB at every stage, no mmap";
  everywhere else says about 8.7 GB peak anon.
- Auto threshold: CLI help, AGENTS.md, `docs/guide/*` and both osmium-parity
  docs say "~80% of available RAM"; the code is 80 % of `MemAvailable - 3 GiB`;
  only `reference/pipeline.md` is right.
- Auto status: CLI help calls `auto` "(recommended)" and
  `docs/guide/performance.md` the "Recommended default", while the default is
  `sparse`.
- Algorithm: AGENTS.md and `docs/guide/advanced.md` call external a
  "rank-bucketed counting sort"; the code is ID-bucketed and `stage2.rs`'s doc
  says the rank path was replaced.
- Geocode file count "19" in `builder/mod.rs`, AGENTS.md and
  `cli-reference.md`, with nothing enumerating the set (HYG-084).
- `src/lib.rs` crate doc says `pbfhogg = "0.4"`; the crate is 0.5.
- `check`'s `NODE_ID_FLOOR = 14e9` and siblings carry comments with today's
  live OSM counts; with the file-size ceiling in place the floors could be
  derived or dropped.
- `idset.rs` doc hard-codes planet memory figures and an incomplete user list.

Per the documentation rule, reword so the drifting number is no longer
hard-coded, or point at `reference/performance.md`; write the auto rule once in
`reference/pipeline.md` and cite it. A docs test could check help text against
`AUTO_STORE_BUDGET_PCT` and `AUTO_ANON_HEADROOM_BYTES`.

Reported by: altw and scanning, geocode index, CLI and plumbing, test harness.

## HYG-070 - CLI-driving tests live in the library crate, so binary resolution is restated and guessed

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
stale-binary class and BUG-061; `tests/common` becomes a shared path or dev-only
crate. If the split stays, keep the rule in one place and treat empty as
unset.

Reported by: test harness, CLI and plumbing.

## HYG-071 - Tests write into cargo's `target/`, the system temp dir and brokkr's scratch

`tests/roundtrip_real.rs::output_path` (`target/denmark-roundtrip.osm.pbf`),
`tests/cli_apply_changes.rs::merge_cross_validate_osmium`
(`target/merge-xval-*.osm.pbf`), `tests/geocode_index.rs`
(`target/geocode-test`, `target/geocode-test-build`), `src/path_guard.rs` unit
tests (tempdirs under `target/`), `merge_changes/mod.rs` tests
(`target/merge-changes-{name}-{pid}`, never cleaned) all hard-code
`CARGO_MANIFEST_DIR/target`, while `pbfhogg_bin()` honours `CARGO_TARGET_DIR`.
`target/` belongs to cargo: `cargo clean` destroys it mid-run, brokkr's
per-worktree `CARGO_TARGET_DIR` means these write into the main tree's
`target/` even when building elsewhere, and the Denmark-sized files survive
every run invisibly until a clean. The `/tmp` and brokkr-scratch writers are in
BUG-064. Three policies exist for one need, and the hunters propose two owners:
`CARGO_TARGET_TMPDIR` (which cargo provides to integration tests for exactly
this), or brokkr's scratch root exported as `BROKKR_SCRATCH_DIR` with a
project-owned ignored directory (`.scratch/` or `data/test-scratch/`) for plain
cargo runs. Either way: one `test_scratch()` helper creating
`TempDir::new_in(<owned root>)`. The O_DIRECT reader test needs a non-tmpfs
filesystem, so its location should come from the harness. Enforce with
`disallowed-methods = ["std::env::temp_dir"]` and text rules on
`join("target")` and `CARGO_MANIFEST_DIR` joined with `data` under `src/` and
`tests/`.

Reported by: test harness, change application, CLI and plumbing, read path.

## HYG-072 - Real-data tests hard-code dataset paths that `brokkr.toml` owns, and they have diverged

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

## HYG-073 - Test skips are invisible and pass

Skips are `eprintln!("Skipping: ...")` followed by `return`, which libtest
reports as PASS, and nothing in `brokkr check` output counts them. With
HYG-072 and BUG-063, a host can pass the whole real-data and platform tiers
having run none of them. Fix: a shared `skip!(reason)` helper writing a
countable marker, or panicking under a "must run" variable set by the owning
profile.

Reported by: test harness.

## HYG-074 - The CliInvoker timeout can never fire, and its output capture is unbounded

`CliInvoker`'s `DEFAULT_TIMEOUT` is 60 s; brokkr's per-test watchdog is 20 s on
`brokkr check`; in-test waits in `read_paths.rs` are 5 s and 10 s. The watchdog
always kills first and reports a `futex_do_wait` hang instead of the
CliInvoker's message naming the command and its stderr; the doc calling the
timeout "load-bearing for brokkr check" is not true under the current watchdog.
`CliInvoker::run` drains stdout and stderr into unbounded `Vec`s, so a command
looping on output grows test memory until a kill. Fix: default below the
watchdog or read it from a brokkr-exported variable; cap capture to first and
last N MB with a truncation mark.

Reported by: test harness.

## HYG-075 - The test harness reimplements PBF framing twice

`tests/common/adversarial.rs` has its own `read_varint`, `write_varint`,
`skip_field`, `parse_datasize`, `locate_blobs`, `decompress_blob`;
`tests/common/mod.rs::count_tagdata_blobs` has a third `read_varint` (no
overflow bound, panics on a short buffer) and its own frame walk. An
independent oracle is defensible, but it should be one oracle, not two that
differ (`Option` vs panic), with a note on why it does not reuse
`src/read/wire.rs`.

Reported by: test harness.

## HYG-076 - Durable source cites transient documents and line numbers

The `notes/` and `.plans/` halves are closed: `scripts/check_doc_links.py`
(the `doc-links` script_check in `brokkr.toml`) now fails on any durable file
that cites `notes/` or the gitignored `.plans/`, or names a repository path
that does not exist. Still open: `extract/common.rs` cites "TODO.md history
2026-04-19" (the script does not claim root-level `TODO.md`); stage 1's fd
comment cites "`stage1.rs` Pass B loop at lines 385-405"; `coord_payloads.rs`
cites `coord_payloads.rs:398`; `sort` cites `drain.rs:408-410`. `decisions/` is
a document folder not covered by the `reference/` / `docs/` / `notes/` table in
AGENTS.md, and source cites it (`path_guard.rs`, `getid`'s ADR-0006,
`derive_parallel.rs`'s `decisions/0003`); it needs a place in that table.
Enforce the remainder with a text check in `brokkr check` for `TODO.md` and
`\.rs:[0-9]+` inside `src/` comments.

Reported by: altw and scanning, filtering and spatial, re-encoding, CLI and
plumbing.

## HYG-077 - The enforcement levers most of these findings need are unused

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

## HYG-082 - `CompactDiffOverlay` deletion state has two writers

The comment above the mutators says invariants (a delete-set insert paired with
an index remove) are why mutation goes through methods. But `deleted_nodes`,
`deleted_ways` and `deleted_relations` are `pub`, and `CompactSink::begin_element`
mutates them directly (`self.overlay.deleted_nodes.remove(&id)`), so the
"un-delete on create" rule lives in the parser, and any library user can insert
without touching the index or tombstones. Fix: private fields, `is_deleted` /
`deleted_ids` accessors, un-delete inside `push_*`.

Reported by: change application.

## HYG-083 - The library evicts page cache by default

Eviction is on by default for the buffered `BlobReader` constructors and
re-advises the whole consumed prefix `0..offset` per blob (the source notes it
is unmeasured and tracked in TODO.md). For a library this evicts pages another
process (an `osmium` alongside, a second pbfhogg pass) is using. It is
documented and opt-out exists, but the default is the surprising direction for
a library consumer and the right one for the CLI; consider making the CLI opt
in.

Reported by: read path.

## HYG-084 - Geocode format vocabulary is held in literals and argument order

- Admin level 11 means "postal code" (invented in `pass1.rs` as `else { 11 }`),
  relied on by `reader.rs` (`best_by_level: [...; 12]` twice, `level < 12`
  guards), `format.rs` and `ReverseResult.admin` docs ("2-11"), and
  `admin::admin_vertex_cap` (postal boundaries take the local cap). If accepted
  admin levels widen to `2..=11` (OSM does use 11), real level-11 boundaries
  collide with postal codes and the smallest-area-per-level ranking mixes them.
  Byte 25 of `AdminPolygon` is reserved and zero; a `kind` byte there (a format
  bump) makes the collision unrepresentable.
- Interpolation type `0/1/2` is a literal in `pass2.rs`, a comment in
  `format.rs`, and `0 => ..., _ => ...` in `reader.rs`, which accepts any
  nonzero byte and never distinguishes odd from even (both use step-2 rounding
  relying on `start_number` parity). A `#[repr(u8)] enum` with `TryFrom<u8>`
  validated in `validate_structure`.
- The S2 format limit `30` is a literal in `Header::from_bytes` with "0..=30" in
  the error; `pub const S2_MAX_LEVEL` beside `MAX_BUILD_S2_LEVEL` with a const
  assert relating them.
- `FusedCellAssignmentParams` takes eight file names as `&str` and `run_stage_b`
  four more, always the `format::FILE_*` constants, so fine/coarse pairing is
  argument order; the reader's `search_level` and `validate_geo_tree` take the
  same mmap quadruple positionally. A `GeoTreeFiles` const pair
  `FINE_TREE` / `COARSE_TREE` in `format.rs`.
- Nothing enumerates the file set; the reader's `open` lists the `FILE_*`
  constants a second time. `pub const ALL_FILES`.
- The zero-length-file anonymous-mmap trick is in `Reader::open` and
  `pass2.rs::mmap_file` ("same as Reader"); one `mmap_or_empty`.
- `max_admin_vertices` is written to the header and never read, and since
  per-level multipliers it no longer describes the stored polygons (a country
  holds 16x it). Drop it or record the effective caps.

Reported by: geocode index.

## HYG-085 - Geocode `Reader::open` validation is O(index size) with no opt-out

`validate_structure` walks every record of every file at open, faulting in many
GB at planet and making `open` take seconds to minutes; the builder's smoke test
repeats the walk. A legitimate trade, but undocumented as a cost and with no
`open_trusted` for an index the caller just built or checksummed. A content
checksum in the header would make trust O(1). The content checks of BUG-046 add
to this walk.

Reported by: geocode index.

## HYG-086 - Geocode street classification is defined twice

Pass 1.5 decides which ways' nodes to keep via
`scan::way::GeocodeTagLiterals::standard()` (`is_street`, `is_building_addr`,
`is_interp`); Pass 2 re-decides in `classify_way_into` with its own tag match
and `EXCLUDED_HIGHWAYS`, in a different module and representation (byte
literals vs `&str`). If Pass 1.5 is narrower, Pass 2 classifies a street whose
nodes were never collected, `filter_map` drops them, and the street silently
loses vertices or vanishes; if wider, memory is wasted. The hunter did not read
`scan/way.rs` in full, so whether they have diverged is unverified. Fix: one
classification consumed by both, plus a property test that Pass 2's set is a
subset of Pass 1.5's over the same blobs.

Reported by: geocode index.

## HYG-087 - Geocode `--force` means two things, and bad-element policy differs per rule

`config.force` is passed to `require_indexdata` (skip the requirement) and also
gates overwriting an existing index, so rebuilding in place also opts out of the
indexdata check, which on unindexed input leads into BUG-024. Pass 2a aborts the
whole build for one node outside the coordinate domain, while missing refs, bad
roles and unparseable levels are skipped: the only data-quality rule that
aborts. Fix: `overwrite` and `allow_unindexed` as two fields and two flags; one
`on_bad_element` policy (skip-and-count, or fail under `--strict`).

Reported by: geocode index.

## HYG-088 - Geocode build drops data silently and its smoke test cannot be acted on

`classify_way_into` drops refs whose node is absent (`filter_map`), shortening
streets and opening admin rings, with no counter or line; on an extract this is
the common case at the border. Admin ways in `needed_admin_ways` but absent from
`way_geom` surface only as a relation-level count. Dropped relations are counted
but `RawAdminRelation` has no `id`, so "412 admin relations dropped" cannot be
acted on; unresolved interpolation ways are a count only (no OSM way ID is kept
past Pass 2); negative node refs are skipped in Pass 1.5 silently.
`"WARNING: smoke test query returned no address or street match"` names no
coordinate or index path and is a stderr line, not a `BuildStats` field, so the
CLI cannot fail on it and tests cannot assert it. Fix: counters via
`emit_counter`, a capped sample list of IDs, `BuildStats.smoke_test:
SmokeOutcome`. The smoke test exists only to need `first_addr_lat_e7` /
`first_addr_lon_e7`, threaded through both Pass 2 merge closures and
`Pass2Output`; reading the first record from `addr_points_mmap` gives the same.

Reported by: geocode index.

## HYG-089 - The geocode builder assumes 64-bit pointers

`interp.rs::read_node_at` says "u64->usize: Linux 64-bit only" and casts;
`coord_array_len = referenced_count as usize * 8` truncates on 32-bit. The
reader uses `usize::try_from`; the builder does not, so 32-bit gets wrong
answers rather than a refusal. Fix: `compile_error!` on
`target_pointer_width != "64"` for the builder.

Reported by: geocode index.

## HYG-090 - No sweep compiles the reader-only geocode build

`geocode-reader` without `commands` is the configuration a geocoding service
would use and `geocode_index/mod.rs` advertises it. No `[[check]]` sweep
enables it alone (`minimal` has no features, `consumer` has `commands`), like
the old untested `default-features = false` claim the `minimal` sweep's comment
describes. Inside the builder, `#[cfg(feature = "geocode-reader")]` around the
smoke test is always true (`commands` pulls in `geocode-reader`), suggesting a
configuration that cannot be built. Fix: one more sweep with
`features = ["geocode-reader"]` scoped to `-p pbfhogg`; drop the always-true
cfg.

Reported by: geocode index.

## HYG-091 - Significant fallbacks and events happen with no line or counter

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

Fix: route each through the diagnostics channel of HYG-019 and a counter.

Reported by: re-encoding, filtering and spatial, read path, write path, change
application.

## HYG-092 - Derive-changes reads the clock per element

`--update-timestamp` calls `SystemTime::now()` per emitted delete element, in
both `derive.rs::write_delete_element` and its copy in `derive_parallel.rs`.
Output is non-deterministic within one run (timestamps straddle a second),
differs between `-j1` and `-jN`, and is untestable for equality. Fix: resolve
`now` once in the CLI and pass `timestamp: Option<u64>`. Enforce with
`disallowed-methods = ["std::time::SystemTime::now"]` in the library (the
scratch-tag helper goes away with HYG-029).

Reported by: re-encoding, CLI and plumbing.

## HYG-093 - `PipelineConfig` clamping lives in the builder, not the type

`ElementReader`'s `read_ahead` / `decode_ahead` setters clamp `n.max(1)`, and
`AdmissionGate::new` clamps again with `cap.max(1)`. `sync_channel(0)` is a
rendezvous channel, not an error, so a `PipelineConfig` built another way
(crate-internal callers construct it directly) gets different semantics. Use
`NonZeroUsize` fields.

Reported by: read path.

## HYG-094 - io_uring setup treats a failed probe as support and gives memlock advice that may be wrong

`uring_init_and_run` refuses only when the probe succeeds and says WriteFixed
is unsupported; a failed `register_probe` proceeds. The `RLIMIT_MEMLOCK` advice
printed for ENOMEM / EPERM can be wrong on kernels 5.12+, where registered
buffers are charged to the memcg rather than memlock, which is exactly the
hosts in use. 16 MB locked memory is assumed.

Reported by: write path.

## HYG-095 - Filtering, inspect and check memory grow without bound

`inspect tags` (`tags_count`) accumulates a global
`FxHashMap<String, FxHashMap<String, u64>>` over the whole file, filters by
`--min-count` / `--max-count` afterwards and copies the key into every
`TagCount`; at planet the distinct-value set is hundreds of millions of strings
and `--min-count 1000` does not bound memory (a count-min sketch or two-pass
approach, or at least a documented bound and an `--alloc` bench row).
`multi.rs` keeps `3 * N` `IdSet`s for up to 500 regions with no estimate or
refusal. `print_requested_ids` / `print_missing_ids` print one ID per line with
no cap, so `-I planet.pbf --verbose-ids` writes billions of lines.
`parse_geojson`, configs and ID files are read with `read_to_string` of any
size (acceptable for operator inputs, noted for completeness).

Reported by: CLI and plumbing, filtering and spatial.

## HYG-096 - Geocode builder holds all intermediate state in RAM, some of it twice

`way_geom` (every admin way's geometry), `admin_polygons`, `interp_ways`,
`StringPool` and `admin_cell_entries` grow with input; the doc acknowledges
planet limits. `StringPool` stores every string twice (bytes in `data` plus an
owned `String` key per entry in the `FxHashMap`); hashing into `data` by offset
is the cheap fix. Altw's `PerWayRcs` and `load_ref_count_sidecar` likewise hold
the whole per-way sidecar in RAM, proportional to way count and absent from the
memory accounting the docs quote; stage 4's straddler payloads are bounded by
the two-piece straddler invariant, which `slot_bucket_count` arithmetic
maintains and nothing checks at run time (`classify_blobs_in_bucket` errors
only after the fact).

Reported by: geocode index, altw and scanning.

## HYG-097 - Geocode query behaviour depends on the cell level rather than distance

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

## HYG-098 - Export's counters depend on filter order

`export` builds a way's full geometry string before the bbox test and then
discards it, and reports `skipped_invalid_ways` for ways the bbox would have
rejected anyway, so the counter depends on filter order.

Reported by: filtering and spatial.

## HYG-099 - Dead or misleading code in the read path

- `header_walker.rs` `pread_exact` and `read_blob_data`: `#[allow(dead_code)]`,
  no callers in `src/` or `cli/src/`.
- `raw_frame.rs` `RawBlobFrame::{index_present, tagdata, file_offset}`: marked
  `#[allow(dead_code)]` but read by `commands/check/index.rs`; dead only in a
  no-`commands` build, so use
  `#[cfg_attr(not(feature = "commands"), allow(dead_code))]`. The `tagdata` doc
  ("current readers don't access it directly") is stale.
- `ReorderBuffer::with_max_window`: test-only in practice.
- `block.rs` carries another `#[allow(dead_code)]` item worth the same check.
- `IndexedReader` builds its own per-blob node and way ranges by full decode,
  ignores `BlobHeader.indexdata` (which carries that plus relations, which it
  does not track), and no command uses it; a public API kept from the osmpbf
  heritage. Rebuild on `BlobIndex` or retire.
- `pool_get_pub` is an exact duplicate of `pool_get(Some(..))`;
  `WireBlob::parse_slice` wants a caller check.
- `BlobReader::new` defaults `parse_indexdata: true` "for compatibility" while
  every engine sets it explicitly; keep or drop deliberately.

Enforce with `clippy::allow_attributes_without_reason`.

Reported by: read path.

## HYG-100 - Dead or misleading code in the write path

- `write_primitive_block_no_indexdata`: public, its only non-test callers are
  `#[cfg(test)]` blocks in getid and getparents; it is a fixture producer on
  the writer's public surface.
- The `#[cfg(feature = "linux-direct-io")]` sync branch of `write_raw_copy` is
  unreachable: `write_raw_copy` exists only on `PbfWriter<FileWriter>`, whose
  every public constructor is pipelined.
- `uring_cq_wait_ns` duplicates `uring_submit_and_wait_ns`.
- `compress_one`'s `io::Result` and `HeaderBuilder::build`'s `Result` are
  nominal; `parallel_gzip::worker_loop` does `Err(_) => return` on
  `compress_one`, so the `DeathNotice` names a chunk but not the cause; give it
  `Out::Failed(io::Error)`.
- `parallel_writer`'s doc frames itself against "plan item #17";
  `uring_writer`'s says it replaces `writer_thread` "in `super::writer`", which
  no longer lives there.
- Re-exports kept "so the existing path keeps resolving"
  (`crate::writer::Compression`, `crate::block_builder::HeaderBuilder`) are
  compatibility shims in a pre-1.0 crate.

Reported by: write path.

## HYG-101 - Dead or stale code in change application

- `MergeStats::blobs_skip_decompress` and `blobs_scan_only` are never
  incremented, yet `print_summary` prints "(N index-hit, 0 scan-only, 0
  skip-decompress)" and adds `blobs_skip_decompress` into `total_blobs`;
  leftovers of the removed batch-classify pipeline, `pub`, so library users see
  them. Building `MergeStats` only through `merge_from` and constructors would
  make an unwritten field show as dead.
- `WorkerOutput` is a one-variant enum hosting `into_drain_item`.
- `scanner.rs`: `let _ = last_node_seq_sent;` ("set here for symmetry"), a
  no-op; `streaming.rs`: `let _ = worker_id; // reserved for per-worker counter
  slots later`.
- `write_osc_way(.., _stats)` and `write_osc_way_local(.., _stats)`: unused
  parameters threaded through every caller.
- `merge_changes::action_tag` wraps `OscAction::tag`, and
  `type Action = OscAction` is an alias "for readability".
- `osc::load_all_diffs` is `pub` with no production caller; it is the one
  multi-diff accumulator, and still shapes the overlay (the `usize` offsets
  comment cites it). Whether it is a library feature with a consumer (nidhogg?)
  or dead cannot be told from the code; it needs the user's answer.
  `parse_osc_file` is used by tests and is a supported API.
- `rewrite.rs::merge()` destructures `allow_version_regression: _` and reads
  `opts.allow_version_regression` anyway.
- Stale docs: `apply_changes/mod.rs` describes the removed batch loop (Phase 1
  parallel classify...); `classify.rs` says blobs pass through raw with
  gap-creates afterward, while `streaming.rs` now forces a rewrite whenever
  `inline_upserts` is non-empty; `stream_output.rs` says "run on the main thread
  as slots are drained"; `rewrite.rs` narrates the legacy batch loop;
  `diff_ranges.rs` cites a removed function "(see osc.rs)" and `osc.rs` does
  not exist; `load_all_diffs`' doc says ".osc.gz" and its error says the only
  recognised naming is `<sequence>.osc.gz`, while `parse_sequence_number` also
  accepts `<seq>.osc`; `scanner.rs::send_err` says the caller still observes Ok
  where it returns `Err`; `run_workers` and `drain.rs` cite a nonexistent
  `rewrite::merge_with_overrides`; `osc/parse.rs` compares memory to a
  `DiffOverlay` that no longer exists. Writing references as intra-doc links
  under `-D rustdoc::broken_intra_doc_links` would catch the renamed-function
  cases.

Reported by: change application.

## HYG-102 - Dead or stale code in altw and scanning

- `#[allow(dead_code)]` markers saying "wired up in step 2 of A1" on
  `BucketLayout`, its `impl`, `max_node_id_from_blob_meta`, `IdRecord` and its
  `impl`, all called from production paths; `bucket_width`'s allow says
  "exposed for testing" but `BucketLayout::new` calls it.
- `IdSet::count_in_range` and `count_below`: no callers outside `idset.rs`,
  docs describing the removed rank path; `drop_rank_index`'s doc cites a stage
  2 use with no external caller.
- `NodeBlobInfo` documents rank-range fields that no longer exist; stage 1 and
  `Stage1Output` docs say it owns an `IdSet` it does not; stage 1's fd comment
  and `raise_nofile_to_hard_cap`'s doc cite the removed pass B;
  `relation_scan.rs` and `coord_payloads.rs` cite deleted functions and files;
  stage 4's comment points at `add_locations_to_ways.rs`, which no longer
  exists; `scan/node.rs` lists "ALTW dense pass 1" as a user.
- Stage 4's reframe returns `missing_locations` (always 0) and `_new_slot_pos`
  (unused); `BlobDescriptor::kind: Option<ElemKind>` in stage 4 is always
  `Some`.
- `scatter_bucket_entries(_bucket_start, _bucket_end)` unused parameters;
  `build_way_schedule` returns `Result` but cannot fail.
- `NodeIndex` is a one-variant enum "as future-proofing";
  `IndexType::External | IndexType::Auto => unreachable!` in
  `build_node_index` and the `altw_index_kind` counter emitting `i64::MIN`
  exist because `IndexType` stays unresolved past dispatch; a `ResolvedBackend`
  without `Auto` makes them unrepresentable.
- The library `IndexType::from_str` "dense" arm has no in-repo caller; it is
  public API, so whether it stays is a decision.
- `radix.rs`'s module doc carries move history; it has one user.

Deleting the stale `allow(dead_code)`s lets rustc's `dead_code` lint do the
job.

Reported by: altw and scanning.

## HYG-103 - Dead or stale code in the re-encoding commands

- `diff/parallel.rs`: `let _ = options.verbose;` documenting a dispatch rule
  that lives in `mod.rs`; a `debug_assert!(!options.verbose)` says it.
- `derive_parallel.rs`: `let _ = element_version; // silence "unused helper"
  if future paths drop it`.
- `ChangeSink::write_create(_kind)` / `write_modify(_kind)`: unused parameter
  on a private method.
- `repack` and `cat_filtered` discard `_max_ids` from
  `build_classify_schedules_split`; check whether any caller uses it.
- `degrade::DEFAULT_BLOCK_CAP` is `pub` and documented as a test seam, and the
  hidden `--block-cap` flag serves the same purpose; one seam is enough.
- Comment rot: `renumber/mod.rs` "(moved from the former
  src/commands/renumber.rs)"; `diff/parallel.rs` names `diff-par-*-{pid}-*`
  while the pattern is `diff-par-{pid}-*`; `derive_parallel.rs` refers to
  `assemble_osc_raw`, the function is `assemble_osc_from_paths`; `sort()`'s doc
  comment sits on `SortOptions`, so rustdoc attributes both to the struct and
  `sort()` has none; `schedule.rs`'s `let _ = walker.next_header()?...` keeps
  the `?` and only discards the header, and a named `_header` binding would
  stop readers.

Reported by: re-encoding.

## HYG-104 - Dead or stale code in filtering and spatial

- `extract/common.rs::pread_execute`: `let _total_blobs = schedule.len();`.
- `extract/mod.rs`: `use crate::geo::ring_crosses_antimeridian;` with a comment
  citing `polygon_bbox_f64`, which is now `bbox_from_polygons`.
- `getparents_with_min_blobs` (see HYG-002).

Reported by: filtering and spatial.

## HYG-105 - Dead or stale code in the geocode index

- `pass3::parse_polygon_rings` is a one-line forwarding wrapper, and
  `format::parse_polygon_rings` is used only by it.
- `collect_candidates` forwards to `search_level` verbatim, "kept only to make
  the two public call sites readable", with nine positional arguments.
- The `SlimInterpWay` "house number 0" limitation is repeated in `pass2.rs`
  twice and `interp.rs`, yet `find_endpoint_house_number_mmap` returns `None`
  for `hn == 0` and the reader rejects `start == 0 || end == 0`, so the
  ambiguity cannot arise; say "house number 0 is unsupported" once or add a
  `resolved` flag.
- `classify_way_into`'s doc says nodes at `lat==0 && lon==0` are dropped,
  contradicting `coord_pack` and the comment 70 lines above it.
- `BuildStats` repeats the header counts; it could be derived from the header
  plus `dropped_admin_relations`.

Reported by: geocode index.

## HYG-106 - Dead or stale code in the CLI, plumbing and brokkr configuration

- `debug::malloc_trim` and `debug::set_mmap_threshold`: no caller in `src/`,
  `cli/` or tests; their docs describe investigations. `debug` is `pub`, so an
  external caller is possible in principle.
- `debug::emit_mallinfo2` calls in `extract`, `check_refs` and `time_filter`
  carry "DIAGNOSTIC (2026-04-11 round 3)" provenance and fire on every run;
  whether brokkr still reads `MI_*` / `checkrefs_*_arena` needs a brokkr-side
  check before removal.
- `BROKKR_TEST_PLATFORM = "1"` in the `platform` profile is read by nothing in
  this repo (see BUG-063); `[test.profiles.serial]` exists "reserved for any
  future serial-only test" and has none.
- `IndexTypeArg::Dense` hidden value and the `TagSortArg` aliases `count` /
  `name` are compatibility with removed things, with no end date.
- `commands/mod.rs`: the "Element type filter" section header sits above
  passthrough-flush helpers; a stray "Map Osmosis sentinel -1 to 0" doc line is
  fused onto `clean_metadata`'s doc, describing `map_sentinel`, now in
  `owned.rs`.
- `check/verify_ids.rs`: `#[allow(clippy::too_many_lines)]` sits on a 10-line
  function instead of `verify_ids_streaming_parallel`.
- `getid::parse_ids` and `parse_ids_from_file` (no-default-type wrappers): the
  CLI uses the `_with_default_type` variants; check tests before deleting.
- `tests/common/cli.rs::path_arg` is a one-line wrapper; the
  `pipeline_test_hooks` re-export of `REORDER_WINDOW_HIGH_WATER` has no reader
  in `tests/`; verify both before deleting.

Reported by: CLI and plumbing, test harness.

