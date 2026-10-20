# Hygiene hunt: read path

Scope: `src/read/*`, `src/reorder_buffer.rs`, `src/blob_meta/`. Findings are
grouped by the ten questions and are deliberately not ranked. Each finding says
what tells us it is real and whether the fixed version could be held
mechanically.

Existing enforcement worth knowing first: clippy `unwrap_used = "deny"` and
`cognitive_complexity = "deny"`, the `all` / `consumer` / `minimal` check
sweeps (the `minimal` sweep is what keeps a no-`commands` build compiling), and
feature gating. Nothing in the build today checks values, channels, or
directory ownership, so most of what follows can only be held by a new rule.

---

## 1. One value, one owner

### 1.1 Decode-thread-count policy is spelled at about twenty sites, and the copies have already drifted

The rule "how many decode threads by default" is reimplemented wherever a
pool gets built:

- `read/pipeline.rs` `run_pipeline`: `available_parallelism() - 2`, min 1,
  fallback **4**.
- `read/pipeline.rs` `run_pipeline_fused`: the same code again, copied.
- `read/reader.rs` `par_map_reduce`: `available_parallelism() - 1`, min 1,
  fallback **3**.
- Outside scope but the same rule: `scan/classify.rs`, `geocode_index/builder/pass1_5.rs`,
  `commands/extract/{common,simple x2,multi x2}.rs`, `tags_filter/mod.rs`,
  `renumber/relations.rs`, `apply_changes/rewrite.rs`,
  `merge_changes/mod.rs`, `diff/derive.rs`, `altw/passthrough.rs`,
  `altw/external/{mod,stage1,stage2,stage3,stage4}.rs`.

The two in-scope engines already disagree (reserve 2 vs reserve 1, fallback 4
vs 3). Each reservation is justified locally by its own topology, which is
fine, but the fallback for "host did not tell us" is not topology and has no
reason to differ. Nothing answers "how many threads will pbfhogg use on this
host" without reading every site, and there is no single place where an
operator override (a `--threads` flag, a cgroup-aware count) could be plugged
in.

Fix: one `fn default_workers(reserved: usize) -> usize` in the read module (or
a `Parallelism` value handed into every engine from the CLI), and the fallback
constant defined once. Enforceable with a `clippy.toml`
`disallowed-methods = ["std::thread::available_parallelism"]` plus a single
`#[allow]` at the owner. That is a cheap, exact rule this build can already
run.

### 1.2 Reorder-buffer depths are bare literals at every caller

`ReorderBuffer::with_capacity(32)` / `(64)` appear in about fifteen command
pipelines (`cat`, `repack`, `degrade`, `time_filter`, `getparents`,
`tags_filter`, `extract/{common,multi}`, `renumber/{pass1,stage2,relations}`,
`altw/external/stage1`), plus `DEFAULT_DECODE_AHEAD = 32` and
`DEFAULT_READ_AHEAD = 16` in `pipeline.rs`, `BLOCK_QUEUE = 8` in `reader.rs`,
and `WRITE_AHEAD` on the write side. The 32s and 64s are not linked to the
decode-thread count they are supposed to cover, and they are not linked to
each other. See also 5.1: the deeper problem is that every one of those sites
is a hand-rolled pipeline.

Enforceable: once the engine is shared (5.1), the literal disappears with the
copies. Before that, a text rule forbidding a numeric literal argument to
`ReorderBuffer::with_capacity` would hold it.

### 1.3 The v1 indexdata size is spelled as a literal next to its named constant

`blob_wire.rs` `WireBlobHeader::parse` accepts `len == crate::blob_meta::INDEX_SIZE || len == 26`
and the warning text says "expected 26 or 42". `blob_meta/mod.rs` defines
`INDEX_SIZE_V1 = 26`, but private, so the wire parser cannot name it. The
accepted-length set is a property of the index format and belongs to
`blob_meta` as one function (`BlobIndex::is_recognised_len`), not to the
header parser. See 4.6 for the correctness consequence of the padding this
check feeds.

