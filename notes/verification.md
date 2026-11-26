# Verification findings

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

Findings are as reported by the hunters and have not been verified. Hunters read
source; costs are derived from the code unless an entry says measured. "Reported
by" records which scopes raised a finding and is a fact about the reports, not a
verdict on the code. Entries are in no order of importance.

## VER-018 - Four hand-rolled caller-pin cells run buffered only

`pinned_cell::check` runs a caller-pin arm on every driver cell in both read modes,
so the remaining hand-rolled caller-pin cells are the ones it cannot express, and
they run buffered with no transport list: `diff` and `derive` with a caller pin
against the same path (`a_caller_pin_is_used_as_it_is` in `fault_diff_input.rs` and
`fault_derive_input.rs`, whose second call owes four verified opens over two inodes),
`every_check_uses_a_caller_pin_as_it_is` (the `check` index and refs entries, which
no driver cell runs; refs owes one buffered open even on a direct run) and the
public `getparents` entry (`fault_getparents_input.rs`, where the driver runs
`getparents_with_min_blobs`). Closing it means either driver cells for the check
index and refs routes and the public getparents entry, or a direct leg with an exact
list in each hand-rolled cell. The getid full-scan unit cell in
`crates/lib/src/commands/getid/mod.rs` is a reference-and-matrix cell outside the
driver with its list asserted by hand; it has no caller-pin arm.

## VER-162 - The reader's exact half-turn tie rule is not certified

`pbfhogg-verify geocode` models a street or interpolation segment spanning exactly half a turn
of longitude as both half-world paths, each a candidate with its own snap, and accepts either
when their distances tie within the tolerance. The reader breaks an exact tie by the path running
east from the lesser `(lon, lat)` endpoint (`segment_distance` in the geocode reader), and no
check holds it to that rule: an answer taking the other path at a tie passes. Certifying it needs
a query constructed on an exact tie (the `geocode-seam` fixture is the natural home) and a
judgement of the path, not only of the distance.
