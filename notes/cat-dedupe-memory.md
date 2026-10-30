# cat --dedupe planet memory regression: research note

Transient research note (notes/, no truth guarantee). Written 2026-10-30
at HEAD `42ea567f`; trimmed the same day after the borrowed-cursor sweep
landed (`src/commands/blob_merge/sweep.rs`), which resolved the regression:
planet merged with itself went from 53m43s / 7.57 GB peak anon (`d271f04f`)
to 5m10s / 1.46 GB on bygg (reference/performance.md). What landed: the
owned-element heap replaced by a k-way merge over borrowed cursors, equal-ID
groups ordered by version with input precedence for duplicates, worker-side
framing in byte-bounded chunks, sparse-tail demotion, and the arena cap.
What remains below is only what that change did not address.

## Remaining ideas

### R1. Collapse byte-equal twin frames before planning

For entries with equal kind, range and frame length across inputs, verify
payload byte equality and collapse to one representative (also recognise
the same file passed twice by identity). The representative then plans as
a lone entry and is copied through. Makes a self-merge a copy and keeps
way enrichments that a rewrite drops. Deliberately not done: it is
benchmark-shaped (two different snapshots share no frames), so it leaves
real dedupe inputs untouched. Semantics: internal duplicates inside a
passed-through twin survive, as they already do in any lone blob.

### R4. Streaming execution for giant dense components

A dense overlap component still pins the decoded blocks of its live
overlap (sparse tails are demoted to owned elements past
`SWEEP_PINNED_BYTES`, dense ones cannot be). A hard bound needs bounded
sorted runs plus an external merge, or an osmium-style per-source stream
merge with globally byte-bounded prefetch inside oversized components.
Only `sort` of adversarially unsorted input reaches this.

## Lateral findings (still open)

- L1 (writer): `PIPELINE_DISPATCH_PERMITS` are released when framing
  finishes, not at ordered delivery, and `ReorderBuffer::with_capacity(64)`
  in `src/write/pipeline.rs` `deliver_in_order` is an initial allocation,
  not an admission limit, so completed frames can pile up behind a slow
  earlier task. The dedupe path no longer frames through the writer, so it
  is not affected; every other pipelined writer user is.
- L2: `writer_reorder_high_water` measures a sequence span
  (`pending_len()`), not resident frames.
- L4: the README planet row describes the old serial engine on another host
  (5900X vs 9950X3D2).
- L5: pass 2 reads frames through buffered `InputFile` even under
  `--direct-io`.
- L6: no test runs the sweep at planet blob density; resource tests should
  assert bounded admission, not host RSS.
- L7 (new, from the landing): the per-run chunk bound is
  `window x depth x max(OVERLAP_RUN_CHUNK_BYTES, largest frame)`; a frame
  near the 32 MiB format limit (`--compression none` on a huge block) still
  travels alone and is not otherwise budgeted.