Enforceable: making the only constructor of the inline index buffer live in
`blob_meta` (a `RawIndex::from_wire(&[u8]) -> Option<RawIndex>` type) makes a
second spelling unrepresentable.

### 1.4 The 256 KiB read buffer is spelled twice

`file_reader.rs` `BufReader::with_capacity(256 * 1024, f)` and
`direct_reader.rs` `BUF_CAPACITY = 256 * 1024`, whose doc says "Matches the 256
KB BufReader capacity used elsewhere". The "matches" is a comment, not a
reference. One `const READ_BUF_CAPACITY` in `file_reader.rs` read by both.
Enforceable by a test asserting the two are equal only if both stay; better to
delete one copy.

### 1.5 The datasize cap check is implemented twice, by design comment

`BlobReader::read_blob_header` carries "the inline twin of the check in
`parse_blob_header_with_index`". The two-copies situation is stated rather than
removed. The header length guard was already consolidated into
`frame::check_header_len`; the datasize guard (negative check plus `>=
MAX_BLOB_DATASIZE`) should go the same way, into `WireBlobHeader::parse` or a
`frame::check_datasize`, so that every frame decoder gets it from one place.
See 5.2 for the larger version of this.

### 1.6 Error constructors re-spelled per site

`crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(...)))`
is written out in full at many read-path sites (`pipeline.rs` x5,
`reader.rs` x3, `indexed.rs`, `header_walker.rs` x5). Each site chooses its own
wording and its own `io::ErrorKind`, which is how 4.2 and 4.3 happened. One
small set of constructors in `error.rs` (`internal(msg)`, `io_at(path, e)`)
would own the choice. A text rule (`disallowed-methods` on
`std::io::Error::other` outside `error.rs`) can hold it.

---

## 2. Values nobody can find, change, or trust

### 2.1 Pipeline tunables are scattered and only half exposed

The read path has these knobs: `DEFAULT_READ_AHEAD`, `DEFAULT_DECODE_AHEAD`,
`BLOCK_QUEUE`, `PAR_INFLIGHT_BUDGET` (256 MiB), `PAR_BATCH_MAX_BLOBS`,
`PAR_BATCH_MAX_BYTES`, `MAX_RETAINED_CAPACITY` and `MAX_POOL_SIZE` in
`decompress.rs`, `HEADER_PROBE_SIZE` and `SAMPLE_CAP` in `header_walker.rs`,
`FULL_SCAN_ARM_MIN_BLOBS`, `DEFAULT_MAX_WINDOW`, `GRID_MAX_INDEX_BYTES`, plus
the thread rule of 1.1. They live in six files. `ElementReader` exposes
`read_ahead`, `decode_ahead` and `decode_threads`; the `par_map_reduce` budget
and batch sizes have no injection point even though `par_fold_blobs` already
takes them as parameters (the public method just passes the constants), and
`BLOCK_QUEUE` for `into_blocks_pipelined` is fixed. Nothing lists the set.

Fix: one `ReadTuning` struct (all of the above that are policy rather than
format limits), defaulted in one place, carried by `ElementReader`, documented
in `reference/pipeline.md`. Enforceable structurally: engines take the struct,
not constants, so a new knob has nowhere else to live. A doc test that prints
`ReadTuning::default()` keeps the reference page honest.

### 2.2 `PipelineConfig` clamping lives in the builder, not in the type

`read_ahead`/`decode_ahead` clamp `n.max(1)` in `ElementReader`'s setters, and
`AdmissionGate::new` clamps again with `cap.max(1)`. `sync_channel(0)` is a
rendezvous channel, not an error, so a `PipelineConfig` built any other way
(crate-internal callers construct it directly) gets different semantics. Use
`NonZeroUsize` fields; the type then holds the rule.

### 2.3 `par_map_reduce` and `for_each` silently ignore `with_blob_filter`

