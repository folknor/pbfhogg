# Library use: the library stops owning the process

Status: Phase 1 (audit + design). Awaiting user sign-off. No code has moved.

Written against `reference/technical-implementation-spec.md` (the contract
for this document). Spawned from bz pbfhogg#1 (`bz show 1`), with
`notes/bugs.md` BUG-066 and BUG-067 and `notes/hygiene.md` HYG-019 as
related items. Test bricks follow `reference/testing.md`. Measurement record:
`reference/performance.md`, `reference/performance-history.md`,
`.brokkr/results.db`.

Baseline for every identity gate: commit `fa94c6c7`, host `bygg`.

## 1. The problem, and two places the ticket's framing was wrong

Every command was tuned with the CLI as the only caller. The CLI owns its
process; a library caller does not. The command layer is `pub` but
`doc(hidden)` (ADR-0012), so it is reachable, and the format layer
(`BlobReader`, `ElementReader`, `PbfWriter`) is documented public API. Both
make decisions that belong to whoever owns the process.

The ticket asks how path-specific choices are "reproduced from CLI flags
without the CLI duplicating library internals". They cannot be, and this
design does not try:

- `time_filter` picks snapshot vs history from the **input header**
  (`has_historical_information`), not from any flag.
- `add-locations-to-ways --index-type auto` routes to external (and therefore
  to the `RLIMIT_NOFILE` raise) from `/proc/meminfo` `MemAvailable` read
  **after** an indexdata walk of the input.
- Even where a flag predicate exists (`cat`: `clean.any()`; `degrade`:
  `needs_decode()`), the cap today sits *after* validation and
  `require_indexdata`, so a CLI that applied it before the call would move it
  ahead of failures that currently precede it.

The resolution: the library keeps its path knowledge and, at exactly the
program point where it mutates process state today, it **calls the caller**
instead. The caller decides what (if anything) to do. The CLI's callback does
precisely what the library did, so its call sequence is identical by
construction.

Second reframing: the ticket treats this as "move the mallopt calls". The
audit found the same defect class in ten other forms (section 3): shared
page-cache eviction, ambient rayon pool initialisation, PID-as-run-identity
scratch names, env reads, once-per-process warning state, stderr ownership.
All are in scope.

## 2. Verified glibc semantics (M_ARENA_MAX)

Source: glibc 2.42 (`malloc/malloc.c`, `malloc/arena.c`), fetched from the
`bminor/glibc` GitHub mirror at tag `glibc-2.42` into
`research/glibc/malloc/` (gitignored). The host runs Debian glibc 2.43; the
mirror carries no 2.43 tag, and the arena code below has been stable across
releases, but this is 2.42 source, not 2.43.

- `__libc_mallopt` takes the main-arena lock, runs `malloc_consolidate` on
  the main arena (a side effect on every call), and for `M_ARENA_MAX` stores
  `mp_.arena_max = value` **only if `value > 0`**. Values `<= 0` are silently
  ignored and the call still returns 1. It overwrites any
  `GLIBC_TUNABLES=glibc.malloc.arena_max` the user set. It is not a syscall.
- `arena_get2` holds a function-static `narenas_limit`. It is computed lazily,
  the first time a thread needs a new arena while the free list is empty and
  either `mp_.arena_max != 0` or `narenas > mp_.arena_test` (`arena_test`
  defaults to `NARENAS_FROM_NCORES(1)` = 8 on 64-bit). Once nonzero it is
  **never recomputed**.
- Arenas are never destroyed; a thread's arena returns to a free list on
  thread exit and is reused.

Consequences, all confirmed against source:

1. Irreversible: once the limit latches at 2, nothing can raise it.
2. **Also silently ineffective**: if the process already latched the default
   limit (more than 8 arenas created before the cap), a later
   `mallopt(M_ARENA_MAX, 2)` changes `mp_.arena_max` but `narenas_limit` never
   looks at it again. In a library process that ran any multithreaded work
   first, the cap is a no-op. (The ticket recalled "values below 1 are
   ignored" and "computed once into a static" correctly; this second
   consequence was not in it.)
3. A cap set when `narenas` is already above 2 does not shrink anything; it
   only stops new arena creation.

## 3. Survey: every process-global decision or assumption in library code

"Reachable" means reachable from a library caller through a `pub` path,
including the `doc(hidden)` command layer.

### 3.1 Process-state mutations that outlive the call

