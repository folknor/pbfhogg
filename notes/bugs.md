# Bug findings

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.
```

Defects turned up by the 2026-11 performance hunt (see `notes/performance.md`
for the scope names). Where the 2026-11-17 defect hunt reported the same
mechanism, its account is appended to the entry under a "Defect hunt
2026-11-17" paragraph naming its scopes; its other findings are in
`notes/bugs-format.md`, `notes/bugs-commands.md`, `notes/bugs-geocode.md`,
`notes/bugs-tooling.md` and `notes/bugs-docs.md`. They are as reported, read from source, not reproduced
unless an entry says so, and some hunters marked their own finding as a code
reading or an API hazard rather than a demonstrated wrong result; that label is
kept. "Reported by" is a fact about the reports. Entries are in no order of
importance.

## BUG-006 - cat --clean drops inline way locations

Reported by: encoding, sort, spatial.

`cat --clean` (`cat_filtered` and its `process_block` in `cat/mod.rs`)
re-encodes every way without its inline coordinates and drops a declared
`LocationsOnWays` with the loss warning, although it writes each way whole,
the shape whose filters now keep them through `owned::WayCoords`. Keeping
them is the sweep's rule (every input declares), but that rule refuses a way
with refs and no coordinates, and `cat` is the documented recovery command
(its `--clean user` / `--clean timestamp` exist so a broken file stays
rescuable), so whether `--clean` may refuse such a way, or must instead drop
the declaration for that run, is a maintainer decision. The other droppers are
deliberate: time-filter (not as of the cutoff), degrade (a fault tool),
apply-changes without `--locations-on-ways` (cannot refresh them).

## BUG-028 - Geocode query bench can be optimized away and measures warm uniform points

Reported by: geocode.

`crates/bench/src/geocode.rs` measures serial queries over a repeated, warmed,
uniform-in-bbox set, so it says nothing about cold pages, address-heavy load or
CLI throughput, and its optimizer sink consumes only `admin.len()`.

Defect hunt 2026-11-17 (bench): both loops (`crates/bench/src/geocode.rs`)
discard address, street and interpolation results and black-box only the
accumulated `admin.len()`; with fat LTO those computations are unprotected
against elimination. Not established that elimination happens; the hunter
labels it a benchmark-validity concern, not a confirmed timing defect.
Separately, the geocode-query script advertises `--hotpath` and `--alloc`, but
neither the instrument nor `Reader::query` and its search helpers carries
profiling scopes, so those modes measure only the process wrapper.
