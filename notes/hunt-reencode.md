# Hygiene hunt: re-encoding commands

Scope: `src/commands/renumber/`, `sort/`, `cat/` (incl. `dedupe.rs`),
`repack/`, `degrade/`, `diff/` (`mod.rs`, `parallel.rs`, `derive.rs`,
`derive_parallel.rs`). Read at HEAD `955664d`. Findings are grouped by the
ten questions; bugs found on the way are pulled to the top because some of
them are worse than hygiene.

Where a finding says "enforceable by", that is about holding the fixed
version in place. The current build enforces a strict clippy set
(`Cargo.toml` `[workspace.lints.clippy]`), feature-sweep compilation
(`brokkr.toml` `[[check]]`), and nothing else structural: there is no
`clippy.toml`, so `disallowed_methods` / `disallowed_types` are unused,
and `print_stderr`, `arithmetic_side_effects`, `exit` are not enabled.
`too_many_arguments` / `too_many_lines` are `deny`, but the scope carries
28 `#[allow]`s of exactly those two, so in practice they are advisory here.

---

## 0. Bugs and near-bugs found while reading

### 0.1 Renumber hangs instead of failing on any mid-stream error (pass 1, stage 2d, R2d)

`renumber/pass1.rs`, `renumber/stage2.rs`, `renumber/relations.rs` all use
the same shape: `sync_channel(32)` created *outside* `thread::scope`,
workers `tx.send(...)` in a work-stealing loop, the consumer loop inside the
scope returns early via `?` on the first `Err` it pops from the reorder
buffer (worker error string, or `writer.write_primitive_block_owned` I/O
error). The receiver `decoded_rx` is borrowed, not moved, so it lives in
the outer frame until after `thread::scope` returns - and `thread::scope`
joins every worker before returning. The workers are still claiming tasks
(there is no cancel flag), fill the 32-slot channel, and block forever in
`send`. Result: any error with more than ~32 + worker-count blobs left in
the phase becomes a silent hang rather than an error. A corrupt blob in
the middle of a planet node section, `ENOSPC` on the output, or the
"indexdata understates its own contents" refusal in `wire_rewrite.rs` all
take this path. `brokkr test` would report it as "did not finish within
20s" with a futex wchan, which reads as a deadlock - and is one.

The fixture-sized tests (`tests/renumber_external.rs`,
`cli_truncation_sweep.rs`) cannot see this: fixtures have far fewer than 32
blobs per kind, and truncation is caught in the header scan before workers
start.

Fix shape: move `decoded_rx` into the scope and `drop` it on the error
path before returning, plus a shared `AtomicBool` abort the workers check
before claiming the next task. Better: these three loops are one function
(see 1.6) and should be written once with that property. Enforceable by a
fault test: a synthetic input with > 64 node blobs and one corrupt blob in
the middle, asserting an error exit (not a timeout). `tests/fault_*.rs`
already has the per-binary fault-injection precedent.

### 0.2 Renumber silently writes wrong way refs if node IDs are not strictly increasing

Pass 1 (`wire_rewrite.rs::reframe_dense_with_new_ids`) assigns each node
its new ID *by position* (`base_new_id` + running count, encoded as a run
of `0x02` deltas). Stage 2d / R2d resolve refs *by rank* in the bitset
(`resolve_with_orphan` -> `start_id + rank_if_set(old_id)`). The two agree
only if node IDs are strictly increasing across the entire node section in
schedule order and contain no duplicates. Nothing checks that:
`require_sorted` trusts the header's `Sort.Type_then_ID` claim, the delta
decode accepts zero and negative deltas (only `old_id < 0` is rejected),
and a duplicate ID sets the same bit twice. A file that lies about being
sorted (exactly what `degrade --unsort` manufactures) or carries a
duplicate node gets a renumbered output where ways point at the wrong
nodes, with exit 0. The comment in `renumber/mod.rs` about scanning the
whole schedule for `max_id` shows the lying-header case was considered for
the panic, not for the mapping.

The check is cheap: `delta > 0` per node inside the group (except the
first), and per-blob `min_id > previous blob max_id` in the schedule
(indexdata is already in hand). Same reasoning applies to way IDs in stage
2d versus relation member resolution. Enforceable by a test that runs
`degrade --unsort` output (or a hand-built duplicate-ID fixture) through
renumber and asserts refusal.

### 0.3 Unchecked `old_id += delta` in the wire rewriter

