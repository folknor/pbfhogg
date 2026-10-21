# Library use: implementation spec (bz pbfhogg#1)

Status: spec, not started. Nothing in this document has landed. Revised
after review rounds r1 and r2 (`notes/library-use-spec-r1.md`,
`notes/library-use-spec-r2.md`); every accepted finding is folded into the
section it concerns, and the rejected ones are recorded in section 11.

Written against `reference/technical-implementation-spec.md` (the contract).
Spawned from bz pbfhogg#1 (`bz show pbfhogg#1`); the design it builds is
`notes/library-use.md` and the user's rulings on it are
`notes/library-use-review.md` (signed off 2026-10-21: C1 accepted, C2
declined, public readers keep the page cache, germany pairs on top of trace
identity, strace/ltrace installed on bygg). Related items: `notes/bugs.md`
BUG-066 and BUG-067, `notes/hygiene.md` HYG-019 and HYG-029. Test bricks are
placed per `reference/testing.md`. Measurement record:
`reference/performance.md`, `reference/performance-history.md`,
`.brokkr/results.db`.

Baseline for every identity gate and every bench pair: commit `fa94c6c7`,
host `bygg`.

## 1. Verdict, and where this spec departs from the design

The ticket should be built. The design's core move holds up against the
code: the library keeps its path knowledge and, at the exact program point
where it mutates process state today, it calls a caller-supplied hook. That
is the only shape that preserves the CLI's call sequence by construction,
because the path choices (`time_filter` history vs snapshot from the input
header, altw `auto` from `MemAvailable` after an indexdata walk) cannot be
rebuilt from flags.

The survey for this spec found problems in the design that the spec has to
fix. None of them changes the approach.

1. **B0's baseline mechanism cannot work as written.** The design blesses
   goldens "with `BROKKR_TEST_BIN_DIR` pointing at the baseline
   (`fa94c6c7`) release build". brokkr always sets `BROKKR_TEST_BIN_DIR` to
   the sweep's own target dir (`brokkr/src/build_purpose/environment.rs`),
   so a caller cannot redirect it. Fix: B0 changes no product code. Goldens
   blessed against B0's own build are therefore goldens of the baseline
   product (section 6, B0).
2. **The goldens would be host- and checkout-dependent.** Thread counts
   (pools sized from `available_parallelism`), `getrlimit` values, the altw
   external worker narration and the altw `auto` budget line all depend on
   the host and the shell. So do the child's inherited environment
   (`RAYON_NUM_THREADS`, `BROKKR_MARKER_FIFO`, `GLIBC_TUNABLES`,
   `RUST_MIN_STACK`, a pre-existing `LD_PRELOAD`, all of which change the
   trace), a cgroup CPU quota below the pinned mask, and the checkout path,
   which reaches argv and stderr through the input paths. Fix: the harness
   pins each child to 4 CPUs and to explicit `RLIMIT_NOFILE` values before
   exec, starts it from a cleared environment, checks the two host
   preconditions, copies the inputs into each scenario's own directory, and
   masks the one `MemAvailable` figure (section 5, I2).
3. **Generating fixtures at compare time would tie the goldens to the
   writer.** Fixtures written by the library's own `PbfWriter` at compare
   time would change bytes, and with them every `posix_fadvise` offset,
   whenever a brick touches the writer. Fix: the bless test writes the
   fixtures once and commits them, and the compare test only reads them.
4. **ltrace cannot trace pbfhogg.** The binaries are linked
   `BIND_NOW`/`NOW PIE` with no PLT relocations (`readelf -d
   target/release/pbfhogg`), so ltrace's PLT breakpoints see nothing. The
   LD_PRELOAD shim does work, because the dynamic linker resolves the GOT
   with interposition. The Q5 cross-check therefore uses strace, which
   covers the syscall-backed calls only (`setrlimit`/`getrlimit` via
   `prlimit64`, `posix_fadvise` via `fadvise64`, `pthread_create` via
   `clone3`). `mallopt` is not a syscall; its coverage comes from positive
   controls in the compare test.
5. **`brokkr test NAME` matches by substring.** `brokkr/src/test_cmd/plan.rs`
   matches the filter as a substring and only adds `--exact` afterwards, on
   the resolved full name. The design's names, `cli_host_identity` and
   `cli_host_identity_bless`, would select each other. Fix: name the tests
   so that none is a substring of another (section 5, I2).
6. **The summary adapter would print an extra newline.** `writeln!` hands
   `write_fmt` an `fmt::Arguments` that already ends in a newline, so an
   adapter that overrides `write_fmt` and forwards it to an
   `eprintln!("{}", args)` sink prints two. Fix: the adapter buffers bytes
   and emits one event per completed line, without the terminator
   (section 4.4).
7. **`&'a dyn Resources` cannot reach the pipeline thread.** The pipeline
   probe runs on the `'static` thread that
   `ElementReader::into_blocks_pipelined` spawns, and a borrowed handle
   cannot live there. Fix: `Host` holds `Arc` handles and is an owned,
   cloneable value (section 4.1).
8. **apply-changes prints its summary twice under the CLI.** The design
   (3.3) says the CLI prints every summary except apply-changes'. In fact
   `apply_changes::merge` prints it at the end of `rewrite.rs` `merge` AND
   `cli/src/main.rs` `run_apply_changes` prints it again. This is a live
   user-visible bug. The spec keeps it byte for byte, because the CLI must
   not change; it is filed separately (section 10).
9. **`osc/merge_join.rs` has no `rayon::join`.** The design's M12 lists one
   there; it exists only in a stale doc comment. Not a site.
10. **The existing glibc textlint blocks `process.rs`.** Rule
    `glibc-allocator-calls-live-in-debug` forbids `libc::mallopt` outside
    `src/debug.rs`, so the design's `src/process.rs` would fail it. Fix: the
    rule's `exclude` gains `src/process.rs`, which carries the same
    `target_env = "gnu"` gating.
11. **`debug::set_mmap_threshold` and `debug::malloc_trim` are public
    process mutations.** Neither has a caller anywhere in the workspace, but
    each is a glibc allocator operation a caller can reach (`mallopt` and
    `malloc_trim(0)`). Both move to `process`, the caller-invoked toolbox
    the rule leaves such operations in, so `debug` keeps only sidecar
    instrumentation.
12. **Enforcement lands with each property, not at the end.** The design
    puts every textlint rule in its last brick. Here each rule lands in the
    brick that establishes the property it guards, so nothing can regress
    between bricks.
13. **The decision record lands where the rule starts binding.** The ADR
    lands in B1, and each brick carries its own CHANGELOG entry. Under the
    project rule, markdown is never committed alone, so there is no
    docs-only final brick.
14. **merge_changes is the one whole-body executor install.** The design
    requires `install` to wrap only the parallel computation. merge_changes
    already runs its whole body inside a pool when `-j` is set. The spec
    keeps that exactly, and for the no-`-j` case wraps the same body in
    `host.executor.install`, because the body never drives a `PbfWriter`
    (section 6, B4).
15. **More process-global values: altw `inject_metrics`,
    `WRITER_METRICS`, `PIPELINE_METRICS`, `process_scratch_tag`.** The altw
    inject counters (`altw/mod.rs`), the writer and reader-pipeline metric
    statics (`write/metrics.rs`, emitted by the public
    `write::emit_writer_counters`; `read/pipeline_metrics.rs`) mix across
    concurrent runs. They are diagnostic-only, the design's M17 class, and
    stay as they are. `diff/derive_parallel.rs` `process_scratch_tag` is a
    process-global `OnceLock` tag; after B5 the per-run token separates
    in-process runs, so sharing the tag between them is harmless, and it
    stays.
16. **The trace instrument compares causal structure, not schedules.**
    Thread start and end records are racy (rayon pool drop does not join
    its workers; a freshly created member may never be scheduled before a
    short run exits), and the rayon global pool is created lazily by
    whichever thread first touches it. Comparing exact per-member histories
    would therefore flake. The canonical form (section 5, I2) drops `S`/`E`
    from comparison, compares each pool as a flattened multiset, and lifts
    the global pool into its own unit.
17. **Output identity is semantic where framing can vary.** apply-changes'
    blob sizes vary with worker scheduling on multi-blob inputs
    (`cli_apply_changes_invariants.rs` `stats_block`), so a raw hash of its
    output cannot be stable. PBF outputs are compared by a decoded-content
    digest, and by raw bytes only where framing is deterministic (section 5,
    I2).
18. **B1 removes the in-process suite's `RLIMIT_NOFILE` raise.** Today the
    shared `all` binary inherits the raise from whichever test first runs
    the external backend. Under `NoProcessActions` the soft limit stays at
    the shell's (1024 by default), stage 1 still budgets from the soft limit
    alone (3 workers x 256 shard files), and two concurrent external runs in
    one process exceed it. Fix: `common::test_host()` raises the soft limit
    to the hard cap once per test process through
    `process::raise_nofile_to_hard_cap`, the caller-performs-it shape the
    rule prescribes, and the one test that needs an unraised limit gets a
    test target of its own (section 5, I3).
19. **"Off-worker, which is every CLI path" is a surveyed fact, not an
    assumption.** Section 3.4 records why no CLI path drives a `PbfWriter`
    from a rayon worker, and B4 adds a counter on the cooperative branch so
    a future path that does shows up in `brokkr sidecar --stalls`.

## 2. The rule being implemented (from the design, narrowed to what the bricks build)

- **Process state and shared-host policy** - the arena cap, `RLIMIT_NOFILE`,
  page-cache eviction of the caller's input files, the rayon global pool,
  framing-panic disposition, the once-per-process warning, env reads,
  stderr: the library never does any of it by default. Either the caller
  performs it through a hook invoked at the library's chosen point, or the
  caller selects it explicitly. Eviction of the library's own scratch files
  (`altw/external/radix.rs`, `altw/external/stage2.rs`) is scratch
  management, not host policy, and stays library behaviour.
- **Per-call resources** - every value the library derives from the host
  is supplied by the caller: thread counts derived from parallelism, memory
  budgets derived from available memory, and the scratch parent wherever
  scratch is placed beside an input or output (altw, diff, derive). The
  command layer takes them only through `Host`, which has no `Default`.
  Fixed values that are not derived from the host - the pool sizes
  `parallel_writer::POOL_SIZE` and renumber's 4 and 6, and byte budgets
  such as `PAR_INFLIGHT_BUDGET`, `PIPELINE_DISPATCH_PERMITS` and
  `AUTO_ANON_HEADROOM_BYTES` - stay constants (section 8). ADR-0016 quotes
  this wording.
- **The CLI's behaviour is identical**: the same allocator calls on the same
  paths, the same rlimits, thread counts and budgets, the same stdout and
  stderr bytes, the same exit codes. The one exception is C1 (B6), which the
  user accepted.

## 3. Survey (complete inventory the bricks consume)

`fa94c6c7`, verified by reading the code for this spec. File paths are from
the repo root; line numbers are deliberately omitted.

### 3.1 Process mutations (hook sites)

| id | site | today | hook call that replaces it, in place |
|---|---|---|---|
| M1 | `src/commands/cat/mod.rs` `cat_filtered`, first statement | `crate::debug::cap_malloc_arenas()` | `host.process.cross_thread_alloc_pool()` |
| M2 | `src/commands/check/refs.rs` `check_refs`, after `CHECKREFS_SCAN_START` | same | same |
| M3 | `src/commands/check/verify_ids.rs` `verify_ids_streaming_parallel` | same | same |
| M4 | same file, `verify_ids_full_parallel` | same | same |
| M5 | `src/commands/degrade/mod.rs` `degrade_decode_path`, after `require_indexdata` | same | same |
| M6 | `src/commands/renumber/mod.rs` `renumber_external`, first statement | same | same |
| M7 | `src/commands/repack/mod.rs` `repack`, after `require_indexdata` | same | same |
| M8 | `src/commands/time_filter/mod.rs` `time_filter_snapshot`, first statement | same | same |
| M9 | `src/geocode_index/builder/mod.rs` `build_geocode_index`, after `config.validate()` | same | same |
| M10 | `src/commands/altw/external/stage1.rs` `stage1_way_pass` | `super::raise_nofile_to_hard_cap()` | `host.process.fd_allowance(wanted)`, `wanted = cpu_cap as u64 * NUM_BUCKETS as u64 + HEADROOM_FDS` |

Host reads that are deliberately not hook sites:

- `altw/sparse.rs` probes free disk with `statvfs` before preallocating the
  sparse index. It guards a preallocation and sizes nothing, so it is not a
  `Resources` call, and the `no-host-probe` textlint (B1) does not match it.
- `debug::read_page_faults` reads `/proc/self/stat` six times per external
  join (`altw/external/mod.rs`), only to feed sidecar counters. B1 gates it
  on the marker arming flag with `write_fifo` and `emit_mallinfo2` (4.7).

### 3.2 Parallelism and memory probes (every one becomes a `Resources` call at the same site)

The formula columns are `(reserve, min, fallback)` for
`n.saturating_sub(reserve).max(min)`, with the fallback taken when the probe
fails; the cap is applied afterwards.

