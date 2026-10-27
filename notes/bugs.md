# Bugs

Open items left over from the apply-changes `--locations-on-ways` work of
2026-10-26. The defect itself (untouched base ways kept stale inline
coordinates) is fixed in `81f3fbe4`, with `check --locations` (`669a31e6`)
as its oracle and an osmium cross-validation test beside it; the measured
cost is in `reference/performance.md` "Inline-location refresh".

An entry is removed when resolved. When none remain, delete the file.

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
