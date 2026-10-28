# Bugs

Open defects and residuals. The first entry is left over from the
apply-changes `--locations-on-ways` work of 2026-10-26. That defect itself (untouched base ways kept stale inline
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

## Ordered pool internal panic still lingers behind parked waits

Residual of the altw stage 4 worker-panic fix (`64e478f8`). Stage 4
workers and their setup now hold an `AbortOnDrop` guard, so a panic in
caller-supplied worker code aborts the coord-payload router at once. A
panic inside the ordered pool's own machinery (admission, the result send)
is outside those guards: the gate closes, but a worker parked in
`wait_ready` for a way blob slot stays until stage 3 publishes it or
observes cancellation. That would be a pool bug, not caller code, and the
Panics section of `src/ordered_pool.rs` still states the general
behaviour. The pool-level alternative, treating a task panic as an ordered
failure that runs `abandon`, was rejected in that fix because `abandon`
would still wait on earlier tasks parked on the same router. Not decided
whether anything beyond the documentation is wanted.

## Node scanner refuses granularities every other reader accepts

Residual of the exact-scaling change (2026-10-28) that made the node
scanner (`src/scan/node.rs`, behind add-locations-to-ways and apply-changes
`--locations-on-ways`) accept every coordinate whose exact value fits an
`i32` decimicrodegree. Its granularity bound still differs: it refuses
granularity outside `1..=MAX_NODE_GRANULARITY` (1e9, `src/tunables.rs`),
deliberately since `36a405dc`, while the block decoder, `check --index` and
the element accessors take any `int32`. So a block with granularity 0, a
negative one, or one above 1e9 reads in extract, cat or inspect and is
refused by altw and `apply-changes -l`. The apply-changes way-location
refresh (`refresh.rs`) takes the block's granularity unbounded, so the two
`-l` paths disagree with each other too. No real producer emits such
values. Open: keep the bound as a vouching path's choice and say so beside
the refresh path, or drop it now that the scaling is exact (granularity 0
then scales every node to the offset). Low priority.

## Vouching paths accept coordinates outside the geographic domain

Also from the 2026-10-28 scaling work: the node scanner and the refresh
path bound only the `i32` decimicrodegree range, so latitude 90 degrees
plus one nanodegree (or about 214 degrees) passes them, while
`check --locations` (`src/commands/check/locations/scan.rs`) judges the
exact value against +-90 / +-180 degrees and reports it. The scanner's
doc says it refuses values that would be "a plausible coordinate
somewhere else on Earth"; an out-of-domain value inside `i32` is not
refused. Open: whether the vouching paths should share the domain rule.