`ElementReader::with_blob_filter` documents that "the pipeline skips
decompressing blobs whose element type does not match". `for_each`
destructures `Self { blob_iter, header, .. }` and drops the filter;
`par_map_reduce` never reads it either. A library user who sets a filter and
calls either method gets every element, with no error and no warning. This is
configuration accepted at build time and then not honoured on one path - the
same class as a typo found hours in, except it is never found.

Fix: either apply the filter in all three engines (it is cheap: the check is
`should_skip_blob`) or make the filter a parameter of the pipelined methods
only, so the unfiltered methods cannot be handed one. Enforceable by the type:
a `FilteredReader` that only offers the methods that honour it.

### 2.4 The page-cache SEQUENTIAL hint depends on unrelated features

`FileReader::buffered` issues `POSIX_FADV_SEQUENTIAL` only under
`cfg(all(target_os = "linux", any(feature = "linux-direct-io", feature = "linux-io-uring")))`.
`libc` is an unconditional dependency (Cargo.toml), and `blob.rs` issues
`POSIX_FADV_DONTNEED` under plain `target_os = "linux"`. So the default
library build reads without the readahead hint, and the CLI (which enables the
features) reads with it - two read behaviours for the same call, chosen by a
feature that has nothing to do with buffered I/O. Any benchmark comparing a
library consumer to the CLI is comparing different I/O. Gate on
`target_os = "linux"` only. Enforceable with a test in the `minimal` sweep that
asserts the hint was issued (the same call-count trick `evict_calls` already
uses).

---

## 3. One channel, one implementation

### 3.1 A library writes to stderr directly

`blob_wire.rs` `warn_unrecognised_indexdata` uses `eprintln!` from inside the
library. It is the only production `eprintln!` in scope, and it is
well-reasoned (once per process, explains the consequence). But a library has
no business choosing stderr: a consumer embedding pbfhogg in a service cannot
route, suppress or count it, and the line names no file and no blob offset, so
on a multi-input command (`diff-snapshots`, `merge-changes`) the operator cannot
tell which input is degraded. The `Once` is also process-global: the second
file with the problem in the same process is never reported.

Fix: route through whatever channel the crate uses for operator diagnostics
(`crate::debug` already owns counters; a `warn` hook or the `log` facade), and
carry `source_path` + blob index, which `BlobReader` has at that moment (the
parse is called from `read_blob_header`, which has `self.source` and
`self.offset`). Better still, make it a counter (`indexdata_unrecognised`) plus
a once-per-reader warning, so `check --index` and the sidecar can both see it.
Enforceable: `clippy::print_stderr` / `print_stdout` as `deny` in the library
crate (not the CLI), with a single allowed site in the owner.

### 3.2 Pipeline metrics are one process-global static

`PIPELINE_METRICS` is a `static`, never reset, and `emit()` is called at the
end of every `run_pipeline` / `run_pipeline_fused`. Consequences:

- A command that runs two pipelines sequentially (two-pass commands, getid
  then something else, geocode passes) emits cumulative counters twice; the
  second emit includes the first pass's wait times and task counts, and the
  high-water marks are max-over-process, not per pass.
- Two concurrent pipelines (`into_blocks_pipelined` on two files, the zipping
  use case the doc advertises; `diff`) add into the same atomics, so neither
  emit describes either pipeline.
- `par_map_reduce` and sequential `for_each` emit nothing, so the sidecar
  silently has no read-side counters for commands built on them.

The module doc says the counters "mirror" `write/metrics.rs`; if the writer
has the same static, it has the same bug. Fix: a `PipelineMetrics` value
owned by each run (an `Arc` handed to the stages), emitted with a pass label,
or reset-then-emit with a pass marker if the global is kept for cost reasons.
Enforceable by a test that runs two pipelines and asserts the second emit is
not cumulative.

### 3.3 Significant events with no diagnostic at all