| # | Site (file, function) | What it does | Reached from | Library-reachable |
|---|---|---|---|---|
| M1 | `src/debug.rs` `cap_malloc_arenas`, called from `src/commands/cat/mod.rs` `cat_filtered` | `mallopt(M_ARENA_MAX, 2)` | `cat::cat` when `clean.any()` (with or without a type filter), after `require_indexdata` | yes |
| M2 | `src/commands/check/refs.rs` `check_refs` | same, after the `CHECKREFS_SCAN_START` marker | `check::refs::check_refs` | yes |
| M3 | `src/commands/check/verify_ids.rs` `verify_ids_streaming_parallel` | same, first statement after the marker | `verify_ids` with `full == false` | yes |
| M4 | same file, `verify_ids_full_parallel` | same | `verify_ids` with `full == true` | yes |
| M5 | `src/commands/degrade/mod.rs` `degrade_decode_path` | same, after `require_indexdata` | `degrade` when `needs_decode()` (`--unsort`, `--unsort-intra`, `--strip-locations`, `--drop-ids`), after flag validation | yes |
| M6 | `src/commands/renumber/mod.rs` `renumber_external` | same, first statement | `renumber::renumber_external` | yes |
| M7 | `src/commands/repack/mod.rs` `repack` | same, after `require_indexdata` | `repack::repack` | yes |
| M8 | `src/commands/time_filter/mod.rs` `time_filter_snapshot` | same; the output `PbfWriter` and its writer thread already exist | `time_filter` when the input header lacks HistoricalInformation; the history path never caps | yes |
| M9 | `src/geocode_index/builder/mod.rs` `build_geocode_index` | same, after `config.validate()` | `build_geocode_index` | yes |
| M10 | `src/commands/altw/external/mod.rs` `raise_nofile_to_hard_cap`, called from `stage1.rs` `stage1_way_pass` | `getrlimit`, `setrlimit(RLIMIT_NOFILE, soft = hard)`, `getrlimit`; worker count sized from the result | `add_locations_to_ways` with `external`, or `auto` routed external; `altw::external::external_join` directly | yes |
| M11 | `src/write/writer.rs` `write_primitive_block`, `write_primitive_block_no_indexdata`, `write_primitive_block_owned` | `rayon::spawn` into the current registry or the **global** pool; first use initialises the global pool from `RAYON_NUM_THREADS` / core count, after which the caller's `ThreadPoolBuilder::build_global` fails | every pipelined `PbfWriter` (public `to_path*` constructors and all command writers) | yes |
| M12 | rayon `par_iter` / `into_par_iter` / `par_chunks` / `par_sort` in `sort`, `repack`, `degrade`, `extract/simple`, `merge_changes`, geocode `admin`/`interp`/`pass3`, `osc/merge_join` (`rayon::join`) | uses the ambient pool, initialising the global pool if needed; work issued from library-spawned std threads always lands in the global pool even if the caller wrapped the call in `pool.install` | those commands | yes |
| M13 | `src/write/writer.rs` framing closures | a panic inside goes to the pool's panic handler; with no handler (the global pool's default) rayon prints `Rayon: detected unexpected panic; aborting` and aborts the process | every pipelined `PbfWriter` | yes (BUG-066) |
| M14 | `src/read/blob_wire.rs` `warn_unrecognised_indexdata` | a static `Once`: the first run in the process consumes the warning for every later run | every public read path that parses a `BlobHeader` | yes |
| M15 | `src/debug.rs` `write_fifo` | reads env `BROKKR_MARKER_FIFO` lazily on the first marker, opens it, caches success **or absence** in a static `OnceLock` forever | every command (hundreds of marker/counter sites) | yes |
| M16 | `src/write/framing.rs` `PIPELINE_SCRATCH`, `src/read/decompress.rs` `ZLIB_DECOMPRESS` (thread_local) | framing buffers and compressor state retained on whatever thread ran them; on the caller's long-lived rayon workers they survive the writer | pipelined `PbfWriter`; every decompress | yes |
| M17 | `src/write/metrics.rs` `WRITER_METRICS`, `src/read/pipeline_metrics.rs` `PIPELINE_METRICS` | process-global counters; concurrent runs mix | everywhere | yes, diagnostic-only (FIFO), harmless |

Not findings (checked): `src/read/reader.rs` swaps the panic hook with
`std::panic::set_hook` only inside `#[cfg(test)] mod tests`
(`a_panicking_pipeline_thread_is_not_reported_as_end_of_file`), unreachable
from library code. No `build_global`, signal handler, `umask`,
`set_current_dir`, `set_var`, `fork`/`exec` or `std::process::exit` anywhere
in `src/`. `debug::malloc_trim` and `debug::set_mmap_threshold` are `pub`
but have no callers. The `test-hooks` statics are compiled out of release
builds. `rayon::current_num_threads` / `RAYON_NUM_THREADS` are never read by
library code.

### 3.2 Shared-host policy (outlives the call, affects other processes)

| # | Site | Behaviour | Library-reachable |
|---|---|---|---|
| H1 | `src/read/blob.rs` `BlobReader::next` (file-backed constructors) | `posix_fadvise(DONTNEED)` over the whole consumed prefix after every blob, **on by default**; opt-out `evict_page_cache(false)`; `ElementReader::from_path_with_eviction` / `evict_page_cache` forward it | yes, every public file reader and every command |

Read hints (`POSIX_FADV_RANDOM` in `src/read/header_walker.rs`
`HeaderWalker::open`, `SEQUENTIAL` in `src/read/file_reader.rs`, `WILLNEED`
in `src/scan/classify.rs`) are per-fd access hints that drop nobody else's
pages; they stay library behaviour. `DONTNEED` on operation-owned scratch
files (`altw/external/radix.rs` `advise_dontneed_file`,
`altw/external/stage2.rs`) is scratch management, not host policy.

### 3.3 Output ownership (stderr)

Stdout is settled (report types render into a caller `io::Write`;
`cli/src/output.rs` is the only stdout owner; `no-print-macro` and
`no-stdout-handle` textlint rules). Stderr is not: **95 `eprintln!` sites in
`src/`** (one is test-only in `read/direct_reader.rs`). By kind:

- Summaries (`print_summary` on `CatStats`, `MergePbf` stats in
  `cat/dedupe.rs`, `DegradeStats`, `DiffStats` (+ `print_osmium_summary`),
  `DeriveChangesStats`, `GetparentsStats`, `GetidStats`, `MergeChangesStats`,
  `ExtractStats`, `RenumberStats`, `TagsFilterStats`, `TagsFilterOscStats`,
  `SortStats`, apply-changes `MergeStats` in `apply_changes/stats.rs`,
  `RepackStats`, `ExportStats`, altw `Stats`, `TimeFilterStats`). The CLI
  calls all of them except apply-changes, whose `merge` calls its own
  `print_summary` at the end of `apply_changes/rewrite.rs` `merge`.
- Progress: `sort/mod.rs` ("Pass 1: indexing blobs..." and four more),
  `cat/dedupe.rs` (same shape), `apply_changes/rewrite.rs` (4),
  `osc/parse.rs` `load_all_diffs` (2, public `osc` module), geocode builder
  (`builder/mod.rs` 21 incl. 9 `key=value` lines under `cfg(feature =
  "hotpath")`, `pass2.rs` 2), `extract/mod.rs` and `extract/multi.rs`,
  `getid` (3 `[getid]` lines), `tags_filter` (2), `tags_count` (1).
- Warnings and notes: `commands/mod.rs` `warn_locations_on_ways_loss`,
  `check/verify_ids.rs` `warn_unindexed_triple_scan`, `inspect/scan.rs`,
  `repack` (3), `sort` (sortedness note), `altw/mod.rs` (6, incl. three
  "auto-selected --index-type" lines), `altw/sparse.rs` (stale temp file),
  `read/blob_wire.rs` (M14).
- Developer diagnostics: `altw/external/stage1.rs` (`[altw external]
  RLIMIT_NOFILE ...` and two `num_workers` lines), `altw/external/mod.rs`
  (1), `write/parallel_writer.rs` `parallel_writer_thread` and
  `write/uring_writer.rs` `uring_writer_thread` (each prints the error it
  also returns).

`eprintln!` panics on a write error (BUG-067). A library caller cannot
silence, route or capture any of it.

