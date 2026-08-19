# Bug hunt: core infrastructure and CLI

Hunt date 2026-08-12. Single Opus hunter, scope: `src/lib.rs`, `src/error.rs`,
`src/osm_id.rs`, `src/owned.rs`, `src/path_guard.rs`, `src/coord_fmt.rs`,
`src/debug.rs`, `src/reorder_buffer.rs`, `src/idset.rs`, `cli/src/main.rs`.
Findings only, unverified.

## Argument-surface and exit-code design

### Disclosed boundary: the exit-code split is unverified against brokkr's runner

The 0.6.0 exit-code split (0 success, 1 semantic-negative answer, 2 failure)
has *not* been verified by source inspection against brokkr, which shells out
to `pbfhogg` for every one of its subcommands: `~/Programs/brokkr` is not
readable from the sandboxes this loop ran in, on either the fix side or the
orchestrating side.

What was verified, empirically from this side: `brokkr inspect --force
--dataset denmark` and `brokkr inspect --force --dataset denmark --nodes` both
run clean, and neither injects `-j`, so neither the new `requires = "nodes"`
on `inspect -j`/`--force` nor the new `diff -v` + explicit `-j` conflict fires
under brokkr's own invocations. brokkr's `--force` is its own clean-tree
override and is not forwarded to the child.

The remaining theoretical exposure: a consumer - brokkr or any script - that
special-cases `== 1` as "the command failed" rather than testing `!= 0`. Such
a consumer now sees `2` where it used to see `1` for every genuine failure.
`1` itself kept its meaning for all four semantic answers, so the reverse
direction is safe. Tracked cross-repo in `TODO.md`.

Confidence: high that the exposure is narrow; unresolved only because the
other tree is unreadable.

## Smaller / lower confidence

- NOT A BUG, recorded so it is not re-filed: `src/debug.rs` marker emission was
  reported as able to tear a line across a partial `write_all` on the
  `O_NONBLOCK` FIFO. It cannot. Marker and counter lines are tens of bytes, and
  POSIX makes a write of at most `PIPE_BUF` (4096 on Linux) to a FIFO atomic -
  on a non-blocking fd it transfers every byte or none and fails with `EAGAIN`.
  Investigated round 3, 2026-08-20. Confidence: high.
- The raw-frame passthrough path (`src/read/raw_frame.rs`, and the byte-level
  reader in `src/commands/cat/dedupe.rs`) raises `io::Error`s that name neither
  the input file nor the blob. Round 4 gave `BlobReader` and `HeaderWalker` an
  input identity (`Error::source_path()` / `Error::blob_index()`, stamped at
  every error return in both), which covers the decode and preflight paths, but
  `raw_frame` is generic over a bare `Read` and has no path to report: plain
  `pbfhogg cat a.pbf b.pbf` on a corrupt passthrough blob still fails with a
  bare `failed to fill whole buffer`. Closing it means threading a path (an
  `Arc<Path>`, to match the two readers that now carry one) through the
  raw-frame helpers and their callers. Surfaced by the round-4 fix pass.
  Confidence: high.
- The per-command "a failed run leaves the destination byte-identical"
  property is only partly testable from the CLI. The round-4 fix pass reported
  it as *un*testable everywhere and filed the whole thing as a gap; the audit
  found that claim too strong. A multi-input re-encode fails after its writer
  is open: `cat good.pbf corrupt.pbf -c version -o dest` writes the first
  input out, then dies reading the second, so
  `cat_failure_leaves_preexisting_destination_intact` in
  `tests/cli_output_guard.rs` covers the survival property end to end and is
  confirmed to fail when `guarded_output` is pointed at the destination.
  What remains genuinely uncovered is the *single-input* commands: `sort`,
  `tags-filter`, `getid`, `getparents`, `time-filter`, `repack`, `degrade`,
  `renumber`, `add-locations-to-ways` all fail before their writer opens for
  every corrupt-input fixture available - a truncated PBF dies in the
  header-walk preflight, a corrupted blob payload dies in pass 1. Those are
  covered only for wiring. Closing that wants a `test-hooks`-gated
  fault-injection flag on the CLI, of the same shape as `apply_changes`'s
  existing `panic_at_blob_seq`. Confidence: high.
- `bench-merge`'s `--output` is the one output-producing command not routed
  through `guarded_output`. Deliberate: it is a synthetic write benchmark, and
  the guard would add a `rename` inside the region being measured. The
  consequence is that `pbfhogg bench-merge -o existing.pbf` still truncates
  `existing.pbf` up front and leaves a partial file if the run dies - the
  pre-guard behaviour, on a command no one points at real data. If the guard
  is ever wanted there, the honest shape is to commit *outside* the timed
  region rather than to wrap the whole run. Surfaced by the round-4 fix pass.
  Confidence: high, low urgency.
- Harness precedent, not a product bug: during round 4 a single
  `brokkr test` run reported `sort_failure_leaves_destination_intact` as
  "did not finish within 20s" with a `futex_do_wait` wchan snapshot, and the
  same test passed in well under a second on every subsequent run and every
  full `brokkr check`. Nothing in the test blocked, and the test has since been
  deleted (it could not bite). Read as a harness flake in the watchdog or the
  test-binary launch, recorded so a second sighting has something to match
  against rather than being investigated from scratch as a deadlock.
- `ReorderBuffer::with_capacity(N)` is called at ~20 sites with an `N` that is a
  tuning guess rather than the caller's true permit window - `time_filter` asks
  for 64 with no matching semaphore anywhere in the file. Round 4 added a
  `max_window` bound so the buffer can no longer grow without limit, but had to
  default it to a deliberately generous 65_536 for exactly that reason. The
  sites whose window *is* known (the `WRITE_AHEAD` writers,
  `read/pipeline`'s `decode_ahead`) could pin it exactly with
  `ReorderBuffer::with_max_window` and do not, because verifying each one is
  concurrency reasoning the round had no budget for and a bound set too low
  turns a memory-growth smell into a production panic. Confidence: high.
- `cli/src/main.rs`: the argument-splatting cleanup is partial. Round 4
  introduced `WriteOpts` (compression + direct_io + io_uring + force) and
  per-command flag structs for `getid`, `tags-filter`, `check`, `inspect`,
  `diff` and the geocode-index levels, which removes every run of three or more
  adjacent same-typed parameters - the class where a transposition compiles
  silently. What remains is length rather than hazard: `run_tags_filter`,
  `run_getid`, `run_getparents`, `run_removeid`, `run_extract`,
  `run_extract_config`, `run_diff`, `run_derive_changes` and `run_tags_count`
  still carry `#[allow(clippy::too_many_arguments)]` with 8-10 parameters of
  mixed types. Surfaced by the round-4 fix pass. Confidence: high, low urgency.