- A blob filter that skips blobs is counted (`blobs_skipped_by_filter`), but a
  filter that skips nothing because the file has no indexdata is silent; that
  is the common "why is my extract slow" case and it is detectable at the
  first OsmData blob.
- `DecompressPool` dropping oversize buffers and refusing returns past
  `MAX_POOL_SIZE` is uncounted; the pool's effectiveness is invisible to the
  sidecar, which is otherwise the place this project answers allocator
  questions.
- `PipelinedBlocks::drop` discards the joined thread's panic payload (`drop(h.join())`).

---

## 4. Errors

### 4.1 Corrupt element data is swallowed into defaults or empty iterators

Read-path decode is lazy, and several lazy accessors convert a parse failure
into a plausible value:

- `elements.rs` (three sites): `WireInfo::parse(data).ok().unwrap_or_default()`
  - corrupt metadata becomes version 0 / timestamp 0, indistinguishable from
  absent. `block.rs` `validate_info` has a doc comment that says exactly
  this, so it is known.
- `block.rs` `PrimitiveGroup::dense_nodes` (and the twin around the second
  `DenseNodeIter::empty` site): a `WireDenseNodes::parse` error yields an
  **empty iterator**. A corrupt dense group reads as "this group has no
  nodes".
- `dense.rs` `DenseNodeIter::new`: `WireDenseInfo::parse(data).ok()` drops
  all dense metadata on corruption.
- `dense.rs` tag iterators: `read_varint().ok()?` ends the tag list early on
  corruption; the keys/vals scan sets `kv_pos = len` on a bad varint.
- `wire.rs` (`read_tag().ok()??`) and `blob_meta/tag_index.rs` /
  `scan_ids.rs` (`read_tag().ok().flatten()` loops) end on the first bad tag.

Whether this is safe depends entirely on whether `PrimitiveBlock::from_vec*`
validated the same bytes up front. `block.rs` has a validation pass
(`validate_info`, member-array length checks), so some of these paths are
"cannot happen after construction" - but that is an invariant held by a
different function in a different file, and nothing ties the lazy accessor to
it. For `blob_meta/` there is no upfront validation: `scan_ids` and
`tag_index` return `None` / a truncated result, and a truncated tag index
written by the write path is an under-claiming index, which a tag filter then
uses to *skip* blobs. That direction is a wrong answer, not a degradation.

Fix: either the accessors return `Result` (honest, noisy API), or construction
produces a validated type whose lazy accessors are infallible by construction
(and the `.ok()` sites become `expect`-free `match` on states that genuinely
cannot occur, with a debug assertion). For `blob_meta`, a parse failure while
*building* an index must be an error, never a short index. Enforceable: a
`disallowed-methods` rule on `Result::ok` inside `src/read/` and
`src/blob_meta/` with per-site `#[allow(reason = ...)]`, which forces each
swallow to state its justification.

### 4.2 `HeaderWalker::open` destroys the `io::ErrorKind`

`header_walker.rs` wraps the open error as `io::Error::other(format!("failed to open {}: {e}"))`.
`file_reader.rs` has `open_error`, whose doc says it exists precisely to keep
`ErrorKind` "so callers matching on `NotFound` still match". The two
implementations of the same rule have already diverged: a missing input opened
through the walker reports `Other`, through `FileReader` reports `NotFound`.
Fix: one `open_error` (move it to `error.rs` or make it `pub(crate)`) used by
both. Enforceable by the same `disallowed-methods` rule as 1.6.

### 4.3 Non-I/O failures are reported as `ErrorKind::Io(Other)` with no subject

`pipeline.rs`: "decode task panicked" (no seq, no blob offset, no path; the
panic payload is discarded), "failed to build decode pool", and the fused
transform's `String` error wrapped as `Io(Other(error))`. `reader.rs`:
"pipeline consumer dropped", "pipeline thread terminated without completing
(panicked)". `indexed.rs`: "IndexedReader requires a seekable BlobReader".
These are internal faults, not I/O, and a caller matching on `ErrorKind::Io`
to decide "retry / bad disk" will misfire. The decode-panic site has the blob
in hand (`blob.offset()`) and the pipeline knows the path, yet the error names
neither - on planet that is a 2.5 M-blob haystack.