`wire_rewrite.rs` accumulates sint64 deltas with plain `+=`. A crafted
delta overflows: panic in debug, wrap in release (where it may wrap to a
negative and be caught by the `< 0` check, or wrap to a large positive and
hit `set_atomic_checked`). The prefix sum for new IDs right next door uses
`checked_add`. Same pattern in the way-ref and memids decode loops. Fix:
`checked_add` -> input error. Enforceable by enabling
`clippy::arithmetic_side_effects` scoped to `renumber/wire_rewrite.rs` (a
module-level `#![deny]`), which is the one file where every integer comes
off the wire.

### 0.4 `cat --dedupe` (`merge_pbf`) still builds the output header from the first input only

`cat/mod.rs::cat_output_header_bytes` has a long doc comment explaining
that "all three cat paths built the output header from `files[0]` alone"
and that this produced a bbox that does not cover the file's own contents
and replication fields from whichever file was first; it now reconciles
bbox (union only when every input has one), replication fields (only when
all agree) and `HistoricalInformation` (OR). `cat/dedupe.rs::build_blob_index`
is the fourth path and still does `if header.is_none() { header = Some(hdr) }`
- first file wins. The fix was applied per call site and the call site it
missed is in the same directory. Fix: `merge_pbf` calls
`cat_output_header_bytes`. Enforceable structurally: make
`cat_output_header_bytes` the only constructor of a multi-input header
(e.g. `build_output_header` takes a `&[HeaderBlock]`), so a single-header
call on a multi-input command does not type-check.

### 0.5 Sort's "counted vs claimed" honesty fix was not ported to dedupe

`sort/mod.rs` distinguishes `index_scanned` and reports
`passthrough_claimed` separately ("folding it into 'Sorted N nodes'
reported hearsay as observation"). `cat/dedupe.rs::count_entry` folds
`entry.index.count` from unverified indexdata straight into "Merged N
elements". Same lie, other copy (see 1.4).

### 0.6 `degrade --block-cap N` can request an unbounded preallocation

`repack` uses `BlockBuilder::with_element_cap_prealloc(cap, file_len)` and
its comment, and `block_builder.rs`'s doc, explain why: a CLI integer must
not drive preallocation, `--elements-per-blob usize::MAX/2` was several
huge allocations. `degrade::run_kind_phase` uses
`BlockBuilder::with_element_cap(block_cap)` = `prealloc(cap, usize::MAX)`.
`--block-cap` is hidden but accepted by the CLI. Same fix, missed copy.
Enforceable by deleting `with_element_cap` (or making it `#[cfg(test)]`)
so every caller has to name a bound.

### 0.7 Degrade's sort-preservation guard is weaker than repack's

Repack drains the central stream before writing a worker's framed blocks
when `!bb.is_empty() || !pending.is_empty()` - the comment explains that a
delayed `pending` batch landing after direct writes emits lower IDs after
higher ones. Degrade's equivalent guard is `!bb.is_empty()` only. I could
not construct a path where `pending` is non-empty while `bb` is empty in
degrade's plain mode (a mid-stream cap fire leaves the triggering element
in `bb`), so this is probably latent today - but it rests on an unstated
flush-before-add property of `BlockBuilder`, while repack's version does
not. One more reason 1.5 matters.

### 0.8 Degrade's "needs at least one flag" error omits `--drop-ids`

`DegradeFlags::any()` includes `drop_ids`; the error message listing the
flags does not. The flag list is spelled at four sites (`any()`,
`needs_decode()`, the error text, the `applied` list in `print_summary`)
plus the CLI; they have already diverged. A test that the error text
mentions every `DegradeFlags` field, or generating the text from a table
of `(field, flag name)`, holds it.

### 0.9 Renumber timing counters truncate per blob

Every `StageCounters` field is accumulated as
`elapsed().as_millis() as u64` *per blob*. A planet node blob's pread is
well under 1 ms, so `pass1_pread_ms` and friends sum truncated zeros and
under-report by close to 100 %; `consumer_recv_ms` per message likewise.
The numbers in sidecar views are therefore not wall time. Accumulate
nanoseconds (`as_nanos`) and divide at emit, or better, follow the
documented `<category>_wait_ns` convention so `--stalls` picks them up.
Same in `relations.rs` and `stage2.rs`.

### 0.10 Smaller

- `sort/mod.rs`: `use_copy_range = io_uring || (!direct_io && cfg!(feature = "linux-direct-io"))`
  - `copy_file_range` availability is keyed on the *O_DIRECT* feature
  name. Either the feature gates `libc` and this is a proxy, or it is
  wrong; either way the condition says something other than what it means.
