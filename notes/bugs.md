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

## brokkr side

Tracked as brokkr#43: whether `brokkr verify` can exercise
`--locations-on-ways` at all (it compares through `pbfhogg diff`, which
ignores inline locations by design), migrating the in-tree osmium check
there, and OSC file names that carry the download date rather than the
data date.
