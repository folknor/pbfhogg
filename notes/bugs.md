# Bugs

Open items left over from the apply-changes `--locations-on-ways` work of
2026-10-26. The defect itself (untouched base ways kept stale inline
coordinates) is fixed in `81f3fbe4`, with `check --locations` (`669a31e6`)
as its oracle and an osmium cross-validation test beside it; the measured
cost is in `reference/performance.md` "Inline-location refresh".

An entry is removed when resolved. When none remain, delete the file.

## Scanner holds way descriptors until EOF under `--locations-on-ways`

Once a way or relation descriptor enters `pending_post_barrier`
(`scanner.rs`), the scanner stops polling the readiness channel and
releases the buffer only at EOF, so publishing the coordinate map does not
promptly start way work. Sidecar markers on the `16e3694` runs: europe map
published at 8.5 s, scanner at its EOF barrier wait at 12.5 s; planet 9.6 s
and 20.2 s. Way work idles that long, and the buffer is unbounded. The
scanner's own comment says a bounded design needs an explicit barrier
change (wait at the first non-node descriptor after sending the boundary,
preserving late-boundary publication and failure shutdown). Anger 10: the
barrier is the most-tested part of the pipeline. Measure the idle time on
current code first.

## `check` overflow IDs cost 8 bytes per element

`check --refs` and `check --ids --full` collect every positive ID above the
bitset allocation into an overflow `Vec` (per phase in `refs.rs`, per blob
until the serial merge in `verify_ids.rs`) and insert them serially. A
compressible file holding billions of distinct IDs above the 14B floor can
cost tens of GB. A sizing change letting small files reserve less was
tried and reverted on 2026-10-26 because it moved this exposure below the
floor. A reviewer's proposed fix: the merge callbacks already run serially
on the consumer thread, so insert overflow IDs there into a separate
`IdSet` the workers never see and merge it after the phase (`set` then
`merge_from` for refs; `set_if_new` with duplicate counting for
verify_ids). Overflow memory then follows the ID range, not the element
count, and the small-file saving becomes safe. Unmeasured.

## The missing-node guard fires after the whole run

`apply-changes --locations-on-ways` knows its unresolved count at the
node-to-way barrier but refuses only after writing every blob (`rewrite.rs`).
The output is staged and discarded, so nothing breaks, but a planet run
spends its full wall before refusing. Failing at the barrier would need the
drain to carry the refusal through the failure path. Separately, the
error says "{count} way node references could not be resolved", where the
count is unique node IDs under apply-changes and per-reference under
add-locations-to-ways.

## Same-way fallback for unresolved refs

On a base without its untagged nodes, an OSC-modified way that keeps a ref
from its old version could take that ref's coordinate from the old
version's inline value, which sits in the blob the worker is already
rewriting. That would resolve many of the refs that make `-l` fail on such
a base (about 2.8M on a europe daily diff), but not refs a way newly adds,
and it carries forward any coordinate an earlier run left stale. It needs
its own resolution and accounting (the global `loc_missing` would still
count those IDs). Not decided whether it is wanted at all, given the docs
now require `--keep-untagged-nodes`.

## Performance-doc figures that do not match their runs

- `reference/pipeline.md` says the plain planet merge rewrites about 92%
  of blobs; the recorded plain-merge planet runs (`3ad57fc5`, `fda9f7a6`)
  rewrote 86% (43,670 of 50,816).
- `reference/performance.md` says those runs changed about 85 MB of
  output; their `bytes_rewritten` is 174 to 343 GB.
- `reference/performance.md`'s add-locations-to-ways section still
  describes the removed dense mmap index.

## brokkr side

Tracked as brokkr#43: whether `brokkr verify` can exercise
`--locations-on-ways` at all (it compares through `pbfhogg diff`, which
ignores inline locations by design), migrating the in-tree osmium check
there, and OSC file names that carry the download date rather than the
data date.