The fused transform signature `Fn(PrimitiveBlock) -> Result<T, String>` is the
structural cause for that one: a typed error cannot get through. Make it
`crate::error::Result<T>`.

Fix: an `ErrorKind::Internal { what, blob_offset }` (or reuse the existing
`at`/`in_input` stamping on these errors) and keep panic payloads as the
message. Enforceable: once `ErrorKind::Internal` exists, the text rule of 1.6
covers it.

### 4.4 Stamping is inconsistent across frame decoders

`BlobReader` stamps path + blob index + byte offset on most paths, but its two
"genuine I/O error while reading the length prefix" branches stamp path and
index without the offset (they set `offset = None` and skip `err.at`).
`HeaderWalker` stamps path + blob index but never the byte offset (it has
`frame_start`). `raw_frame.rs` returns `BoxResult` with no stamping at all, so
`cat`, `extract`, `check --index`, `getid` raw paths report frame errors with
no path and no offset. Three decoders, three provenance policies. This folds
into 5.2.

### 4.5 `read_raw_frame` advances the caller's offset before the read succeeds

`*file_offset += frame_len` precedes `reader.read_exact(...)?`. On a truncated
payload the error propagates with the offset already past the frame. Any
caller that reports or resumes from `file_offset` after an error points past
the failure. Move the increment after the read.

### 4.6 Zero-padding a 26-byte indexdata can manufacture a real bbox

`WireBlobHeader::parse` (and the frame path) copy a 26-byte indexdata into a
zeroed 42-byte buffer. `BlobIndex::deserialize` then trusts the version byte,
not the original length: a 26-byte field whose version byte says v3 and kind
Node is read with bbox words `[0, 0, 0, 0]`, which under v3 is a *real* box at
Null Island (`BBOX_ABSENT` is the inverted box, and v3 no longer treats zeros
as absent). A spatial filter would then skip that node blob for any region not
containing (0, 0). No conforming writer produces this, but the reasoning that
introduced v3 (absence must never collide with a real value) is undone by the
padding step. Fix: carry the original length (`RawIndex { bytes, len }`) and
reject a v2/v3 version byte on a 26-byte field. Enforceable by a unit test.
`deserialize` also accepts `min_id > max_id`; an inverted ID range makes every
range-based skip decision wrong in the "skip" direction.

### 4.7 `ReorderBuffer::push` panics on producer behaviour

The stale / duplicate / window-exceeded asserts are documented as
programming-error panics because seqs come from `enumerate()`. That holds for
the read pipeline. It does not hold structurally: the buffer is generic and
fifteen callers feed it, some from worker-computed sequence numbers. A
panicking consumer thread inside `std::thread::scope` is turned into a
re-raised panic at join, so a library caller gets a process abort for what
may be a scheduling bug. Returning `Err` from `push` and letting the engines
convert it into a pipeline error costs nothing at the read pipeline's call
site.

### 4.8 `DirectReader::skip` does not detect skip-past-EOF on an aligned target

When `aligned == target`, `skip` seeks and clears the buffer with no EOF
check. `FileReader::skip`'s doc says "`DirectReader::skip` already validates
EOF and errors on past-end, so no extra check is needed" - true only for the
unaligned branch. A truncated file whose truncation point is page-aligned
passes `skip` and surfaces later (or, for callers like `has_indexdata` that
the `FileReader::skip` doc names, never). This is exactly the shape-4 hole the
buffered arm closes with its sentinel byte. Fix: same sentinel approach, or
compare against file size. Testable with an aligned truncated fixture on the
`platform` profile.

---

## 5. Policy invented per call site

### 5.1 Ordered parallel decode is hand-rolled about fifteen times