### 3.4 Resource sizing to the whole machine

Per-call resources, released when the call returns (except M16). All read
`std::thread::available_parallelism()` with **site-specific formulas and
fallbacks** that must be preserved exactly:

| Site | Formula (fallback) | Caller override today |
|---|---|---|
| `read/reader.rs` `par_map_reduce` | n-1, min 1 (3) | `decode_threads` |
| `read/pipeline.rs` decode pools (two) | n-2, min 1 (4) | `decode_threads` |
| `scan/classify.rs` `resolve_thread_count` | n-2, min 1 (4) | `Option` param |
| `commands/tags_filter/mod.rs` `tags_filter_two_pass` | n-2, min 1 (4) | one path only |
| `commands/apply_changes/rewrite.rs` | n-2, min 2 (4) | `jobs` |
| `commands/merge_changes/mod.rs` `merge_changes` | n (8); `jobs.is_some()` (including `Some(0)`) decides whether a scoped pool is built | `jobs` |
| `commands/diff/derive.rs` `assemble_osc_from_paths` | n (4), gzip workers | **none** (ignores the `jobs` the CLI resolved) |
| `commands/altw/passthrough.rs` `write_output_passthrough` | see source | none |
| `commands/altw/external/mod.rs` `external_join`, `stage1.rs`, `stage2.rs`, `stage3.rs`, `stage4.rs` | n-2 and variants | none |
| `commands/extract/simple.rs` (two), `extract/multi.rs` (two), `extract/common.rs` `pread_execute` | n-2 variants | none |
| `commands/renumber/relations.rs` `relation_r2d_assembly` | see source | none |
| `geocode_index/builder/pass1_5.rs` `run_pass1_5` | see source | none |
| CLI `run_diff`, `run_derive_changes` | n (1) when `-j 0` | CLI resolves, library takes the value (the existing precedent) |

Memory sized to the machine: only `commands/altw/mod.rs`
`page_cache_budget_bytes` (`MemAvailable` minus 3 GB headroom), read after
the estimate walk in `auto_select_index_type`; `None` routes external.

Fixed, not host-derived: `parallel_writer::POOL_SIZE` = 16 workers,
`renumber` `PASS1_WORKERS` = 4 / `STAGE2D_WORKERS` = 6. Uring registered
buffers are charged to `RLIMIT_MEMLOCK` (consumption, not a decision).

### 3.5 Process identity used as run identity (correctness bugs for library callers)

| Site | Name | Collision |
|---|---|---|
| `altw/external/radix.rs` `ScratchDir::new` | `.pbfhogg-<name>-<pid>` | two `external_join` runs in one process share the dir; either `Drop` `remove_dir_all`s it under the other |
| `altw/sparse.rs` `open_anonymous_sparse_file` | `.pbfhogg-sparse-index-<pid>` | one run's stale-file removal deletes another's file during its create/preallocate/unlink window |
| `diff/derive.rs` `ChangeSink::new` | `derive-{creates,modifies,deletes}-<pid>.xml.tmp` | collide |
| `diff/parallel.rs`, `diff/derive_parallel.rs` | PID plus `process_scratch_tag()` (one random tag per **process**) | concurrent runs share the tag |

Scratch location: next to the output (`output.parent()`, `"."` fallback) in
`altw`, `derive`; **next to the old input** in `diff/parallel.rs`
`diff_block_pair_parallel`. No caller override anywhere.

### 3.6 Standing decisions this touches

- ADR-0012 (command layer stays `doc(hidden)`): honoured. Adding a `host`
  parameter to every command reshapes the bridge, which ADR-0012 permits
  pre-1.0; nothing is promoted to documented API except the new `host`
  module, which the documented `PbfWriter` constructors need.
- ADR-0011 (`commands` feature scope): unaffected; `host` compiles in every
  feature configuration.
- ADR-0013 (metadata trusted): unaffected.
- ADR-0014 (output backends stay separate): honoured; each backend keeps its
  own thread, the diagnostics handle is passed to each.
- `reference/testing.md` "What the shared binary shares" states that the
  arena cap and `RLIMIT_NOFILE` raise are shared by the `all` binary. After
  this lands, tests that build a `Host` without the legacy actions no longer
  set them: that paragraph is updated in brick B2.
- `reference/performance.md` says the cap is applied "inside
  `renumber_external()`": reworded in brick B2 to name the hook, not a
  mallopt call site.

## 4. The rule

Two categories, decided by whether the effect outlives the call or reaches
other parties:

- **Process state and shared-host policy** (M1-M10, M13-M15, H1, the global
  rayon pool): the library never does it by default. Either the caller
  performs it through a callback invoked at the library's chosen point, or
  the caller selects it explicitly.
- **Per-call resources** (threads, memory budgets, fixed pools): every one
  must be settable by every caller. The command layer takes them only through
  `Host`, which has no `Default`. The documented format layer may keep
  documented machine-sized defaults where it already has an override
  (`ElementReader::decode_threads`). Thread-local retention (M16) is the one
  per-call resource that survives the call; it is documented on `Executor`,
  and `Executor::Pool` is how a caller bounds it.

## 5. Target artifacts

### 5.1 `src/host.rs` (new, `pub mod host`, compiled in every feature set)