- `sort/mod.rs`: the doc comment for `sort()` ("Sort a PBF file into
  standard order ... Uses blob-level permutation sort") sits on
  `SortOptions`, followed by `SortOptions`'s own one-liner. rustdoc
  attributes both to the struct; `sort()` has none.
- `renumber/mod.rs` opens and parses the input header twice in a row
  ("Re-parse header for writer construction").
- `derive.rs::write_delete_element` and its copy in `derive_parallel.rs`
  call `SystemTime::now()` per deleted element under `--update-timestamp`
  (see 6.1).
- `dedupe.rs`: `HasId::version` maps absent version to `0`; `derive.rs`
  carries a comment that "absent is not zero". Two policies for the same
  metadata in one scope.

---

## 1. One value, one owner

### 1.1 Scratch directory: three rules for "where do temp files go"

- `diff/parallel.rs`: `old_path.parent().unwrap_or(".")` - next to the
  *old input*. Comment claims it is "the same assumption the `--format osc`
  sibling driver makes"; it is not.
- `diff/derive.rs` (sequential and parallel): `output.parent()` - next to
  the *output*. Comment: "same as brokkr scratch".
- `renumber`: no temp files, but its comment explains that output-side
  safety is the front end's (`path_guard::OutputFile`) job.

The `diff` rule is the dangerous one: inputs routinely live in a
read-only or shared dataset directory (brokkr's data dir, a mounted
planet mirror). `diff -j N` then fails at `File::create` with an
unattributed I/O error (see 4.2), or worse, drops multi-GB `.txt.tmp`
files into another tool's directory. `diff` writing to stdout has no
output path, which is presumably why it picked the input; the right owner
is a single `scratch_dir` resolved once by the CLI (an explicit
`--tmp-dir`, else `$TMPDIR`-equivalent via `std::env::temp_dir`, else the
output's directory) and passed into the library. Not enforceable by lint;
enforceable structurally by giving every command that needs scratch a
required `scratch: &ScratchDir` parameter whose only constructor is in the
CLI.

### 1.2 Scratch file naming: three schemes

- `diff/parallel.rs`: `diff-par-{pid}-{kind}-{shard}.txt.tmp`
- `derive.rs`: `derive-{creates,modifies,deletes}-{pid}.xml.tmp`
- `derive_parallel.rs` shard files: `derive-par-*-{pid}-{tag}-{kind}-{shard}.xml.tmp`
  where `tag` is a process-lifetime nanosecond hash, with a comment that
  PID alone collides across containers.
- `derive_parallel.rs` *outer* files: `derive-par-*-{pid}.xml.tmp` - no
  tag, in the same function that defines the tag.

The collision rationale applies equally to all four; only one honours it.
Two concurrent `derive-changes` writing to the same directory with
recycled PIDs (CI containers) clobber each other's outer files. The real
answer is `tempfile`-style `O_EXCL` creation in a per-run directory
(`create_dir` with a random name, everything inside it, one `remove_dir_all`
guard), which also collapses 5.1. Enforceable by `disallowed_methods` on
`std::fs::File::create` inside `src/commands/diff/` pointing at the scratch
helper.

### 1.3 Worker / thread counts: five policies

- `renumber::PASS1_WORKERS = 4` (fixed)
- `renumber::STAGE2D_WORKERS = 6` (fixed; comment notes `>= 1` is
  load-bearing because of `remove(0)`)
- `renumber/relations.rs`: `available_parallelism() - 2`, min 1, fallback
  4, capped at 6
- `diff/derive.rs::assemble_osc`: gzip workers `available_parallelism()`,
  fallback 4
- CLI `run_diff` / `run_derive_changes`: `jobs == 0 -> available_parallelism()`,
  fallback **1**; other CLI commands pass `jobs == 0` as `None` into the
  library and resolve there.
- `sort` / `repack` / `degrade` / `cat --clean` use rayon's global pool
  and `parallel_classify_phase`'s own count.

None of renumber's three is reachable from `-j`. The `jobs == 0` resolution
is copy-pasted twice in `cli/src/main.rs` with a fallback (1) that
disagrees with the library's (4). One `fn resolve_jobs(Option<NonZeroUsize>) -> NonZeroUsize`
in the library, called once, and `RenumberOptions` growing a `jobs` field
would make this one value. `NonZeroUsize` in the signature also deletes
the `STAGE2D_WORKERS >= 1` comment by making zero unrepresentable.

### 1.4 Sort and dedupe are one algorithm in two copies, already diverged

`sort/mod.rs` and `cat/dedupe.rs` each define `BlobEntry`, `HasId`,
`build_blob_index`, `type_order`, `detect_overlaps`,
`write_passthrough_blob`, `read_frame_into`, `extract_blob_bytes`,
`count_entry`, a sweep merge and `flush_heap_below`. Divergence today:

- sort reads headers through `HeaderWalker` (pread + `fadvise(RANDOM)`,
  justified by a 21 s / 36 GB planet measurement); dedupe reads through
  `FileReader` + `read_frame_header`.
- sort has `intra_unsorted` detection, `index_scanned` /
  `passthrough_claimed` (0.5), parallel overlap rewrite, and the
  `copy_file_range` coalescer; dedupe has none of them.
- the kind-boundary run-splitting fix (`486d4d1`) exists in both, as a
  comment in each citing the other.
- `detect_overlaps`' running-max fix (non-transitive overlap) is in both;
  I did not diff them line by line.

`dedupe` is sort with N inputs and a dedup predicate in the sweep. One
module parameterised on input set and merge policy.

### 1.5 Repack and degrade decode paths are one pipeline in two copies

Both define `FRAME_BATCH = 32`, `KindPayload`, `WorkerOutput`,
`PhaseStats`, `run_kind_phase` (reorder buffer 32, central `BlockBuilder`,
`pending` batch, `write_error` / `classify_error` latches), `frame_owned`,
`frame_and_write_batch`. Degrade's copies add strip-indexdata /
strip-tagdata booleans. Divergence: 0.6 (prealloc bound), 0.7 (ordering
guard). `cat/mod.rs::run_kind_phase` is a third member of the family
(reorder buffer 64). `DEFAULT_BLOCK_CAP = 8000` in degrade restates
`BlockBuilder`'s 8000 cap and the CLI's repack default; it should be
`BlockBuilder::DEFAULT_CAP` read in one place.

### 1.6 Renumber's three worker loops

`pass1.rs`, `stage2.rs`, `relations.rs` each spell: `sync_channel(32)`,
`ReorderBuffer::with_capacity(64)`, an `AtomicUsize` work-stealing index,
pread into `read_buf`, `decompress_blob_raw`, a reframe call, per-phase
`StageCounters`, and the consumer loop. 0.1 is in all three. The reorder
depth (64) and channel depth (32) are the same numbers as
`cat::run_kind_phase` (64), `repack`/`degrade` (32) without a shared name.
One `parallel_reframe_phase(schedule, workers, |task, scratch| -> blocks, writer)`
is the fix; `scan::classify::parallel_classify_phase` is already most of
it and renumber predates or bypasses it.

### 1.7 Diff parallel and derive parallel: shared core in two copies, plus a third in `osc::merge_join`

`diff/parallel.rs` and `diff/derive_parallel.rs` each define `BlobDesc`,
`WalkedFile`, `walk_file`, `Shard`, `plan_shards`, `build_shard`,
`pread_decode`, `Side`, `element_merge`, `borrowed_elements_equal`,
`borrowed_nodes_equal`, `borrowed_relations_equal`, `kind_type_char`,
`append_and_cleanup`, and the scope/join/partition/sweep driver. The
`borrowed_*_equal` trio is a third copy of `osc::merge_join`'s private
functions, with a `TODO: lift ... instead of duplicating` in
`parallel.rs`. The copies have already drifted textually (merge_join uses
`iter_tags_equal` for the dense/non-dense cross cases; the two copies use
`.eq`). They look semantically equivalent today; the point is that
"is this element modified" - the one predicate that decides what `diff`
and `derive-changes` output - has three owners and the sequential path
uses a different one from the parallel path. `plan_shards` has also
drifted (one has the dedup-justification comment, the other a different
one). `emit_merge_stats_counters` is duplicated verbatim between
`diff/mod.rs` and `derive.rs`; `extract_delete_info` and
`write_delete_element` between `derive.rs` and `derive_parallel.rs`.

Enforceable by an equivalence test that runs `diff -j1` and `diff -j8`
(and derive) on the same pair and asserts byte-identical output - cheap,
and catches drift in any of the three equality copies. Better, make
`merge_join`'s equality `pub(crate)` and delete the copies.

### 1.8 The indexdata requirement message is spelled per site

`require_indexdata(.., reason)` callers in sort, dedupe, cat (two
variants), repack each write their own "input PBF has no blob-level
indexdata..." sentence. Sort and dedupe's are identical text. Renumber's
schedule scan returns its own differently-worded error ("run `pbfhogg cat`
to add indexdata") instead of calling `require_indexdata`, and has no
`--force` escape at all. A `NeedsIndexdata` error kind owned by
`require_indexdata` with a per-command *reason* enum would hold it.

### 1.9 `M_ARENA_MAX = 2` set from library code in four scope sites

`renumber_external`, `cat_filtered`, `repack`, `degrade_decode_path` each
`unsafe { libc::mallopt(M_ARENA_MAX, 2) }` with a comment saying it is
"scoped to this command - other pbfhogg commands are unaffected". It is a
process-global, irreversible glibc setting. Inside the CLI that is true by
accident of one-command-per-process; for a library consumer that calls
`pbfhogg::repack` in a long-lived service it permanently changes the
allocator behaviour of the whole process, and `time_filter`'s own comment
shows at least one path that regresses under it. It also tells you
nothing on musl (silent no-op) beyond `target_os = "linux"`. This belongs
in the CLI's `main` (per subcommand if needed), not in library entry
points. Enforceable by `disallowed_methods = ["libc::mallopt"]` for the
library crate with an allow in `cli/`.

---

## 2. Values nobody can find, change, or trust

- **No tunables surface.** The scope's knobs are: worker counts (1.3),
  channel depth 32 and reorder depth 32/64 (1.6), `FRAME_BATCH = 32` x2,
  renumber's implicit `zlib:1` override (below), gzip chunk size
  (`parallel_gzip::DEFAULT_CHUNK_SIZE`), `CORRUPT_INDEXDATA_INFLATION`,
  `DEFAULT_BLOCK_CAP`. Nothing enumerates them; they are found by
  grepping for `const` and literal numbers. A `tuning.rs` (or a section
  in `reference/pipeline.md` generated from it) is the answer to "what can
  I turn".
- **Renumber silently overrides the compression default.** `if compression == Compression::default() { Zlib(1) }`.
  A caller who explicitly asks for the default level (`--compression zlib:6`
  if that is the default) cannot get it - the function cannot distinguish
  "unspecified" from "explicitly the default". The comment says "respect
  explicit caller overrides", which the code cannot do. Make the parameter
  `Option<Compression>` and let `None` mean "command's own default".
  Enforceable by the type.
- **Renumber's worker counts have no injection point** (1.3), so tests
  cannot run renumber with 1 worker to get deterministic interleaving, nor
  with many workers on a small fixture to reach 0.1.
- **Library error text names CLI flags.** `repack`: "--elements-per-blob
  must be > 0"; `degrade`: "--block-cap", "--unsort", etc.; renumber
  errors suggest "`pbfhogg cat --clean`". For a library consumer these are
  flags they never passed. Either the library owns typed errors and the
  CLI maps them to flag names, or accept this as CLI-first and say so in
  `LIBRARY.md`. Validation of `elements_per_blob == 0` / `block_cap == 0`
  is also the type's job (`NonZeroUsize`).
- **Configuration validated late.** `diff -j` scratch-dir writability is
  discovered only after both inputs' headers are walked and the first
  phase spawns shards. `derive-changes` likewise discovers an unwritable
  output directory when `ChangeSink::new` creates temp files, after the
  sortedness checks - acceptable, but the parallel path creates the outer
  files only after `walk_file` on both inputs (two full header walks at
  planet scale).

---

## 3. One channel, one implementation

- **Library writes progress to stderr unconditionally.** `sort()` prints
  "Pass 1: indexing blobs...", blob counts, "Pass 2: writing sorted
  output..." and a sortedness warning; `merge_pbf` the same set; repack
  prints the "never fired" warning; sort/dedupe/repack/renumber/cat call
  `warn_*_loss` helpers that `eprintln!`. These are library functions
  (`pbfhogg::sort::sort`), so a library consumer gets uninvited stderr
  with no way to silence it, and the CLI has no `--quiet` that reaches
  them (`diff` has `quiet` but it only gates the summary the CLI prints).
  Meanwhile `print_summary()` methods on every `*Stats` are the right
  shape - the library returns data, the CLI decides to print. Warnings
  should travel the same way: a `warnings: Vec<Warning>` on the stats, or
  a `&dyn Diagnostics` sink parameter. Enforceable by
  `clippy::print_stderr = "deny"` on the library crate with the CLI
  exempt; the scope has 25+ sites that would light up.
- **Two diagnostics channels that do not know about each other.** The
  marker/counter FIFO (`crate::debug`) is excellent and consistently used;
  operator text is `eprintln!`. Events that matter to an operator (sort
  found N overlapping blobs; repack's cap never fired; renumber orphan
  refs) go to stderr as prose *and* as counters, spelled independently.
- **Level inconsistency.** "Warning: N orphan refs preserved" is printed
  by `RenumberStats::print_summary` (library method, called by CLI);
  "Warning: input PBF has way-level enrichment metadata" is printed
  mid-run by the library; sort's "input does not declare Sort.Type_then_ID;
  verifying intra-blob order" is an indented informational line with no
  "Warning:" prefix even though it means pass 1 will now decode every blob
  (a large, user-visible slowdown). No common prefix convention, no
  levels.
- **Lines missing the subject.** `sort` and `dedupe` progress lines do
  not name the input; `merge_pbf` with N inputs prints "N OSMData blobs
  indexed from K files" without saying which file is slow or unsorted.
  `warn_locations_on_ways_loss` is called once per input in `cat` with no
  filename, so K identical warnings print for K inputs.
- **Silence where something significant happens.** `cat --clean` on a
  multi-file input that triggers the triple-decode path refuses (good);
  but `sort`'s `--force` past `require_indexdata` and `diff`'s silent
  fallback from parallel to sequential when either input lacks indexdata
  or `--verbose` is set say nothing: a user who passed `-j 16` gets a
  single-threaded run with no line explaining why. Same for derive's
  `both_indexed && jobs > 1` gate.

---

## 4. Errors

- **Renumber errors are `String`s without subject.** Worker results are
  `Result<_, String>`; decompress and reframe errors are
  `e.to_string()`'d with no blob seq, offset or file path ("reframe: no
  StringTable in PrimitiveBlock"). Pass 1 does name `task.seq` for the
  count mismatch and the offset for pread; stage 2d and R2d mostly do not.
  The error then becomes `Box<dyn Error>` from a `String`, losing any
  `ErrorKind` classification the CLI might map to an exit code. A
  `BlobError { path, seq, offset, kind }` wrapping at the worker boundary
  is the fix.
- **Diff/derive I/O errors name no path.** `File::create(&cp).map_err(|e| new_error(ErrorKind::Io(e)))`
  and `append_and_cleanup` surface as a bare "Permission denied" or "No
  space left on device" with no file name - and the file is a scratch file
  the user did not choose, in a directory they may not know about (1.1).
  `dedupe`'s "failed to scan block IDs" names neither file nor offset;
  its `require_sorted` call does pass the path.
- **Shard panic is flattened.** `h.join().unwrap_or_else(|_| Err(io::Error::other("shard worker panicked")))`
  discards the panic payload, which is the only thing that would say what
  happened. `std::panic::resume_unwind` or formatting the payload's
  `&str`/`String` downcast keeps it.
- **Swallowed cleanup errors are fine; swallowed `walker.next_header()` is
  not an issue** - `schedule.rs` `let _ = walker.next_header()?...` keeps
  the `?`; it only discards the header value. Worth a named `_header`
  binding so `let_underscore_must_use` readers do not stop on it.
- **`expect("checked")` density.** `diff/parallel.rs` and
  `derive_parallel.rs` carry ~20 `.expect("checked")` /
  `.expect("checked peek")` on `Option::take` / `Peekable::next`, and
  degrade three more. They are correct, but `unwrap_used = deny` was
  chosen to forbid exactly this, and `expect` slips through because the
  lint set lacks `expect_used`. Restructuring the merge loop as
  `match (old_iter.peek(), new_iter.peek())` that binds by value
  (`next_if`) removes all of them. Enforceable by adding
  `expect_used = "deny"` (tests exempt).
- **Abort on caller-controlled input:** see 0.3 (overflow) and 0.6
  (allocation). The `IdSet::set_atomic` panic was already converted to a
  refusal; those two were not.

---

## 5. Policy invented per call site

### 5.1 Scratch cleanup: four mechanisms

- `derive.rs`: explicit `sink.cleanup()` after a closure, no Drop.
  A panic inside the closure leaks the three files.
- `derive_parallel.rs` outer files: `PathGuard` (citing
  `decisions/0003-*`), then `commit()` and manual `remove_file` anyway.
- `derive_parallel.rs` and `diff/parallel.rs` shard files: on error, a
  sweep that recomputes every possible shard path and deletes it; on
  success, `append_and_cleanup` deletes each after copying. If
  `append_and_cleanup` fails halfway through the shard list, the remaining
  shard files leak - no guard covers that window.
- `diff/parallel.rs` on success: same as above; no outer files.

The decision record names `PathGuard` as the policy; two of four sites use
it. One per-run scratch directory with one guard (1.2) makes all of this a
single `Drop`.

### 5.2 Validation of values off the wire / out of storage

Each command decides independently how much to trust indexdata:
`sort` trusts it only under a sorted header and otherwise decodes;
`dedupe` trusts it whenever present; `renumber` trusts its `count` but
then cross-checks per blob (good) and trusts its `max_id` for
preallocation but checks per id (good); `repack`/`degrade` trust
`parallel_classify_phase`'s schedule. `reference/` should state the
trust model once ("indexdata is a hint; these properties are verified,
these are believed and here is the consequence") and each command cite it.
`CORRECTNESS.md` partly does this for sort.

### 5.3 Overflow policy

Prefix sums: `checked_add` with a string error. Delta decode: unchecked
(0.3). `current_new_id += group_node_count as i64`: unchecked but bounded
by the checked prefix sum. Renumber's `--start-id` is not range-checked
at all (negative starts are accepted and can produce a renumbered id 0).

### 5.4 Error latching in pipelined consumers

`repack`/`degrade`: `write_error` + `classify_error` latches, keep
draining, return after. `renumber`: return immediately (and hang, 0.1).
`sort`: `par_iter().collect::<Result<_, String>>()` (rayon short-circuits).
Three policies for "a worker failed".

---

## 6. Ambient dependencies

- **6.1 The clock in derive-changes.** `--update-timestamp` calls
  `SystemTime::now()` per emitted delete element, in both copies. Output
  is therefore non-deterministic within a single run (timestamps straddle
  a second boundary), differs between `-j1` and `-jN`, and is untestable
  without a regex. Resolve `now` once at the CLI and pass a
  `timestamp: Option<u64>` into `derive_changes`. Enforceable by
  `disallowed_methods = ["std::time::SystemTime::now"]` in the library
  with an explicit allow for the scratch-tag helper (or by replacing that
  too, below).
- **6.2 Randomness from the clock.** `process_scratch_tag` derives a
  "random" tag from the low 32 bits of `SystemTime` nanoseconds. Two
  processes started in the same tick, or a clock with coarse resolution,
  collide. `O_EXCL` creation removes the need for a tag at all.
- **6.3 Host core count read from production logic** at three sites
  (1.3), with three fallbacks.
- **6.4 `std::process::id()` as identity** for scratch files (1.2).
- No direct `env::var` or `current_dir` reads in scope; the
  `Path::new(".")` fallbacks in 1.1 are an implicit cwd dependency when a
  bare filename is passed.

---

## 7. Shared state and locking

- **Renumber's single shared `IdSet` with `set_atomic`** is structurally
  fine for concurrent writes; its correctness for *reads* (rank) depends on
  `build_rank_index` happening after the scope joins, which the code
  order guarantees but the type does not. A `IdSetBuilder` (atomic, no
  rank) -> `IdSet` (frozen, rank) split would make "rank before all
  writes landed" unrepresentable.
- **Stage 2d's per-worker `IdSet` merge relies on `remove(0)`** and a
  comment saying the vec must be non-empty (1.3).
- **`mallopt`** is shared process state mutated from library code (1.9).
- **Scratch-file namespace** is shared state between concurrent
  processes, coordinated by PID only (1.2).

---

## 8. Unbounded growth

- **Sort holds every overlap run's re-encoded output in memory** before
  the write loop (`overlap_outputs: Vec<(Vec<OwnedBlock>, _)>`). The
  comment acknowledges "for pathologically unsorted input it can approach
  the input size". That is a planet-sized RSS on exactly the input sort
  exists for (a fully unsorted file - `degrade --unsort` output). Bound by
  processing runs in a sliding window of K in-flight runs, writing in
  order as they complete.
- **Reorder buffers are bounded only statistically.** `cat`'s
  `run_kind_phase` doc claims the in-flight set is capped "to roughly the
  decode-thread count". The channel bounds *queued* messages, but the
  consumer drains the channel into the `ReorderBuffer` regardless of
  whether the head-of-line seq has arrived, so one slow blob lets the
  buffer grow by however many blobs the other workers finish meanwhile.
  Same in all three renumber loops and repack/degrade. A credit scheme
  (worker may not claim seq > head + K) makes it structural.
- **Diff/derive scratch disk** is the full output size, written once to
  shard files and again to the final output (twice for derive: shard ->
  outer -> gzip), with no space check up front. At planet scale the
  derive comment estimates ~45 GB of XML.
- **Dedupe's sweep heap** is bounded by overlap-run width, which an
  adversarial input (one blob spanning the whole id range) makes the whole
  kind.

---

## 9. Code that is no longer load-bearing

- `diff/parallel.rs`: `let _ = options.verbose;` with a comment that the
  arm never runs with verbose set. A statement that does nothing,
  documenting a dispatch rule that lives in `mod.rs`. Replace with a
  `debug_assert!(!options.verbose)`.
- `derive_parallel.rs`: `let _ = element_version; // silence "unused helper" if future paths drop it`
  - suppressing a dead-code warning for a hypothetical future. If
  `element_version` is used, the line is dead; if not, the import is.
- `sort::build_blob_index(_direct_io)` and
  `diff::parallel::diff_block_pair_parallel(_direct_io)`: parameters kept
  "for signature stability" (sort's doc says so). Pre-1.0 and private;
  nothing needs the stability. Their presence implies `--direct-io` does
  something on these paths; it does not, and the CLI accepts the flag
  silently.
- `ChangeSink::write_create(_kind)` / `write_modify(_kind)`: unused
  parameter on a private method.
- `repack`: `_max_ids` from `build_classify_schedules_split` discarded;
  same in `cat_filtered`. Fine if the function serves other callers;
  worth checking whether any caller uses it.
- `degrade::DEFAULT_BLOCK_CAP` is `pub` and documented as a test seam;
  `--block-cap` is a hidden CLI flag for the same purpose. One of the two
  seams is enough.
- Comment rot: `renumber/mod.rs` header still says "(moved from the
  former src/commands/renumber.rs)"; `diff/parallel.rs` names
  `diff-par-*-{pid}-*` in a comment while the actual pattern is
  `diff-par-{pid}-*`; `derive_parallel.rs` refers to `assemble_osc_raw`,
  the function is `assemble_osc_from_paths`. `sort` cites
  `drain.rs:408-410` (line numbers, which AGENTS.md asks to avoid).

What tells me these are dead: in each case the binding is `_`-prefixed or
explicitly discarded at the only site, and the surrounding comment says
so. None requires a deletion that cannot be undone by reading.

---

## 10. Host assumptions and writes outside owned locations

- **Scratch next to the old input** (`diff -j`, 1.1): writes into a
  directory the command does not own and that is frequently another
  tool's (brokkr's dataset directory; a shared mirror). If read-only:
  refusal with an unattributed error. If writable: multi-GB temp files in
  a data directory that survive a `kill -9` and that brokkr's own
  `clean` does not know to remove. Should be: an explicit scratch dir
  owned by the invocation (1.1).
- **Scratch next to the output** (derive): acceptable when the CLI hands
  a `path_guard` sibling, but a library caller writing output to
  `/dev/stdout` or a FIFO gets `Path::new("/dev")` as scratch dir.
- **`std::os::unix::fs::FileExt`** (`read_exact_at`) is used directly in
  renumber and both parallel diff modules, making these commands
  Unix-only without a `cfg` saying so; the crate otherwise gates
  platform specifics behind features. On Windows this is a compile error,
  which is at least loud.
- **`mallopt`** assumes glibc (1.9); on musl the call compiles and does
  nothing, so the documented ~26 GB vs <1 GB RSS difference silently
  reappears.
- **Core count**: renumber's fixed 4 / 6 workers oversubscribe a 2-core
  host and under-use a 64-core one, silently (1.3).
- **`fadvise(RANDOM)` via `HeaderWalker`** in sort/renumber assumes a
  page cache that honours it; harmless elsewhere.

---

## Lateral

- `cat_output_header_bytes`'s "only ever understate" rule is good and
  deserves to be the crate-wide policy for any multi-input header
  (dedupe, and wherever else inputs are merged outside this scope:
  `merge-changes`?).
- The `parallel_classify_phase` abstraction in `scan::classify` is close
  to being the one pipeline for renumber, repack, degrade and cat
  (1.5, 1.6); renumber is the outlier that reimplements it and is also
  the one with the hang.
- `diff` dispatches to the parallel path only when both inputs are
  indexed; `derive` the same. The CLI defaults `-j 0` to all cores, so
  for indexed inputs the parallel path is the default and the sequential
  `diff_block_pair` / `derive_changes_block_pair` paths run only for
  `--verbose` or `-j 1`. The test suite should treat parallel as primary.
