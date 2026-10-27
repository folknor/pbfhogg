# Bugs

Open items left over from the apply-changes `--locations-on-ways` work of
2026-10-26. The defect itself (untouched base ways kept stale inline
coordinates) is fixed in `81f3fbe4`, with `check --locations` (`669a31e6`)
as its oracle and an osmium cross-validation test beside it; the measured
cost is in `reference/performance.md` "Inline-location refresh".

An entry is removed when resolved. When none remain, delete the file.

## Scanner holds way descriptors until EOF under `--locations-on-ways`

Once a way or relation descriptor enters `pending_post_barrier`
(`scanner.rs`), the scanner releases the buffer only at EOF (since
`8efb71bd` it polls the readiness channel per held descriptor, but only to
record the signal or stop on a closed channel), so publishing the coordinate map does not
promptly start way work. Sidecar markers on the `16e3694` runs: europe map
published at 8.5 s, scanner at its EOF barrier wait at 12.5 s; planet 9.6 s
and 20.2 s. Way work idles that long, and the buffer is unbounded. The
scanner's own comment says a bounded design needs an explicit barrier
change (wait at the first non-node descriptor after sending the boundary,
preserving late-boundary publication and failure shutdown). Anger 10: the
barrier is the most-tested part of the pipeline. Measure the idle time on
current code first.

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
