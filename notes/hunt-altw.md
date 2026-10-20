# Hygiene hunt: add-locations-to-ways and scanning

Scope: `src/commands/altw/` (mod, sparse, passthrough, reframe, external/*),
`src/scan/`, `src/idset.rs`, `src/coord_pack.rs`, plus how they use the
read/write paths and size themselves against the host. Read at `955664d`.
Nothing was built or run; findings marked "likely bug" are from reading
only and want a test before anyone acts on them.

Enforcement already paid for in this build, read first: the workspace clippy
deny list in `Cargo.toml` (`too_many_arguments`, `too_many_lines`,
`cognitive_complexity`, `unwrap_used`, the cast lints), feature gating
(`commands`, `linux-direct-io`, `test-hooks`), and the `brokkr.toml` check
sweeps (`all`, `consumer`, `minimal`). There is no `clippy.toml`, so
`disallowed_methods` / `disallowed_types` - the obvious way to enforce
several of the consolidations below - is unused. The scope carries ~190
`#[allow(...)]` attributes; `too_many_arguments` and `too_many_lines` are
denied workspace-wide and locally allowed on nearly every big function here
(`add_locations_to_ways`, `external_join`, `stage1_pass_a`,
`stage2_node_join`, `stage3_slot_reorder`, `stage4_assembly`,
`write_output_passthrough`, `decode_one`, `process_block`,
`write_output_checked`, `write_output_decode_all`). The lints are in place
but do nothing in this scope.

---

## Likely bugs found along the way

These come first because they are what an operator would hit.

### B1. External backend should reject every input that has a zero-ref way (likely bug)

`scan::way::parse_way_refs` returns without calling the callback when
`way.refs_data.is_empty()`. Stage 1 pass A (`external/stage1.rs`) pushes
`per_way_rcs.push(refs.len())` *inside* that callback, so a way with no
refs gets no entry in the per-way refcount sidecar. Stage 4's
`reframe_way_blob_with_locations` (`external/stage4.rs`) reads one sidecar
varint for *every* `Way` message (field 3) in the blob. If a blob has a
zero-ref way, stage 4 reads the next way's count for it, then hits "encountered
more way messages than the per-way sidecar record declares" and the run fails.
The sparse backend accepts the same input (pass 0 does not care, and
`reframe.rs` handles an empty refs field explicitly). `tests/edge_cases.rs`
has `zero_ref_way_roundtrips` for the plain read/write path. Nothing tests
altw external against one. So which backend you pick, including whatever
`--index-type auto` picks for you, decides whether a legal input is accepted.

Fix: have the scanner call back with an empty slice, or have stage 1 count
Way messages and not callbacks. Enforce with a cross-backend test:
a fixture with a zero-ref way run through `--index-type sparse` and
`--index-type external` must give byte-identical way output.

### B2. Sparse backend probably fails when `-o` is a bare relative filename (likely bug)

`add_locations_to_ways` computes `scratch_dir = output.parent().unwrap_or(Path::new("."))`.
For a bare name like `out.osm.pbf`, `Path::parent()` returns `Some("")`, not
`None`. The fallback never fires (the CLI's `OutputFile::new` keeps the
scratch sibling bare, `.pbfhogg-tmp-PID-N.out.osm.pbf`). In `sparse.rs`,
`preallocate_sparse_file` passes the scratch dir to `statvfs`, and
`statvfs("")` fails with ENOENT, so the run should end with "failed to query
free space for sparse index in : No such file or directory". External's
`ScratchDir::new` happens to survive because `"".join(name)` is relative. The
same `unwrap_or(Path::new("."))` idiom appears in `external/mod.rs` (and in
the CLI), each with the same blind spot. Integration tests use absolute
tempdir paths, so they never reach this case.

Fix: one helper that maps an empty parent to `.`, shared by every scratch
derivation. A CLI test that runs altw with `current_dir` set and a bare `-o`
would pin it.

### B3. Unbounded `IdSet` growth on a crafted ref id (aborts on untrusted input)

`IdSet::set` grows `chunks` to `id >> 25` entries. In sparse pass 0
(`collect_way_referenced_node_ids`), every non-negative way ref from the input
goes into `referenced.set(node_id)`, and the relation-member scans do the same
with member ids. A single ref near `i64::MAX` makes `resize_with` allocate
about 2^38 `Option<Box<..>>` slots (around 2 TB of vector), and the process
aborts on allocation failure. That is an abort driven by an input value, which is what
categories 4 and 8 ask about. External is protected because `BucketLayout::locate`
bounds ids by indexdata `max_id`. Sparse has no bound. Separately, a large but
plausible id still allocates a full 4 MB chunk per distinct 2^25 range, so
memory is bounded by id spread, not by element count.

Fix: give `IdSet` a constructor that takes a bound (sparse can take it from
indexdata when present, or from a sanity cap) and a fallible `try_set`, as
`set_atomic_checked` already does for the atomic path. Enforce through the type:
the untrusted-input callers get an `IdSet` that cannot be grown past its bound.

### B4. Stage 3's empty-bucket error path reports a panic that did not happen

In `stage3_slot_reorder` the non-empty-bucket error path records the error,
calls `router.abort(...)` with the real message, and returns. The
empty-bucket branch (`entry_counts[bucket_idx] == 0`) records the error and
returns `Err` without calling `router.abort`. The `AbortOnDrop` guard is still
armed, so its `Drop` fires `router.abort("stage 3 worker panicked (AbortOnDrop
guard fired)")`. Stage 4 waiters then see a panic message for a plain error.
The top-level message stays right only because `external_join` prefers the
stage 3 result. The mechanism behind it is a finding too: `AbortOnDrop` stands
in for both "panicked" and "returned early", and its message assumes the first.

### B5. `s2_orphan_records` undercounts

The counter is `bkt.records.len() - record_ptr` after the blob walk. Records
that the walk passed over without resolving (id falls in a gap between tuples
inside a blob's range) are not counted, so the counter only covers orphans
past the last blob. It is diagnostic only, but it is the counter you would
check when `missing_locations` is non-zero.

### B6. Page-fault deltas can underflow

`external_join` computes `s1_majflt_after - s1_majflt_before` (and the same
for s2/s3) on `u64`. `read_page_faults` returns `(0, 0)` on any read
failure, so a failure on the second read underflows: a panic in debug, a
huge counter in release. The parser also uses `split_whitespace` on
`/proc/self/stat`, so fields shift if the comm field (field 2) contains a
space. A library consumer's binary name can. Use `saturating_sub` and parse
from the last `)`.

---

## 1. One value, one owner

### 1.1 Worker-count rule written out at eight sites, with three different caps

`available_parallelism().map(|n| n.get().saturating_sub(2).max(1)).unwrap_or(4)`
appears in `scan/classify.rs::resolve_thread_count` (the owner, with a doc
comment saying so), `external/stage1.rs` (uncapped, then capped by fds),
`external/stage2.rs` (`.min(6)`), `external/stage3.rs` (`.min(4)`),
`external/mod.rs` (`.min(4)`, for the payload tmp files),
`external/stage4.rs` (uncapped), and `passthrough.rs` (uncapped). The copies
have already diverged in their caps, and the reasons for each cap live only in
comments at each site. Two of them must agree: `stage3.rs` says "Must match
the worker tmp file count allocated by mod.rs", and `router.worker_file(worker_id)`
indexes `worker_files[worker_id]`. If either copy changes, that index panics.
That is a real invariant held only by two literal `.min(4)`s.

Fix: compute the stage 3 worker count once in `external_join`, pass it into
`stage3_slot_reorder`, and size `worker_files` from the same value (or have the
router own the count). For the general rule: one `fn decode_threads(cap: Option<usize>)`
in a host-topology module that everyone calls. Enforce with `disallowed_methods`
on `std::thread::available_parallelism` in a new `clippy.toml`, allowed only in
that module.

### 1.2 Geographic-domain check written twice

`coord_pack::pack` owns the +/-90 / +/-180 refusal (`LAT_BIAS`, `LON_LIMIT`).
`external/stage2.rs` writes it again with literals
(`-900_000_000..=900_000_000`, `-1_800_000_000..=1_800_000_000`) and its own
copy of the error text, and its comment says the backends "must agree on
refusal". Today they agree. Nothing keeps them in step. Fix: export
`coord_pack::in_domain(lat, lon)` and one error constructor, and call both from
stage 2. A shared test that feeds both backends an out-of-domain node and
asserts the same error would hold it.

### 1.3 Relation-member classification written twice

`altw/mod.rs::collect_relation_member_ids` and
`external/relation_scan.rs::collect_relation_member_ids_indexed` both encode
"a member way counts only when the relation has `type=multipolygon|boundary`,
negative ids skipped, want_nodes/want_ways gating". They are the same rule
behind two scheduling strategies. If the set of "complete" relation types
changes in one copy, the WayMembers payloads from the two backends disagree.
Fix: one `fn relation_member_ids(rel, want_nodes, want_ways, push)` and two
schedulers. Enforceable by structure only: both callers must call the one
function.

### 1.4 Output-header feature set written three times

The closure that adds `LocationsOnWays` plus, under inject, `WAY_MEMBERS_V1` /
`SHARED_NODE_PINS_V1` appears in `passthrough.rs::build_schedule`,
`external/stage4.rs::stage4_assembly`, and (minus the inject part)
`mod.rs::write_output_decode_all`. Fix: `fn altw_header_features(hb, inject)`.

### 1.5 Passthrough-eligibility rule and `BlobDescriptor` written twice

`passthrough.rs` and `external/stage4.rs` each define a `BlobDescriptor` with
the same fields and comments, plus the same rule: relations always pass
through, nodes only under `keep_untagged_nodes`, ways never. Stage 4's comment
points at "write_output_passthrough (add_locations_to_ways.rs)", a file that no
longer exists. See 5.1 for the pipeline these belong to.

### 1.6 Scratch file names spelled at each use

`format!("id-W{worker_id}-{bucket_idx:03}")` is spelled in `stage1.rs`
(create), `stage2.rs` (open), and `external/mod.rs` (cleanup). That is three
copies of one name, while `ScratchDir::bucket_path` exists and is used for
`slot-NNN`. If one copy changes, stage 2 silently skips missing shards
(`NotFound => continue`), so the drift would show up as missing locations, not
as an error. Fix: `ScratchDir::id_shard_path(worker, bucket)`, which keeps the
format in one place by construction.

### 1.7 `BUCKET_BUF_SIZE` defined twice

`radix.rs` exports `BUCKET_BUF_SIZE = 256 * 1024`. `stage2.rs` declares a
private `const BUCKET_BUF_SIZE: usize = 256 * 1024` that shadows it, and also
has `FLUSH_THRESHOLD = 256 * 1024`. Stage 1 uses `64 * 1024` and `256 * 1024`
literals for its sidecar writers. Same values today, separate owners.

### 1.8 Channel depths and reorder capacities as literals

The `sync_channel(16)` descriptor and `sync_channel(32)` result depths appear
in `classify.rs` (three times), `stage4.rs`, `passthrough.rs`, and `stage1.rs`
(32), with `ReorderBuffer::with_capacity(64)` in stage 1. Stage 4 records why
32 is right (a 256-depth A/B test). The others inherit the number without the
reason. These are tunables. There is no list of them (see 2.1).

### 1.9 The "dense was removed" message is in two places

`IndexType::from_str` in the library and `IndexTypeArg::resolve` in the CLI
both carry the paragraph, and the library comment admits it ("the CLI
carries its own copy"). They have already diverged: one says "Use 'sparse'",
the other "Use --index-type sparse". Nothing in-tree calls the library
`FromStr` (the CLI uses clap's `ValueEnum`), so the library copy has no
callers in this repo (see 9). If the public parse stays, the CLI should map
its error rather than restate it.

### 1.10 Sizing figures disagree across docs and code

- Temp disk at planet: `IndexType::External` doc and the CLI help say ~224
  GB. `external_join`'s doc, AGENTS.md, `reference/pipeline.md`,
  `reference/cli-reference.md`, `docs/guide/*` and `docs/cli/commands.md`
  say ~256 GB.
- Memory: the `external/mod.rs` module doc says "Memory at every stage: <1 GB
  ... No mmap". Everywhere else says ~8.7 GB peak anon, and stage 4 holds two
  IdSets, the router and `per_way_rcs`.
- Auto threshold: the CLI help, AGENTS.md, `docs/guide/*`, and both
  osmium-parity docs say "~80% of available RAM". The code is 80 % of
  `MemAvailable - 3 GiB`. Only `reference/pipeline.md` has the right rule.
- Auto status: the CLI help calls `auto` "(recommended)" and
  `docs/guide/performance.md` calls it the "Recommended default", while
  `default_value = "sparse"` and `#[default] Sparse`.
- Algorithm: AGENTS.md and `docs/guide/advanced.md` describe external as a
  "rank-bucketed counting sort". The code is id-bucketed (`BucketLayout`). The
  rank path was replaced, and `stage2.rs`'s own module doc says so.

Per the documentation rule, reword these so they stop hard-coding a number
that drifts, or point at `reference/performance.md` instead. The auto rule
should be written once in `reference/pipeline.md` and cited. A docs test could
check the help text against `AUTO_STORE_BUDGET_PCT` and
`AUTO_ANON_HEADROOM_BYTES`, but nothing currently ties docs to these
constants.

### 1.11 `missing_locations` has two producers on the external path

Stage 4's reframe returns a `missing` per blob that is always zero
(`let missing_locations: u64 = 0;`, documented as such). `external_join` then
overwrites `stats.missing_locations = total_slots - resolved_count`. So there
are two producers, one of them a constant, and no check that they agree. The
sparse path counts per ref. The result field should be removed from the stage 4
signature (see 9). A debug assertion could check the external count against
the per-blob count, with zero-coord decoding as the cross-check.

---

## 2. Values nobody can find, change, or trust

### 2.1 No inventory of tunables

Here is the full set for this scope, as far as I can find it: `NUM_BUCKETS`
(256), `BUCKET_BUF_SIZE`, stage 2 `FLUSH_THRESHOLD`, stage 1 `HEADROOM_FDS` (64,
a local `const` inside a function), the `1 << 30` RLIMIT clamp (twice), the
1024 fallback soft limit (twice), `AUTO_STORE_BUDGET_PCT`,
`AUTO_ANON_HEADROOM_BYTES`, `WILLNEED_WINDOW_BYTES`,
`WILLNEED_COALESCE_GAP_BYTES`, every worker cap in 1.1, every channel depth in
1.8, `BucketLayout`'s `1 << 30` bucket-width ceiling, the `granularity >
1e9` refusal in `scan/node.rs`, and the "8 bytes per slot" store sizing.
They live in seven files, some as module constants, some as function-local
constants, some as bare literals. Nothing answers "what can I tune here".
Fix: an `altw::tuning` module (or a `Tuning` struct passed down from
`AltwOptions`) holding all of them, with the measurement each one rests on.
That also covers 2.2.

### 2.2 No injection points

None of these can be set from a test. Consequences:

- The auto-routing rule reads `/proc/meminfo` inside
  `page_cache_budget_bytes`. `route_sorted_indexed` is pure and tested, but
  the end-to-end decision (does a given input route external on this host?)
  can only be tested by owning a host with a given amount of RAM.
- The RLIMIT path (`raise_nofile_to_hard_cap`) mutates the process-wide soft
  limit as a side effect of a library call, with no option to turn it off,
  and the fd-cap arithmetic cannot be exercised without changing the test
  process's own limits.
- `NUM_BUCKETS` is a constant, so small-fixture tests always run 256 id
  buckets times N workers' shard files. That is why tiny tests create
  thousands of files. Straddler and slot-bucket edge cases are reachable only
  through `total_slots / max_blob_slots`, not by choosing a bucket count.
- Stage 3's fault hook is a process-global `static AtomicUsize` behind
  `test-hooks`, and the doc tells callers to reset it "at the start AND end of
  any test". That makes parallel tests that use it order-dependent (see 7.3).

### 2.3 Validation happens late, at the point of use

- The external free-space requirement is never checked. Sparse checks
  `statvfs` before preallocating. External writes ~224-256 GB into the output
  directory with no precheck and finds ENOSPC whenever stage 1 or stage 2
  happens to hit it, minutes to hours in.
- Sorted plus indexed preconditions are checked by four separate opens of the
  input before work starts: `auto_select_index_type` opens an `ElementReader`
  and a `BlobReader`, then the explicit-sparse hint opens another
  `ElementReader`, `external_join` calls `require_indexdata` and opens another
  `ElementReader` for `is_sorted`, and stage 4 opens a `BlobReader` just for
  the header. Each of these is a separate header read with its own failure
  message.
- `inject_prepass && !indexdata_present` is refused only inside
  `write_output_checked`, after pass 0 and pass 1 have already run. On a large
  unindexed input that means building the whole sparse store before learning
  that the flag combination was never allowed. It belongs with the
  other option validation at the top of `add_locations_to_ways`.

Fix: one `AltwPlan::resolve(input, options)` that opens the input once,
reads the header and the first indexdata, validates every option combination
and the scratch filesystem's free space, chooses the backend, and returns a
plan both backends take. That also gives auto-routing its injection point:
the plan takes `HostFacts { mem_available, nofile, cpus, scratch_free }`.

---

## 3. One channel, one implementation

### 3.1 Operator text written straight to stderr from library code

Library functions call `eprintln!` directly: `Stats::print_summary`, the
explicit-sparse "hint:", the three `auto-selected --index-type ...` lines, the
"removed stale sparse index temp file" line, `[altw external] RLIMIT_NOFILE...`,
`[altw external] num_workers = ...` (twice), and
`[coord_payloads] streaming router ... MB in worker tmps`. A library caller
cannot silence, redirect or capture any of them. Three different prefix styles
are in use (`add-locations-to-ways:`, `[altw external]`, `[coord_payloads]`)
plus unprefixed lines. The `[coord_payloads]` line prints on every external
run. It is internal diagnostics phrased for a developer, and it belongs in the
counter channel, where the same numbers already go.

Fix: an `AltwObserver` / narration sink in `AltwOptions` (or a
crate-wide one), with the CLI's implementation printing. Enforce with a
`disallowed_macros` entry for `eprintln` / `println` in `clippy.toml`,
allowed only in the CLI crate and the sink.

### 3.2 Counter emission is roughly 300 lines of boilerplate

Every stage declares 10 to 35 `AtomicU64` locals, a matching `_ref` binding
for each, and an emit block that repeats `emit_counter("x", x.load(Relaxed) as i64)`
under `#[allow(clippy::cast_possible_wrap)]`. The compare-exchange "track the
max" loop is open-coded at least five times (stage 2 twice, stage 3 once,
stage 4 twice), while `AtomicU64::fetch_max` exists and stage 4 already uses
it for one counter. `passthrough.rs` has a `Pass2Counters` struct and stages
1 to 4 do not. Fix: a `#[derive]`-style counter struct (or a macro) that emits
every field under its own name, plus `fetch_max` everywhere. This is a
structural rule: counters live in a struct that knows how to emit itself.

### 3.3 Lines that leave out the identifiers needed to act on them

- `pass A pread: {e}`, `stage2 node pread: {e}`, `relation scan pread: {e}`,
  `router pread worker {id} blob {idx}`, and the classify helpers'
  `ErrorKind::Io(e)` name no file and no offset. The schedule already has
  `data_offset`.
- `open id shard: {e}`, `read id shard: {e}`, `stat id shard: {e}`,
  `open slot bucket: {e}` name no path, though `prepare_bucket` has it in a
  local.
- `write id shard W{worker_id}: {e}` names the worker but not the bucket or path.
- The external metadata scan's "OsmData blob missing indexdata" gives no offset.

### 3.4 Places where something significant happens and nothing says so

- `raise_nofile_to_hard_cap` permanently changes the process's
  `RLIMIT_NOFILE`. It is narrated as a summary line, but the library caller is
  never told and cannot opt out.
- Stage 2 on `linux-direct-io` builds calls `sync_data` and drops the result,
  and evicts page cache with `posix_fadvise(DONTNEED)` on every slot bucket and
  id shard. Whether this happens depends on a compile-time feature, not on
  `--direct-io` (see 10.4), and no counter records which build ran it (the
  `s2_fadvise_*` counters read as zero on non-feature builds with no "not
  compiled" marker).

---

## 4. Errors

### 4.1 Failures reclassified as `ErrorKind::Io`

Stage 4 wraps every string error (router abort, reframe failure, assembly
failure) as `ErrorKind::Io(std::io::Error::other(e))`. Sparse
`SharedSparseWriter::insert` does the same for an out-of-domain coordinate,
which is an input-data error, not I/O. A library caller matching on
`ErrorKind` sees I/O for corrupt input. The cause is structural.
`external_join` explains that `BoxResult`'s `Box<dyn Error>` is not `Send`, so
thread results get stringified at every scope boundary (`.map_err(|e| e.to_string())`
appears more than ten times) and rebuilt afterwards, which loses the kind and
the source chain. Fix: make the crate error `Send + Sync` and drop the
`String` round-trips. That is enforceable through the signatures: thread
closures return `crate::Result`.

### 4.2 Errors swallowed

- `drop(std::fs::remove_file(...))` on id shards and slot buckets mid-run.
  At planet scale a failed cleanup leaves 80+ GB in place while stages 3 and 4
  allocate their own scratch. Nothing logs or counts the failure.
- `drop(w.get_ref().sync_data())` in `SharedSlotBuckets::finish` (feature-gated).
- The stage 1 fd fallback treats a failed `getrlimit` as a soft limit of 1024
  and carries on. That is fine, but the second `getrlimit` failure after a
  *successful* `setrlimit` also reports `ProbeFailed` with 1024, which
  under-reports the limit it just raised.
- The `slot_entry_counts` stat uses `.unwrap_or(0)` on `metadata()`. A bucket
  whose file cannot be stat'd is treated as empty, so stage 3 takes the
  empty-bucket branch and writes zero coordinates for every slot in it. The
  missing-locations gate still catches this, because `resolved_count` comes
  from stage 2. But the counts that drive stage 3 already exist in memory as
  `SharedSlotBuckets::entry_counts` (atomics, populated on every flush) and are
  thrown away in favour of a filesystem stat. Use the in-memory counts and
  compare them with the file length.
- `auto_select_index_type`'s `has_index` probe turns every error (open, read,
  parse) into `false`. A corrupt first blob routes sparse with
  "indexed=false" when it should be refused.
- `ScratchDir::drop` ignores `remove_dir_all` failure ("user can clean"), and
  the user is never told where.

### 4.3 Aborts on input someone controls

- `IdSet` growth (B3).
- `SparseArrayIndex::get` and `SharedSparseWriter::insert` assert on a
  rank outside the store. That one is a real internal invariant, so the assert
  is appropriate. Their messages are paragraph-length.
- `BucketLayout::new` asserts `bucket_width <= 1 << 30`. `max_node_id` comes
  from indexdata (`max_node_id_from_blob_meta`, which validates sign and order
  but not size), so a node blob advertising `max_id` above about 2.7e11 (256 x
  2^30) panics the process instead of returning an error. Return a `Result`.
- `IdSet::rank` indexes `chunk_prefix[cid]` with no bounds check, so it panics
  for any id past the last chunk or any negative id (cast to `u64`).
  `count_below` guards this, `rank` does not, and `rank` is `pub`.
- `IdSet::set_atomic` on a negative id panics with a message blaming
  `pre_allocate`, because `-1 as u64` lands in chunk 2^39. The doc's list of
  causes leaves this one out.

### 4.4 `rank_if_set` and `rank` disagree about a missing rank index

`rank`, `resolve` and `total_count` `expect` a built rank index.
`rank_if_set` returns `None` when it is absent, which in sparse means "node
missing". Forgetting `build_rank_index()` would therefore show up as every
coordinate missing, then the missing-locations refusal, not as a panic naming
the cause. Make the rank index a separate type (`RankedIdSet`, produced by
`IdSet::into_ranked()`) so that a rank query on an unranked set cannot
compile. `drop_rank_index` and the `Option` fields then go away.

---

## 5. Policy invented per call site

### 5.1 One pread-decompress-dispatch-reorder pipeline, built eight times

The same shape (a dispatcher thread feeding a `Mutex<Receiver>`, N workers
doing pread into a reused buffer then `decompress_blob_raw` then work, a
bounded result channel, a consumer with or without a `ReorderBuffer`, a
first-error slot) is hand-built in:

- `scan/classify.rs`: `parallel_classify_phase_abortable`,
  `parallel_scan_blobs_raw`, `parallel_classify_accumulate`. These are three
  near-verbatim copies inside one file. Only `_abortable` supports early exit,
  and only the classify ones use a `DecompressPool`.
- `external/stage1.rs` pass A (its own atomic-index dispatch instead of the
  channel, its own reorder buffer).
- `external/stage2.rs` (atomic bucket dispatch, `Mutex<Option<String>>` error slot).
- `external/stage3.rs` (atomic dispatch, error slot plus router abort).
- `external/stage4.rs` and `passthrough.rs`. `passthrough.rs`'s module doc says
  it was "lifted from external/stage4.rs", so these two are a fork:
  descriptor, partition, worker pool, `StallGauge` send-wait, passthrough
  pre-seed, drain closure.

Each copy makes its own choices about error policy (first error wins vs. last
error wins: stage 2 *overwrites* `*err_ref = Some(e)`, so the last error wins
there, while stage 1's flush slot keeps the first), cancellation (stage 2 polls
the error mutex once per bucket, classify relies on channel drop, stage 1's
workers keep running until the schedule drains, the stage 1 and relation-scan
scope deliberately waits out the slower thread), buffer reuse, pread error
context (3.3), and whether WILLNEED prefetch runs. Only the classify helpers
prefetch, so stage 1 pass A, stage 2, stage 4 and passthrough get no sliding
WILLNEED window. The same blob-body validation `blob_body_within_file` runs in
the classify schedule builders. The external `scan_blob_metadata` and the
passthrough `build_schedule` skip it, so a past-EOF `data_size` fails late
with an opaque UnexpectedEof in those backends and early with a clear message
in the others.

This is the main structural finding. One `BlobPipeline` (schedule, worker
state factory, per-blob fn, ordered or unordered consumer, cancellation token,
first-error-wins slot, prefetch policy, pread error context) should replace
all eight. The external stages differ only in what they dispatch (blobs vs.
buckets), and a generic work-item dispatcher covers that. The
`passthrough`/`stage4` fork should become one assembly pipeline parameterized
by a `CoordSource` trait, with `NodeIndex` (sparse) and router payloads
(external) as the two impls. That would also merge the two reframe
implementations (`reframe.rs::reframe_way_blob_with_locations` and
`stage4.rs::reframe_way_blob_with_locations`, same name, different
signatures), which already share `check_default_coord_framing` and the
WayMembers payload builder, so they are partly merged already. Enforceable by
dependency rule: `std::thread::scope` and `sync_channel` allowed only in the
pipeline module (`disallowed_methods`).

### 5.2 Cleanup on the error path, decided per site

- The sparse store uses `PathGuard` from create to unlink. Good.
- `ScratchDir` cleans on drop. Mid-run, id shards and slot buckets are
  deleted with swallowed errors (4.2).
- `ScratchDir` names are pid-scoped, and nothing reaps old ones. A crashed
  external run leaves `.pbfhogg-external-join-<pid>/` (hundreds of GB at
  planet) in the output directory forever, and only a run with the same pid
  ever touches it. Sparse's "removed stale sparse index temp file" check has
  the same weakness: it looks only for its own pid's name, so it almost never
  finds anything. The useful cleanup would be reaping `.pbfhogg-*-<pid>` for
  dead pids, and nothing does it.
- `DestinationGuard` in `altw/mod.rs` is a third output-guard idea, on top
  of `OutputFile` and `PathGuard`, documented as the "merge-changes
  precedent". Whether this belongs in `path_guard` as the one library-level
  output guard is a question for the write-path scope. It is a policy, and it
  is being reinvented here.

### 5.3 Validating values read back from storage

Sidecars and shards that the run itself wrote get careful framing checks
(`load_ref_count_sidecar` trailer and cumulative check, `prepare_bucket`
multiple-of-12 check, `scatter_bucket_entries` bounds check, per-way record
trailing-bytes check). That is good, but each one is written by hand.
`IdRecord::read_from` and `ResolvedEntry::write_to` have no shared record
codec, and stage 3 decodes `ResolvedEntry` by hand-slicing `chunk[0..4]`,
`chunk[4..8]`, `chunk[8..12]` instead of a `ResolvedEntry::read_from`. That
asymmetry is where a layout drift would land. Fix: one `FixedRecord` trait
(`SIZE`, `write_to`, `read_from`) for `IdRecord` and `ResolvedEntry`, with
the size constants derived from it.

### 5.4 The inject-prepass pin bit is stolen in-band from latitude

Stage 2 writes `lat = (tuple.lat << 1) | pin` into `ResolvedEntry` when
`inject_prepass` is set, and stage 3 and 4 decode it based on the same flag
passed separately. That is a second coordinate encoding next to
`coord_pack`, whose doc says "nobody re-derives the layout". It is safe
(`9e8 << 1` fits in i32). But the meaning of a stored latitude depends on a
boolean carried separately through three stages, and the stage 3
zero-filled scatter buffer stands for absence (all-zero means Null Island,
unpinned), which is the exact convention `coord_pack` was written to retire.
External's absence accounting is fine only because it counts presence in stage 2.
Fix: give the pin its own field in `ResolvedEntry` (the record has room once
it is 16 bytes, or put the flag in `local_slot_pos`'s high bit), or route both
through a `coord_pack`-style codec with a type per encoding.

---

## 6. Ambient dependencies

- Clock: `std::time::Instant::now()` around nearly every operation, for
  counters only. Harmless for logic, but it is roughly 60 call sites of
  instrumentation tangled into the control flow (see 3.2).
- Environment and host: `/proc/meminfo` (auto routing), `/proc/self/stat`
  (page faults, read unconditionally even without a sidecar), `RLIMIT_NOFILE`
  (read and *written*), `available_parallelism` (1.1), `std::process::id()`
  (scratch naming, in three places: `ScratchDir`, the sparse store, and
  `OutputFile`).
- Process-global state: `inject_metrics` statics accumulate across calls.
  Its doc says this "matches brokkr's one-process-per-run model", which means
  a library caller running altw twice gets the first run's counts added into
  the second's.

All of these come in through the same missing seam: a `HostFacts` /
`RunContext` handed to the plan (2.3). The routing and fd-capping logic then
becomes pure and testable, and the inject metrics become per-run fields of
`Stats` or of the observer.

---

## 7. Shared state and locking

### 7.1 Router condvar paired with an unrelated mutex

`ConcurrentBlobLocationRouter` waits on `notify` with `notify_mu: Mutex<()>`,
while the predicate lives under per-slot mutexes and atomics. The comment
argues there is no missed wakeup because waiters re-check under `notify_mu`.
But publishers never take `notify_mu`: `publish_worker` sets the slot, drops
the slot lock, and calls `notify_all()` without holding `notify_mu`. A
publisher can therefore notify after the waiter's re-check and before it
enters `wait`, and that notification is lost. The waiter then sleeps until the
next publish, `mark_producer_done`, or `abort`. This is classic lost-wakeup
territory, and the safety rests on there always being another publication
later. The final `mark_producer_done` does guarantee one, so it should not
hang, but a stage 4 worker can stall for an arbitrary stretch of stage 3. Fix:
publishers take `notify_mu` around the slot update plus notify (or put the
predicate state under `notify_mu`), so safety follows from the structure and
not from timing. A loom test of `wait_ready` against `publish_worker` would
enforce it.

### 7.2 `SharedSparseWriter` is a raw pointer with hand-written `Send + Sync`

Soundness rests on "lifetime is bounded by the synchronous parallel scan"
(the comment), meaning on `mmap` outliving `writer` lexically in
`build_node_index_sparse`. Nothing ties the pointer's lifetime to the mmap.
Fix: borrow the mmap as `&[AtomicU64]` (cast once, with a lifetime) and drop
the `unsafe impl`s. The borrow checker then enforces it.

### 7.3 Test hooks are process-global

`stage3::test_hooks::PANIC_AT_BUCKET_IDX` is a `static` that any concurrently
running test in the same binary inherits. Correctness depends on tests not
running in parallel or always resetting. Pass the hook through a test-only
field of the plan instead.

### 7.4 Two error slots with different policies in one stage

Stage 3 has both `s3_error` (first writer wins at join) and the router's
`abort_error` (first writer wins), and the workers write `err_ref` *and* call
`router.abort` (B4). Stage 2's `err_ref` is last-writer-wins. One
`FirstError` type would settle the policy once.

---

## 8. Unbounded growth

- `IdSet` growth from an input id (B3).
- External scratch on the output filesystem, with no precheck and no cap
  (2.3), plus orphaned pid directories that accumulate across crashes (5.2).
- Stage 4 pre-seeds the `ReorderBuffer` with every passthrough descriptor
  (about 40K at planet with `keep_untagged_nodes`). The size is sized
  deliberately, but it scales with input, not with a bound.
- `PerWayRcs` holds the whole per-way sidecar in RAM (`std::fs::read`) and
  `load_ref_count_sidecar` does the same. Both are proportional to way count.
  That is bounded by input, but it is never in the memory accounting the docs
  quote.
- `straddler_partials` and `BlobLocation::Straddler { bytes }` keep encoded
  straddler payloads in RAM until the end of the run. A few hundred at planet
  today, bounded by bucket count times two, so fine. Noted because the bound
  depends on the 2-piece straddler invariant, which is maintained by
  `slot_bucket_count` arithmetic in `external_join` and checked nowhere at
  runtime: `classify_blobs_in_bucket` errors only after the fact.
- Sparse pass 0 with `inject_prepass` builds a second full `IdSet`
  (`shared`) the size of `referenced`. That roughly doubles pass-0 memory, and
  the memory table never mentions it.

---

## 9. Code that is no longer load-bearing

Each item says what shows it is dead.

- `#[allow(dead_code)]` markers that say "wired up in step 2 of A1" on
  `BucketLayout`, its `impl`, `max_node_id_from_blob_meta`, `IdRecord` and
  its `impl`. All of these are called from production paths
  (`external_join`, stage 1, stage 2). The allows are stale and hide any real
  future dead code in those items. `bucket_width`'s allow says "exposed for
  testing", but `BucketLayout::new` calls it.
- `IdSet::count_in_range` and `IdSet::count_below`: no callers outside
  `idset.rs` (grep). The doc still says "Used by external join's stage 1 to
  compute per-node-blob referenced rank ranges", which describes the rank path
  that A1 removed.
- `IdSet::drop_rank_index` doc: "Used by stage 2 of the external ALTW join
  to free ~100 MB". It has no external callers. It is only used internally by
  `set` and `merge`.
- `NodeBlobInfo` doc: "the half-open rank range `[ref_rank_start, ref_rank_end)`".
  Those fields no longer exist. Stage 1's doc and `Stage1Output`'s doc still
  say the struct "Owns the `IdSet` (kept alive because stage 2 needs
  `rank_if_set`...)", and it owns no IdSet. Stage 1's fd comment refers to
  "Pass B below ... (see `stage1.rs` Pass B loop at lines 385-405)", but pass B
  was removed, and the line citation is exactly what the docs rules forbid.
  `raise_nofile_to_hard_cap`'s doc also cites "stage 1 pass B".
- `IdRecord` doc cites `.plans/merry-watching-lake.md`. `plans/` is retired
  and source must not cite transient documents.
- `relation_scan.rs` module doc: "The generic `collect_relation_member_node_ids`
  in `add_locations_to_ways.rs` reads every blob sequentially via
  `BlobReader::next()`". Both the name and the file are gone, and the generic
  version now uses the parallel classify scheduler, so the stated reason for a
  second implementation no longer holds (see 1.3).
- `coord_payloads.rs` comment cites "the current build_blob_location_router
  ... check at coord_payloads.rs:398". That function was deleted (mod.rs says
  "the deleted `build_blob_location_router`").
- Stage 4's `reframe_way_blob_with_locations` returns `missing_locations`
  (always 0) and `_new_slot_pos` (unused by the only caller).
- `BlobDescriptor::kind: Option<ElemKind>` in stage 4 is always `Some`, and
  the `expect` comment says so at length. Make it non-optional.
- `stage1_way_pass(_direct_io)`, `build_node_index_sparse(_direct_io)`,
  `collect_way_referenced_node_ids(_direct_io)`,
  `collect_relation_member_ids(_direct_io)`: the parameter is accepted and
  ignored on every pread path. `--direct-io` therefore has no effect on reads
  in passes 0 and 1, stage 1 or stage 2, and the signatures suggest otherwise.
  Remove the parameters so the CLI help for `--direct-io` can say what it
  really affects.
- `scatter_bucket_entries(_bucket_start, _bucket_end)`: unused parameters.
- `build_way_schedule` returns `Result` but cannot fail.
- `NodeIndex` is a one-variant enum in production, kept "as future-proofing".
  With a `CoordSource` trait (5.1) it disappears.
- `IndexType::External | IndexType::Auto => unreachable!` in
  `build_node_index`, and the `altw_index_kind` counter that emits `i64::MIN`
  for variants that cannot reach that code. Both exist because `IndexType`
  stays unresolved past dispatch. A `ResolvedBackend` enum without `Auto`
  makes them unrepresentable.
- `IndexType::from_str` "dense" arm: no in-repo caller of the library parse
  (1.9). It is public API, so whether it stays is a decision. If it does, it
  is the only copy that should exist.
- `radix.rs`'s module doc ("Shared scaffolding for 256-bucket radix-partitioned
  external joins ... Moved here from `src/commands/external_join.rs` during the
  renumber planet refactor") has one user. It is altw-private scaffolding
  carrying history that belongs in git.
- `scan/node.rs` module doc lists "ALTW dense pass 1" as a user. Dense was
  removed.
- `idset.rs` module doc: "Shared across commands that need planet-scale ID
  sets (extract, tags_filter)". The user list is incomplete (altw, renumber,
  check, geocode) and the planet memory figures in it are hard-coded. Its
  `rank` comments disagree with each other ("Max 7 words scanned" vs. "max 31
  words").

Enforcement: deleting the stale `allow(dead_code)`s lets rustc's
`dead_code` lint do the job. A text check in `brokkr check` for
`\.plans/|notes/|\.rs:[0-9]+` inside `src/` comments would hold the
no-citation rule.

---

## 10. Host assumptions and where it writes

### 10.1 Scratch goes in the output's directory, with no override

Both backends put scratch in `output.parent()`: sparse's `referenced * 8`
store (29 GB at europe) and external's ~224-256 GB directory. There is no
`--tmp-dir`, no `TMPDIR`, no library option. On a host where the output
volume is small or slow (network share, USB disk), the user has no choice. The
B2 bare-filename case is a symptom of the same derivation. Fix: a
`scratch_dir` in `AltwOptions`, defaulting to the output's directory (with
`""` mapped to `.`), validated in the plan (2.3).

### 10.2 Linux-only APIs used without gating

`external/*` uses `std::os::unix::fs::FileExt` and `libc::getrlimit` /
`setrlimit` with no `cfg`. `raise_nofile_to_hard_cap` assumes `rlim_t ==
u64` (true on Linux x86_64 and aarch64, not on every Unix). `sparse.rs`
gates `statvfs`/`posix_fallocate` on `target_os = "linux"` and falls back to
`set_len` elsewhere, which zero-fills sparsely and skips the free-space check:
a quiet degradation on macOS, where a full disk surfaces as SIGBUS on an mmap
store, not as an error. `page_cache_budget_bytes` returns `None` off Linux,
so auto routes external for every sorted indexed input there. That is
documented as "the cheap direction", but it is the opposite of right for
small inputs on a Mac. Say in the auto message that the reason was "no
/proc/meminfo".

### 10.3 Auto routing assumes `MemAvailable` describes the page cache this run gets

Under a cgroup memory limit (containers, systemd slices), `MemAvailable` is
the host's figure, not the cgroup's. A container with 8 GB on a 256 GB host
routes europe to sparse and thrashes, the exact failure auto exists to
prevent. Read `memory.max` / `memory.current` from the cgroup v2 hierarchy
and take the minimum. The 3 GiB anon headroom was measured at europe on one
host. It grows with the IdSet (planet: ~1.6 GB IdSet plus the shared set
under inject) and should be derived from the IdSet estimate.

### 10.4 Cache eviction picked at compile time

`posix_fadvise(DONTNEED)` on scratch files (stage 2 id shards and slot
buckets, stage 3 slot buckets) runs whenever the `linux-direct-io` *feature*
is compiled in, whether or not the user passed `--direct-io`. So the
page-cache behaviour of a default run depends on which features the binary
was built with, and the "consumer" sweep and the release build behave
differently with no flag in sight. Decide this at run time (an option), or
always (it is not specific to direct I/O).

### 10.5 RLIMIT side effect

`raise_nofile_to_hard_cap` raises the process soft limit from inside a library
function and never restores it. A host application that relied on its own
limit (some do, to bound socket counts) finds it changed. Raise it in the CLI,
or restore it on exit from `external_join`. Stage 1's fd math also assumes 256
shards per worker are open at once. That is by design of pass A, and the
assumption goes away if workers write through a per-bucket shared appender
(stage 2 already uses that shape for slot buckets).

### 10.6 Write locations

Writes in this scope are: the output (via `OutputFile`/`DestinationGuard`),
the sparse store (unlinked immediately, good), and `ScratchDir` under the
output directory. None go into `target/` or another tool's directory. The
open issue is ownership of the output directory: scratch lands as hidden
dotfiles next to the user's file. That is invisible in a normal `ls`, it
survives a crash, and nothing ever reaps it (5.2). Either reap `.pbfhogg-*-<pid>`
entries whose pid is dead at startup, or move scratch to an explicit,
documented location (10.1).

### 10.7 Sorted-input assumptions that are trusted but never checked

`scan/node.rs` and `scan/way.rs` document that blob filtering relies on
indexdata `kind` being honest (one kind per blob). Stage 2's merge walk
assumes node blobs are id-sorted and non-overlapping, and checks monotonicity
only under `debug_assertions`. If a sorted-flagged file is not sorted, the
partition_point slice in stage 2 is wrong and records silently become orphans,
which turn into missing locations, then the refusal. So the failure is a
misleading "missing locations" error, not "input not sorted". Stage 1 could
check `prev.max_id < next.min_id` across node blobs in `build_node_blob_mapping`
at no cost (it already loops them) and refuse with the right message.
