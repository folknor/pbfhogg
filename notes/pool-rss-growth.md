# Pool-heavy RSS at planet: what remains (transient)

Status: transient note, 2026-10-30. The shared mechanism this note was
written about is fixed: decode buffers are sized from the blob's
`raw_size` with exact growth and recycled by the per-worker pools (zero
pool drops at planet), and the CLI's `cross_thread_alloc_pool` hook pins
`M_MMAP_THRESHOLD` at 32 MiB beside the arena cap. Numbers and baselines
are in reference/performance.md, "Decode buffers sized from the blob and
recycled"; the table corrections are in notes/pre-release.md. What follows
is what is left.

## Rulings not to reopen

- The old peaks were glibc retention of per-blob decode buffers, not live
  decode storage (mallinfo2 mid-run in getparents: ~half the arena free).
  Raising the pool cap alone made it worse (754 MB): the 4x-compressed
  reservation must go with it.
- The per-buffer retention cap is a cache policy (16 MiB), not a format
  bound: payload plus inline tables can exceed 32 MiB on table-heavy input.
  The 256 MiB budget is per pool, idle capacity only.
- The threshold pin belongs in the host hook (ADR-0016), not the library.
  It is not a memory bound; it trades returning freed pages for not
  faulting them in again.
- The getparents, check `--ids` and time-filter row growth against the
  README was worker count (same MB per worker at 22 and 30 workers); the
  extract rows compared different bboxes; multi-extract was ~4 %.

## Remaining

1. **apply-changes: closed at 2.36 GB** (per-block framing; see
   reference/performance.md). What is left resident is the overlay
   (575 MB, never reclaims superseded records), the drain reorder (415 MB
   high water, no admission bound) and the 256-slot drain channel, whose
   occupancy no counter measures. The pwrite queue claim was wrong: the
   1.8 MB p50 is passthrough input frames only (the `blob_sizes`
   histogram skips rewritten output); rewrite ops average 146 KB framed.
   Do not request the allocator hook here: measured at twenty times the
   wall (entry 3).
2. **cat `--clean` (994 MB vs README 750 MB).** Peak is now in the
   relations phase; the 64-task window holds framed vectors, and each
   worker builds a fresh `BlockBuilder` per blob and copies frames with
   `into_vec`.
3. **Hook coverage by ownership pattern.** apply-changes is settled:
   not requested, recorded in the hook's doc (2606 s vs 125 s, kernel
   bound, mechanism unverified - worth understanding, since the same
   policy serves the other pool commands). getparents, extract,
   tags-filter and inspect do not call
   `cross_thread_alloc_pool`, though some free on another thread than they
   allocate (writer-pool framing). They showed no regression, but per the
   hook's stated condition the call should follow the ownership pattern,
   not benchmark luck. Settle per command.
4. **check `--ids` ways phase (566 MB vs nodes 301 MB).** Each of the three
   phases spawns fresh workers and pools; the nodes phase's buffers are
   freed before the ways phase builds new ones. Not investigated.
5. **Not re-measured:** check `--ids --full`, extract `--complete`,
   multi-extract `--simple` (same decode path; set-dominated peaks).

## Proposed chain (out of release scope)

- **P2, encode-side recycling.** Encode into reusable worker storage,
  compress each block as it completes, frame into output storage with a
  bounded return path from the writer. `BlockBuilder::take_owned` takes
  `encode_buf`, `FrameScratch::take_with_capacity` allocates a replacement
  per handoff and `into_vec` copies the frame, so keeping a builder per
  worker alone recycles nothing. This makes the library's throughput
  independent of the host's allocator policy (embedded callers with
  `NoProcessActions` currently pay the mmap churn the hook avoids).
- **Separate offset-table workspace.** Keep the inline string and group
  tables out of the payload buffer, so the payload never relocates and a
  buffer's size is the payload's.
- **Byte-budgeted fused task engine** (decode, transform, encode, frame
  under one budget with completion envelopes, leases released at storage
  reuse). Makes transient memory a chosen constant independent of worker
  count; also the shape that bounds the apply-changes drain.
- **Projection scanners** for check `--ids` and getparents (IDs and refs
  without a full `PrimitiveBlock`).
- **Membership track** for the set-dominated rows (fewer coexisting sets,
  release after last use, shared region membership in multi-extract).
- **Parallel compression for multi-extract** under one shared budget
  (write phases run at 2-3 cores).