| site | (reserve, min, fallback) | cap |
|---|---|---|
| `src/read/reader.rs` `par_map_reduce` default | (1, 1, 3) | - |
| `src/read/pipeline.rs` `run_pipeline` default | (2, 1, 4) | - |
| `src/read/pipeline.rs` `run_pipeline_fused` default | (2, 1, 4) | - |
| `src/scan/classify.rs` `resolve_thread_count` | (2, 1, 4) | - |
| `src/commands/tags_filter/mod.rs` two-pass decode | (2, 1, 4) | - |
| `src/commands/apply_changes/rewrite.rs` worker count | (2, 2, 4) | - |
| `src/commands/merge_changes/mod.rs` worker count | (0, 1, 8) | - |
| `src/commands/diff/derive.rs` gzip workers | (0, 1, 4) | - |
| `src/commands/altw/passthrough.rs` decode | (2, 1, 4) | - |
| `src/commands/altw/external/mod.rs` workers | (2, 1, 4) | `.min(4)` |
| `src/commands/altw/external/stage1.rs` `cpu_cap` | (2, 1, 4) | - |
| `src/commands/altw/external/stage2.rs` workers | (2, 1, 4) | `.min(6)` |
| `src/commands/altw/external/stage3.rs` workers | (2, 1, 4) | `.min(4)` |
| `src/commands/altw/external/stage4.rs` decode | (2, 1, 4) | - |
| `src/commands/extract/simple.rs` (both) | (2, 1, 4) | - |
| `src/commands/extract/multi.rs` (both) | (2, 1, 4) | - |
| `src/commands/extract/common.rs` `pread_execute` | (2, 1, 4) | - |
| `src/commands/renumber/relations.rs` | (2, 1, 4) | `.min(6)` |
| `src/geocode_index/builder/pass1_5.rs` | (2, 1, 4) | - |

`(0, 1, x)` is exactly today's `.map(NonZeroUsize::get).unwrap_or(x)`,
because `n >= 1`. The CLI's own `-j 0` resolution in `run_diff` and
`run_derive_changes` stays in the CLI, unchanged.

Memory: `src/commands/altw/mod.rs` `page_cache_budget_bytes`, called in
`auto_select_index_type` after the estimate walk.

### 3.3 Readers (page-cache policy, diagnostics)

The `posix_fadvise(DONTNEED)` eviction of input files behind the read head
(H1) exists only in `BlobReader::next`, for readers built by `from_path` and
`open` (buffered `FileReader`); `ElementReader::from_path`, `open` and
`from_path_with_eviction` forward to them. The other `posix_fadvise` calls
stay as they are: `DONTNEED` on altw external's own scratch files
(`radix.rs`, `stage2.rs`) is scratch management (section 2), and the
`RANDOM` (`HeaderWalker::open`), `SEQUENTIAL` (`FileReader`) and `WILLNEED`
(`scan/classify.rs`, issued from the dispatcher thread) hints are per-fd
access hints that drop nobody's pages. All of them appear as trace `F`
records, so the identity gate pins them too. These production sites construct
public readers today and switch to crate-internal constructors:

- `ElementReader::open`: `commands/time_filter`, `commands/check/verify_ids`
  (2), `commands/altw/mod.rs` (3), `commands/altw/external/mod.rs`,
  `commands/getparents`, `commands/getid`, `commands/repack`,
  `commands/degrade` (2), `commands/tags_filter`,
  `geocode_index/builder/mod.rs`, `geocode_index/builder/pass1.rs`.
- `ElementReader::from_path`: `commands/export`.
- `BlobReader::open`: `osc/merge_join.rs`, `commands/extract/simple.rs` (4),
  `commands/extract/complete.rs`, `commands/extract/multi.rs`,
  `commands/extract/smart.rs` (3), `commands/inspect/show_element.rs`,
  `commands/renumber/mod.rs` (2), `commands/altw/mod.rs`,
  `commands/altw/external/stage4.rs`, `commands/tags_filter` (2),
  `commands/diff/mod.rs` (3), `commands/diff/derive.rs` (2).
- Inline tests using `ElementReader::from_path`: `commands/getid`,
  `commands/getparents`, `commands/tags_filter`.

The unrecognised-indexdata warning (M14) is raised inside
`WireBlobHeader::parse` (`src/read/blob_wire.rs`). The paths that reach it
and therefore own the emission are:

- `BlobReader::read_blob_header`;
- `parse_blob_header_with_index`, the funnel for `HeaderWalker::next_header`
  and `read::frame::read_frame_header` (hence `read_raw_frame`,
  `read_blob_header_only` and the `cat --dedupe` frame loop).

The write-side callers in `write/framing.rs` parse with indexdata off and
can never warn.

### 3.4 rayon sites that use the ambient pool (M11, M12)

- `src/write/writer.rs`: three `rayon::spawn` sites, in
  `write_primitive_block`, `write_primitive_block_no_indexdata` and
  `write_primitive_block_owned`.
- par-iterators: `commands/repack/mod.rs` `frame_and_write_batch`,
  `commands/sort/mod.rs` overlap rewrite, `commands/degrade/mod.rs`,
  `commands/extract/simple.rs`, `geocode_index/builder/admin.rs`,
  `geocode_index/builder/interp.rs` (3), `geocode_index/builder/pass3.rs`
  (5), and `commands/merge_changes/mod.rs` (3, inside `do_work`).
- `src/read/pipeline.rs` builds its own decode pools. Those are per-call
  resources sized by `Resources`, not the ambient pool.

