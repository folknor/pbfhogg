# `apply-changes --locations-on-ways` - remaining optimization items

Target: `pbfhogg apply-changes --locations-on-ways` on planet with a
daily OSC.

This doc used to hold the full measurement history, review-round
synthesis, and plan-of-attack for the apply-changes rewrite
(descriptor-first streaming pipeline, worker-emits-framed, parallel
reader, parallel prefill, classify instrumentation, writer-backend
matrix). All of those landed. Archaeological content lives in git
history - `git log --oneline -- notes/apply-changes-opportunities.md`
surfaces the pre-trim versions.

## Current state (2026-04-21)

- **Best planet wall: 80.9 s** (`--compression zstd:1` on scratch
  pointing at a physically separate NVMe; parallel writer, which is
  now the default). Pre-P1 baseline was 144.4 s, so the rewrite + all
  follow-ups delivered -44% wall.
- **Parallel writer is the default** for apply-changes as of
  2026-04-21. Flag `--parallel-writer` removed; buffered path dropped.
  `--io-uring` and `--direct-io` remain as opt-in backends.
- **Architecture landed**: descriptor-first scanner + long-lived worker
  pool + single drain actor with byte-budget reorder + `copy_file_range`
  coalescing + worker-emits-framed + prefill fusion. See
  `src/commands/apply_changes/{scanner,streaming,drain,rewrite}.rs`.
- **Cross-validation**: denmark byte-equal vs pre-P1, 6/6 property tests
  in `tests/apply_changes_invariants.rs`, 18/18 integration tests in
  `tests/merge.rs`.

## Remaining open items

All three are low priority; the command is comfortably inside any
realistic production budget at 80.9 s.

### #11 - Splice-in-place for low-touch rewrites

For `NeedsRewrite` blobs with ≤K affected elements (K~64), splice the
raw wire bytes for unmodified element runs instead of full decode +
re-encode. Estimated ~1.5-2 s wall at daily. Raw-group passthrough
scaffolding lives in
[`src/write/raw_passthrough.rs`](../src/write/raw_passthrough.rs).

**Deferred.** The planet wall is now writer-bound at `--compression
none` (`writer_write_ns` ~64 s under io_uring, ~94 s under parallel
writer at zstd:1). Splice-in-place saves classify + rewrite CPU but
does not reduce output bytes, so it's unlikely to move the wall on any
current target. Revisit if the writer ceiling moves.

### #13 - Exact-membership metadata / sidecar

Current on-disk metadata gives per-blob ID range only; pure creates
inside an existing blob range force slow-path decode (the documented
FalsePositive case at
[`src/commands/apply_changes/classify.rs`](../src/commands/apply_changes/classify.rs)).
At planet today: 15,224 FalsePositive blobs / 92,677 slow-path blobs =
16% of slow-path work burned on blobs that turn out not to overlap.

Not negligible, not headline. A format/index project, not a quick
cleanup. Two shapes: (a) a wire-format exact-overlap scanner on
decompressed bytes (skips full parse for FalsePositives); (b) a per-blob
membership sketch in indexdata (rejects FalsePositives without
decompress at all).

### #15 - Document zstd:1 as the internal-pipeline recommendation

Measured at planet: `--compression zstd:1` delivers 80.9 s (best) and
121.2 s (same-disk) vs 135.5 s for `none` and 143.7 s for `zlib:6`,
because workers parallelize zstd cheaply and the ~20% output-byte
reduction relieves the writer bottleneck.

Already gated behind the `--compression` flag; the remaining work is
documentation: update
[`reference/performance.md`](../reference/performance.md) and README
to recommend `--compression zstd:1` as the default for pbfhogg-internal
pipelines (consumers that don't require osmium interop). zlib:6 stays
the library default for ecosystem compatibility.

## Open questions

Items resolved by measurement are closed; these are the remaining live
ones.

- **What's the actual overlap-blob ratio at planet under different OSC
  sizes?** Plan estimated ~8% for daily; confirmed post-P1. Needs
  re-measurement if apply-changes starts being called with substantially
  larger input diffs, because it governs load on the worker pool. At
  20% the worker pool needs more cores; at 4% the scanner's fast-path
  dominates and the worker pool is mostly idle.
- **What's the right initial value for the byte-budget reorder
  capacity?** R2 suggested ~128 slots + byte permits. If under-sized,
  workers block on slow rewrites; if over-sized, RSS spikes during
  straggler tails. Current setting is working at planet; revisit if
  RSS or stall counters tell us to.
- **Does the scanner's HeaderWalker keep up with worker throughput?**
  At 1.37 M headers on planet, walker emits one descriptor every
  ~15 µs; worker pool consumes ~22 candidates per ~1 ms. If walker
  emits faster than workers consume, dispatch backs up and we apply
  backpressure to scanner (fine). If walker is the slow side, workers
  idle (problem). Measurement question; no signal from current runs
  that it's an issue.
