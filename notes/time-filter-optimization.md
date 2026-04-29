# time-filter optimization

`brokkr time-filter --dataset <X>`: filter a sorted PBF to a snapshot
at a cutoff timestamp. Snapshot path is planet-safe; history path is
sequential.

Code: [`src/commands/time_filter/mod.rs`](../src/commands/time_filter/mod.rs).

## Remaining work

### Parallel history-input path

`time_filter_history` is a sequential pending-group state machine on
the 3-stage pipelined reader (~2.4 avg cores). No history PBFs in the
dataset inventory today, so the wall doesn't show up in benches;
deferred until a real history workload lands.

Shape: workers decode + run per-block version selection emitting
`(prefix_complete_blocks, head_partial_group, tail_partial_group)`.
Consumer stitches blob N+1's head with blob N's tail when they match
(kind, id); writes the stitched winner as its own group.

### Adaptive passthrough via shadow counter

A prior raw-frame passthrough attempt hit 0.63 % all-survive on Europe
at cutoff 2024-01-01 - far below the ~14 % break-even. Passthrough is
viable only at very recent cutoffs ("filter out edits from the last
week" on a year-old snapshot) where low-ID blobs contain exclusively
pre-cutoff edits.

Cheap next move (~20 lines): inside `filter_block_snapshot`, run the
`ts <= cutoff && visible` predicate as a separate pass and return
`(all_survive, total)`. End-of-phase counter reports
`timefilter_all_survive_blobs / timefilter_total_blobs`. If a real
workload reports > 20 %, re-investigate passthrough - but from the
pipelined-reader path, not a pread swap.

### Blob-level timestamp range index

Blob index v1 carries `kind/min_id/max_id/count/bbox`, no timestamp
range. With a timestamp range per blob the scheduler could drop blobs
entirely above the cutoff without decompressing, and flag blobs
entirely below (and all-visible) as raw-passthrough candidates
without per-element scanning. Format bump, multi-command coordination;
only worth it if time-filter becomes a hot command.

## Do not re-attempt

- **Per-element `Instant::now()` timers in the callback.** Doubled
  Japan wall (37 s -> 73 s on 344 M elements). Use `--hotpath` for
  per-function breakdown.
- **Raw-frame passthrough via pread-from-workers.** Even at 0 %
  passthrough rate the I/O swap alone regressed Europe wall
  (92.6 s -> 112.2 s). Future passthrough work must stay on the
  pipelined reader.

## Cross-references

- The migration template that unblocked planet:
  `parallel_classify_phase` + `ReorderBuffer`, mirrored from
  `cat --clean` and `check --ids`.
- [`src/commands/time_filter/mod.rs`](../src/commands/time_filter/mod.rs)
