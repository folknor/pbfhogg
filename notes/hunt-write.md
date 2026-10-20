# Hygiene hunt: write path (`src/write/*`)

Scope: block builder, string table, encoding, compression, framing, header
builder, copy-range, `PbfWriter` and its four backends, parallel gzip, the
write pipeline and write metrics. Plus how `src/commands/` constructs writers.
Findings are unranked. "Enforceable by" closes each one.

Existing enforcement this build already pays for, and which the findings below
lean on: the workspace clippy deny list in `Cargo.toml` (no `expect_used`, no
`print_stderr`, no `disallowed_methods`/`disallowed_types` configured, and no
`clippy.toml` exists); compile-time `const _: () = assert!(..)` invariants in
`pipeline.rs` (`WRITE_AHEAD >= PIPELINE_DISPATCH_PERMITS`) and
`uring_writer.rs` (`NUM_BUFS <= 64`); `include_str!` source-scan tests in
`metrics.rs` (funnel-only writes for `sync_all_*`/`recv_wait_ns`, emission only
from the CLI exit hook); the `minimal`/`consumer`/`all` sweeps in `brokkr.toml`.
The source-scan test is the template most of the "text rule" answers below
would copy.

---

## 1. One value, one owner

### 1.1 The 256 KiB I/O buffer size is spelled five times in the write path
`FileWriter::buffered` (`BufWriter::with_capacity(256 * 1024, ..)`),
`direct_writer::BUF_CAPACITY`, `uring_writer::BUF_SIZE`,
`copy_range::copy_range_fallback` (`vec![0u8; 256 * 1024]`),
`parallel_writer::copy_range_fallback_pwrite` (same), plus the
`PbfWriter::new` doc that tells callers to use `256 * 1024`. Read side repeats
it (`read/direct_reader.rs`, `read/file_reader.rs`, `read/blob.rs`). The uring
and direct docs both claim to "match" each other and BufWriter, which is a
claim kept true by nobody. Not yet diverged.
Enforceable by: one `pub(crate) const IO_BUF_BYTES` in `write/mod.rs` (beside
`PAGE_SIZE`), a const assert that it is a multiple of `PAGE_SIZE`, and a
source-scan test (the `metrics.rs` pattern) rejecting the literal `256 * 1024`
under `src/write/` and `src/read/`.

### 1.2 `O_DIRECT` open spelled twice
`DirectWriter::create` and `uring_init_and_run` each build the same
`OpenOptions::new().write(true).create(true).truncate(true).custom_flags(libc::O_DIRECT)`
(and `read/direct_reader.rs` a third reader variant). One open policy (flags,
mode, what EINVAL on tmpfs means) in two places.
Enforceable by: `fn open_direct_for_write(path)` in `write/mod.rs` and a
`disallowed_methods`/text rule on `custom_flags(libc::O_DIRECT)` outside it.

### 1.3 `elapsed_ns_u64` exists three times, inlined twice more
`write/pipeline.rs::elapsed_ns_u64`, a private copy in `uring_writer.rs`,
`read/pipeline_metrics.rs::elapsed_ns_u64`, `apply_changes/streaming.rs::elapsed_ns`,
and the open-coded `u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)`
in `parallel_writer::worker_loop` and in `WriterMetrics::record_sync_all` /
`record_recv_wait`. Identical today.
Enforceable by: one crate-level helper (or better, a `Stopwatch` type whose
only exit is `record_into(&AtomicU64)`), plus a text rule on
`as_nanos()).unwrap_or(u64::MAX)`.

### 1.4 The 8000 default block cap is spelled twice
`block_builder::MAX_ENTITIES_PER_BLOCK = 8000` (private) and
`commands/degrade/mod.rs::DEFAULT_BLOCK_CAP = 8000` (pub). Plus "8000" in
roughly ten doc comments as a load-bearing number (string table, classify,
check, tags_filter memory bounds). Not diverged.
Enforceable by: make the block_builder constant `pub` and have degrade
re-export it; a const assert in degrade if a distinct value is ever wanted.