```rust
pub struct Host<'a> {
    pub process: &'a dyn ProcessActions,
    pub resources: &'a dyn Resources,
    pub executor: Executor,
    pub diagnostics: Diagnostics,
    pub page_cache: PageCachePolicy,
    pub scratch_parent: Option<&'a std::path::Path>,
}
// No Default impl, deliberately.

/// Process-state actions, invoked by the library at exactly the program
/// points where it needs them. Invocation count and position are part of
/// the compatibility contract: never deduplicated, never hoisted.
pub trait ProcessActions: Send + Sync {
    /// A worker pool is about to start whose buffers are allocated on one
    /// thread and freed on another (cross-thread arena fragmentation).
    fn cross_thread_alloc_pool(&self);
    /// The run wants `wanted` descriptors open concurrently.
    fn fd_allowance(&self, wanted: u64) -> FdAllowance;
}

pub struct FdAllowance {
    /// Descriptors this operation may hold open at once.
    pub descriptors: u64,
    /// What the actions did to the process limit, for narration. `None`
    /// when nothing was attempted.
    pub adjustment: Option<FdAdjustment>,
}
pub enum FdAdjustment { ProbeFailed, AlreadyAtCap, Raised { from: u64 }, RaiseFailed }

/// No process mutation. fd_allowance = current soft limit via getrlimit
/// (clamped like today), adjustment None; 1024 if the probe fails.
pub struct NoProcessActions;

/// Probed where the library needs the figure, not up front.
pub trait Resources: Send + Sync {
    /// `None` = probe unavailable; each call site keeps its own fallback.
    fn parallelism(&self) -> Option<std::num::NonZeroUsize>;
    /// Bytes of memory the run may assume available; `None` = unknown
    /// (altw auto then routes external, as today).
    fn memory_available(&self) -> Option<u64>;
}
/// Exactly today's probes: available_parallelism().ok(), and
/// /proc/meminfo MemAvailable (kB * 1024).
pub struct MachineProbe;
pub struct Fixed { pub parallelism: Option<std::num::NonZeroUsize>, pub memory: Option<u64> }

#[derive(Clone)]
pub enum Executor {
    /// rayon's ambient pool: the current registry, else the global pool
    /// (initialising it if needed). Thread-local framing and decompress
    /// scratch persists on those workers.
    Ambient,
    /// A caller-owned pool. Its panic_handler decides framing-panic
    /// disposition; dropping it frees thread-local scratch.
    Pool(std::sync::Arc<rayon::ThreadPool>),
}
impl Executor {
    pub(crate) fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R;
    pub(crate) fn spawn(&self, f: impl FnOnce() + Send + 'static);
    /// Some(..) iff the current thread is a worker of the pool `spawn`
    /// targets (Ambient: rayon::current_thread_index(); Pool:
    /// pool.current_thread_index()).
    pub(crate) fn on_worker(&self) -> bool;
    /// rayon::yield_now() / pool.yield_now().
    pub(crate) fn yield_now(&self) -> Option<rayon::Yield>;
}

#[derive(Clone, Copy)]
pub enum PageCachePolicy {
    /// DONTNEED behind the read head on caller-supplied input files.
    Evict,
    /// Leave the page cache alone.
    Keep,
}

#[derive(Clone)]
pub struct Diagnostics {
    sink: std::sync::Arc<dyn DiagnosticSink>,
    once: std::sync::Arc<OnceKeys>,
}
impl Diagnostics {
    pub fn new(sink: std::sync::Arc<dyn DiagnosticSink>) -> Self; // fresh OnceKeys
    pub fn with_once(sink: std::sync::Arc<dyn DiagnosticSink>, once: std::sync::Arc<OnceKeys>) -> Self;
    pub fn discard() -> Self;
    pub(crate) fn emit(&self, kind: EventKind, key: &'static str, args: std::fmt::Arguments<'_>);
    /// Emits iff `key` was not yet emitted through this OnceKeys.
    pub(crate) fn emit_once(&self, kind: EventKind, key: &'static str, args: std::fmt::Arguments<'_>);
    /// Adapter for write_summary: each write_fmt call is one Summary event.
    pub(crate) fn summary_writer(&self) -> impl std::io::Write + '_;
}
pub trait DiagnosticSink: Send + Sync {
    fn event(&self, event: &Event<'_>);
}
pub struct Event<'a> { pub kind: EventKind, pub key: &'static str, pub args: std::fmt::Arguments<'a> }
#[non_exhaustive]
pub enum EventKind { Progress, Warning, Note, Summary, Developer }
pub struct OnceKeys { /* Mutex<HashSet<&'static str>> */ }
```

`key` is a stable dotted identifier per message (`sort.pass1`,
`indexdata.unrecognised_length`, `altw.external.nofile`, ...), so library
callers can filter without parsing text. The `emit` helpers are wrapped in
crate macros (`diag!(host, Warning, "key", "fmt", args)`) so each former
`eprintln!` becomes exactly one event with the identical format string.

### 5.2 `src/process.rs` (new, `pub mod process`)

The former private mutations, as plain public helpers a caller may invoke:

```rust
/// mallopt(M_ARENA_MAX, 2) on glibc; no-op elsewhere. Documents the
/// section 2 semantics: irreversible, and ineffective once glibc has
/// latched its arena limit.
pub fn cap_malloc_arenas();
/// Today's raise_nofile_to_hard_cap, returning FdAllowance with
/// adjustment Some(..).
pub fn raise_nofile_to_hard_cap() -> crate::host::FdAllowance;
```

`debug::cap_malloc_arenas` is deleted; `FdBudget` / `FdRaiseStatus` in
`altw/external/mod.rs` are replaced by `FdAllowance` / `FdAdjustment`, whose
`Display` renders today's four strings byte for byte (`RLIMIT_NOFILE: ...`).
For `adjustment == None` it renders `RLIMIT_NOFILE: <n> (not adjusted)`
(library-only text; the CLI never produces it).

### 5.3 Command layer

Every command entry point gains `host: &Host<'_>` as its last parameter:
`cat`, `merge_pbf`, `sort`, `merge` (apply-changes), `merge_changes`,
`renumber_external`, `time_filter`, `degrade`, `repack`, `getid`,
`getparents`, `tags_filter`, `tags_filter_osc`, `tags_count`,
`add_locations_to_ways`, `external_join`, `extract`, `extract_multi`,
`export`, `diff`, `derive_changes`, `inspect`, `check_refs`, `verify_ids`,
`check_index`. `build_geocode_index(config, host)`: `BuildConfig` keeps its
`Default` and does not hold the host. `osc::parse::load_all_diffs` gains
`diagnostics: &Diagnostics`.

Inside:

- Each `crate::debug::cap_malloc_arenas()` (M1-M9) is replaced **in place**
  by `host.process.cross_thread_alloc_pool()`. Nothing moves.
- `stage1_way_pass` replaces `raise_nofile_to_hard_cap()` with
  `host.process.fd_allowance(wanted)`, `wanted = cpu_cap * NUM_BUCKETS +
  HEADROOM_FDS`, keeping the subsequent arithmetic on
  `allowance.descriptors` and narrating `allowance` via
  `Developer`/`altw.external.nofile`.