The read path has a careful ordered pipeline (admission gate, permit riding
with the item, shutdown flag, panic capture, reorder buffer, metrics), and
then a near-verbatim copy of it, `run_pipeline_fused`, which the comment says
to "keep additive". The two copies have already drifted:

- the error-forwarding branch records `decoded_send_wait_ns` in
  `send_direct_error` even on failure, but the fused copy records it only on
  success;
- the plain engine spawns tasks with `ThreadPool::spawn`, the fused one with
  `in_place_scope`, so the two have different lifetime and drain semantics;
- the comment block explaining the `raw_rx` drop-ordering invariant exists in
  one copy only.

Outside this scope, the `ReorderBuffer` call sites list (1.2) is a list of
further pipelines that each re-establish the same invariants, which the
`reorder_buffer.rs` doc confirms in its own words ("every one of the twenty
call sites had to re-establish for itself"). The right move is one ordered
engine, generic over the per-blob transform (the plain engine is the fused
one with `transform = Ok`), with the frame source and the work item as type
parameters so the command-local pipelines can use it too. That is a big
rewrite and the payoff is real: every bounded-memory, shutdown and
panic-safety fix currently has to be made N times. Enforceable afterwards by
a dependency rule: `ReorderBuffer` becomes private to the engine module, so a
new hand-rolled pipeline cannot be written.

### 5.2 Three frame decoders for one frame format

`BlobReader::read_blob_header` (streaming, stamps offsets),
`frame::read_frame_header` (streaming, used by `raw_frame.rs`), and
`HeaderWalker::next_header_inner` (pread). `frame.rs` is called "the one owner
of frame policy", but it owns only the length-prefix guard; the EOF policy
(1-3 trailing bytes tolerated), the datasize guard (1.5), the truncation
messages ("shape 3" / "shape 4" text is spelled separately in `BlobReader` and
`HeaderWalker` with different wording) and provenance stamping (4.4) are each
reimplemented. `reference/truncation-handling.md` is the spec all three cite,
and only tests hold them to it.

Fix: a frame-decoding state machine over an abstract byte source
(`Read`-stream and `pread`-at-offset are two impls of one trait), returning a
stamped `FrameHeader`. Every caller then gets the same EOF policy, the same
guards and the same stamping. Enforceable by a shared conformance test that
runs every truncation shape through every source impl (the `blob.rs` test
with `read_arm` / `skip_arm` / `walk_arm` is already this test in embryo).

### 5.3 Header-type validation differs between sequential and pipelined `for_each`

Sequential `for_each` debug-asserts strictly increasing node ids
(`id > last_node_id`) whenever the file is sorted; `for_each_pipelined` uses
`>=` for history files. The sequential path therefore fires its debug assertion
on any sorted history file in debug builds. One rule, two spellings, already
diverged. Extract `fn check_sorted_order(header) -> impl FnMut(i64)` and use it
in both. Testable with a sorted history fixture in a debug build.

### 5.4 Advisory syscalls ignore their return values ad hoc

`posix_fadvise` is called at three sites (SEQUENTIAL in `file_reader.rs`,
RANDOM in `header_walker.rs`, DONTNEED in `blob.rs`) and each discards the
return code with its own comment. That is correct policy for hints, but it is
three statements of it; one `fn advise(fd, off, len, advice)` that counts
failures would own it and make 2.4 trivially fixable.

---

## 6. Ambient dependencies

- Thread count from `available_parallelism()` inside the engines (1.1), so a
  test cannot exercise the default path on a chosen topology, and a cgroup
  limit is honoured only to the extent std honours it.
- `Instant::now()` around every channel op feeds `PIPELINE_METRICS`; harmless
  for correctness, but metrics are a global (3.2) rather than handed in, which
  is what makes them untestable per run.
- `test-hooks` statics (`BLOCK_DECODE_SEQ` etc.) are global too, so two tests
  using them cannot run in parallel; they rely on `serial` placement. A hook
  object passed in the engine config would remove that constraint.

---

## 7. Shared state and locking

### 7.1 `evict_fd` is a copied raw fd whose validity rests on field co-location

`BlobReader` stores `evict_fd: Option<RawFd>` obtained from `self.reader`.
Safety ("fd is valid (owned by FileReader in same struct)") holds today
because `reader` is never replaced. Nothing structural stops a future
`set_reader` / `into_inner` / `mem::replace` from leaving `evict_fd` pointing
at a closed or reused fd, and `posix_fadvise(DONTNEED)` on a reused fd evicts
someone else's cache. Make eviction a method on `FileReader` (it owns the fd)
and store only a bool.

### 7.2 Global page-cache side effect by default

Eviction is on by default for the buffered constructors, and it re-advises the
whole consumed prefix `0..offset` per blob (the source comment notes it is
unmeasured and tracked in TODO.md). For a library this is a process-external
effect: it evicts pages another process (an `osmium` alongside, a second
`pbfhogg` pass) is using. It is documented, and opt-out exists, but the
default is the surprising direction for a library consumer and the right
direction for the CLI. Consider making the CLI opt in rather than the
library opt out.

### 7.3 Two process-global statics without owners

`PIPELINE_METRICS` (3.2) and the `warn_unrecognised_indexdata` `Once` (3.1).
Both turn per-run facts into per-process facts.

---

## 8. Unbounded growth

- `ReorderBuffer::with_capacity(capacity)` sets `max_window =
  DEFAULT_MAX_WINDOW.max(capacity)`, and `altw/passthrough.rs` passes
  `passthrough_items.len() + decode_threads`, so its window is as large as the
  input. `with_max_window` has no production caller (only tests), so the
  "callers that know their exact permit count can pin it" escape is unused and
  every buffer runs at 65 536 slots.
- `BatchQueue` is unbounded in slot count by design (memory bounded by
  `ByteBudget`), but `ByteBudget::acquire` admits unconditionally when
  `used == 0`, and `release` uses `saturating_sub`, so an accounting bug
  degrades silently to "no budget" instead of failing. A `debug_assert!(used >= n)`
  in `release` would catch it.
- `IndexedReader` keeps a `Vec<BlobInfo>` for the whole file and its
  `read_ways_and_deps` collects referenced node ids into a `BTreeSet<i64>`
  (documented as potentially billions of entries). Documented, but there is
  no guard or refusal; at planet scale the failure is an OOM kill.
- `HeaderWalker` probe reads trust `header_len` up to `MAX_BLOB_HEADER_SIZE`
  (bounded), fine. `WireBlob::parse_slice` copies the whole input into new
  `Bytes`; bounded by the datasize cap.

---

## 9. Code that is no longer load-bearing

- `header_walker.rs` `pread_exact` and `read_blob_data`: `#[allow(dead_code)]`,
  no callers anywhere in `src/` or `cli/src/` (grep). Dead; delete.
- `raw_frame.rs` `RawBlobFrame::{index_present, tagdata, file_offset}`: marked
  `#[allow(dead_code)]` but read by `commands/check/index.rs`. They are dead
  only in a no-`commands` build. The blanket allow hides real deadness in the
  `all` build too; use `#[cfg_attr(not(feature = "commands"), allow(dead_code))]`.
  The `tagdata` doc comment ("current readers ... don't access it directly")
  is stale: `check --index` reads it.
- `ReorderBuffer::with_max_window` is test-only in practice (8 above).
- `block.rs` carries another `#[allow(dead_code)]` item; worth checking with
  the same grep.
- `IndexedReader` builds its own per-blob node/way id ranges by full decode
  and ignores `BlobHeader.indexdata`, which carries exactly that (plus
  relations, which `IndexedReader` does not track at all). No command uses
  `IndexedReader` (grep over `src/commands`, `src/scan`, `cli/src`); it is a
  public API kept from the osmpbf heritage. Either rebuild it on `BlobIndex`
  (and get relation ranges and no-decode indexing for free) or retire it; as
  it stands it is a second, weaker index implementation that will be counted
  as a precedent.
- `WireBlob::parse_slice` and `pool_get_pub` (an exact duplicate of
  `pool_get(Some(..))`) are worth a caller check; `pool_get_pub` is a pure
  duplicate regardless.
- `BlobReader::new` defaults `parse_indexdata: true` "for compatibility";
  every engine sets it explicitly. The default exists for direct `BlobReader`
  users; keep or drop deliberately.

Enforceable: `#[allow(dead_code)]` without `reason = ...` can be denied with
`clippy::allow_attributes_without_reason`, which forces each one to say why it
is not dead.

---

## 10. Where production code assumes it runs, and what it touches

### 10.1 A unit test writes into brokkr's scratch directory and removes it

`direct_reader.rs` test `direct_reader_reads_file` writes
`$CARGO_MANIFEST_DIR/data/bench-tmp/direct_reader_test.bin` and then calls
`remove_dir(&dir)`. `data/bench-tmp` is brokkr's configured `scratch`
(`brokkr.toml`), which is per-host configurable (one host entry points
elsewhere), so the test hardcodes one host's copy of brokkr's setting. It
writes into a directory another tool owns, with a fixed filename, so parallel
sweeps (recently enabled, commit `b353de8`) race on the same file, and when
the directory happens to be empty the test deletes brokkr's scratch dir.
Fix: `tempfile::tempdir_in` under the test's own location; the header walker
tests already use `tempfile::tempdir()`. The O_DIRECT case needs a
non-tmpfs filesystem, so the location should come from the test harness
(`BROKKR_TEST_BIN_DIR`-style env var or `CARGO_TARGET_TMPDIR`, which cargo
owns for exactly this purpose). Enforceable by a text rule banning
`CARGO_MANIFEST_DIR` joined with `data` in `src/`.

### 10.2 O_DIRECT alignment is a hardcoded 4096

`PAGE_SIZE = 4096` "universally safe across Linux filesystems". O_DIRECT
alignment is the device's logical block size and the filesystem's DIO
alignment, which Linux reports via `statx(STATX_DIOALIGN)` (6.1+). Devices
with larger logical blocks get `EINVAL` (a refusal, at least), and the skip
arithmetic assumes 4096 too. Detect via `statx` with 4096 as fallback.

### 10.3 Linux-only APIs in modules that are not gated as such

`header_walker.rs` uses `std::os::unix::fs::FileExt` unconditionally and
`raw_fd` is unix-typed; `fadvise` is `target_os = "linux"` gated. The crate is
effectively Unix-only on these paths without saying so in `Cargo.toml` or a
`compile_error!`. A non-Unix build fails to compile rather than degrading,
which is the honest outcome; worth stating explicitly.

### 10.4 Default page-cache eviction (7.2) is the read path touching state it does not own

The page cache is shared host state; see 7.2.

---

## Lateral findings

- `run_pipeline` documents "decode admission bounded by `decode_ahead`" but the
  decoded channel is *also* `decode_ahead` deep, so the effective in-flight
  ceiling is admission-bound; fine, but the two uses of one number for two
  meanings is what makes it easy to break.
- `PipelinedBlocks` "pipeline consumer dropped" is returned as an `Err` into
  `run_pipeline` on every early drop, so the error path runs (and
  `PIPELINE_METRICS.emit()` emits) on normal early exit; an early-exit
  run is indistinguishable from a failed run in the sidecar.
- `BlobReader` truncation messages embed "(shape 3)" / "(shape 4)", jargon
  from `reference/truncation-handling.md` that an operator reading the error
  cannot decode; the message should say what is truncated and where, and the
  shape number belongs in the error kind.
- `elements.rs` default metadata on corrupt `Info` (4.1) means `time-filter`
  and history handling can silently treat a corrupt element as timestamp 0.