### 1.5 Feature-name strings: constant exists, six sites ignore it
`HeaderBlock::LOCATIONS_ON_WAYS` is defined in `read/block.rs`, yet
`"LocationsOnWays"` is spelled literally in `altw/external/stage4.rs`,
`altw/mod.rs`, `altw/passthrough.rs`, `apply_changes/rewrite.rs`,
`degrade/mod.rs`, `repack/mod.rs`, `export/mod.rs`, `error.rs` and
`cli/src/main.rs`. `HeaderBuilder::build` spells `"OsmSchema-V0.6"` and
`"DenseNodes"` as byte literals while using constants for
`SORT_TYPE_THEN_ID` / `HISTORICAL_INFORMATION`; `inspect/report.rs` repeats
both required-feature literals to filter them out. A typo here produces a
file whose header advertises nothing and is read as un-enriched.
Enforceable by: `HeaderBuilder::optional_feature` taking a `Feature` enum (or
a `&'static FeatureName` newtype with associated consts) so a free string is
unrepresentable; add `OSM_SCHEMA_V06` / `DENSE_NODES` constants; text rule on
the quoted literals outside `read/block.rs`.

### 1.6 Protobuf field numbers are bare integers on the write side
`encode_blob_body` (Blob fields 1, 2, 3, 7), `encode_blob_header_fields`
(BlobHeader 1-5), `HeaderBuilder::build` (HeaderBlock 1, 4, 5, 16, 17, 32-34,
HeaderBBox 1-4), the string table (1). The read side has its own spelling in
`read/blob_wire.rs`. The degrade strippers (`strip_blob_header_fields`) take
field numbers from callers as `&[u32]`. Two independent spellings of one wire
schema, one per direction.
Enforceable by: a `wire_schema` module of named consts shared by reader and
writer; `strip_*` taking a typed field enum. A round-trip proptest already
partly holds this; a const table makes it structural.

### 1.7 Two copies of the blob framing algorithm
`PbfWriter::write_framed_blob` (sync path) re-implements `frame_blob_into`
line for line: compress, `MAX_BLOB_DATASIZE` guard, datasize/i32, header encode,
u32 header_len, metrics. The test `the_sync_path_refuses_an_oversize_blob_body`
records that they already diverged once (the size guard landed in one copy
only). `reframe_raw_with_index_scratch` is a third framing assembly that
skips the `MAX_BLOB_DATASIZE` check entirely (a passthrough blob that came in
oversize goes out oversize - arguably right for passthrough, but nobody
decided it).
Enforceable by: one `frame_parts(blob_type, body_len, ..) -> FramedHeader`
that every path calls, sync writing the three parts directly. Structural.

### 1.8 Three copies of the `write_primitive_block*` rayon-spawn body
`write_primitive_block`, `write_primitive_block_no_indexdata` and
`write_primitive_block_owned` each carry ~70 identical lines: writer-gone check,
permit recv with its own error string, metrics, `rayon::spawn` with
`PermitGuard`, frame, metrics, send, `writer_gone.record()`. The test
`every_primitive_block_variant_stops_once_the_writer_thread_is_dead` exists
precisely because a fix to one did not reach the others. The permit acquire is
also duplicated against `acquire_permit` (which the raw paths use) rather than
calling it. Comments in two copies point at a `PIPELINE_DISPATCH_PERMITS doc
comment` while the block in the first copy is visibly two comments pasted
together.
Enforceable by: one private `dispatch_framed(FrameRequest)` where
`FrameRequest` owns bytes plus `IndexSource::{Scan, Precomputed(BlobIndex),
None}`. Structural; the three public methods become one-liners.

### 1.9 Four constructors repeat the pipeline assembly; two repeat the init handshake
`to_path_uring` and `to_path_parallel` contain the same spawn + `init_rx`
match block (with messages that differ only by the word "parallel"), and all
four constructors build the same `WritePipeline { .. }` literal; the test
helper builds a fifth copy.
Enforceable by: `WritePipeline::spawn(|rx, expected, init_tx| ..)` returning
the struct. Structural.