- Every `available_parallelism()` in section 3.4 becomes
  `host.resources.parallelism()` at the same site, keeping that site's
  formula and fallback (`.map(|n| ...)` on the `Option`, `.unwrap_or(<same
  constant>)`). `merge_changes` keeps `jobs.is_some()` as the pool test. The
  CLI keeps resolving `-j 0` in `run_diff` / `run_derive_changes`.
  `derive.rs` gzip workers stay an independent probe (through `resources`).
  Internal helpers that probe (`scan/classify.rs` `resolve_thread_count`,
  `read/pipeline.rs`) take the resolved `Option<NonZeroUsize>` from their
  command caller; the public `ElementReader` path keeps calling the std
  probe for its documented default.
- `page_cache_budget_bytes` becomes `host.resources.memory_available()
  .map(|b| b.saturating_sub(AUTO_ANON_HEADROOM_BYTES))`, called at the same
  point after the walk. `MachineProbe` reads `/proc/meminfo` exactly as the
  old function did.
- Every `par_iter`-family call and `rayon::join` in section 3.1 M12 is
  wrapped in `host.executor.install(|| ...)` **around the parallel
  computation only**, never around a whole command body (so no producer
  path moves onto a rayon worker). `Ambient.install(f)` is `f()`.
- Every internal reader construction takes `host.page_cache` and
  `host.diagnostics` (crate-internal constructors with both as required
  arguments; see 5.5). Every internal pipelined writer takes
  `host.executor` and `host.diagnostics`.
- `RunToken` (5.7) is allocated once at the entry of each command that
  creates scratch (`altw` both backends, `diff`, `derive_changes`), and
  `scratch_parent.unwrap_or(<today's default>)` is the scratch directory.
- Every `eprintln!` becomes one `diag!` event. `print_summary(&self)`
  becomes `write_summary(&self, w: &mut impl std::io::Write) ->
  std::io::Result<()>` with exactly one `writeln!` per former `eprintln!`
  (same format strings); `print_osmium_summary` likewise. apply-changes
  `merge` calls `final_stats.write_summary(&mut
  host.diagnostics.summary_writer())` at the existing point.

### 5.4 `PbfWriter` (documented API, breaking)

- `to_path`, `to_path_direct`, `to_path_uring`, `to_path_parallel` gain a
  required `executor: Executor` argument. `PbfWriter::new` (sync, no rayon)
  is unchanged.
- Crate-internal `pub(crate)` constructors additionally take `Diagnostics`;
  the public ones pass `Diagnostics::discard()`. The writer threads'
  `[parallel_writer] thread exit: {e}` / `[uring_writer] error: {e}` lines go
  through it (`Developer`).
- The three `rayon::spawn` sites become `executor.spawn`.
- BUG-066 liveness (5.8).

### 5.5 Readers (documented API, breaking default)

- `BlobReader::from_path` / `open`, `ElementReader::from_path` / `open` and
  friends default to **no eviction**; `evict_page_cache(true)` opts in.
  `ElementReader::from_path_with_eviction` stays.
- `WireBlobHeader::parse` stops printing; it reports an unrecognised
  indexdata length in its return value. The owning reader
  (`BlobReader`, `HeaderWalker`, the raw-frame readers, the pipeline frame
  pumps) emits `diagnostics.emit_once(Warning,
  "indexdata.unrecognised_length", ...)` with today's text. Readers built by
  public constructors hold `Diagnostics::discard()`; a `.diagnostics(d)`
  builder sets one.
- Crate-internal constructors take `(PageCachePolicy, &Diagnostics)` as
  required arguments, and a textlint rule (B5) forbids the public discard
  constructors under `src/commands/` and `src/geocode_index/builder/`, so no
  internal path can silently drop a CLI warning or eviction.

### 5.6 Markers

`debug.rs` gains `pub fn enable_marker_fifo_from_env()`, which only sets a
static `AtomicBool`. `write_fifo` checks that flag **before**
`STATE.get_or_init`, so an early disabled emission cannot cache `None`
forever; the env read, FIFO open and clock anchoring stay inside the
original lazy first-emission path. Without the call, markers and counters
are no-ops and the library never reads the environment. Process-global by
design: the only consumer (brokkr) attributes by process. `emit_mallinfo2`
gates its `mallinfo2()` call on the same flag (see section 11).

### 5.7 `RunToken` (`src/scratch.rs`, crate-internal)

```rust
pub(crate) struct RunToken(u64);
impl RunToken {
    /// From a process-wide AtomicU64; fetch_add with overflow check (wrap is
    /// an error, never reuse).
    pub(crate) fn next() -> crate::BoxResult<Self>;
    /// "" for token 0, "-<n>" otherwise.
    pub(crate) fn suffix(&self) -> String;
}
```

Every scratch name is a pure function of `(token, existing identifiers)`:
`.pbfhogg-<name>-<pid><suffix>`, `.pbfhogg-sparse-index-<pid><suffix>`,
`derive-creates-<pid><suffix>.xml.tmp`, the shard paths in `diff/parallel.rs`
and `derive_parallel.rs` (which keep `process_scratch_tag()` as extra
cross-process protection). Failed-shard cleanup recomputes from the same
token. Token 0 reproduces every legacy name, and the CLI runs one command per
process, so CLI names are unchanged. `altw/sparse.rs` stale-file removal
only ever targets its own token's name.

### 5.8 BUG-066 (in the main design)

