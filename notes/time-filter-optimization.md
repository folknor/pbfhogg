# time-filter optimization

`brokkr time-filter --dataset <X>`: filter a sorted PBF to a snapshot
at a cutoff timestamp. Snapshot path is planet-safe; history path is
sequential.

Code: [`src/commands/time_filter/mod.rs`](../src/commands/time_filter/mod.rs).

## History-file support: OUT OF SCOPE for 1.0 (decided 2026-07-13)

pbfhogg's history-file handling (per-element version history and
visibility, consumed by `time_filter_history`) is **functional but
deliberately outside the 1.0 planet-scale validation surface**. The
code stays; nothing is deleted. Rationale:

- History has the weakest demand signal of anything in the roadmap:
  nothing in the project's own pipeline consumes history PBFs and no
  consumer has asked. Every other deferred item is gated on a
  demonstrated workload; 1.0-validating history would be the one place
  we build ahead of any consumer.
- "In scope" is a commitment, not a checkbox: `brokkr.toml` has no
  history variant on any dataset, so every time-filter bench to date
  runs a regular PBF and measures a near-no-op (every timestamp
  compare decides keep). Real validation means downloading a history
  dataset (planet history is ~120 GB; europe is the realistic
  iteration size), benching the actual multi-version workload,
  building the parallel history path below, and extending the
  32 GB-host planet-safety bar to a new input shape whose per-element
  version fan-out changes memory behavior.

**Re-entry trigger**: a real history consumer appears. First step is
then a europe history variant in `brokkr.toml` (exercise the path
without claiming planet safety), then a bench of the actual workload,
then the parallel history path. README's planet table carries a
footnote pointing at this scope decision.

## Remaining work (gated on the re-entry trigger above)

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