### 1.10 Four writer-loop copies of the reorder/drain and per-chunk metrics
`pipeline::writer_thread`, `uring_writer::uring_main_loop`,
`parallel_writer::dispatch_loop` each re-implement recv / `record_recv_wait` /
push / high-water / pop_ready / `check_stream_complete`; and every backend
re-implements the `OutputChunk` match with `write_ns` + `bytes_written`
bookkeeping (FileOutputSink, uring, parallel). The ADR
`decisions/0014-output-backends-stay-separate.md` argues against unifying the
*backends*, but the *loop* is not backend logic; the uring loop could be an
`OutputSink` whose `write_chunk` does the opportunistic `reap_cqes(false)`.
This duplication has already diverged in meaning (see 3.5: `bytes_written`
counted at dispatch in parallel, at completion elsewhere; `write_ns` is wall in
two backends and a 16-thread sum in the third).
Enforceable by: make `writer_thread<S: OutputSink>` the only loop; extend
`OutputSink` with `on_idle()` for uring. A source-scan test that only
`pipeline.rs` contains `pop_ready()` under `src/write/`.

### 1.11 Three `copy_file_range` loops, three `pread` loops, diverged policy
`copy_range::copy_range` (implicit out offset), `parallel_writer::copy_range_to_fd`
(explicit out offset), plus fallbacks `copy_range_fallback` and
`copy_range_fallback_pwrite`, plus `uring_writer::handle_copy_range_uring`'s
own pread loop. They have already diverged:
- EINTR: `copy_range_fallback_pwrite` retries EINTR on pread;
  `copy_range_fallback` does not; `handle_copy_range_uring` does not;
  neither `copy_file_range` loop retries EINTR; `DirectWriter::flush_buf`
  retries on write (and its comment cites the pwrite fallback as precedent).
- Short read: copy_range loops on short pread; uring treats any short pread as
  a hard error ("pread short") even when it is a legitimate partial read.
- Fallback trigger: both loops fall back only on `EXDEV`. `EINVAL`,
  `EOPNOTSUPP`, `ENOSYS` (kernels or filesystems without cross-fs support,
  FUSE, some NFS) are hard errors.
- Cast: `copy_range` does `len as usize`; `copy_range_to_fd` uses
  `usize::try_from(..).unwrap_or(usize::MAX)`.
Enforceable by: one `splice::copy(in, in_off, Out::{Positional(off), Cursor}, len)`
module owning the syscall, the retry rule and the fallback set; a
`disallowed_methods` entry for `libc::pread`, `libc::copy_file_range` and
`libc::write` everywhere else.

### 1.12 Backend selection policy implemented twice, by two bools
`commands/mod.rs::writer_from_header_bytes` and
`writer_from_header_bytes_parallel` are the same function except the default
branch; `writer_from_header` / `_parallel` repeat the pair again. Backend
choice is `(direct_io: bool, io_uring: bool)`, so `true, true` is
representable and silently means uring. Commands then re-derive capability
from the same two bools: `sort` computes
`use_copy_range = io_uring || (!direct_io && cfg!(feature = "linux-direct-io"))`,
`apply_changes/rewrite.rs` computes `use_copy_range = io_uring || !direct_io`
without the `cfg!` - already diverged. The `sort` comment justifying it
("io_uring uses linked ReadFixed+WriteFixed for CopyRange") is stale: the uring
module doc records that chain was removed for producing mid-file zero gaps.
The parallel backend cannot be chosen by an operator at all; it is picked
per-command in code.
Enforceable by: `enum OutputBackend { Buffered, Parallel, Direct, Uring }`
parsed once from the CLI with conflicts refused, `PbfWriter::open(path, backend,
compression, header)`, and `PbfWriter::supports_copy_range()` as the only
capability query. The bool pair becomes unrepresentable.