Panic half: **no `catch_unwind`.** Disposition belongs to the pool owner,
which rayon already exposes (`ThreadPoolBuilder::panic_handler`). With no
handler (the CLI's global pool) behaviour is today's abort, byte for byte.
With a swallowing handler, the closure has already unwound: `PermitGuard`
returns the permit, the sender clone drops, no item is sent for that seq,
and `check_stream_complete` in the writer thread reports the interior or
trailing gap at `flush()`. Contract, documented on `flush` and `Drop`:
**`flush()` is completion; `Drop` is cleanup only** (it announces no total
and discards the join result, so a missing tail is not reported there).

Deadlock half: a blocking wait while the current thread is a worker of the
pool the framing tasks were spawned into starves those tasks. Concrete
library reproducer: `ThreadPoolBuilder::new().num_threads(1).build()?
.install(|| /* any command or direct PbfWriter use */)`: the 65th
`write_primitive_block` blocks in `permit_rx.recv()` on the only worker.
All four blocking points become cooperative when `executor.on_worker()`:

1. `write_primitive_block`, `write_primitive_block_no_indexdata`,
   `write_primitive_block_owned` permit wait;
2. `acquire_permit` (raw-write paths);
3. `flush` before joining: wait until all `PIPELINE_DISPATCH_PERMITS`
   permits are back (every framing task has passed its send), then join
   and check the writer result as today;
4. `Drop`: same wait, then join.

Cooperative wait: `loop { match rx.try_recv() { Ok => break,
Err(Disconnected) => error, Err(Empty) => match executor.yield_now() {
Some(Executed) => continue, Some(Idle) => recv_timeout(1 ms) then
continue, None => blocking recv } } }`. Off-worker (every CLI path today;
codex checked sort, extract pass 2, repack, degrade, reader pipelines,
altw stage 4, merge_changes) the code path is the existing blocking
`recv`, so CLI scheduling is unchanged. Not covered: a `PbfWriter` moved
between pools mid-stream under `Ambient`; documented.

### 5.9 BUG-067 (in the main design)

Library: closed, because the library no longer writes stderr at all.
CLI: unchanged in the main design (the CLI sink uses `eprintln!`, so a
closed stderr still panics and exits 101 exactly as today). The fix is
proposed in section 9.

### 5.10 CLI side (`cli/src/host.rs`, new)

```rust
struct LegacyProcessActions;
impl ProcessActions for LegacyProcessActions {
    fn cross_thread_alloc_pool(&self) { pbfhogg::process::cap_malloc_arenas() }
    fn fd_allowance(&self, _wanted: u64) -> FdAllowance { pbfhogg::process::raise_nofile_to_hard_cap() }
}
struct StderrSink;
impl DiagnosticSink for StderrSink {
    fn event(&self, e: &Event<'_>) { eprintln!("{}", e.args) }
}
```

`std::io::stdio::print_to` has no `#[track_caller]`, so the panic text
(`failed printing to stderr: <e>`, located in std), the per-call stderr
lock and the bytes are identical to the old per-site `eprintln!`. The
`summary_writer` adapter overrides `write_fmt` to emit one Summary event per
call, so each `writeln!` is one `eprintln!`-equivalent, with the same lock
granularity and failure behaviour. The CLI's `OnceKeys` is one
process-lifetime instance, which reproduces the old `Once`.

`main` calls `pbfhogg::debug::enable_marker_fifo_from_env()` first, then
builds one `Host { process: &LegacyProcessActions, resources: &MachineProbe,
executor: Executor::Ambient, diagnostics: Diagnostics::with_once(StderrSink,
process OnceKeys), page_cache: PageCachePolicy::Evict, scratch_parent: None
}` and passes it to every command. CLI-side `print_summary()` calls become
`stats.write_summary(&mut host.diagnostics.summary_writer())`.

## 6. Breaking changes (CHANGELOG material)

1. `PbfWriter::to_path*` take a required `Executor`.
2. Public file readers no longer evict the page cache by default.
3. Every command entry point takes `&Host`; `print_summary` is replaced by
   `write_summary`; `osc::parse::load_all_diffs` takes `&Diagnostics`.
4. The library no longer writes to stderr, calls `mallopt`, raises
   `RLIMIT_NOFILE`, or reads `BROKKR_MARKER_FIFO` unless the caller asks.
5. `debug::cap_malloc_arenas` moves to `process::cap_malloc_arenas`.

## 7. Verification instruments (built before the bricks they gate)

strace, ltrace, gdb, bpftrace and uftrace are not installed on `bygg`.
Codex confirmed `target/release/pbfhogg` is dynamically linked and imports
`mallopt`, `getrlimit`, `setrlimit`, `pthread_create`, `posix_fadvise` as
undefined `GLIBC_2.x` symbols, so `LD_PRELOAD` interposition sees them;
the harness re-checks the imports of both binaries under test.

**I1. `tools/process-trace-shim/`**: a `cdylib` workspace member, built by
the `all` sweep's `build_packages`. Wraps `mallopt`, `getrlimit`,
`setrlimit`, `posix_fadvise` (and any `64` ABI variants present in the
imports), and `pthread_create` (with a start trampoline recording logical
thread id, start and end). Each record: logical thread id, call, arguments,
return value, errno, and for `getrlimit` the output struct; for
`posix_fadvise` the file identity (dev, inode via `fstat`) and advice.
Logging is allocation-free: fixed-size records formatted on the stack,
written with one `write(2)` to an fd preopened at load time from
`PROCESS_TRACE_FD`, symbol resolution via `dlsym(RTLD_NEXT)` guarded
against recursion, errno preserved. (An env var read by a test-only shim,
not by product code.)

**I2. `tests/cli_host_identity.rs`** (`mod tier3`, `#[ignore]`, CliInvoker,
binary from `BROKKR_TEST_BIN_DIR`): runs the scenario matrix below under
I1, records the trace, stdout, stderr, exit status and terminating signal
(abort is never collapsed into an exit code), and either writes goldens
(`PBFHOGG_IDENTITY_BLESS=1`, test-only) to
`tests/fixtures/host_identity/` or compares. Comparison is structured:
logical thread ids, file identities mapped to logical names, causal order
per thread (not one global order), wall-clock figures and PIDs normalised,
everything else byte-exact. Raw captures are kept beside the normalised
ones.

Scenario matrix (tiny in-tree fixtures plus generated ones): `cat`
passthrough / `--type way` / `--clean version` / `--type way --clean
version` / `--dedupe`; `check` default / `--ids` / `--ids --full` /
`--refs` / `--index` / `--index --refs`; `degrade --strip-indexdata` vs
`--strip-locations`; `repack`; `renumber`; `time-filter` on a snapshot
fixture and on a history fixture (HistoricalInformation set); `build-geocode-index`;
`add-locations-to-ways --index-type sparse` / `external` / `auto`;
`diff`, `derive-changes`, `apply-changes` (in-process summary ordering).
Failure boundaries: a validation failure before each hook (`degrade` with
no flag, `repack --elements-per-blob 0`, unindexed input without
`--force`), `external` under a child soft `RLIMIT_NOFILE` lowered below the
minimum and under a hard limit equal to soft (raise rejected / already at
cap), and stderr closed (BUG-067 path: exit 101 must survive in the main
design).

**I3. Library contract tests** (`tests/host_contract.rs`, module of
`all`, stable allowlist plus `pbfhogg::host`): `Fixed` resources reach each
site with its formula and fallback (`None` gives 3 / 4 / 8 / 1 as today);
`NoProcessActions` never changes `getrlimit`; public reader constructors do
not call `posix_fadvise(DONTNEED)` (via `BlobReader` evict-call counter);
`PbfWriter::to_path(.., Executor::Pool(one-thread pool))` driven from
inside that pool's `install` completes 200 blocks and `flush`es (BUG-066
deadlock regression), and the same with an early return (Drop path); a
swallowing `panic_handler` plus a test-hooks framing panic yields an error
from `flush`; two concurrent `cat --clean` runs with different
`Diagnostics` each receive only their own events; two concurrent
`external_join` runs in one process get distinct scratch dirs and both
succeed.

