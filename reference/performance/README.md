# Performance Data

Current planet/europe baselines and current per-command phase breakdowns
across the active architecture. Optimization arcs, retired phase
breakdowns, and old commit-pinned cross-dataset tables live in
[History](#history).

## Host: plantasjen

- CPU: AMD (details via `brokkr env`)
- RAM: 30 GB
- Swap: 8 GB
- Storage: nvme (source, data, scratch), hdd (target/cargo)
- Governor: performance
- Profile: `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`

## Datasets

| Dataset | Raw PBF | Indexed PBF | ALTW PBF | Elements |
|---------|---------|-------------|----------|----------|
| Malta | 8 MB | 8 MB | - | ~1M |
| Greater London | 122 MB | 122 MB | - | ~17M |
| Denmark | 461 MB | 465 MB | - | 59M |
| Switzerland | 524 MB | - | - | - |
| Norway | 1.4 GB | 1.4 GB | - | - |
| Japan | 2.4 GB | 2.4 GB | - | 344M |
| Germany | 4.5 GB | 4.5 GB | - | - |
| North America | 18.8 GB | 18.8 GB | - | 2.58B |
| Europe | 32.4 GB | 33.6 GB | - | 4.2B (3.7B nodes, 454M ways, 8.2M rels) |
| Planet | 87.3 GB | 87.7 GB | 88.4 GB | 11.6B (10.4B nodes, 1.17B ways, 14.1M rels) |

## Reading rules

How a keep/revert verdict is read off this file and `.brokkr/results.db`.
Calibrated 2026-07-10 at commit `6d6e158` on denmark
`add-locations-to-ways` across all three modes: clean bench `7bd88e83`
(4.0 s wall, 1.49 GB peak anon), hotpath `53ed877e` (3.9 s), alloc
`a7e1159b` (4.3 s, ~7 GB cumulative churn).

Instrumentation here is sparse and per-command. Where a single production
pipeline would be annotated densely and pay a large fixed profiling tax,
pbfhogg spreads a handful of coarse `hotpath::measure` points across each
command, so `--hotpath` / `--alloc` overhead is workload-dependent and
usually small.

- **Use matched, interleaved cells for verdicts.**
  `brokkr bench NAME --dataset D` defaults to three iterations and stores
  the best. For A/B comparisons, alternate `brokkr bench --iterations 1
  [--commit REF] NAME --dataset D` cells and compare medians and sign
  consistency across pairs. Planet is often run with one iteration on
  cost, so a single-shot delta of a few percent
  at sub-150-s walls, and a larger fraction at sub-10-s walls, sits inside
  observed bench-to-bench variance (this file flags such rows "within
  noise"). A spec claiming a win states the expected bound up front and the
  verdict is read against that bound, not against zero.
- **`--hotpath` ranks; it does not measure absolute wall.** On the denmark
  calibration hotpath wall was 3.9 s against a 4.0 s clean bench - no
  measurable inflation, because altw's annotated functions are coarse
  (hundreds of microseconds to milliseconds per call over a few thousand
  calls). The per-call record cost only surfaces on an annotation taking
  tens of millions of sub-microsecond calls, which pbfhogg's sparse
  instrumentation mostly avoids. Use hotpath to see where the time goes; read
  the clean bench for the wall.
- **A hotpath percentage over 100% is not an error.** It is cross-thread CPU
  seconds relative to wall clock: on the denmark run `frame_blob_into`
  reports 614% and `process_block` 158%, the parallel compression and
  block-processing pools spending several core-seconds per wall-second.
- **Read peak RSS from the child sidecar.** Current script benches store
  `/proc` samples for profiling runs too. In the historical typed denmark
  calibration above, hotpath and alloc captured only point-in-time thread
  snapshots (415.9 MB hotpath, 315.6 MB alloc), below the clean 1.49 GB;
  those snapshots are not peak figures.
- **`--alloc` numbers are valid only from commit `678478d` onward.** That
  commit installs `CountingAllocator` as the global allocator under the
  hotpath-alloc feature; before it, tracking was not wired to the global
  allocator and byte totals undercount. The denmark alloc run above is post
  `678478d`, so its ~7 GB churn - led by `parse_and_inline_with_scratch`
  1.7 GB, `parallel_scan_blobs_raw` and `parallel_classify_phase` 1.6 GB
  each - is real; older alloc profiles do not compare.
- **A row's `command` is the brokkr subcommand that ran, not the pbfhogg
  one; `cli_args` is.** The 20 `inspect` hotpath rows of 2026-02-28 to
  2026-03-07 are tags counting (brokkr's `inspect` drove `pbfhogg
  tags-count`, later `inspect tags`), and the 2026-03-01 extended-metadata
  scan `e4f4508c` is stored under `fileinfo`, the subcommand `inspect`
  replaced. Select by argv (`--grep extended`, `--grep tags`), not by
  `--command`. The first hotpath profile of the full-decode inspect
  (`inspect --extended`) postdates them: from the change that measures
  `Blob::decompress_pooled` and the projection scan `scan_block_buffer`
  per blob, it splits decompression from validation plus projection, where
  before both folded into `run_pipeline` (denmark, `a9eb4708` plus that
  change, bygg, unstored: 7,396 blobs, `scan_block_buffer` 1.82 core-s,
  `decompress_pooled` 1.54 core-s, in a 0.19 s wall). `decompress_pooled`
  serves every pipelined block decode and includes the pool checkout and
  size checks, so its share is an upper bound on codec cost.
- **`writer_write_ns` is retired (2026-10-26); it never meant one thing.**
  On the buffered, `O_DIRECT` and io_uring backends it was writer-thread
  wall inside the backend write; on the `pwrite` pool (the default for
  `apply-changes` and add-locations-to-ways pass 2) it was pool-thread time
  summed across every worker, up to the pool size times wall. Runs from that
  date carry `writer_handoff_ns` (the handoff thread's elapsed time per
  chunk, the same definition on every backend, the pool's dispatch
  included) and `writer_pool_write_ns` (the pool's summed execution time,
  pool backend only); the definitions are on the counters in
  `src/write/metrics.rs`. An old `writer_write_ns` compares with
  `writer_pool_write_ns` on a pool-backend command (the pool's timer did not
  move) and roughly with `writer_handoff_ns` on every other backend, whose
  timer now also covers flattening a framed chunk, dropping the payload and
  the header write. `writer_bytes_written` kept its name but
  now covers the header frame and the synchronous writer's raw writes
  (`multi-extract`), so on a successful run writing fresh regular files it
  equals the summed output size on every backend; older
  rows undercount by the header on pipelined backends and by every raw
  write on synchronous ones. The synchronous writer's `writer_payload_*`
  counters, previously always zero, count too.
- **Sidecar rows from `56a50e0d` until `c2409c45` do not compare on the
  send-side counters.** `56a50e0d` sized the ordered engine's result channel
  to the admission window, which no run can fill, so a converted pool's
  sends never blocked there and the wait moved to the uncounted admission
  gate: `s4_send_ms`, the `WAIT_S4_SEND` and `WAIT_P2_SEND` spans and
  `altw_pass2_send_ms` read near zero for it. `c2409c45` restored the channel
  depth, and the engine now records its own gate, send and receive waits
  (`ordered_pool_*_wait_ns`) for every ordered pool. A row from that range
  does not compare with either side on those counters.
- **`s4_readiness_wait_ns` (2026-10-28) is not `s4_readiness_wait_ms` in
  other units.** The new counter is the add-locations-to-ways external
  stage 3 to 4 handoff as `brokkr sidecar --stalls` sees it: time from a
  stage 4 worker's first park on the router to its return, summed across
  workers, error-ending waits included, calls that never park excluded. The
  `_ms` trio is unchanged and still times every successful way-blob call,
  fast path included; `s4_readiness_wait_blobs` therefore counts way blobs,
  not blocked ones. Neither total bounds the other, so older rows, which
  have only `_ms`, do not compare with the new counter.
- **Four late-October 2026 changes affect how these counters read.**
  `merge_streaming_blobs_forced_rewrite` also counts, from `f703b02b`, the
  otherwise untouched unindexed blobs an `apply-changes --force` run must
  re-encode because no single index describes them (indexable ones are
  reframed and not counted); earlier forced runs copied those raw, and runs on indexed input
  read the same either side. `cat_blobs_decoded` and cat's "Decoded N blobs"
  count input blobs decoded, not output blobs written, from `8da26b23`; a
  `cat --clean` or `cat --type` row before it is not comparable on that
  counter. The unindexed diff and derive-changes merge decodes through a
  recycling pool from `126cbce2` and feeds the
  `pipeline_decompress_pool_*` drop counters, which earlier such rows lack;
  indexed runs at any `-j` decode into a fresh buffer per block and are
  unaffected. `publish_sync_calls` and
  `publish_sync_ns` count the publication's own syncs from `0fe60da5`,
  rather than a delta of the run's `writer_sync_all_*` totals; a CLI run
  never publishes concurrently, so the two definitions give the same values
  there.
- **Counter names across the counter-machinery commits (bygg, 2026-11-09,
  denmark, `--bench 1`, stored; `scripts/counter_names_diff.py`).**
  add-locations-to-ways `--index-type external`, `9dcf53a7^` (`8aa10c65`,
  cell `5dc8b38d`) against `a12fde41` (`91a4670b`): 193 names shared, only
  the retired `writer_write_ns` gone, 22 added (`header_walk_*`,
  `ordered_pool_*_wait_ns`, `s2_slot_fadvise_*`, `s2_slot_flush_lock_wait_ns`,
  `s4_readiness_wait_ns`, the writer and `pipeline_read_buf_*` additions);
  markers 30 shared, `CLI_COMMAND_START/END` added. Across `13dd02aa`:
  apply-changes (`2d467fbc` / `4cc4b16e`) 89 counters and 12 markers,
  apply-changes `-l` on `altw-keep-untagged` (`23477ae3` / `7a4ba6cf`) 94
  and 16, renumber (`0f5d47bd` / `8dfc6d0a`) 81 and 12, all identical. The
  verdict is bounded to these executions: names a path did not reach, and
  units or recording points under unchanged names, are not covered.
  Renumber's names changed earlier, in `fa69b667` (`_ms` timers to `_ns`,
  `*_send_ms` and `r1_scan_ms` gone, `*_window_wait_ns` and `*_reframe_ns`
  added): denmark cells before it (`6a8e9463`) do not compare with later
  ones on those counters.
- **Rows across the late-October 2026 changes.** `6592af1f` (protohoggr
  0.5.0) changed output bytes: `BlockBuilder` writes dense uid and
  user_sid deltas of magnitude `2^30` or more five bytes shorter, so
  output sizes and digests do not compare across it. `CAT_NODES_*`,
  `CAT_WAYS_*` and `CAT_RELATIONS_*` are absent from `8da26b23` until
  `168e581a` and since then mean write-side kind transitions nested in
  `CAT_FILTERED_*`, not execution barriers, so per-kind rows against the
  per-kind planet baseline (sidecar `f2315551`) are approximate and the
  wall is the exact comparison; `SORT_OVERLAP_PARALLEL_*`,
  `DIGEST_ENVELOPE_*` and `writer_write_ns` are gone for good. Hotpath
  names moved: getparents, time-filter, repack, degrade and
  multi-extract's writer report under `parallel_classify_phase_ordered`,
  command pipelines under `run_pipeline` instead of `run_pipeline_fused`,
  and cat `--clean`'s walk under
  `build_classify_schedule_blob_filtered_accepting_partial`.
- **`7feb4370` (encode storage kept across blocks) changed what the
  writer counters cover.** The writer's framed output chunk is gone:
  pipelined framing hands out one exactly sized frame, so
  `writer_frame_ns` now includes the header-and-body copy the writer
  thread used to do on the write side. Writer counters split across it
  compare only with that in mind.
- **`5bfa37ab` changed decode-buffer sizing and pool retention
  everywhere,** so the peak anon of any pool command compares across it
  only with that in mind ("Decode buffers sized from the blob and
  recycled").
- **Exit codes.** `854f9465` split them (1 an answer, 2 a failure;
  before, both were 1); `50a9fc06` made `diff --format osc` exit 1 on
  differences (it always exited 0 before). brokkr asks each build which
  side of the split it is on and does not accept exit 1 from a build
  before it (`brokkr man pbfhogg`, "Answer exit codes"); rows stored from
  such a build before that gate keep an exit code of 1 that cannot say
  whether it was an answer or a failure.

## Target index

Current baselines come first in each target file; historical records follow under History.
Tables covering several commands stay intact in cross-command or subsystem files.

- [add-locations-to-ways](add-locations-to-ways.md)
- [apply-changes](apply-changes.md)
- [cat](cat.md)
- [check](check.md)
- [classify](classify.md)
- [cross-command](cross-command.md)
- [diff](diff.md)
- [digest](digest.md)
- [end-to-end](end-to-end.md)
- [extract](extract.md)
- [geocode](geocode.md)
- [getid](getid.md)
- [inspect](inspect.md)
- [library-capabilities](library-capabilities.md)
- [memory](memory.md)
- [merge-changes](merge-changes.md)
- [multi-extract](multi-extract.md)
- [projections](projections.md)
- [read-pipeline](read-pipeline.md)
- [renumber](renumber.md)
- [repack-degrade](repack-degrade.md)
- [sort](sort.md)
- [tags-filter](tags-filter.md)
- [write-pipeline](write-pipeline.md)

## History

#### Performance History

Durable record of optimization arcs, phase breakdowns at landed-then-superseded
architectures, regression-window retrospectives, and old commit-pinned
cross-dataset tables. The current state of each command lives in
[Current measurements](README.md); this file is the "how we got here"
companion. Plan docs in `notes/` are working scaffolding and get deleted
when the work lands - this file is where their durable findings settle.

### TAINTED window (2026-04-09 to 2026-04-17, retired 2026-04-27)

A `has_indexdata` / `check_sorted_and_indexed` regression in the commit range
`4ce7e93..c0ae9a7` (Apr 9-17 2026) added an unaccounted O(N) all-blobs-scan
cost to commands whose hot path walks blob headers. Affected commands:
`add-locations-to-ways`, `build-geocode-index`, `cat --type`/`--dedupe`,
`check-ids`, `diff`, `extract` non-simple, `getid`, `inspect --nodes`/`--tags`,
`sort`, `tags-filter` non-invert. Other commands during that window were
unaffected.

The fix arc landed across `aa3147c` (`BlobReaderSource` `BufReader::seek_relative`
override), `ca6711e` (`has_indexdata` short-circuit), and the broader
`HeaderWalker` adoption sweep `de8daf1`..`01c67da`. Per-command headlines from
that arc:

- `cat` planet 497 s → 86.5 s (5.8×, both regression rollback and substantial
  improvement on the pre-regression path).
- `getid` planet 44 s → 6.1 s (7.2×, via shared `pread`-only `HeaderWalker`).
- `inspect` default planet 21.4 s → 6.5 s (3.3×).
- ALTW external europe -15.6 s / -5.5 % (Meta scan -49 % from the
  `BufReader::seek_relative` preservation; small downstream gains in
  Stages 1+2 from page-cache benefit).

Tainted DB rows were invalidated 2026-04-27 once each axis had either a
post-fix replacement in `.brokkr/results.db` or was waived as no-longer-active
(stage-isolation diagnostics, ALTW alt-compression study, sequential
diff-snapshots osc, cat-from-raw-input planet). The audit script used at the
time was local working material, never committed, and is gone.

---

### Retired plan docs

Plan and research docs live in `notes/` while their work is active and
are deleted on landing or on closure. Their durable findings are
absorbed into the sections above (or the named durable home). Git holds
the deleted files themselves:
`git log --diff-filter=D --name-only -- notes` lists every retired note
with the commit that removed it, which is the breadcrumb for searching
old commit messages. By topic:

- The `sort` optimization plan (retired 2026-07-13; the arc is complete
  and its durable measurement record is the "Sort" section above).
- diff-snapshots opportunities (Tier 1 items landed; sharded parallel
  block-pair merge is the canonical implementation).
- getid include-mode optimization (HeaderWalker + 1-pread probe shape
  landed at `d263d76`).
- The scan optimization audit (Tier 1 items landed; dense node index
  Pattern 2, O(1) `check_sorted_and_indexed`/`has_indexdata` probes, and
  unsorted extract paths remain intentional non-goals).
- Pipelined-reader decode backpressure (admission gate landed `a0a2e3b`,
  validated same day; findings in "Pipelined-reader decode-admission
  bound" above; root cause + bound semantics live in `run_pipeline`'s
  doc comment and `pipeline_metrics.rs`).
- The 2026-07-12 env-gated read-path batch: plan of record plus the
  fusion and batched-pipeline-rebuild specs; verdicts in "Env-gated
  read-path batch" above, fusion kept as ADR-0009, the batched engine
  and byte knobs reverted.
- Columnar integration and hybrid-batching research (deleted 2026-07-13;
  measured findings absorbed into the "Parallel classify" section above).
- apply-changes opportunities (deleted 2026-07-13; all items landed or
  were carried into the work-item list - 80.9 s planet best, with
  splice-in-place and exact-membership metadata deferred).
- zlib level tuning (deleted 2026-07-13; superseded by the write-path
  plan's item-2 compression matrix; the zstd:1 internal-pipeline
  recommendation lives in README.md and `reference/performance/README.md`).
- Streaming-pipeline composition (deleted 2026-07-13; closed - the
  valuable composition, inline indexdata in all write paths, already
  ships; multi-pass commands cannot consume streams).
- The reverse-geocoding spec (deleted 2026-07-13; original
  implementation spec for `build-geocode-index` - the shipped
  implementation deviated, format truth is `src/geocode_index/format.rs`
  doc comments).
- Spatial index in PBF and way-blob bbox speculation (deleted
  2026-07-13; speculative research, no measurements; conclusions
  carried into the work-item list's research ideas).