### 1.13 `--elements-per-blob` validated twice, named in library code
`repack/mod.rs` refuses `--elements-per-blob must be > 0`, and
`BlockBuilder::with_element_cap_prealloc` asserts the same thing with the
same CLI flag name. Framing errors also name `--elements-per-blob` (and
`encode_blob_header_into`'s WayMembers message). Library users and every
other command (altw, apply-changes, extract) that hits the cap never had that
flag.
Enforceable by: `NonZeroUsize` for the cap (removes the assert and the second
validation); library errors carry a typed `BlobTooLarge { field, size, limit }`
and the CLI maps it to flag advice. Text rule: no `--` flag names in `src/`
outside `commands/*` argument parsing.

### 1.14 `Compression` level range spelled in three places, one wrong
The `Zstd` doc says "1-22"; `FromStr` accepts `-7..=22`. `Zlib(u32)` is
validated to `0..=9` only by `FromStr`; a library caller writing
`Compression::Zlib(12)` goes straight to `flate2::Compression::new(12)` in
`compress_zlib` (behaviour for >9 is backend-defined). `Zstd(level)` out of
range surfaces only at the first blob, on a rayon worker, as an opaque
`io::Error::other`. `parallel_gzip` ignores `Compression` and hardcodes
`flate2::Compression::fast()`.
Enforceable by: private-field `ZlibLevel`/`ZstdLevel` newtypes with checked
constructors (the enum is already `#[non_exhaustive]`); doc pulled from the
same consts.

---

## 2. Values nobody can find, change, or trust

### 2.1 There is no answer to "what are the write path's tunables"
`PIPELINE_DISPATCH_PERMITS` (64), `WRITE_AHEAD`, `parallel_writer::POOL_SIZE`
(16, "could become a CLI flag later"), `PER_WORKER_CAPACITY` (8),
`uring_writer::NUM_BUFS` (64) / `BUF_SIZE`, `direct_writer::BUF_CAPACITY`,
`parallel_gzip::DEFAULT_CHUNK_SIZE` / `QUEUE_SLOTS_PER_WORKER` / level,
`MAX_ENTITIES_PER_BLOCK`, the zlib reserve slack (`>> 10` + 64) - each in the
module that first needed it, most `pub(crate)` or private. None has an
injection point, so every test that touches the parallel writer spawns 16 OS
threads and every uring test pins 16 MB of `RLIMIT_MEMLOCK`. The memory budget
doc on `PER_WORKER_CAPACITY` ("8 x 16 x 800 KB ~ 100 MB") multiplies two
constants from two places; nothing checks it.
Enforceable by: a `WriterConfig` struct (defaults = today's consts) passed to
`PbfWriter::open`, with const asserts for the cross-constant invariants
(budget, multiple-of-page). Tests construct a small config.

### 2.2 `POOL_SIZE = 16` ignores the host
16 pwrite workers regardless of core count or device; on a 4-core host or a
SATA/HDD target this is 16 threads contending on one fd. Silent degradation,
no refusal. Same family: `parallel_gzip` takes `worker_count` from the caller,
so it is the one backend that is handed the number.

### 2.3 Configuration validated at first use, deep in a worker
Zstd compressor construction (`zstd::bulk::Compressor::new(level)`) happens on
the first blob inside a rayon closure; zlib level >9 likewise. An invalid
level fails after the header has been written and the output file created,
and surfaces via `WriterGone` lag. `copy_file_range` incompatibility with
`--direct-io` is discovered when the first `CopyRange` chunk reaches the sink
(`FileOutputSink::write_chunk` returns `Unsupported` from the writer thread),
which is mid-run; commands guard it today only by their own (diverged, 1.12)
bool arithmetic.
Enforceable by: validate compression in the constructor; make
`write_raw_copy` unavailable on a direct writer at the type level
(`PbfWriter<Direct>` has no such method) or check it in the producer before
sending.

---

## 3. One channel, one implementation

### 3.1 Backend threads print errors with `eprintln!` and also return them
`uring_writer_thread` prints `[uring_writer] error: {e}` and
`parallel_writer_thread` prints `[parallel_writer] thread exit: {e}`, then both
return the same error to `PbfWriter`, which returns it to the command, which
prints it again. The other two backends print nothing. A library consumer gets
unconditional stderr output from a library crate. Neither line names the
output path.
Enforceable by: delete both; `clippy::print_stderr` / `print_stdout` = deny
for the library crate with an `#[allow]` only in the debug/sidecar module.

### 3.2 No error from the write path names the output file
`check_stream_complete` names a backend ("pipelined writer", "uring writer",
"parallel writer") but not the path; ENOSPC from any backend reaches the
operator as "No space left on device"; `DirectWriter::write` after flush, uring
short writes, register_buffers failures - all subject-less. `parallel_writer`
attaches `create {path}` to the copy it sends on `init_tx`, but returns the
bare `e` from the thread, so the join result and the init result disagree.
`multi-extract` is the one caller that wraps with the path, by hand.
Enforceable by: the writer owns its `PathBuf` and wraps every escaping error
once at the `PbfWriter` boundary (`WriteError { path, backend, source }`).
Test: an ENOSPC/forced-error test asserts the path appears.

### 3.3 Pool panic payload printed as `Any { .. }`
`parallel_writer_thread_inner` formats a worker `JoinHandle::join` error with
`{e:?}` on a `Box<dyn Any + Send>`, which prints `Any { .. }`, discarding the
panic message. The uring/gzip paths say "panicked" without payload too.
Enforceable by: one `panic_message(&Box<dyn Any>)` helper (downcast `&str` /
`String`) used by every join site.

### 3.4 Significant events with no record
- `tagdata` dropped for oversize headers is counted (`writer_tagdata_dropped_oversize`)
  but the counter is only emitted under the sidecar; an operator running the
  CLI sees nothing that `tags-filter` speedups were lost.
- `copy_file_range` falling back to userspace on EXDEV (planet-scale
  performance cliff) is silent and uncounted.
- A `--bench` result that hits Drop instead of `flush` (error path) loses the
  writer error by design; nothing records that the join result was discarded.
Enforceable by: counters for fallback; a once-per-process warning for
tagdata drops routed through the CLI's output channel.

### 3.5 Metrics with the same name mean different things per backend
`writer_write_ns`: wall time on the writer thread for buffered and uring;
summed across 16 pool threads for parallel (so it can exceed wall). Parallel
counts `writer_bytes_written` at dispatch (before the pwrite succeeds), others
after the write. `uring_cq_wait_ns` is incremented with exactly the same
elapsed value as `uring_submit_and_wait_ns` at the only site that touches it -
two counters, one number. `metrics.rs`'s funnel tests scan six files but not
`framing.rs` (which writes `compress_ns`, `frame_ns`, `bytes_framed`,
`tagdata_dropped_oversize`) or `parallel_gzip.rs`, and the funnel covers only
the `sync_all` pair and `recv_wait_ns`, not `write_ns`/`bytes_written`, which
are hand-rolled at ~10 sites. The field list is also spelled three times
(struct, `new`, `emit`) with hand-kept name strings.
Enforceable by: a `metrics!` macro generating struct, `new` and `emit` from
one list; `record_write(start, bytes)` funnel added to the scan; extend the
scan's `SOURCES` to every file in `src/write/` via a build-time listing (or a
test that fails when a `.rs` file in the dir is missing from `SOURCES`). Drop
`uring_cq_wait_ns` or give it a distinct meaning.

---

## 4. Errors

### 4.1 Failed writes leave a plausible PBF at the destination path
Every backend creates the final path up front (`File::create` /
`O_TRUNC`) and none removes it, renames into place, or marks it on failure.
`pipeline::writer_thread`'s own comment states the consequence: a short file
"ends on a blob boundary and reads as a valid shorter PBF". `DirectWriter::Drop`
even pads and `set_len`s it into shape. The error is returned, but if any
caller (script, brokkr, a library user) ignores exit status or the run is
killed, the next consumer reads a silently truncated planet. No command cleans
up its PBF output (`remove_file` appears only in diff/altw temp handling).
Enforceable by: write to `<path>.partial` (same directory, so rename is
atomic) and `rename` only after `flush` succeeds; `Drop` unlinks the partial.
One policy in `PbfWriter`, testable with a forced-error test asserting the
destination does not exist.

### 4.2 `parallel_gzip` swallows the compressor's error
`worker_loop` does `Err(_) => return` on `compress_one`; the `DeathNotice`
then reports "a compress worker died at chunk N; its bytes were never
produced" - the actual cause is discarded. (`compress_one` is documented as
effectively infallible today, which is why it has not bitten.)
Enforceable by: `Out::Failed(io::Error)` carrying the error.