## 8. Bricks, gates and landing order

Each brick is one commit, kept or reverted on its gates. `brokkr check`
stays green at every boundary. Gate datasets: the identity matrix uses
in-tree fixtures, sufficient because every question here is wiring and
call order, never scale; `verify` uses denmark `indexed` because it is the
smallest dataset the oracle suite is configured for.

**Neutrality.** The design touches measured paths but claims no
performance effect: the executed instruction stream on CLI paths changes
only by dyn calls at setup points and per diagnostic line. The neutrality
gate is I2 (identical `mallopt`, `setrlimit`, `pthread_create` and
`posix_fadvise` traces mean identical threads, arena timing relative to
thread creation, and eviction). A bench pair is listed as an optional gate
on B4 only; see open question Q4.

- **B0 instruments.** I1 and I2, goldens blessed against the baseline
  build. Gates: `brokkr check`; then, with `BROKKR_TEST_BIN_DIR` pointing at
  the baseline (`fa94c6c7`) release build, `brokkr test
  cli_host_identity_bless --sweep all --timeout 280` writes goldens, and
  `brokkr test cli_host_identity --sweep all --timeout 280` passes against
  the same build (the harness is deterministic).
- **B1 `host` and `process` modules, CLI host, hooks in place (M1-M10),
  Resources at every 3.4 site, markers armed by the CLI (5.6).** Commands
  take `&Host`. Diagnostics plumbed but still unused (sinks wired, every
  `eprintln!` still in place). Gates: `brokkr check`; `brokkr check
  --profile full`; `brokkr test cli_host_identity --sweep all --timeout
  280`; `brokkr verify all --dataset denmark --variant indexed`.
- **B2 diagnostics.** Every `eprintln!` in `src/` becomes one `diag!`
  event; `write_summary`; `blob_wire` detection moved to readers; writer
  thread lines through the writer's `Diagnostics`; apply-changes summary at
  its existing point. Doc updates: `reference/testing.md` shared-binary
  paragraph, `reference/performance.md` renumber wording,
  `debug.rs`/`process.rs` rustdoc. Gates: `brokkr check`; `brokkr test
  cli_host_identity --sweep all --timeout 280` (stderr byte identity is the
  point of this brick); I3 diagnostics-isolation test via `brokkr check`.
- **B3 page-cache policy and readers.** Public default flips to `Keep`;
  crate-internal constructors require policy and diagnostics. Gates: `brokkr
  check --profile full` (roundtrip suite: reader change); `brokkr test
  cli_host_identity --sweep all --timeout 280` (fadvise trace identical);
  `brokkr verify all --dataset denmark --variant indexed`.
- **B4 executor and BUG-066.** `Executor`, `PbfWriter` constructors,
  `executor.spawn`, `install` around M12 sites, cooperative waits at all
  four points. Gates: `brokkr check --profile full` (writer change:
  roundtrip suite); `brokkr test cli_host_identity --sweep all --timeout
  280` (pthread_create trace identical); I3 BUG-066 tests via `brokkr
  check`; optional per Q4: interleaved `brokkr cat --dataset denmark
  --variant indexed --clean version --bench 1` cells against `--commit
  fa94c6c7`.
- **B5 RunToken, scratch parent, enforcement.** 5.7, plus textlint rules
  in `brokkr.toml`: `no-eprint-macro` (`\beprint(ln)?!` in `src/**`,
  region code, excluding `#[cfg(test)]` modules is not expressible, so the
  one test-only site in `read/direct_reader.rs` switches to a skip return
  without printing); `no-process-mutation` (`mallopt|setrlimit` outside
  `src/process.rs`); `no-host-probe` (`available_parallelism|/proc/meminfo`
  outside `src/host.rs` and the documented `ElementReader` default in
  `src/read/`); `no-env-read` (`std::env::var` in `src/**` code outside
  `src/debug.rs`); `no-public-reader-in-commands`. Gates: `brokkr check`;
  `brokkr test cli_host_identity --sweep all --timeout 280`; I3 scratch
  test.
- **B6 ADR and CHANGELOG.** `decisions/0016-the-library-does-not-own-the-process.md`
  (rule of section 4; alternatives: grant flags, CLI pre-application from
  flags, `log` facade, `catch_unwind`, per-allocation scratch counter) and
  index entry; CHANGELOG entries for section 6. Gate: `brokkr check`
  (gremlins).

## 9. Defended CLI changes (separate from the main design)

The main design changes no CLI behaviour. Two changes are proposed on their
own merits; each lands only with explicit sign-off.