Where a `PbfWriter` is driven (the premise of B4's "off-worker, which is
every CLI path"). The only code in `src/` that runs on a rayon worker is:
the pipeline decode pools' tasks and `run_pipeline_fused`'s `transform`
(bounded `Fn + Sync`); par-iterator closures (`Fn + Sync`); the writer's own
framing spawns; and merge_changes' `pool.install(do_work)`, which writes OSC
XML and never constructs a `PbfWriter`. Every `PbfWriter` write method takes
`&mut self`, and no `Mutex<PbfWriter>` exists in `src/`, so none of the
`Fn + Sync` contexts can reach one. Every other worker in `src/` is a
`std::thread` (`thread::scope` spawns in extract, renumber, tags_filter,
apply_changes, altw, classify, the pipeline stages), which
`rayon::current_thread_index()` reports as off-worker. Therefore every CLI
writer runs off-worker, and B4 leaves every CLI `flush` and `Drop` on
today's blocking path.

### 3.5 Scratch names keyed on process identity

| site | name |
|---|---|
| `commands/altw/external/radix.rs` `ScratchDir::new` | `<parent>/.pbfhogg-<name>-<pid>` |
| `commands/altw/sparse.rs` `open_anonymous_sparse_file` | `<scratch>/.pbfhogg-sparse-index-<pid>` |
| `commands/diff/derive.rs` `ChangeSink::new` | `derive-{creates,modifies,deletes}-<pid>.xml.tmp` |
| `commands/diff/derive_parallel.rs` outer files | `derive-par-{creates,modifies,deletes}-<pid>.xml.tmp` |
| `commands/diff/derive_parallel.rs` `shard_xml_paths` | `derive-par-<kind>-<pid>-<tag>-<n|w|r>-<shard>.xml.tmp` |
| `commands/diff/parallel.rs` `shard_text_path` | `diff-par-<pid>-<n|w|r>-<shard>.txt.tmp` |

The scratch parent today is `output.parent().unwrap_or(".")` for altw (both
backends) and derive, and `old_path.parent().unwrap_or(".")` for
`diff/parallel.rs`. `path_guard.rs` `OutputFile` already adds a
per-process counter and is not in scope.

### 3.6 Library stderr and the event keys they become

Every `eprintln!`/`eprint!` site in `src/` becomes exactly one event with
the identical format string. The table is the inventory; no site count is
kept, because it drifts.
Summaries become `write_summary` methods; their key is supplied by the
caller of `summary_writer`. The keys:

| file | events (kind: key) |
|---|---|
| `commands/mod.rs` `warn_locations_on_ways_loss` | Warning: `header.way_metadata_loss` |
| `commands/cat/dedupe.rs` | Progress: `cat.dedupe.pass1`, `cat.dedupe.indexed`, `cat.dedupe.overlap_runs`, `cat.dedupe.pass2` |
| `commands/sort/mod.rs` | Progress: `sort.pass1`, `sort.indexed`, `sort.overlap_runs`, `sort.intra_unsorted`, `sort.pass2`; Note: `sort.verify_unclaimed_order` |
| `commands/apply_changes/rewrite.rs` | Progress: `apply_changes.parse_osc`, `apply_changes.diff_counts`, `apply_changes.diff_ranges`, `apply_changes.low_needed`; Summary (internal): `apply_changes.summary` |
| `osc/parse.rs` `load_all_diffs` | Progress: `osc.load.parsing`, `osc.load.loaded` |
| `geocode_index/builder/mod.rs` | Progress: `geocode.pass1`, `geocode.pass1.admin_relations`, `geocode.pass1_5`, `geocode.pass2`, `geocode.pass2.admin_polygons`, `geocode.pass2.admin_dropped`, `geocode.pass2.interp_resolved`, `geocode.pass3`, `geocode.pass3.cells`, `geocode.smoke_test`, `geocode.done`; Warning: `geocode.smoke_test.no_match`; Developer (`cfg(feature = "hotpath")`): `geocode.hotpath.pass1_relations_ms`, `.pass2_nodes_ways_ms`, `.pass3_cells_ms`, `.addr_points`, `.street_ways`, `.interp_ways`, `.admin_polygons`, `.strings_bytes`, `.strings_unique` |
| `geocode_index/builder/pass2.rs` | Progress: `geocode.pass2.referenced_nodes`, `geocode.pass2.counts` |
| `commands/extract/mod.rs` | Progress: `extract.multi.slot` |
| `commands/extract/multi.rs` | Progress: `extract.multi.single_pass`, `extract.multi.slot_counts`, `extract.multi.node_blobs` |
| `commands/getid/mod.rs` | Note: `getid.blobs_skipped` (both sites), `getid.invert.passthrough` |
| `commands/tags_filter/mod.rs` | Note: `tags_filter.blobs_skipped_by_tag`, `tags_filter.pass2_blobs_skipped` |
| `commands/tags_count/mod.rs` | Note: `tags_count.blobs_skipped_by_tag` |
| `commands/inspect/scan.rs` | Note: `inspect.direct_io_ignored` |
| `commands/check/verify_ids.rs` | Warning: `check.ids.unindexed_triple_scan` |
| `commands/repack/mod.rs` | Warning: `repack.way_metadata_loss`, `repack.cap_never_fired` |
| `commands/altw/mod.rs` | Note: `altw.external_hint`, `altw.auto_select` (all three sites) |
| `commands/altw/sparse.rs` | Note: `altw.sparse.stale_temp_removed` |
| `commands/altw/external/mod.rs` | Developer: `altw.external.coord_payloads` |
| `commands/altw/external/stage1.rs` | Developer: `altw.external.nofile`, `altw.external.workers` (both variants) |
| `read/blob_wire.rs` | Warning, once per `OnceKeys`: `indexdata.unrecognised_length` |
| `write/parallel_writer.rs` | Developer: `writer.parallel.thread_exit` |
| `write/uring_writer.rs` | Developer: `writer.uring.error` |
| `read/direct_reader.rs` (test-only skip line) | removed; the test returns without printing |

Summary methods (`print_summary` becomes `write_summary`; the key is the
one the CLI passes to `summary_writer`): `CatStats` `cat.summary`,
`MergePbfStats` (`cat/dedupe.rs`) `cat.dedupe.summary`, `SortStats`
`sort.summary`, apply-changes `MergeStats` `apply_changes.summary`,
`RepackStats` `repack.summary`, `DegradeStats` `degrade.summary`,
`RenumberStats` `renumber.summary`, `TagsFilterStats`
`tags_filter.summary`, `TagsFilterOscStats` `tags_filter.osc.summary`,
`ExportStats` `export.summary`, `DiffStats` `diff.summary` (and
`print_osmium_summary` becomes `write_osmium_summary`, key
`diff.osmium_summary`), `DeriveChangesStats` `derive.summary`, `GetidStats`
`getid.summary`, `GetparentsStats` `getparents.summary`, `ExtractStats`
`extract.summary`, `MergeChangesStats` `merge_changes.summary`, altw `Stats`
`altw.summary`, `TimeFilterStats` `time_filter.summary`.

### 3.7 Library entry points the CLI calls

All of these gain `host: &Host` as the last parameter:

- `cat::cat`, `cat::dedupe::merge_pbf`, `sort::sort`, `repack::repack`,
  `degrade::degrade`, `renumber::renumber_external`;
- `tags_filter::tags_filter`, `tags_filter::osc::tags_filter_osc`,
  `export::export`, `tags_count::tags_count`;
- `diff::diff`, `diff::derive::derive_changes`;
- `getid::getid`, `getid::removeid`, `getid::parse_ids_from_pbf`,
  `getparents::getparents`, `getparents::getparents_with_min_blobs`;
- `extract::extract`, `extract::extract_multi`;
- `altw::add_locations_to_ways`, `altw::external::external_join`,
  `time_filter::time_filter`;
- `inspect::inspect`, `inspect::show_element_to`,
  `inspect::node_stats::node_stats`;
- `apply_changes::merge`, `merge_changes::merge_changes`;
- `check::verify_ids::verify_ids`, `check::index::check_index`,
  `check::refs::check_refs`;
- `geocode_index::builder::build_geocode_index(config, host)`.

`has_indexdata(path, direct_io)` and `has_tagdata` gain `diagnostics:
&Diagnostics`, and `osc::parse::load_all_diffs(dir)` gains `diagnostics:
&Diagnostics`, both in B2. Every caller outside `src/` passes a host:
`cli/src/main.rs`; `tests/*.rs`, through `common::test_host()`; and the
`fault_*` targets.

### 3.8 Standing decisions touched

- **ADR-0012** (the command layer stays `doc(hidden)`): honoured.
  `pbfhogg::host` and `pbfhogg::process` are documented public modules,
  because the documented `PbfWriter` constructors need `Executor`.
- **ADR-0011**: `host` and `process` compile in every feature set.
- **ADR-0014**: each output backend keeps its own thread; the diagnostics
  handle is passed to each backend.
- **ADR-0013**: unaffected.
- `CORRECTNESS.md` and `DEVIATIONS.md`: checked. Both mention stderr
  output (a pass-1 notice, a summary line), and the spec keeps every such
  line byte for byte, so neither is touched. No accepted invariant or
  documented osmium deviation concerns process state.
- `reference/testing.md` "What the shared binary shares" and
  `reference/performance.md` (renumber "mallopt inside
  `renumber_external()`") become wrong in B1 and are reworded there.
  `reference/osmium-parity.md` cites `MergeStats::print_summary`, which B2
  renames, and is reworded there. The intra-doc link to
  `crate::debug::cap_malloc_arenas` in `src/commands/check/refs.rs` breaks
  in B1 and is rewritten there.
- New ADR-0016 (B1) records the rule.

## 4. Target artifacts

### 4.1 `src/host.rs` (new; `pub mod host` in `lib.rs`; every feature set)

```rust
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct Host {
    pub process: Arc<dyn ProcessActions>,
    pub resources: Arc<dyn Resources>,
    pub diagnostics: Diagnostics,         // added in B2
    pub page_cache: PageCachePolicy,      // added in B3
    pub executor: Executor,               // added in B4
    pub scratch_parent: Option<PathBuf>,  // added in B5
}
// No Default impl. #[cfg(test)] pub(crate) fn for_tests() -> Host builds
// NoProcessActions, MachineProbe, Diagnostics::discard(), Keep, Ambient, None.

/// Invocation count and position are part of the contract: never
/// deduplicated, never hoisted.
pub trait ProcessActions: Send + Sync {
    /// A worker pool is about to start whose buffers are allocated on one
    /// thread and freed on another.
    fn cross_thread_alloc_pool(&self);
    /// The run wants `wanted` descriptors open at once.
    fn fd_allowance(&self, wanted: u64) -> FdAllowance;
}
pub struct NoProcessActions; // fd_allowance -> crate::process::current_nofile()

pub trait Resources: Send + Sync {
    fn parallelism(&self) -> Option<NonZeroUsize>;
    fn memory_available(&self) -> Option<u64>;
}
pub struct MachineProbe; // std::thread::available_parallelism().ok();
                         // /proc/meminfo MemAvailable kB.saturating_mul(1024)
pub struct Fixed { pub parallelism: Option<NonZeroUsize>, pub memory_available: Option<u64> }

/// The one formula every site in 3.2 uses.
pub(crate) fn sized_workers(r: &dyn Resources, reserve: usize, min: usize, fallback: usize) -> usize {
    r.parallelism().map(|n| n.get().saturating_sub(reserve).max(min)).unwrap_or(fallback)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageCachePolicy { Evict, Keep }

#[derive(Clone, Debug)]
pub enum Executor { Ambient, Pool(Arc<rayon::ThreadPool>) }
impl Executor {
    pub(crate) fn install<R: Send>(&self, op: impl FnOnce() -> R + Send) -> R; // Ambient: op()
    pub(crate) fn spawn(&self, op: impl FnOnce() + Send + 'static);           // rayon::spawn / pool.spawn
    pub(crate) fn on_worker(&self) -> bool; // rayon::current_thread_index() / pool.current_thread_index()
    pub(crate) fn yield_now(&self) -> Option<rayon::Yield>; // rayon::yield_now() / pool.yield_now()
}
```

`Executor`'s rustdoc states three things:

- Thread-local framing and decompress scratch persists on whichever pool
  runs it, and dropping a `Pool` is how a caller frees it.
- Framing-panic disposition is the pool's `panic_handler`; with no handler,
  rayon aborts the process.
- A `PbfWriter` moved between pools mid-stream under `Ambient` is not
  covered by the cooperative wait.

The `ElementReader` handle: a crate-internal newtype
`#[derive(Clone)] pub(crate) struct ResourcesHandle(Arc<dyn Resources>)`
(the clone is an `Arc` clone), with a manual `Debug` that prints
`ResourcesHandle`, so `ElementReader` keeps its derived `Clone` and
`Debug`.

`Host`'s fields are public and the struct is not `#[non_exhaustive]`, so a
caller builds one with a struct literal and a test changes one field.
Every field a later version adds breaks such literals; pre-1.0 that is
accepted, and ADR-0016 says so in one sentence.

### 4.2 `src/process.rs` (new; `pub mod process`; glibc calls under `cfg(all(target_os = "linux", target_env = "gnu"))` with no-op fallbacks)

```rust
pub fn cap_malloc_arenas();                 // moved from debug.rs; rustdoc carries notes/library-use.md section 2's three consequences
pub fn set_mmap_threshold(bytes: i32) -> i32; // moved from debug.rs
pub fn malloc_trim() -> i32;                // moved from debug.rs
pub fn raise_nofile_to_hard_cap() -> FdAllowance; // today's body, returning FdAllowance
pub fn current_nofile() -> FdAllowance;     // getrlimit only: {soft.min(1 << 30), None}; on failure {1024, Some(ProbeFailed)}

pub struct FdAllowance { pub descriptors: u64, pub adjustment: Option<FdAdjustment> }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdAdjustment { ProbeFailed, AlreadyAtCap, Raised { from: u64 }, RaiseFailed }
```

`impl Display for FdAllowance`. The four `Some` arms render today's
`FdBudget` strings byte for byte:

- `RLIMIT_NOFILE: probe failed; assuming {d}`
- `RLIMIT_NOFILE: {d} (already at hard cap; no adjustment)`
- `RLIMIT_NOFILE: {from} -> {d} (soft raised to hard cap)`
- `RLIMIT_NOFILE: {d} (raise to hard cap rejected; still at soft cap)`

`None` renders `RLIMIT_NOFILE: {d} (not adjusted)`. `FdBudget` and
`FdRaiseStatus` in `altw/external/mod.rs` are deleted.

### 4.3 `src/scratch.rs` (new, crate-internal, B5)

```rust
pub(crate) struct RunToken(u64);
static NEXT: AtomicU64 = AtomicU64::new(0);
impl RunToken {
    pub(crate) fn next() -> crate::BoxResult<Self>; // fetch_add(1); n == u64::MAX -> Err("scratch run token space exhausted")
    pub(crate) fn suffix(&self) -> String;          // "" for 0, "-<n>" otherwise
}
```

### 4.4 Diagnostics (in `src/host.rs`, B2)

```rust
#[derive(Clone)]
pub struct Diagnostics { sink: Arc<dyn DiagnosticSink>, once: Arc<OnceKeys> }
impl Diagnostics {
    pub fn new(sink: Arc<dyn DiagnosticSink>) -> Self;            // fresh OnceKeys
    pub fn with_once(sink: Arc<dyn DiagnosticSink>, once: Arc<OnceKeys>) -> Self;
    pub fn discard() -> Self;
    pub fn summary_writer(&self, key: &'static str) -> SummaryWriter<'_>;
    pub(crate) fn emit(&self, kind: EventKind, key: &'static str, args: std::fmt::Arguments<'_>);
    pub(crate) fn emit_once(&self, kind: EventKind, key: &'static str, args: std::fmt::Arguments<'_>);
}
pub trait DiagnosticSink: Send + Sync { fn event(&self, event: &Event<'_>); }
pub struct Event<'a> { pub kind: EventKind, pub key: &'static str, pub args: std::fmt::Arguments<'a> }
#[non_exhaustive] #[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind { Progress, Warning, Note, Summary, Developer }
#[derive(Default)] pub struct OnceKeys { /* Mutex<HashSet<&'static str>>; poison -> into_inner */ }
pub struct SummaryWriter<'a> { /* &Diagnostics, key, Vec<u8> line buffer */ }
```

Event semantics: `args` never contains the line terminator, and a sink
renders one event as one line.

`SummaryWriter` implements `io::Write`:

- `write` appends to the line buffer and, for every `\n` it finds, emits one
  `Summary` event whose `args` is `format_args!("{}", line)`, with the line
  decoded by `String::from_utf8_lossy` and the terminator dropped. It
  always returns `Ok(buf.len())`.
- `flush` is a no-op.
- `Drop` emits a non-empty partial line as a final event.

`write_summary` implementations use only `writeln!`, one per former
`eprintln!`, so each line is one event, which the CLI prints with one
`eprintln!`. Bytes and lock granularity are identical to today.

`Debug` for `Diagnostics` is manual, printing `Diagnostics`, because
`BlobReader` derives `Debug`.

Crate macros in `host.rs`, exported as `pub(crate) use`:

- `diag!($d:expr, $Kind:ident, $key:literal, $($fmt:tt)+)` expands to
  `$d.emit(EventKind::$Kind, $key, format_args!($($fmt)+))`;
- `diag_once!` is the same, calling `emit_once`.

### 4.5 Readers (B1 adds the constructors; B2 and B3 extend them)

- `BlobReader<FileReader>` gains
  `pub(crate) fn open_in(path: &Path, direct: bool, host: &Host) -> Result<Self>`.
  - B1: eviction exactly as `open` (on for buffered).
  - B2: stores `host.diagnostics.clone()`.
  - B3: eviction from `host.page_cache`.
- `ElementReader<FileReader>` gains `pub(crate) fn open_in(path, direct, host)`.
  It sets the eviction policy before reading the header (as
  `from_path_with_eviction` does) and stores
  `Some(ResourcesHandle(host.resources.clone()))` in a new field
  `resources: Option<ResourcesHandle>`.
  - `None`, set by every public constructor, means `MachineProbe`.
  - `run_pipeline` and `run_pipeline_fused` take
    `parallelism: Option<ResourcesHandle>`;
    `into_blocks_pipelined` moves it into its thread;
    `par_map_reduce` resolves through it.
  - The probe still runs at the original program point: inside the stage-2
    dispatcher thread, and inside `par_map_reduce`.
- `BlobReader` gains a `diagnostics: Diagnostics` field (B2).
  - `new`, `new_seekable`, `from_path`, `from_path_direct` and `open` set
    `Diagnostics::discard()`.
  - A public builder `pub fn diagnostics(mut self, d: Diagnostics) -> Self`
    sets one; `ElementReader` gets the same builder, forwarding to its
    `BlobReader`.
- `WireBlobHeader` gains `unrecognised_indexdata_len: Option<usize>` (B2).
  `parse` sets it instead of calling `warn_unrecognised_indexdata`, which is
  deleted along with its `Once`.
  - `BlobReader::read_blob_header` emits through its own handle,
    `diag_once!(self.diagnostics, Warning, "indexdata.unrecognised_length", <today's text, {len}>)`.
  - `parse_blob_header_with_index` gains `diagnostics: &Diagnostics` and
    emits the same way. Its callers pass one down:
    - `HeaderWalker::open(path, diagnostics: &Diagnostics)` stores a clone;
    - `read::frame::read_frame_header(reader, buf, diagnostics)`;
    - `read::raw_frame::read_raw_frame(reader, offset, diagnostics)`;
    - `read::raw_frame::read_blob_header_only(reader, offset, diagnostics)`.
- B3 changes the public default:
  - `BlobReader::from_path` and `open`, and `ElementReader::from_path` and
    `open`, no longer evict. `evict_page_cache(true)` opts in, and
    `from_path_with_eviction` stays.
  - `BlobReader::evict_page_cache`'s rustdoc rewrites "the default is to
    evict" as "the default is to keep".
  - The comment in `BlobReader::next` "pulling the whole file into RSS"
    becomes "into the page cache". Page cache filled by `read(2)` is not
    process RSS; it is charged to the cgroup.

### 4.6 `PbfWriter` (B4)

- `to_path`, `to_path_direct`, `to_path_uring` and `to_path_parallel` take a
  trailing `executor: Executor`. `PbfWriter::new` is unchanged.
- Crate-internal twins `to_path_in`, `to_path_direct_in`, `to_path_uring_in`
  and `to_path_parallel_in` take
  `(path, compression, header, executor, diagnostics: Diagnostics)`.
  The public constructors call them with `Diagnostics::discard()`.
  `parallel_writer_thread` and `uring_writer_thread` take the
  `Diagnostics`; their `eprintln!` became `diag!` events in B2, and B4
  routes the handle to them.
- `WritePipeline` gains `executor: Executor`. Under `cfg(feature =
  "test-hooks")` it also gains `framing_panic_at: Option<usize>`, which
  makes the framing closure for that seq panic before framing.
  `WritePipeline` is `pub(super)`, so the hook an integration test calls
  lives on `PbfWriter`:
  `#[cfg(feature = "test-hooks")] pub fn test_panic_in_framing_at(&mut self, seq: usize)`,
  which sets the field on its pipeline (and is a no-op on a sync
  `PbfWriter::new`, which has none). It is per-instance, so the test that
  uses it may live in the shared `all` binary.
- The three `rayon::spawn` sites become `pipeline.executor.spawn`.
- `write/pipeline.rs` gains:

```rust
/// Blocking recv off-worker (today's path, byte for byte); cooperative on a
/// worker of the executor's pool.
pub(super) fn recv_permit(rx: &Receiver<()>, executor: &Executor) -> Result<(), RecvError>;
//  if !executor.on_worker() { return rx.recv(); }
//  loop { match rx.try_recv() {
//    Ok(()) => return Ok(()),
//    Err(Disconnected) => return Err(RecvError),
//    Err(Empty) => match executor.yield_now() {
//      Some(Yield::Executed) => {}
//      Some(Yield::Idle) => match rx.recv_timeout(Duration::from_millis(1)) {
//        Ok(()) => return Ok(()), Err(Timeout) => {}, Err(Disconnected) => return Err(RecvError) },
//      None => return rx.recv(),
//  } } }
/// Receive all PIPELINE_DISPATCH_PERMITS tokens via recv_permit (stops early on disconnect).
pub(super) fn drain_permits(rx: &Receiver<()>, executor: &Executor);
```

- The three `permit_rx.recv()` sites and `acquire_permit` call
  `recv_permit`, and `WRITER_METRICS.permit_wait_ns` is still measured
  around it.
- `WriterMetrics` gains `coop_wait_ns: AtomicU64`. `recv_permit` and
  `drain_permits` add the elapsed time of every cooperative loop (the
  on-worker branch only) to it, and `WriterMetrics::emit` emits it as the
  counter `writer_coop_wait_ns`. The `_wait_ns` suffix puts it in
  `brokkr sidecar --stalls`. On every CLI path (3.4) it stays 0; a
  non-zero reading means a `PbfWriter` is being driven from a rayon worker.
- `flush`: announce, then drop `tx`; if `executor.on_worker()` then
  `drain_permits`; then join and check, as today.
- `Drop`: drop `tx`; if `on_worker` then `drain_permits`; then join.
- Off-worker, which is every CLI path, `flush` and `Drop` are unchanged.
- Rustdoc on `flush` and `Drop` gains the contract: `flush()` is
  completion; `Drop` is cleanup only and reports no missing tail.
- The `WRITE_AHEAD` rustdoc's third bullet ("no send can ever block on a
  full channel") is rewritten. The new text: a send can block while the
  writer thread is inside `write_chunk`. It cannot deadlock, because the
  writer thread is not a rayon thread and moves every received item into
  its `ReorderBuffer` without waiting for any particular seq. The permit
  pool bounds tasks dispatched but not yet sent, not items sent but not yet
  written.

### 4.7 Markers (`src/debug.rs`, B1)

- `pub fn enable_marker_fifo_from_env()` stores `true` in a private
  `static ARMED: AtomicBool`.
- `write_fifo` returns immediately when `!ARMED.load(Relaxed)`, before
  `STATE.get_or_init`. The env read, FIFO open and clock anchoring stay
  inside the lazy first-emission path.
- `emit_mallinfo2` returns immediately when `!ARMED`.
- `read_page_faults` returns `(0, 0)` immediately when `!ARMED`. Its only
  callers feed `emit_counter`, which drops the values in an unarmed
  process anyway, so the CLI's armed behaviour is unchanged.
- The doc comment on `emit_marker` gains: no-op unless the process called
  `enable_marker_fifo_from_env`.

### 4.8 CLI (`cli/src/host.rs`, new)

```rust
pub(crate) struct LegacyProcessActions;
impl ProcessActions for LegacyProcessActions {
    fn cross_thread_alloc_pool(&self) { pbfhogg::process::cap_malloc_arenas() }
    fn fd_allowance(&self, _wanted: u64) -> FdAllowance { pbfhogg::process::raise_nofile_to_hard_cap() }
}
pub(crate) struct StderrSink; // event: eprintln!("{}", e.args)  (B6: crate::output::stderr_line(e.args))
pub(crate) fn cli_host() -> Host; // Legacy, MachineProbe, Diagnostics::new(Arc::new(StderrSink)), Evict, Ambient, None
```

- `main` calls `pbfhogg::debug::enable_marker_fifo_from_env()` as its first
  statement, builds one `cli_host()`, and passes `&host` to every `run_*`.
- Every `stats.print_summary()` in the CLI becomes
  `stats.write_summary(&mut host.diagnostics.summary_writer("<key from 3.6>"))?`.
  The write cannot fail; `?` keeps the type honest.
- The CLI's direct public-reader uses keep eviction explicitly (B3):
  - `run_bench_read` constructs its readers with
    `ElementReader::from_path_with_eviction(path, true)` (three) and
    `BlobReader::from_path(path)?.evict_page_cache(true)` (one);
  - `run_inspect`'s `HeaderGet` arm uses
    `pbfhogg::BlobReader::open(path, direct_io)?.evict_page_cache(true)`.
- `run_bench_write` passes `Executor::Ambient`.

## 5. Verification instruments

### I1. `tools/process-trace-shim/` (B0)

A workspace member: `members = ["cli", "tools/process-trace-shim"]`, with
`publish = false` and `[lib] crate-type = ["cdylib"], test = false,
doctest = false`. It depends on `libc` and uses `[lints] workspace = true`.

Like `pbfhogg-cli`, it carries no-op features `commands`, `test-hooks`,
`linux-direct-io` and `linux-io-uring`, so the `all` sweep can apply its
feature list to every `build_packages` entry. A comment beside them says
why.

The crate root is
`#![cfg(all(target_os = "linux", target_env = "gnu"))]`.

Behaviour:

- **Init.** A `.init_array` constructor reads `PROCESS_TRACE_PATH` through
  the real `getenv`, resolved with `dlsym(RTLD_NEXT, ..)`. When the
  variable is unset, every wrapper only forwards. When it is set, the
  constructor opens the path `O_WRONLY|O_CREAT|O_APPEND|O_CLOEXEC`, mode
  0644. It assigns the loading thread logical id 0.
- **Interposed symbols.** Each forwards to its `RTLD_NEXT` symbol and
  records afterwards: `mallopt`, `getrlimit`, `getrlimit64`, `setrlimit`,
  `setrlimit64`, `posix_fadvise`, `posix_fadvise64`, `pthread_create`,
  `getenv`, `sched_getaffinity`.
- **Reentrancy.** A thread-local reentrancy flag makes every wrapper
  forward without recording when re-entered. errno is saved before
  recording and restored afterwards.
- **Thread identity.** `pthread_create` takes the next id from an
  `AtomicU64` and boxes `{start, arg, id}`, then calls the real
  `pthread_create` with a trampoline. The trampoline stores `id` in TLS,
  records `S`, runs the start routine and records `E`.
- **Records.** Each record is one line, formatted on the stack in a
  `[u8; 4352]` (room for a `PATH_MAX` path plus the fields) with a
  hand-rolled integer formatter and written with a single `write(2)`.
  `<t>` is the calling thread's logical id; `inf` is `RLIM_INFINITY`.
  errno is recorded only when the call failed: a successful call leaves
  errno unspecified, so recording it would make the field
  nondeterministic. The wrapper saves errno immediately after the real
  call, before any formatting.
  - `<t> C <child> <ret>` - pthread_create (child id `-` when ret != 0)
  - `<t> S` / `<t> E` - thread start / end (raw capture only; not compared,
    see the canonical form)
  - `<t> M <param> <value> <ret>` - mallopt
  - `<t> GR <resource> <ret> <err> <cur> <max>` - `<err>` is `-` on
    success; on failure `<cur>` and `<max>` are `- -`, because the output
    struct is undefined then
  - `<t> SR <resource> <cur> <max> <ret> <err>` - `<err>` is `-` on success
  - `<t> F <path> <offset> <len> <advice> <ret>` - `<path>` is
    `readlink("/proc/self/fd/<fd>")` (raw syscall, no allocation), `?` if
    that fails. A path names the file directly, so the trace needs no
    device/inode mapping and is immune to a deleted scratch file's inode
    being reused by the next one.
  - `<t> V <name> <0|1>` - getenv, found or not
  - `<t> A <ret> <cpu_count>` - sched_getaffinity, `CPU_COUNT` of the
    returned mask

### I2. `tests/cli_host_identity.rs` (B0)

A module of `all`, mounted in `tests/all.rs` as
`#[cfg(all(feature = "commands", target_os = "linux", target_env = "gnu"))] mod cli_host_identity;`.
Its tests live in `mod tier3` and are `#[ignore]`.

`build_packages` of the `all` sweep becomes
`["pbfhogg-cli", "process-trace-shim"]`. The shim is
`$BROKKR_TEST_BIN_DIR/libprocess_trace_shim.so`; the test panics naming
`--sweep all` if the shim is missing.

There are three tests, and no name is a substring of another:
`host_identity_bless_goldens`, `host_identity_matches_goldens`,
`stderr_closed_aborts_like_baseline`.

**CliInvoker additions** (`tests/common/cli.rs`):

- `child_limits(cpus: usize, nofile: (u64, u64))` installs a `pre_exec`
  closure through `std::os::unix::process::CommandExt` that calls
  `libc::sched_setaffinity(0, ..)` on the first `cpus` CPUs of the test
  process's own mask. The mask is computed before spawning, and spawning
  panics if fewer than `cpus` CPUs are available. The closure then calls
  `libc::setrlimit(RLIMIT_NOFILE, soft, hard)`. Only async-signal-safe
  calls run inside the closure.
- `stderr_closed()` creates a pipe with `std::io::pipe()`, drops the
  reader, and passes the writer as the child's stderr. `run` then skips the
  stderr drain thread.
- `clean_env()` calls `Command::env_clear()`. The harness then sets exactly
  `LD_PRELOAD=<shim>` and `PROCESS_TRACE_PATH=<trace>` and nothing else,
  so no variable of the developer's shell or of brokkr's test environment
  (`RAYON_NUM_THREADS`, `RAYON_RS_NUM_CPUS`, `BROKKR_MARKER_FIFO`,
  `GLIBC_TUNABLES`, `MALLOC_ARENA_MAX`, `RUST_MIN_STACK`,
  `RUST_BACKTRACE`, an inherited `LD_PRELOAD`) reaches the child. Every
  `V` record therefore reads `0` except where the child itself sets a
  variable.

**Host preconditions**, checked once by each test before any scenario, each
panicking with a message that names the precondition:

- `std::thread::available_parallelism()` in the test process is at least 4.
  It is the minimum of the affinity mask and the cgroup `cpu.max` quota,
  so a result of 4 or more proves the quota is at least 4, and a child
  pinned to 4 CPUs then sees exactly 4. The `A` record cannot show this,
  because it reports the mask only.
- The test process's hard `RLIMIT_NOFILE` is at least 4096, because
  `child_limits(4, (4096, 4096))` sets the child's hard limit to 4096 in
  `pre_exec`, which fails when that would raise it.

**Scenario directories.** Each run gets a fresh directory from
`tempfile::Builder::new().prefix("<scenario>-").tempdir_in(<target>/host-identity/run)`,
where `<target>` is the project's `target` directory (on the project
filesystem, never `/tmp`, so `--direct-io` and `--io-uring` scenarios run
on a filesystem that supports them). Before the run, the inputs the
scenario names are copied into `<DIR>/inputs/`, and argv names them there.
Nothing the child writes can land in the committed
`tests/fixtures/host_identity/inputs/`, including `diff`'s shard scratch,
which `diff/parallel.rs` places beside the old input; and no checkout path
reaches argv, stderr or the trace.

**Fixtures.** Written by the bless test only, into
`tests/fixtures/host_identity/inputs/` (committed). Helpers marked
"(pub-crate)" are private test helpers today and become `pub(crate)` in B0.

The base element set: `generate_nodes(300, 1)`, `generate_ways(40, 1, 5,
1)` and `generate_relations(5, 1, 2, 1)`, then tagged so the tag-driven
commands match something: every node whose id is a multiple of 10 gets
`("amenity", "bench")`, every way with an odd id gets
`("highway", "residential")`, and every relation gets
`("type", "multipolygon")`. Node coordinates are `1e-4` degrees times the
id (`generate_nodes`), so the bbox `0,0,0.015,0.015` holds nodes 1 to 150
and every way's refs (all ways reference nodes 1 to 5).

Multi-blob on purpose: a one-blob-per-kind fixture collapses pool sizing by
schedule length, reorder buffers and blob classification to degenerate
cases. `common::write_multi_block_test_pbf(path, &nodes, &ways, &relations,
100)` gives three node blobs, one way blob and one relation blob. It uses
the sync writer, which emits no indexdata, so the indexed fixtures are
made by running the baseline binary's passthrough `cat` over it
(`CliInvoker`, no shim, cleared environment).

- `base-noindex.osm.pbf`: `write_multi_block_test_pbf` of the base set,
  block size 100.
- `base.osm.pbf`: `pbfhogg cat base-noindex.osm.pbf -o base.osm.pbf`.
- `changed.osm.pbf`: the same two steps over the base set with node 7
  removed, node 301 added (`generate_nodes(1, 301)`, untagged) and way 40
  removed.
- `apply-base.osm.pbf` and `apply.osc.gz`: the base and OSC of
  `cli_apply_changes::merge_basic_create_modify_delete`. Both are inline
  literals in that test today, so B0 extracts them into
  `cli_apply_changes::write_crud_base(path: &Path)` and
  `cli_apply_changes::CRUD_OSC: &str` (pub-crate), which the test and its
  `merge_basic_create_modify_delete_direct_io` and `_uring` twins then call.
  The OSC is written through `cli_apply_changes::write_osc` (pub-crate).
- `extract.json`: `{"directory": ".", "extracts": [{"output": "a.osm.pbf",
  "bbox": [0, 0, 0.015, 0.015]}, {"output": "b.osm.pbf", "bbox": [0.015,
  0.015, 0.03, 0.03]}]}`. The scenario passes `-d <O>`, so the config holds
  no path.
- `history.osm.pbf`: `cli_time_filter::write_history_input` (pub-crate).
- `snapshot.osm.pbf`: `cli_time_filter::write_snapshot_input` (pub-crate).
- `geocode.osm.pbf`: `geocode_index::write_synthetic_indexed_input`
  (pub-crate).
- `bad-indexdata.osm.pbf`: `base.osm.pbf` with the first OsmData blob's
  indexdata length changed to 30 via
  `common::adversarial::mutate_blob_header_indexdata`.

**Scenario matrix.** Each scenario runs with `clean_env()` and
`child_limits(4, (1024, 4096))` unless noted. `<O>` is the scenario's
output path inside its directory; inputs are the copies in
`<DIR>/inputs/`. Every command whose code B1 to B5 touch has at least one
scenario; the coverage statement after the table says what the matrix does
not reach.

| scenario | argv | expected exit |
|---|---|---|
| cat_passthrough | `cat base -o <O>` | 0 |
| cat_type_way | `cat base --type way -o <O>` | 0 |
| cat_clean_version | `cat base --clean version -o <O>` | 0 |
| cat_type_way_clean | `cat base --type way --clean version -o <O>` | 0 |
| cat_dedupe | `cat base changed --dedupe -o <O>` | 0 |
| check_default | `check base` | 0 |
| check_ids | `check base --ids` | 0 |
| check_ids_full | `check base --ids --full` | 0 |
| check_refs | `check base --refs` | 0 |
| check_index | `check base --index` | 0 |
| check_index_refs | `check base --index --refs` | 0 |
| degrade_strip_indexdata | `degrade base --strip-indexdata -o <O>` | 0 |
| degrade_strip_locations | `degrade base --strip-locations -o <O>` | 0 |
| repack | `repack base --elements-per-blob 100 -o <O>` | 0 |
| renumber | `renumber base -o <O>` | 0 |
| sort | `sort base -o <O>` | 0 |
| time_filter_snapshot | `time-filter snapshot -o <O> 250` | 0 |
| time_filter_history | `time-filter history -o <O> 250` | 0 |
| geocode | `build-geocode-index geocode --output-dir <O>` | 0 |
| altw_sparse | `add-locations-to-ways base --index-type sparse -o <O>` | 0 |
| altw_external | `add-locations-to-ways base --index-type external -o <O>` | 0 |
| altw_auto | `add-locations-to-ways base --index-type auto -o <O>` | 0 |
| diff_text | `diff base changed -c` | 1 |
| diff_osc | `diff base changed --format osc -o <O>.osc.gz` | 0 |
| apply_changes | `apply-changes apply-base apply.osc.gz -o <O> --force` | 0 |
| apply_changes_uring | `apply-changes apply-base apply.osc.gz -o <O> --force --io-uring` | 0 |
| sort_direct_io | `sort base --direct-io -o <O>` | 0 |
| sort_uring | `sort base --io-uring -o <O>` | 0 |
| bad_indexdata | `cat bad-indexdata --type way --force -o <O>` | 0 |
| extract_complete | `extract base -b 0,0,0.015,0.015 -o <O>` | 0 |
| extract_simple | `extract base -b 0,0,0.015,0.015 --simple -o <O>` | 0 |
| extract_smart | `extract base -b 0,0,0.015,0.015 --smart -o <O>` | 0 |
| extract_multi | `extract base -c extract.json -d <O>` | 0 |
| tags_filter | `tags-filter base highway -o <O>` | 0 |
| tags_filter_omit_ref | `tags-filter base -R highway -o <O>` | 0 |
| tags_filter_osc | `tags-filter apply.osc.gz --input-kind osc name -o <O>.osc.gz` | 0 |
| tags_count | `inspect tags base` | 0 |
| getid | `getid base n10 w3 r1 -o <O>` | 0 |
| getid_add_referenced | `getid base -r w3 -o <O>` | 0 |
| removeid | `getid base --invert n10 -o <O>` | 0 |
| getparents | `getparents base n1 -o <O>` | 0 |
| export_nodes | `export base --type node -o <O>` | 0 |
| inspect | `inspect base` | 0 |
| inspect_nodes | `inspect base --nodes` | 0 |
| inspect_show | `inspect base --show w3` | 0 |
| inspect_get | `inspect base --get header.writing_program` | 0 |
| merge_changes | `merge-changes apply.osc.gz -o <O>.osc.gz` | 0 |
| merge_changes_j2 | `merge-changes apply.osc.gz -j 2 -o <O>.osc.gz` | 0 |
| bench_read_sequential | `bench-read base --mode sequential` | 0 |
| bench_read_parallel | `bench-read base --mode parallel` | 0 |
| bench_read_pipelined | `bench-read base --mode pipelined` | 0 |
| bench_read_blobreader | `bench-read base --mode blobreader` | 0 |
| fail_degrade_no_flag | `degrade base -o <O>` | 2 |
| fail_repack_zero_cap | `repack base --elements-per-blob 0 -o <O>` | 2 |
| fail_cat_clean_unindexed | `cat base-noindex --clean version -o <O>` | 2 |
| altw_external_at_cap | as altw_external, limits `(4, (4096, 4096))` | 0 |
| altw_external_nofile_too_low | as altw_external, limits `(4, (300, 300))` | 2 |

A "setrlimit rejected" boundary cannot be induced unprivileged and is not
in the matrix. `FdAdjustment::RaiseFailed`'s rendering is covered by the
unit test on `Display`.

The `--direct-io` and `--io-uring` scenarios need a host that supports
them. If one fails and `CliOutput::is_o_direct_unsupported` or
`is_uring_unsupported` is true, the bless test panics naming the scenario
and the missing platform capability rather than blessing a failure.

`derive-changes` has no subcommand of its own; it is `diff --format osc`
(`run_derive_changes`), so `diff_osc` covers derive's gzip-worker probe and
every derive scratch name in 3.5.

**Coverage.** Every command holding a 3.2 probe, a 3.3 reader site or a 3.6
event runs in at least one scenario, and B3's explicit CLI eviction sites
(`run_bench_read`'s four readers, `run_inspect`'s `HeaderGet`) run in the
`bench_read_*` and `inspect_get` scenarios. Whether one particular probe
or event line executes on these fixtures is not asserted per line; the
argument that the rewrite is complete is three-legged: the identity gate
proves wiring and call order for everything the matrix reaches; the
`sized_workers` unit table proves the formula for every 3.2 row; and the
B1 and B2 textlints prove no unconverted probe or `eprintln!` remains.
These 3.6 keys need inputs or builds the matrix was not designed to
provide, so the identity gate is not relied on for them, whether or not
one happens to fire on these fixtures (each is a one-for-one format string
move, checked in review against 3.6): `sort.overlap_runs`,
`sort.intra_unsorted`, `sort.verify_unclaimed_order`,
`cat.dedupe.overlap_runs`, `getid.blobs_skipped`,
`tags_filter.blobs_skipped_by_tag`, `tags_filter.pass2_blobs_skipped`,
`tags_count.blobs_skipped_by_tag`, `repack.cap_never_fired`,
`repack.way_metadata_loss`, `header.way_metadata_loss`,
`altw.sparse.stale_temp_removed`, `altw.external_hint`,
`inspect.direct_io_ignored`, `geocode.smoke_test.no_match`, every
`geocode.hotpath.*` key (`hotpath` is not in the `all` sweep),
`writer.parallel.thread_exit` and `writer.uring.error`.

**Build.** `[test] debug = true` in `brokkr.toml`, so the harness traces
the debug build of `pbfhogg`, not the release build users run. Call order
is the same, but any `cfg(debug_assertions)`-only thread, file or env
activity is traced as product behaviour, and a later change to it shows as
a golden diff.

**Capture.** Each run gets `LD_PRELOAD=<shim>` and
`PROCESS_TRACE_PATH=<DIR>/trace.txt`, and nothing else in its environment.
The harness records:

- argv;
- exit status, keeping the code and the terminating signal separately;
- stdout and stderr;
- the trace;
- per output file, or per file of the output dir sorted by name, one or
  two FNV-1a-64 digests, implemented in the test:
  - every PBF output gets a content digest: FNV over a rendering, made by
    the harness through the public `ElementReader`, of the decoded
    `OsmHeader` (bbox, required and optional features, writing program,
    replication fields) and of every element in file order (kind, id, tags,
    metadata, and coordinates, refs with way locations, or members), one
    line per element;
  - every PBF output also gets a raw byte digest, except in the scenarios
    whose framing varies with worker scheduling: `apply_changes` and
    `apply_changes_uring`. Their blob sizes vary
    (`cli_apply_changes_invariants.rs` `stats_block`), so only the content
    digest is stable;
  - every other output gets a raw byte digest; `.osc.gz` outputs are
    digested after `flate2::read::MultiGzDecoder`.

  If the bless test finds another scenario's raw digest varying between
  its three runs, it fails naming the scenario, and the remedy is adding it
  to the framing-variable list above, recorded the same way as a mask.

Raw captures go to `target/host-identity/<scenario>/`. Normalised captures
are what get compared, and what the bless test writes to
`tests/fixtures/host_identity/golden/<scenario>/{argv,exit,stdout,stderr,trace,outputs}.txt`.

**Normalisation.** This list is complete. It applies to argv, stdout,
stderr and the trace alike:

- The scenario directory's path becomes `<DIR>`.
- The child's PID becomes `<PID>` only where a PID is placed, the scratch
  names of 3.5: a decimal token immediately following `.pbfhogg-<name>-`,
  `sparse-index-`, `derive-<kind>-`, `derive-par-<kind>-` or `diff-par-`.
  A count or offset that happens to equal the PID is never rewritten.
- A deleted anonymous file's `/proc/self/fd` link name `/#<inode> (deleted)`
  becomes `/#<INO> (deleted)`.
- In stderr lines that start `auto-selected --index-type`,
  `of <float> GB page-cache budget` becomes `of <MEM> GB page-cache budget`,
  and `store estimate <float> GB` is kept.
- `Done in <float>s` becomes `Done in <T>s`.
- `elapsed_ms=<n>` (the bench commands' kv line) becomes `elapsed_ms=<T>`.
- In stderr lines starting `  Bytes:` or `  Blob sizes:`, numbers become
  `<N>`. These lines vary with scheduling; see
  `cli_apply_changes_invariants.rs` `stats_block`.

**Canonical trace form.** What is compared is the causal structure the
bricks must preserve, and nothing that depends on the schedule:

- within each thread that is not a pool member, the exact order of its
  `M`, `GR`, `SR`, `C`, `F`, `V` and `A` records (this is where
  arena-cap-before-pool, rlimit-before-workers and the eviction offsets
  live);
- the size of every pool;
- the multiset of records issued inside each pool.

Construction:

1. Group records by logical thread. Drop `S` and `E` records: whether a
   pool member is scheduled before the process exits, and whether a
   dropped rayon pool's workers (rayon does not join them on drop) record
   their end before `exit_group`, is a race. A thread's existence is
   carried by its `C` record, which its creator writes synchronously when
   `pthread_create` returns.
2. Lift out the rayon global pool. rayon-core 1.13 initialises the global
   registry in whichever thread first touches it, by reading
   `RAYON_NUM_THREADS` and `RAYON_RS_NUM_CPUS`, probing
   `available_parallelism`, and creating its workers. In the trace that is
   the record run `V RAYON_NUM_THREADS 0`, `V RAYON_RS_NUM_CPUS 0`, an `A`
   record, then a run of `C` records; explicitly sized pools
   (`num_threads(n)` with `n > 0`, every pool in `src/`) read neither
   variable. That run is removed from its creator's sequence, replaced
   there by one `G` token, and the pool it creates is compared as the
   top-level unit `global`.
3. Build the creation tree from the remaining `C` records. A child is named
   by its parent's name plus the index of its `C` record among the
   parent's `C` records, for example `0.3`.
4. A run of two or more consecutive `C` records in one thread, with no
   other record between them, is a pool (rayon's registry and every
   `thread::scope` fan-out spawn their members in a loop). A pool, and
   `global`, are compared as their size plus the sorted multiset of every
   record issued by any member or any member's descendant, flattened. The
   multiset is per pool, not per member, so the same work landing on
   different members compares equal. Adjacency can over-group, for example
   a writer thread created immediately before a pool; over-grouping only
   loosens ordering among the threads involved, it never produces a false
   difference, so it is accepted. Under-grouping (a pool whose `C` records
   are interleaved with other records) compares members exactly and is
   caught by the bless test's three-run check.

If the bless test finds the `G` token's position varying (two threads
racing to first touch the global pool), the remedy is dropping the `G`
token, recorded as a mask.

**Compare test** (`host_identity_matches_goldens`). It runs every scenario
and diffs the normalised captures against the goldens, reporting the first
differing line per artefact per scenario. It also asserts positive controls
on the goldens themselves, so a shim that stopped interposing cannot pass
by vacuity:

- `cat_clean_version`: thread `0` has `M -8 2 1` before its first `C`
  record. The harness cannot tell which `C` is the classify pool
  (`cat_filtered` creates the writer thread first), but the arena cap is
  `cat_filtered`'s first statement, so it precedes every thread the
  command creates;
- `altw_external`: `GR 7` and `SR 7 4096 4096`;
- `altw_external_at_cap`: `GR 7` and no `SR`;
- `time_filter_history`: at least one `F` record with advice `4`
  (`POSIX_FADV_DONTNEED`);
- `cat_passthrough`: `V BROKKR_MARKER_FIFO 0`;
- `cat_clean_version` and `altw_external`: at least one `A` record (both
  reach a 3.2 probe: the classify pool and stage 1's `cpu_cap`). The
  control is not applied to every scenario: the `fail_*` scenarios return
  before any probe (`repack`'s zero-cap check is its first statement;
  `degrade`'s flag check and `require_indexdata` precede any thread), and
  their correct goldens have no `A` record. The child's affinity is set by
  the parent in `pre_exec`, before the shim loads, so it never produces an
  `A` record either.

**Bless test** (`host_identity_bless_goldens`). It checks the host
preconditions, writes the fixtures, runs every scenario three times,
asserts the three normalised captures agree per artefact, and writes the
goldens. If the captures disagree, it fails and names the scenario and
artefact. The remedy is a named mask added to the normalisation list above,
recorded in the harness's module doc, not a retry. The bless test prints
its own wall time, and B0 records the first bless run's wall in the
harness's module doc, so a later scenario addition knows its headroom under
the 280 s ceiling. If the first bless exceeds 280 s, B0 splits the matrix
by command family into test pairs named `host_identity_<family>_bless` and
`host_identity_<family>_matches`, keeping the rule that no test name is a
substring of another.

**Closed-stderr test** (`stderr_closed_aborts_like_baseline`, B0 to B5
only). `sort base -o <O>` with `stderr_closed()` must exit with code 101,
and no file may exist at `<O>`. B6 deletes this test (section 6, B6).

`brokkr.toml`:

- `[test.profiles.full].skip` gains `"host_identity_"` and
  `"stderr_closed_aborts_like_baseline"`. These are over-watchdog ignored
  tests run by name, and the bless test must never run under a profile.
  Before adding the entries, grep the suite for each string to confirm it
  matches nothing else.
- `[[check]] doctests` `test_exclude_packages` gains `process-trace-shim`.
- The `glibc-allocator-calls-live-in-debug` exclude gains `src/process.rs`
  in B1, and its comment's list of wrappers (`cap_malloc_arenas`,
  `malloc_trim`, `emit_mallinfo2`) is reworded to name both files.

`TODO.md` "Important: ignored tests" gains the three names and the
command, `brokkr test <name> --sweep all --timeout 280`.

### I3. `tests/host_contract.rs` (root tier, module of `all`; grows per brick)

This file uses `pbfhogg::host` and `pbfhogg::process`, which are added to
the stable allowlist in `reference/testing.md` in B1. It also uses
`doc(hidden)` command entries (`cat`, `sort`, `repack`, `altw`), and its
header says why: the property under test, two runs or a non-CLI host in one
process, has no CLI expression.

Every `Host` comes from `common::test_host()`, which uses
`NoProcessActions`, `MachineProbe`, `Diagnostics::discard()`, `Keep`,
`Ambient` and no scratch parent. A test changes one field.

`common::test_host()` also raises the test process's soft `RLIMIT_NOFILE`
to its hard cap, once per process, through a `std::sync::Once` calling
`pbfhogg::process::raise_nofile_to_hard_cap()`. The test binary is the
caller here, performing the process action itself, as the rule prescribes.
This replaces the raise the shared binary used to inherit from whichever
test first ran the external backend (section 1, item 18), so concurrent
external joins in the `all` binary (`concurrent_external_joins_use_distinct_scratch`,
and `non_indexed_parity`, `edge_cases`, `renumber_external` under
`parallel = {}`) keep today's headroom, and it now happens
deterministically, before any test's first external run.

Because of that raise, the one test that needs an unraised limit cannot
live in `all`. It gets its own target, `tests/process_nofile.rs`, a
`[[test]] name = "process_nofile"` entry in `Cargo.toml` with a comment
beside it saying why (it lowers its own process's soft limit), in the same
style as the `fault_*` targets:

- `no_process_actions_leave_rlimit_nofile_unchanged` (B1): reads the hard
  limit and panics naming the precondition unless it exceeds 1024; lowers
  the soft limit to 1024; runs `altw::external::external_join` on
  `write_indexed_pbf` input with a host built directly (not through
  `test_host()`, which would raise it) from `NoProcessActions`; asserts
  libc `getrlimit` after equals `(1024, hard)`. At 1024 stage 1 runs 3
  workers (`(1024 - 64) / 256`), so the join succeeds.

The tests in `host_contract.rs`, by the brick that lands each:

- **B1**
  - `process_actions_fire_once_at_each_hook_site`: a recording
    `ProcessActions` with counters. `cat --clean version` gives
    `(alloc 1, fd 0)`, `external_join` gives `(0, 1)`, and `cat` without
    flags gives `(0, 0)`.
  - `resources_provider_is_consulted`: a counting wrapper around
    `MachineProbe`. `repack` gives `calls >= 1`.
- **B2**
  - `diagnostics_are_per_run`: two threads run `sort::sort` concurrently on
    inputs with 3 and 5 data blobs (`common::write_multi_block_test_pbf`,
    run through `cat` to index them), each with its own recording sink.
    Each sink holds exactly its own `sort.indexed` line
    (`"  3 OSMData blobs indexed"` and `"  5 OSMData blobs indexed"`
    respectively).
  - `unrecognised_indexdata_warning_is_per_handle`: public
    `BlobReader::from_path(bad)?.diagnostics(d)` over a fully iterated file
    yields one event. A second reader with a different `Diagnostics` yields
    one more. A third reader sharing the first `Diagnostics` yields none.
  - `fixed_memory_routes_auto_to_external`:
    `Fixed { parallelism: Some(4), memory_available: Some(0) }` on
    `add_locations_to_ways --index-type auto` over sorted indexed input
    yields an `altw.auto_select` event whose text starts
    `auto-selected --index-type external`.
- **B4**
  - `writer_on_a_one_thread_pool_flushes`: inside `pool.install`, with
    `Executor::Pool(one-thread pool)`, 200 `write_primitive_block` calls
    and `flush` give `Ok`. The output has 200 OsmData blobs, counted with
    the public `BlobReader`.
  - `writer_on_a_one_thread_pool_drops_unflushed`: the same, dropped
    without `flush`. The test completing is the assertion.
  - `ambient_writer_inside_a_foreign_pool_flushes`: `Executor::Ambient`
    inside a one-thread `pool.install`.
  - `swallowed_framing_panic_surfaces_at_flush`
    (`#[cfg(feature = "test-hooks")]`): a pool with `panic_handler(|_| {})`,
    10 blocks, `PbfWriter::test_panic_in_framing_at(3)`. `flush` returns `Err`
    containing `an upstream framer dropped without sending an earlier seq`.
- **B5**
  - `concurrent_external_joins_use_distinct_scratch`: two threads run
    `external_join` with outputs in one directory. Both succeed, and both
    outputs equal a sequential single run's
    `common::read_all_elements_with_coords`.
  - `scratch_parent_override_is_honoured`: `scratch_parent =
    Some(<dir>/missing)` on `external_join` returns `Err` naming that path.

Inline unit tests:

- `src/host.rs`:
  - `sized_workers` table, covering `Fixed(None)` giving 3, 4, 8 and 4, and
    `Some(1)`, `Some(2)` and `Some(16)` for every `(reserve, min)` pair in
    3.2 (B1);
  - `FdAllowance` `Display`, all five strings (B1);
  - `emit_once` across shared and separate `OnceKeys` (B2);
  - `SummaryWriter` line splitting, covering two `writeln!` calls giving
    two events, a `write!` split across calls giving one event, and the
    `Drop` partial (B2);
  - `Executor::Ambient` `install`, plus `on_worker` false off-pool and true
    inside `pool.install` for `Pool` (B4).
- `src/scratch.rs`: `RunToken` suffix (B5).
- `src/read/blob.rs`: the existing
  `evict_page_cache_opt_out_disarms_eviction_without_changing_the_read` is
  rewritten as `public_readers_keep_the_page_cache_unless_asked` (B3). It
  asserts `evict_call_count() == 0` after `from_path` iteration, `> 0` with
  `.evict_page_cache(true)`, and the same element count either way.
- `src/read/reader.rs`: the eviction test is rewritten to match (B3).

## 6. Bricks

Each brick is one commit, kept or reverted on its gates, and `brokkr check`
is green at every boundary. Commit messages and CHANGELOG follow the
project rules. Every commit bundles any dirty markdown and
`.brokkr/results.db`, and runs `brokkr fmt` first.

**Gate dataset choices.**

- The identity matrix uses the committed tiny fixtures: every question it
  asks is wiring and call order, never scale.
- `brokkr verify all` uses denmark `indexed`. It is the smallest bygg
  dataset with a `bbox`, which `verify extract` needs; malta and
  greater-london have none.
- The roundtrip gate is the named over-watchdog test `roundtrip_denmark`,
  which `--profile full` skips by name.
- Bench pairs use germany `indexed`, chosen by ruling Q4. Its input stays
  page-cache-resident between cells, so the pairs read CPU and
  synchronization cost rather than drive state (`reference/performance.md`,
  2026-10-21 A/B lessons).

**Neutrality without a per-brick bench (a deliberate deviation).** The
contract asks for a bench whenever a brick claims neutrality on a measured
path. B1, B2, B3 and B5 claim it without one: each changes only setup-time
dispatch (a dyn call per probe, an event per former line, an enum read per
reader, an atomic add per command), never per-blob work, and the identity
trace proves the threads, cap timing, eviction and probe points are
unchanged. The measured backstop is section 7's cumulative arc, `fa94c6c7`
against B5, with bisection across B1 to B5 when it fails. B4 is the one
brick that changes per-block synchronization, and it is benched on its own.

### B0 - instruments (no product code)

**Change:**

- I1, plus I2 with CliInvoker `child_limits`, `stderr_closed` and
  `clean_env`, the host-precondition checks and the scenario-directory
  handling;
- the (pub-crate) fixture helpers made `pub(crate)`, and the
  `merge_basic_create_modify_delete` base and OSC extracted into
  `cli_apply_changes::write_crud_base` and `CRUD_OSC`, with that test and
  its `_direct_io` and `_uring` twins calling them;
- `tests/all.rs` mount;
- the `brokkr.toml` entries and TODO.md runbook lines in section 5;
- notes filing of section 10's findings into `notes/bugs.md` and
  `notes/hygiene.md`, bundled here.

`Cargo.lock` changes with the new member and is committed.

B0 is committed only after all five gates pass, because the inputs and
goldens the bless test writes (gate 2) are part of B0's commit.

**Gates, in order:**

1. `brokkr check`
2. `brokkr test host_identity_bless_goldens --sweep all --timeout 280`.
   This writes the inputs and goldens and runs the three-run determinism
   check.
3. `brokkr test host_identity_matches_goldens --sweep all --timeout 280`,
   run twice. Both runs must pass.
4. `brokkr test stderr_closed_aborts_like_baseline --sweep all --timeout 280`
5. strace cross-check (Q5). Run these two commands exactly, immediately
   after gate 4: the `all` and `consumer` sweeps share `target/debug`, so
   `target/debug/pbfhogg` and the shim are the `--sweep all` build only
   because gate 4 built it last. The preload variables go to the tracee
   only, through strace's `-E`; set on strace itself (`env ... strace`)
   they would load the shim into strace too, and both processes would
   append to one trace, each with its own thread 0. The trace goes to
   `target/host-identity/xcheck-*.trace`, the strace log to
   `target/host-identity/xcheck-*.strace`.
   - `strace -f -qq -E LD_PRELOAD=target/debug/libprocess_trace_shim.so -E PROCESS_TRACE_PATH=target/host-identity/xcheck-altw.trace -e trace=prlimit64,fadvise64,clone,clone3 -o target/host-identity/xcheck-altw.strace target/debug/pbfhogg add-locations-to-ways tests/fixtures/host_identity/inputs/base.osm.pbf --index-type external -o target/host-identity/xcheck-altw.osm.pbf`
   - `strace -f -qq -E LD_PRELOAD=target/debug/libprocess_trace_shim.so -E PROCESS_TRACE_PATH=target/host-identity/xcheck-hist.trace -e trace=prlimit64,fadvise64,clone,clone3 -o target/host-identity/xcheck-hist.strace target/debug/pbfhogg time-filter tests/fixtures/host_identity/inputs/history.osm.pbf -o target/host-identity/xcheck-hist.osm.pbf 250`

   Read both file pairs. The checks are:
   - `SR 7` records equal `prlimit64` lines naming `RLIMIT_NOFILE` with a
     non-NULL new limit;
   - `GR 7` records equal `RLIMIT_NOFILE` lines with a NULL new limit;
   - `F` records equal `fadvise64` lines;
   - `C` records with a numeric child id equal the `clone3` and `clone`
     lines whose return value is a positive TID. Failed attempts (a
     `clone3` returning `ENOSYS` before glibc falls back to `clone`) are
     not thread creations and are not counted.

   Any mismatch means the shim misses a call path and blocks B0.

**Keep/revert:** keep when all five gates pass. Nothing in the product is at
risk.

### B1 - `Host`, `process`, hooks in place, `Resources` everywhere, markers armed

**Change:**

- `src/host.rs`, containing `Host` with `process` and `resources`,
  `ProcessActions`, `NoProcessActions`, `Resources`, `MachineProbe`,
  `Fixed`, `sized_workers` and `ResourcesHandle`, plus `src/process.rs`.
  `debug::cap_malloc_arenas`, `debug::set_mmap_threshold` and
  `debug::malloc_trim` move to `process` and are deleted from `debug`. The
  intra-doc link to `crate::debug::cap_malloc_arenas` in
  `src/commands/check/refs.rs` is rewritten to name the
  `ProcessActions::cross_thread_alloc_pool` hook it now calls.
- Every 3.7 entry point takes `host: &Host`. Internal command-layer helpers
  that need a host facility take `host: &Host`. Read, scan and write-layer
  functions take only the narrow facility:
  `scan::classify::parallel_classify_phase`, `_abortable`,
  `_accumulate` and its sibling take `resources: &dyn Resources`, and
  `resolve_thread_count(threads, resources)` keeps `Some(n) if n > 0 => n`.
- M1-M9 are replaced in place by `host.process.cross_thread_alloc_pool()`.
- M10: in `stage1_way_pass`, `let allowance =
  host.process.fd_allowance(wanted);` followed by
  `eprintln!("[altw external] {allowance}")`, which stays a plain
  `eprintln!` until B2 turns it into the `altw.external.nofile` event. The
  arithmetic continues on `allowance.descriptors`.
- Every 3.2 probe becomes `crate::host::sized_workers(&*host.resources, r,
  m, f)` with the site's cap appended. The read layer resolves through
  `ResourcesHandle`, falling back to `MachineProbe`.
  `page_cache_budget_bytes` becomes `host.resources.memory_available()
  .map(|b| b.saturating_sub(AUTO_ANON_HEADROOM_BYTES))` at the same call
  point.
- All 3.3 reader sites become `BlobReader::open_in` or
  `ElementReader::open_in` (B1 semantics, 4.5). Inline tests use
  `Host::for_tests()`.
- Markers per 4.7; the CLI per 4.8, minus diagnostics and readers;
  `tests/common::test_host()`; every test and fault-target call site.
- `brokkr.toml` textlints, each with `region = "code"` and
  `paths = ["src/**/*.rs"]`:
  - `no-process-mutation`, pattern
    `\blibc::(?:mallopt|setrlimit|prlimit|prlimit64)\b`, exclude
    `src/process.rs`;
  - `no-host-probe`, pattern `available_parallelism|/proc/meminfo`,
    exclude `src/host.rs`;
  - `no-env-read`, pattern `\benv::(?:var|var_os|vars|vars_os)\b`, exclude
    `src/debug.rs`;
  - `no-public-reader-in-commands`, pattern
    `\b(?:BlobReader|ElementReader)::(?:open|from_path|from_path_direct|from_path_with_eviction)\b`,
    paths `src/commands/**/*.rs`, `src/geocode_index/builder/**/*.rs`,
    `src/scan/**/*.rs`, `src/osc/**/*.rs`;
  - the glibc rule's exclude gains `src/process.rs`, and its comment is
    reworded per section 5.
  - `altw/sparse.rs`'s `statvfs` is deliberately outside `no-host-probe`
    (3.1); the rule's comment says so.
- `common::test_host()` with its once-per-process `RLIMIT_NOFILE` raise,
  and the `process_nofile` test target with its `Cargo.toml` entry (I3).
- I3 B1 tests and unit tests.
- Docs:
  - `decisions/0016-the-library-does-not-own-the-process.md`. The decision
    is section 2's rule. Its alternatives are: grant flags; CLI
    pre-application from flags (impossible, section 1); a `log` facade
    (cannot isolate runs; `tiny_http` already pulls `log`); `catch_unwind`
    around framing (disposition belongs to the pool owner); a
    per-allocation scratch counter (per-operation token instead). It also
    records that `Host` has public fields and no `#[non_exhaustive]`, so
    an added field breaks callers' struct literals, accepted pre-1.0. Add
    the `decisions/README.md` index entry.
  - `reference/testing.md`: the "What the shared binary shares" paragraph
    is rewritten. The arena cap is no longer shared: library tests run
    with `NoProcessActions` and the CLI runs as a child process. The
    `RLIMIT_NOFILE` raise is still shared, but now deterministically:
    `common::test_host()` raises the soft limit to the hard cap once per
    test process, and `process_nofile` is its own target because it needs
    the unraised limit. Add `pbfhogg::host` and `pbfhogg::process` to the
    stable allowlist.
  - The `tests/all.rs` header comment, to match.
  - `reference/performance.md`: both "`mallopt(M_ARENA_MAX, 2)` inside
    `renumber_external()`" sentences become "the arena cap requested at the
    start of `renumber_external()` through the caller's process actions
    (the CLI applies it)".
  - `AGENTS.md` Conventions gains one line: library code never mutates
    process state, reads the environment or writes stderr; see ADR-0016.
  - CHANGELOG Breaking: commands take `&Host`; `debug::cap_malloc_arenas`,
    `debug::set_mmap_threshold` and `debug::malloc_trim` moved to
    `process::`; the library no
    longer calls `mallopt`, raises `RLIMIT_NOFILE` or reads
    `BROKKR_MARKER_FIFO` unless the caller asks (markers are inert until
    `enable_marker_fifo_from_env`).

**Gates:**

- `brokkr check`
- `brokkr check --profile full`
- `brokkr test roundtrip_denmark --sweep all --timeout 280`. This is a
  reader change: every production reader construction moves to `open_in`,
  and the pipeline's thread sizing goes through `ResourcesHandle`.
- `brokkr test host_identity_matches_goldens --sweep all --timeout 280`
- `brokkr test stderr_closed_aborts_like_baseline --sweep all --timeout 280`
- `brokkr verify all --dataset denmark --variant indexed`. Every command's
  code path is touched.

**Neutrality:** this brick is on every measured path but changes, per call,
only the dyn calls at setup points. The neutrality gate is the identity
trace: the same `M`/`SR`/`GR`/`C`/`F`/`A` structure means the same threads,
cap timing, eviction and probe points. No bench (see "Neutrality without a
per-brick bench" above).

**Keep/revert:** all gates green, otherwise revert.

### B2 - diagnostics

**Change:**

- `Host.diagnostics`; 4.4 and the 4.5 B2 items.
- Every library macro site in 3.6 becomes its `diag!` or `diag_once!`
  event, and every summary becomes `write_summary(&self, w: &mut impl
  io::Write) -> io::Result<()>`, with one `writeln!` per former line and
  the same format strings.
- `apply_changes::merge` keeps its internal summary at the same point,
  through `host.diagnostics.summary_writer("apply_changes.summary")`. The
  duplicate print is kept; see section 10.
- `load_all_diffs`, `has_indexdata` and `has_tagdata` take `&Diagnostics`.
- The writer threads' lines go through a `Diagnostics` parameter:
  `parallel_writer_thread` and `uring_writer_thread` take one, and B2 adds
  the crate-internal twins `to_path_in`, `to_path_direct_in`,
  `to_path_uring_in` and `to_path_parallel_in` with signature
  `(path, compression, header, diagnostics)`. The public `to_path*`
  constructors call them with `Diagnostics::discard()`, and
  `writer_from_header_bytes*` calls them with `host.diagnostics.clone()`.
  B4 then inserts `executor` before `diagnostics` in every twin, as 4.6
  states.
- The CLI `StderrSink`, and `write_summary` at every CLI summary site.
- The `direct_reader.rs` test skip line is removed.
- Textlint `no-eprint-macro`, pattern `\beprint(?:ln)?!`, paths
  `src/**/*.rs`, `region = "code"`.
- I3 B2 tests and unit tests.
- `reference/osmium-parity.md`: its `MergeStats::print_summary` citation
  becomes `MergeStats::write_summary`.
- CHANGELOG Breaking: `print_summary` and `print_osmium_summary` replaced by
  `write_summary` and `write_osmium_summary` into an `io::Write`; the library
  writes nothing to stderr and reports through `Diagnostics`;
  `has_indexdata`, `has_tagdata` and `osc::parse::load_all_diffs` take
  `&Diagnostics`; the unrecognised-indexdata warning is once per
  `Diagnostics` handle instead of once per process.

**Gates:**

- `brokkr check`
- `brokkr check --profile full`. B2 changes the signatures of
  `HeaderWalker::open`, `read_frame_header`, `read_raw_frame` and
  `read_blob_header_only`, which the tier-3 tests exercise.
- `brokkr test roundtrip_denmark --sweep all --timeout 280`. This is a
  reader change: `BlobReader` and `WireBlobHeader` gain fields and header
  parsing changes.
- `brokkr test host_identity_matches_goldens --sweep all --timeout 280`.
  stderr byte identity is the point of this brick.
- `brokkr test stderr_closed_aborts_like_baseline --sweep all --timeout 280`

**Neutrality:** event emission replaces a line print one for one, and no
per-blob work changes. Gate: identity. No bench (see "Neutrality without a
per-brick bench" above).

### B3 - page-cache policy

**Change:**

- `Host.page_cache`; the CLI host uses `Evict`, and `test_host` and
  `for_tests` use `Keep`.
- `open_in` honours it, and the public default flips (4.5); the CLI's
  explicit eviction sites (4.8).
- Unit tests rewritten (I3).
- Docs:
  - `docs/guide/reading.md` gains a "Page cache" paragraph: readers leave
    the page cache alone by default; `evict_page_cache(true)` and
    `from_path_with_eviction(path, true)` drop pages behind the read head
    for one-pass reads of files larger than RAM.
  - The rustdoc changes in 4.5.
  - CHANGELOG Breaking: public file readers no longer evict the page cache
    by default.

**Gates:**

- `brokkr check`
- `brokkr check --profile full`
- `brokkr test roundtrip_denmark --sweep all --timeout 280`. This is a
  reader change.
- `brokkr test host_identity_matches_goldens --sweep all --timeout 280`.
  The `F` traces must be identical.
- `brokkr verify all --dataset denmark --variant indexed`

**Neutrality:** a policy enum read once per reader construction. Gate:
identity. No bench (see "Neutrality without a per-brick bench" above).

### B4 - executor and BUG-066

**Change:**

- `Host.executor`, Ambient in the CLI; 4.6 complete. The public `to_path*`
  constructors take `Executor`.
- `writer_from_header*` passes `host.executor.clone()` and
  `host.diagnostics.clone()`.
- Every 3.4 par-iterator expression is wrapped as `host.executor.install(||
  <the parallel expression>)`, around the parallel computation only, with
  the result bound outside. The geocode builder functions involved take
  `executor: &Executor`.
- merge_changes:
  - `jobs.is_some()` keeps its own pool and `pool.install(do_work)`
    exactly.
  - `jobs.is_none()` calls `host.executor.install(|| do_work(..))`, which
    is a direct call under `Ambient`, so the CLI is identical.
  - A comment states the exception: `do_work` drives no `PbfWriter`, so a
    whole-body install puts no producer on a worker.
- Textlint `no-ambient-rayon`, pattern
  `\brayon::(?:spawn|join|scope|in_place_scope|broadcast)\b|build_global`,
  paths `src/**/*.rs`, exclude `src/host.rs`, `region = "code"`.
  par-iterator wrapping cannot be textlinted. The complete list in 3.4 is
  what the review checks against.
- I3 B4 tests and unit tests.
- Updated `to_path` examples in `src/lib.rs` (a compiled `no_run` doctest,
  gated by doc-twin), `LIBRARY.md`, `README.md` and `docs/guide/writing.md`.
  `writing.md` gains an "Executor" subsection with 4.1's three rustdoc
  points.
- The `WRITE_AHEAD` rustdoc rewrite (4.6).
- The `writer_coop_wait_ns` counter (4.6).
- CHANGELOG Breaking: the `PbfWriter::to_path*` constructors take an
  `Executor`. Fixed: a `PbfWriter` driven from inside its own rayon pool
  (including a one-thread pool) no longer deadlocks in `write`, `flush` or
  `drop`; a framing panic under a caller's non-aborting `panic_handler`
  surfaces as an error from `flush`.

**Gates:**

- `brokkr check`
- `brokkr check --profile full`
- `brokkr test roundtrip_denmark --sweep all --timeout 280`. This is a
  writer change.
- `brokkr test host_identity_matches_goldens --sweep all --timeout 280`.
  The `C` traces must be identical.
- `brokkr verify all --dataset denmark --variant indexed`
- **Bench pair (ruling Q4).** After committing B4 as `<B4>` on parent
  `<B3>`, run `sudo fstrim -av` (if sudo is denied, ask the user to run it),
  then these twelve cells strictly in order, one at a time:

  ```
  brokkr renumber --dataset germany --variant indexed --bench 1 --commit <B3>
  brokkr renumber --dataset germany --variant indexed --bench 1 --commit <B4>
  brokkr renumber --dataset germany --variant indexed --bench 1 --commit <B4>
  brokkr renumber --dataset germany --variant indexed --bench 1 --commit <B3>
  brokkr renumber --dataset germany --variant indexed --bench 1 --commit <B3>
  brokkr renumber --dataset germany --variant indexed --bench 1 --commit <B4>
  brokkr repack --dataset germany --variant indexed --bench 1 --commit <B3>
  brokkr repack --dataset germany --variant indexed --bench 1 --commit <B4>
  brokkr repack --dataset germany --variant indexed --bench 1 --commit <B4>
  brokkr repack --dataset germany --variant indexed --bench 1 --commit <B3>
  brokkr repack --dataset germany --variant indexed --bench 1 --commit <B3>
  brokkr repack --dataset germany --variant indexed --bench 1 --commit <B4>
  ```

  `renumber` drives `write_primitive_block_owned`, the per-block
  `rayon::spawn` and the permit wait. `repack` drives `acquire_permit` and
  an `install`-wrapped `into_par_iter`. Read the walls with
  `brokkr results --command renumber --dataset germany --top 6` and
  `brokkr results --command repack --dataset germany --top 6`.

  The pairs are (1,2), (3,4) and (5,6) per command, with
  `delta = (<B4> - <B3>) / <B3>`.

  Expected noise: the only page-cache-resident germany record
  (`reference/performance.md`, the 2026-10-21 apply-changes cross-check)
  shows one binary's `--bench 3` walls spanning 3.1 to 4.2 s, far wider
  than 2 %. renumber and repack noise on germany is unmeasured, so the
  `<B3>` arm prices it: its spread, `(max - min) / median` of its walls, is
  recorded beside the verdict. Three pairs cannot resolve 2 % at such a
  spread, which is why the rule below escalates to more pairs rather than
  reading one median as final, and why sign agreement is evidence, never a
  keep condition.

  Per command:

  1. **Keep** if the median of the three deltas is at most +2.0 %.
  2. Otherwise, **check the host first**: run
     `brokkr sidecar --compare <B3 uuid> <B4 uuid> --human` on the worst
     pair. If its host-difference annotations (memory, governor, kernel)
     differ, run `sudo fstrim -av` and rerun that command's six cells once,
     in the same order; the rerun replaces the first set and the rule
     restarts at step 1.
  3. With no host difference, **add three pairs** for that command, in the
     same cell order as cells 1 to 6, and judge the six-pair median:
     - at most +2.0 %: keep;
     - above +2.0 % with at least five of the six deltas positive: the
       regression is real; read `brokkr sidecar <B4 uuid> --stalls` against
       the `<B3>` cell of the same pair for the writer wait counters, and
       revert B4;
     - above +2.0 % with fewer than five positive: the spread exceeds what
       the pairs can resolve. Keep B4 and record all six deltas and the
       `<B3>` spread in `reference/performance.md` as the measured noise
       band for this command, so the cumulative arc (section 7) reads
       against it.

  Cooperative-wait check: `brokkr sidecar <uuid> --counters --grep
  writer_coop` on one `<B4>` cell of each command must show
  `writer_coop_wait_ns` as 0 or absent. A non-zero value contradicts 3.4's
  survey (a CLI path drives a `PbfWriter` from a rayon worker), and B4 is
  not kept until the path is found and 3.4 corrected.

  The numbers are recorded in B5's commit (section 7).

### B5 - `RunToken`, scratch parent

**Change:**

- `src/scratch.rs`; `Host.scratch_parent`, `None` in the CLI.
- Tokens are allocated once, at the entries of `altw::add_locations_to_ways`,
  `altw::external::external_join`, `diff::diff` and
  `diff::derive::derive_changes`. `external_join` becomes a thin public
  wrapper allocating a token and calling `external_join_in`, which takes
  today's parameters plus `host: &Host` and `token: &RunToken`;
  `add_locations_to_ways` calls `external_join_in` with its own token.
- Every 3.5 name gains `{suffix}` immediately after `{pid}`, the derive
  shard paths become `derive-par-<kind>-<pid><suffix>-<tag>-<n|w|r>-<shard>.xml.tmp`, and
  failed-shard cleanup recomputes names from the same token.
- Every scratch parent becomes
  `host.scratch_parent.as_deref().unwrap_or(<today's expression>)`.
- I3 B5 tests and the unit test.
- `docs/guide/advanced.md` gains an "Embedding pbfhogg" section listing the
  six `Host` fields and what the CLI chooses for each. It also notes that
  the CLI's arena cap overrides `GLIBC_TUNABLES=glibc.malloc.arena_max`.
- `notes/hygiene.md` HYG-029: the in-process collision part is removed; its
  cross-process and reaping parts stay.
- The B4 bench verdict goes into `reference/performance.md` (section 7).
- CHANGELOG Fixed: concurrent library runs of `add-locations-to-ways`,
  `diff` and `derive-changes` in one process no longer share or delete
  each other's scratch; a `Host::scratch_parent` override is honoured.

**Gates:**

- `brokkr check`
- `brokkr check --profile full`. B5 changes scratch naming and the
  failed-shard cleanup paths, which the tier-3 scratch and fault tests
  exercise.
- `brokkr test host_identity_matches_goldens --sweep all --timeout 280`.
  Token 0 reproduces every legacy name, so the `F` and outputs artefacts
  are unchanged.
- `brokkr verify all --dataset denmark --variant indexed`. Scratch handling
  of altw, diff and derive changed.

**Neutrality:** one atomic add per command. Gate: identity. No bench (see
"Neutrality without a per-brick bench" above).

### B6 - C1: a closed stderr no longer kills the CLI (ruling Q1)

**Change:**

- `cli/src/output.rs` gains `STDERR_READER_GONE: AtomicBool`, separate from
  stdout's flag. It never feeds `reader_gone()` or `tolerate_reader_gone`.
- It also gains
  `pub(crate) fn stderr_line(args: fmt::Arguments<'_>)` and
  `pub(crate) fn stderr_fragment(args: fmt::Arguments<'_>)`, the
  `eprintln!` and `eprint!` equivalents. Each:
  1. takes `io::stderr().lock()`;
  2. returns if the flag is set;
  3. writes `args`, plus `"\n"` for `stderr_line`, with `write_fmt` under
     the lock;
  4. on `Err(e)` with `BrokenPipe`, stores the flag;
  5. on any other `Err(e)`, panics with `failed printing to stderr: {e}`,
     std's own text.
- `cli_eprintln!` and `cli_eprint!` macros wrap them.
- Every CLI `eprintln!` and `eprint!` moves onto them: `main`'s
  `Error: {e}`, `print_requested_ids`, `print_missing_ids`, the bench `kv`
  lines, `run_extract_config`, `run_show_element`, `run_tags_count`,
  `run_tags_filter`, `run_getid` and `run_build_geocode_index`.
  `StderrSink` uses `stderr_line`.
- Exit status logic in `main` is unchanged, so the command's own status
  stands, including `ExitWithCode`.
- Textlint `no-eprint-macro` paths gain `cli/src/**/*.rs`, exclude
  `cli/src/output.rs`.
- `stderr_closed_aborts_like_baseline` is deleted, together with the two
  entries B0 added for it: its `"stderr_closed_aborts_like_baseline"`
  entry in `[test.profiles.full].skip` in `brokkr.toml`, and its line in
  `TODO.md` "Important: ignored tests". `tests/cli_exit_codes.rs`
  gains three root-tier tests, all with `stderr_closed()`:
  - `a_closed_stderr_does_not_kill_the_run`: `sort` on
    `write_indexed_pbf` input exits 0, and the output reads back with the
    input's element count;
  - `a_closed_stderr_keeps_the_differences_status`: `diff` with
    differences exits 1;
  - `a_closed_stderr_keeps_the_failure_status`: `sort` of a missing file
    exits 2.
- The cumulative bench record (section 7) goes into
  `reference/performance.md`.
- Closing actions (section 9).
- CHANGELOG Changed (CLI): a closed stderr (`pbfhogg sort ... 2>&1 | head`)
  no longer aborts the command with status 101; diagnostics stop, the
  command completes, and the exit status is its own.

**Gates:**

- `brokkr check`. The `cli` and `all` sweeps carry the new tests.
- `brokkr test host_identity_matches_goldens --sweep all --timeout 280`.
  stderr with an open reader is unchanged byte for byte.

**Neutrality:** one atomic load per stderr line. No bench.

## 7. Measurement record

**B4 pair.** In B5's commit, add a block to `reference/performance.md`
beside the 2026-10-21 apply-changes A/B entry: the date; host `bygg`;
commits `<B3>`/`<B4>`; every cell's UUID and wall as a table (twelve, more
if B4's rule escalated); the per-pair deltas and medians; the `<B3>` arm's
spread per command; the `writer_coop_wait_ns` readings; the verdict. No
baseline is replaced, because the claim is neutrality.

**Cumulative arc.** After B5 lands, and before B6 is committed, run
`sudo fstrim -av`, then three interleaved pairs per command of
`--commit <B5>` against `--commit fa94c6c7`, in the cell order of B4's list:

- `brokkr renumber --dataset germany --variant indexed --bench 1 --commit <X>`
- `brokkr repack --dataset germany --variant indexed --bench 1 --commit <X>`
- `brokkr cat --dataset germany --variant indexed --clean --bench 1 --commit <X>`

The verdict rule is B4's, including its escalation steps, with `fa94c6c7`
in the role of `<B3>` and `<B5>` in the role of `<B4>`; the cooperative-wait
check does not apply (`fa94c6c7` has no such counter). Record the result in
B6's commit the same way. If the rule ends in "the regression is real", B6
waits, and bisecting runs B1 through B5 against `fa94c6c7` on the failing
command with the same pair protocol; the brick it lands on is reverted or
fixed before B6.

`reference/performance-history.md` gains nothing: no optimisation arc, no
superseded baseline.

## 8. Stopping rule and out of scope

In scope: every row of section 3, BUG-066, BUG-067, and C1.

The teardown stops at:

- the command entry points;
- the `PbfWriter` and reader constructors;
- `read::frame`, `read::raw_frame` and `HeaderWalker`;
- `osc::parse::load_all_diffs`;
- `debug.rs`.

No algorithm, budget constant, pool size or pipeline shape changes. The
fixed pools (`parallel_writer::POOL_SIZE` 16, renumber 4 and 6) stay fixed.

Excluded, named:

- **C2**, the framing-panic cancellation guard: declined by ruling Q2.
- **The `uring_writer` shutdown bound** in BUG-066's second paragraph. It
  is not a process-ownership question.
- **HYG-019's message cleanup** (prefix unification, wording). That is a
  CLI output change; this spec moves every message byte for byte.
- **HYG-029's cross-process scratch naming and reaping**, and
  `process_scratch_tag`'s clock source.
- **HYG-092.**
- **The `pbfhogg-commands` split.** ADR-0012 defers it.
- **Fixing apply-changes' double summary.** It is a CLI output change and
  needs its own sign-off (section 10).

## 9. Closing actions (in B6's commit)

- Delete `notes/library-use.md`, `notes/library-use-review.md` and this
  spec. Their durable content is ADR-0016, the CHANGELOG entries, the
  `docs/` sections, the rustdoc, and the performance records.
- Remove BUG-066 and BUG-067 from `notes/bugs.md`, except BUG-066's
  uring-shutdown paragraph, which stays as its own entry. BUG-066 is
  closed on two different grounds, and the commit message says which is
  which: its deadlock half is fixed (B4's cooperative wait); its
  abort-by-default half is not fixed but resolved by decision (ruling Q2
  declined C2, and abort under a pool without a `panic_handler` is now the
  documented disposition in `Executor`'s rustdoc and
  `docs/guide/writing.md`).
- Remove HYG-019 from `notes/hygiene.md` (what remains of it is message
  wording; refile that part as a new HYG item with the wording list).
- After the commit, run
  `bz close pbfhogg#1 -m "Landed in B0..B6; see ADR-0016"`, with the
  commit range in place of `B0..B6`.

## 10. Lateral findings (filed in B0's commit)

1. **apply-changes prints its summary twice.** The CLI's
   `run_apply_changes` prints the summary after `apply_changes::merge`
   already printed it. Visible on every CLI `apply-changes` run, and
   bench-merge prints the library copy plus its own kv lines. Fixing it is a
   CLI output change; it goes to `notes/bugs.md` as a new BUG with a
   one-line removal from `run_apply_changes` as the proposed fix.
2. **Stale `rayon::join` comment.** `src/osc/merge_join.rs`'s doc comment
   cites a `rayon::join` that no longer exists (hygiene).
3. **ltrace is useless on these binaries.** `BIND_NOW`/full RELRO means
   ltrace cannot trace pbfhogg, which voids half of ruling Q5's premise.
   strace and LD_PRELOAD interposition both work.
4. **altw `inject_metrics` statics are process-global.** Concurrent library
   runs sum into one set of counters (diagnostic-only, M17 class; hygiene).
   The same note names `WRITER_METRICS`, `PIPELINE_METRICS` and
   `process_scratch_tag` (section 1, item 15).
5. **The design's B0 bless plan does not fit brokkr.** It relied on
   redirecting `BROKKR_TEST_BIN_DIR`, and on unique test-name matching;
   section 1 items 1 and 5.
6. **Already in the design and still open, filed by this commit:**
   - the `diff/parallel.rs` scratch beside the old input;
   - derive's gzip workers ignore `-j`;
   - `emit_mallinfo2`'s unconditional work (resolved for unarmed library
     processes in B1);
   - the panic-hook-swapping test in `src/read/reader.rs` silencing
     concurrent tests' panic output;
   - `RLIMIT_NOFILE` raises being inherited by children and breaking
     `select(2)`.
7. **The static-hook textlint has a blind spot at
   `tests/fault_apply_changes.rs`.** `static-test-hooks-only-in-fault-targets`
   exempts `tests/fault_*.rs` by filename, on the premise that every such
   file is its own `[[test]]` target. `fault_apply_changes.rs` is not: it
   is a module of the shared `all` binary (legitimately, since it uses the
   per-instance `MergeOptions::panic_at_blob_seq` hook, as its header
   says). A static hook added to it later would run inside `all` and the
   textlint would not catch it. Hygiene: rename the file out of the
   `fault_` namespace, or narrow the exclude to the files that are
   `[[test]]` targets.
8. **altw external budgets descriptors from the soft limit alone.** Stage
   1 sizes its workers from `RLIMIT_NOFILE` without counting descriptors
   already open in the process, so concurrent external joins in one
   process can hit `EMFILE` whenever the soft limit is low. The spec
   sidesteps it for the test suite (section 1, item 18); a library caller
   running joins concurrently still can hit it (hygiene, or a BUG if a
   caller reports it).

## 11. Review findings rejected (r1, r2)

Every other finding of `notes/library-use-spec-r1.md` and
`notes/library-use-spec-r2.md` was checked against the code at `fa94c6c7`,
found valid, and folded into the section it concerns. r2's findings 5 and 6
duplicate r1's B-4 and S-1 and were folded with them. These were rejected,
in whole or in part:

- **r1 B-2, naming threads by `dladdr` on the start routine.** Every Rust
  thread, rayon's included, enters through std's single `thread_start`
  trampoline, with the closure in `arg`, so `dladdr` on `start` names the
  same symbol for every thread and gives no role. The rest of B-2 (racy
  `S`/`E`, the lazily created global pool) is accepted; section 5's
  canonical form handles it without per-thread names.
- **r1 S-1, `derive-changes` missing from the matrix.** The CLI has no
  `derive-changes` subcommand; derive is `diff --format osc`
  (`run_derive_changes`), which `diff_osc` already runs. The rest of S-1 is
  accepted.
- **r1 S-3, deleting `malloc_trim` and `set_mmap_threshold`.** The
  consistency point ("move both or neither") is accepted, and both move to
  `process`. Deletion is not taken: under the rule, a process mutation the
  caller invokes explicitly is legitimate, and `process` is where such
  operations live; removing a working public capability is not part of
  this ticket.
- **r1 S-6, having the identity harness assert the cooperative counter
  stays zero.** The harness runs the child with a cleared environment and
  no marker FIFO, so no counter is observable, and the shim cannot see a
  `rayon::yield_now` loop. The underlying concern is accepted and answered
  differently: 3.4 now records why no CLI path is on a worker, and the
  counter is read from the B4 bench cells' sidecar.
- **r1 S-5, widening the bound.** Not taken; the 2 % bound stays and the
  rule escalates to more pairs instead, with an explicit end state for each
  outcome.
- **r1 minor, deleting `osc::parse::load_all_diffs`.** It has no production
  caller, but it is public API in the documented `osc` module (not
  `doc(hidden)`), and a library caller loading a directory of diffs is
  exactly the user the ticket serves. Threading `&Diagnostics` into it is
  the rule applied uniformly, not shoehorning; pruning public API is
  outside this ticket.
- **r1 lateral, `fault_apply_changes.rs` breaking the static-hook rule.**
  It does not: it uses the per-instance `MergeOptions::panic_at_blob_seq`
  hook, and its header says why it lives in `all`. The textlint blind spot
  r1 noted alongside is real and is filed (section 10, item 7).
- **r2 2, "adjacency alone does not establish pool membership".** Accepted
  as an observation, but adjacency stays the grouping rule: rayon's
  registry and every `thread::scope` fan-out create their members in an
  uninterrupted loop, over-grouping can only loosen a comparison, and
  under-grouping is caught by the bless test. Section 5 states this
  argument rather than introducing a different membership signal.