### 4.3 Panics on caller-controlled input in public API
- `BlockBuilder::add_node/add_way/add_relation/add_way_with_locations` (and the
  `_raw` crate variants) `assert!(self.can_add_*())`, i.e. abort on a wrong
  element kind or a full block. The same builder already has a non-panicking
  `poison` mechanism for other bad input; misuse here is the one class that
  aborts.
- `BlockBuilder::with_element_cap(0)` panics (see 1.13).
- `PbfWriter::into_inner` panics on a pipelined writer while `writer_mut`
  was deliberately changed to return an error for the same state.
- `new_permit_pool` uses `.expect` (genuinely unreachable; fine) - but
  `expect_used` is not in the deny list at all, so nothing distinguishes the
  sound uses from the others.
Enforceable by: `add_*` returning `Result` (or poisoning), a typestate
`PbfWriter<Sync>` vs `PbfWriter<Pipelined>` so `into_inner` exists only where
it works; add `clippy::expect_used = "deny"` with local allows.

### 4.4 `Drop` joins discard writer errors; the rule lives in comments
`PbfWriter::Drop` and `ParallelGzipWriter::Drop` and `DirectWriter::Drop` all
best-effort and say "callers MUST call flush/finish". Nothing enforces it:
there is no `#[must_use]` on the writer and no debug-build assertion that a
pipelined writer reached `flush` on a non-panicking path.
Enforceable by: `debug_assert!(std::thread::panicking() || self.finished)` in
`Drop`, which turns every forgotten `flush` in tests into a failure.

