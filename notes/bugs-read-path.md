# Bug hunt: read path (`src/read/`)

Hunt date 2026-08-12. Single Opus hunter, scope: `src/read/` in full plus
`src/reorder_buffer.rs` as consumed by the pipeline.

Everything found has been fixed and removed from this document except the two
entries below. The first is open pending a measurement; the second is recorded
so it is not re-filed.

## Open: `BlobReader::from_path` advises `POSIX_FADV_DONTNEED` over `0..offset`

The API half is closed. `BlobReader::evict_page_cache(bool)`,
`ElementReader::evict_page_cache(bool)` and
`ElementReader::from_path_with_eviction(path, bool)` give the public surface an
opt-out that covers the whole file, header blob included. The default is
unchanged.

What remains open is the syscall shape. The advised range is the whole consumed
prefix `0..current_offset`, re-issued per blob, so planet issues ~2.5 M
`posix_fadvise` calls each nominally covering a growing prefix where
`offset_prev..offset` would express the same intent over a bounded range. Kernel
side is cheap per already-evicted page, so this may well be worth nothing.

**Why this is not a change to make on inspection, and what closing it costs.**
The cost has never been measured, and the fix is not free to evaluate: changing
*which* pages get dropped changes the page-cache state that every subsequent
benchmark on the host observes, so the measurement perturbs the baseline it is
measured against. Closing this means an interleaved matched A/B at a scale where
the syscall count actually matters (planet, not Denmark), on a trimmed drive,
with the host's cache state accepted as spent for the duration. That is a
scheduled measurement with a real setup cost, not a one-line cleanup - which is
why it has survived four rounds untouched. Do not "fix" it without the numbers.

Confidence: high that the shape is odd; unknown that it costs anything.

## Not a bug: `ReorderBuffer::push(0)` on pool-build failure

Recorded so it is not re-filed. The original report has `run_pipeline` panicking
with "stale sequence number" if `ThreadPoolBuilder::build` fails after any item
has been delivered. It cannot: in both pipeline variants the pool is built
*before* the dispatcher's loop over `raw_rx` begins, so nothing has been
dispatched when the failure path sends `(0, Err(..))`, and seq 0 is legitimately
the first push. The magic seq is still a smell - the error path picks a value the
reorder buffer's contract constrains - but there is no reachable panic, and no
code change was made.