**C1. BUG-067: a closed stderr no longer kills the run.** Today
`pbfhogg sort ... 2>&1 | head` exits 101 once `head` leaves, abandoning the
output file mid-write. Defence: the same reasoning `cli/src/output.rs`
already applied to stdout; a diagnostics reader going away is not a
failure of the command, and killing a long write because nobody is reading
the progress lines is strictly worse for every user. Mechanism: every CLI
stderr path (the `StderrSink`, the summary adapter, the CLI's own
`eprintln!` sites including `main`'s final `Error: {e}`) goes through one
`PipeSafeStderr`. Its stderr reader-gone flag is separate from stdout's and
never feeds the stdout success branch. The flag is checked while holding
the stderr lock, so concurrent workers cannot write past it. Only
`BrokenPipe` is suppressed; any other stderr error keeps today's panic.
After EPIPE all further diagnostics are dropped, the command runs to
completion, and the exit status is the command's own (including verdict
codes such as diff's).

**C2. Framing-panic cancellation guard.** An unwind guard in each framing
closure, armed before the framing work and disarmed once the real item is
sent, sends `PipelineItem { seq, data: Err(cancelled) }` for its own seq
before the permit is returned. It changes nothing about panic disposition.
Defence: with a swallowing panic handler, later items otherwise accumulate
behind the gap up to the `ReorderBuffer` window (65,536 slots) instead of
the nominal 64-permit bound, and the error only surfaces at `flush`. CLI
impact, which is why this is here and not in the main design: under the
default abort, the cancellation item can wake the writer thread and print
a writer diagnostic before rayon's abort, so panic-path stderr may differ.
The send itself can block transiently while the writer is inside
`write_chunk` (see section 11, item 3); the guard's contract must allow
that.

Nothing else is proposed for change.

## 10. Stopping rule and out of scope

- In scope: everything in section 3, BUG-066, BUG-067.
- Out of scope, named: the `uring_writer` shutdown bound in BUG-066's
  second paragraph (`drain_best_effort` rounds, `submit_and_wait(1)`); it is
  not a process-ownership question. HYG-019's message-hygiene cleanup
  (prefix and wording unification, dropping always-zero sub-counts) is a
  CLI output change and stays in HYG-019; this spec moves every message
  byte for byte. The `commands` crate split (ADR-0012) stays deferred.
- Fixed-size pools (`parallel_writer` 16, renumber 4/6) stay fixed; making
  them `Resources`-driven would change CLI thread counts.
- The teardown stops at the command entry points, the `PbfWriter` and
  reader constructors, `osc::parse::load_all_diffs` and `debug.rs`. No
  algorithm, budget constant or pipeline shape changes.

## 11. Lateral findings (outside or beside the scope)

1. The arena cap can be silently ineffective (section 2, consequence 2). In
   the CLI, on the paths read for this audit, every cap precedes the
   command's worker pools, and in `time_filter_snapshot` only the writer
   thread exists first, so the CLI should not be affected (I2's
   `pthread_create`-vs-`mallopt` ordering will confirm it); any library
   process that ran multithreaded work first is.
2. The CLI's `mallopt` overrides a user's
   `GLIBC_TUNABLES=glibc.malloc.arena_max`. Unchanged by this design; worth
   a note in `docs/`.
3. `src/write/pipeline.rs` `WRITE_AHEAD` rustdoc claims "no send can ever
   block on a full channel". False: permits return after `send`, not after
   the writer consumes, so the channel can hold 64 delivered-permit items
   while 64 new permit holders send. The writer thread receives
   unconditionally into its `ReorderBuffer`, so the block is transient (it
   lasts while the writer is in `write_chunk`) and there is no deadlock, but
   the stated liveness argument is wrong and should be rewritten.
4. Scratch collisions in section 3.5 are real correctness bugs for library
   callers today (concurrent `external_join` runs delete each other's
   scratch).
5. `diff/parallel.rs` puts scratch next to the **old input**, which may be
   read-only or on another filesystem.
6. `diff/derive.rs` `assemble_osc_from_paths` sizes gzip workers from the
   machine, ignoring the `jobs` the CLI resolved.
7. `emit_mallinfo2` calls `mallinfo2()` (which walks every arena under
   locks) and formats six counter names with `format!` even when no FIFO is
   configured, in `check_refs`, `extract` and `time_filter`. Gated in 5.6.
8. `src/read/blob.rs` eviction comment says DONTNEED keeps the file "from
   pulling the whole file into RSS". With `read(2)`-based I/O, page cache is
   not process RSS (it is charged to the cgroup). The comment overstates the
   mechanism. Medium confidence; not verified against a measurement.
9. `PbfWriter::drop` neither announces the expected total nor surfaces the
   join result, so an unflushed writer with a lost trailing framing task
   reads as clean inside the writer thread. Now a documented contract
   (5.8), not a silent one.
10. Raising `RLIMIT_NOFILE` above 1024 is inherited by any child the caller
    later forks, and breaks `select(2)` users in the caller (FD_SETSIZE);
    another reason it must be the caller's call.
11. `reference/testing.md` and `reference/performance.md` both describe the
    library setting the arena cap; both become stale at B1/B2 and are
    updated there.
12. `tiny_http` (through `hotpath`) already pulls `log` into the lockfile; a
    `log`-based design would have leaked its records into CLI stderr unless
    the logger filtered by target.
13. The test in `src/read/reader.rs` that swaps the panic hook is
    process-global within the `all` binary: for its window it silences
    every concurrent test's panic message. Test-only, worth a note.

## 12. What sparring changed

Four exchanges with a fresh codex session (read-only profile), consensus on
every point:

- **Kept**: in-place callbacks over grant flags or CLI pre-application.
- **Changed**: resources became a provider queried at the original point
  (a startup snapshot would move the `MemAvailable` read before the walk,
  and a single width cannot reproduce the 3 / 4 / 8 / 1 fallbacks).
- **Changed**: diagnostics moved from the `log` facade to a per-run handle;
  a global logger cannot isolate concurrent runs, and the `blob_wire` `Once`
  would let a silent run consume a later run's warning.
- **Changed**: summaries render line by line through `eprintln!`-equivalent
  calls (lock granularity and panic identity); apply-changes keeps its
  summary point.
- **Changed**: explicit `Executor`; host width does not govern ambient rayon.
- **Changed**: BUG-066 needed the `Drop` and `acquire_permit` paths too;
  `catch_unwind` dropped in favour of pool-owner panic disposition; the
  cancellation guard moved to section 9 after codex showed its send can
  block and its panic-path stderr can differ.
- **Added**: page-cache eviction is shared policy (default flip), scratch
  identity (`RunToken` per operation, not per allocation), the armed-flag
  ordering for markers, and the verification extensions in section 7.

## 13. Open questions for the user

- **Q1.** Section 9 C1 (closed stderr no longer kills the run): accept?
- **Q2.** Section 9 C2 (cancellation guard, panic-path-only CLI difference):
  accept?
- **Q3.** Flipping the public readers' eviction default to `Keep` is a
  silent behaviour change for existing library users (higher page-cache
  residency on big reads). The alternative is keeping `Evict` as a
  documented exception to the section 4 rule. Which?
- **Q4.** The spec contract wants a bench for anything risking a perf
  effect; this phase forbade benches. Is I2 trace identity accepted as the
  neutrality gate, or should B4 carry the optional interleaved denmark
  bench pair?
- **Q5.** Installing `strace` and `ltrace` on `bygg` would let I1 be a
  cross-check rather than the only instrument. Worth doing?