### 4.5 `HeaderBuilder::build` returns `io::Result` but cannot fail, and accepts NaN
The `Result` is nominal. Meanwhile `bbox` takes raw `f64`s and converts with
`(x * 1e9).round() as i64` (saturating, NaN -> 0) with no check for NaN,
infinity, `left > right` or out-of-range latitude. A bad `--bbox` or a broken
input header round-trips into a header claiming `0,0,0,0`.
Enforceable by: validate in `bbox()` (or a `Bbox` type with checked
constructor) and make `build` the place that returns the error.

### 4.6 `register_probe` failure treated as "supported"
`uring_init_and_run` only refuses when the probe succeeds and says WriteFixed
is unsupported; a failed probe proceeds. The `RLIMIT_MEMLOCK` advice is printed
for ENOMEM/EPERM, but on kernels 5.12+ registered buffers are charged to the
memcg, not memlock, so the advice can be wrong on exactly the hosts in use.

---

## 5. Policy invented per call site

- **Syscall retry / fallback** - see 1.11; four loops, three EINTR rules.
- **End-of-stream completeness** - shared via `check_stream_complete` (good),
  but `parallel_gzip::writer_loop` has its own independent version with a
  `BTreeMap` reorder buffer instead of `ReorderBuffer`, its own "chunks
  missing" message, and no `SeqTotal`-style announced total.
- **Durability** - pipelined backends fsync at end of stream; `PbfWriter::new`
  (sync mode) never fsyncs. `multi-extract`, every command that uses the sync
  writer, and the library examples therefore produce un-synced output while
  single-output commands produce synced output. Decide once, in the writer.
- **Buffering of sync writers** - `PbfWriter::new` docs say wrap in a 256 KiB
  BufWriter; `multi-extract` wraps in a default 8 KiB `BufWriter::new`, as do
  the test helpers in getid/getparents/tags_filter/header_walker. A
  `PbfWriter::to_path_sync(path)` constructor would own it.
- **Cleanup on error** - see 4.1; each backend decides alone what a failed
  file looks like (Direct pads and truncates; uring deliberately does not
  `set_len`; parallel leaves holes up to the highest written offset).
- **Reorder buffer** - three backends use `crate::reorder_buffer`, gzip uses a
  `BTreeMap`.

---

## 6. Ambient dependencies

- `WRITER_METRICS` is a process-global static written from every layer; the
  `metrics.rs` docs explain why per-writer attribution is not done. The effect
  on tests is real: `end_of_stream_finalization_still_fsyncs` and
  `flush_pushes_bytes_onward_without_fsyncing` assert on deltas of a global
  that concurrent tests also bump, and are written with "loose enough to
  tolerate" bounds to survive that. A `&WriterMetrics` handle (or
  `Arc<WriterMetrics>` in `WritePipeline`, with the thread-local scratch
  carrying a pointer) would make those assertions exact.
- Test hooks (`PANIC_AT_POOL_OP_COUNT`, `DISPATCH_COUNT`, ...) are
  process-global statics in three modules, correct only under
  `--test-threads=1`. `brokkr check` enables them via `--all-features` in
  sweeps that are not all single-threaded; any arming test outside a `serial`
  module is a cross-test hazard. A per-instance `FaultPlan` in `WriterConfig`
  (2.1) removes the global.
- `std::time::Instant::now()` is called directly in ~25 places for metrics.
  Harmless for production logic, but it is part of why the metrics cannot be
  asserted deterministically.

---

## 7. Shared state and locking

### 7.1 `StringTable::pre_seed` "identity mapping" is not structural - latent corruption
`pre_seed` feeds input strings 1..len through `add`, which dedups. The
`add_*_raw` / `add_way_raw_bytes*` paths then write the *input* block's raw
string indices straight into the output, on the documented assumption that
"input index N maps to output index N". That holds only if the input string
table has no duplicate entries and no empty string at an index other than 0.
Neither is guaranteed by the PBF format or checked: a duplicate at index j
makes `add` return i, the output table is one shorter, and every index after
j points at the wrong string - wrong tags, no error. Raw sids are also never
bounds-checked against the table length, and negative sids are cast with
`as u32` into `tag_key_indices`.
Fix: make `pre_seed` push unconditionally (no dedup) and insert into the index
only if absent, so identity holds by construction; or return a remap table.
Enforceable by: a test that pre-seeds from a block with a duplicated and an
empty string and asserts `strings.len() == input_len`; a debug assert in
`encode_block` that every raw sid is `< strings.len()`.

### 7.2 The FxHash safety argument in `string_table.rs` is false
The doc says every string "comes from the caller's in-process data ... never
from untrusted PBF input". `pre_seed` inserts strings from the input PBF, and
`add` is called with tag strings decoded from input files and OSC XML in
every rewrite command. The HashDoS claim may still be acceptable (8000-element
blocks bound it), but the stated reason is wrong and the "Where NOT to use
FxHash" guidance is contradicted by its own caller.

### 7.3 `ParallelGzipWriter` shares one `Receiver` behind a `Mutex`
Fine as written (lock held only across `recv`), but this is `Mutex<Receiver>`
as a work queue; `recv` blocks while holding the lock, so idle workers queue
on the mutex rather than the channel. Not a bug; a crossbeam/rayon-style MPMC
would make it structural.

### 7.4 `UringState.logical_size` updated in two places by two protocols
`write()` adds before copying; `handle_copy_range_uring` adds the whole range
up front and then may fail mid-way. Safe today only because no `set_len`
runs on error (documented on the field). The invariant is a comment.

---

## 8. Unbounded growth

- `parallel_gzip::writer_loop` reorder `BTreeMap` is bounded only indirectly by
  channel capacity; with a gap that `DeathNotice` does not cover (a worker
  killed between dequeue and arming - not reachable today) it grows until the
  stream ends.
- `ParallelGzipWriter::new(inner, 0, n)`: `chunk_size == 0` makes `write`
  loop forever (`space == 0`, `take == 0`). `worker_count` is clamped to 1;
  `chunk_size` is not.
- `FrameScratch::take_with_capacity` reserves every replacement buffer to the
  high-water mark ever seen on that thread; one oversize blob (e.g. a 143 MB
  body before the cap refuses it, or a repack at a huge cap) pins that
  capacity in every rayon thread's thread-local scratch for the process
  lifetime, then re-allocates that size per blob. Bounded by
  `MAX_BLOB_DATASIZE` times thread count, which at planet core counts is
  gigabytes.
- `BlockBuilder.poison: Option<String>` and string-table growth are bounded by
  the element cap - fine.

---

## 9. Code that is no longer load-bearing

- `write_primitive_block_no_indexdata`: public, its only non-test callers are
  in test code of getid/getparents (`#[cfg(test)]` blocks), per the grep of
  `src/` and `cli/src`. Kept as a fixture producer; that belongs in a test
  helper or `degrade`, not on the writer's public surface, where it is
  counted as a third copy in 1.8.
- `uring_cq_wait_ns` duplicates `uring_submit_and_wait_ns` (3.5).
- `compress_one`'s `io::Result` is documented as nominal (4.2).
- `HeaderBuilder::build`'s `Result` is nominal (4.5).
- `parallel_writer`'s module doc still frames itself against "plan item #17"
  and a scaffold history; `uring_writer`'s doc says it "replaces the
  synchronous `writer_thread` in `super::writer`", which no longer lives
  there.
- The `#[cfg(feature = "linux-direct-io")]` sync branch of `write_raw_copy`
  is reachable only via `PbfWriter::new`, but `write_raw_copy` is defined only
  on `PbfWriter<FileWriter>`, whose every public constructor is pipelined - so
  the sync branch is dead code (no public way to get a non-pipelined
  `PbfWriter<FileWriter>`).
- Re-exports kept "so the existing path keeps resolving"
  (`crate::writer::Compression`, `crate::block_builder::HeaderBuilder`) are
  compatibility shims inside a pre-1.0 crate.

---

## 10. Host assumptions and where it writes

- **`copy_file_range` is gated on `linux-direct-io`.** Kernel-space
  passthrough has nothing to do with `O_DIRECT`, yet `copy_range`,
  `OutputChunk::CopyRange`, `write_raw_copy` and the parallel backend's
  `WriteOp::CopyRange` all compile only with that feature. A default-feature
  build silently loses passthrough splicing and falls back to userspace copies;
  `sort` notices via `cfg!`, `apply_changes/rewrite.rs` does not (1.12).
  Also `linux-direct-io = []` does not gate `libc`, which is unconditional.
  Fix: a separate `linux-copy-range` (or just `cfg(target_os = "linux")`).
- **`parallel_writer` is compiled unconditionally** and uses
  `std::os::unix::fs::FileExt`, so the library does not build on non-unix
  targets even with no Linux features enabled; `to_path_parallel` is public
  API.
- **`PAGE_SIZE = 4096` is assumed** for O_DIRECT alignment. The real
  requirement is the device/filesystem DIO alignment (`statx` `STATX_DIOALIGN`,
  Linux 6.1+), which can be 512 or larger than 4096. On a mismatching host the
  open succeeds and the first `write` fails with EINVAL - a refusal, but late
  and unexplained.
- **io_uring kernel and limits** - 5.1+ assumed via a probe whose failure is
  ignored (4.6); 16 MB locked memory assumed; `setup_clamp` set.
- **O_DIRECT on tmpfs** - documented as unsupported; the only test of
  `DirectWriter` returns early (passes) when the open fails, so on a tmpfs
  `TMPDIR` the test is green without testing anything.
- **Writes outside owned locations** - none found in `src/write/`: every
  backend writes only to the caller-supplied path. The one write-adjacent
  location issue is 4.1: in-progress output is written at the final
  destination rather than a sibling `.partial` the writer owns.
- **Tests read `../../cli/src/main.rs` via `include_str!`** from the library
  crate (`metrics.rs`). That couples the library test build to a sibling
  crate's source path; it breaks when the library is built alone from a
  published crate tarball.

---

## Other things noticed

- `BlockBuilder` delta encoding (`id - last_dense_id`, `meta.uid -
  last_dense_uid` on `i32`, user-sid deltas) overflows on extreme but legal
  inputs (uid `i32::MAX` followed by a negative uid from an OSC). Release has no
  `overflow-checks`, so it wraps - which is actually what a varint delta
  decoder that also wraps needs - but debug builds panic, so debug and release
  disagree on the same input. Use `wrapping_sub` explicitly and say why.
- `encode_blob_header_into`'s WayMembers culprit heuristic (`len > 1024`)
  is a magic threshold guessing which field caused the overflow; it can just
  compute the sizes.
- `FileWriter` is a public enum whose variants are public, so library users
  can construct `FileWriter::Buffered(BufWriter::new(..))` with any capacity,
  bypassing the constructor's 256 KiB policy.
- `to_path_uring`'s "latent blocking scenario" (an unbounded `init_rx.recv()`)
  is documented and deliberately left; it is the only unbounded wait at
  construction.
