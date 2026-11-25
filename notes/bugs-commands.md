# Command defects

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

Defects from the 2026-11-17 defect hunt in the library commands
(`crates/lib/src/commands/`) and the OSC module. Every finding was made by one
codex hunter per scope reading source at `63d4ec1d`. None was built, run or
reproduced. "Reported by" names the scope or scopes, and the bracketed
priority is the hunter's own label. Findings that match an existing mechanism
were added to `notes/bugs.md` instead of being filed here: tag truncation
(since fixed: block validation refuses a tag string index past the table),
apply-changes base version precedence (since fixed: a change set older
than the base element is refused), plain nodes in
renumber (since fixed: renumber rewrites plain node groups), degrade drop
counting (BUG-022) and cat decode counters (since fixed: cat counts
each blob's fate and the payloads it opened). Entries are in no
order of importance.

## CMD-068 - OSC readers do not check the XML declaration's pseudo-attributes

Reported by: the OSC lexical-checks item (spar with codex).

The driver (`check_markup` and `drive_osc` in `osc/xml_parse.rs`) refuses an
XML declaration that is not first, but never reads its body:
`<?xml?>`, `<?xml encoding="x"?>` (no `version`), a misordered or
unknown pseudo-attribute, or `standalone="maybe"` are read, where Expat
refuses them. Parity only: input is decoded as UTF-8 whatever the
declaration says. quick-xml's `BytesDecl` offers `version()`,
`encoding()` and `standalone()`; a fix on the rare `Decl` event costs
nothing on normal input. A BOM split across short first reads is also
missed by quick-xml's BOM detection before this check runs.

## CMD-069 - OSC readers ignore the root's version, which libosmium requires

Reported by: the OSC lexical-checks item (scout lateral, not reproduced).

Root attributes are only lexically checked, so `<osmChange>` with no
`version` or `version="0.7"` is read. libosmium's `top_level_element`
(`xml_input_format.hpp`) throws `format_version_error` unless `version`
is `0.6`. DEVIATIONS.md "OSC input: the root level is stricter than
libosmium" does not record this. Either refuse a missing or other version
or record the deviation.

## CMD-070 - A malformed object-tag attribute is reported without its element

Reported by: the OSC lexical-checks item (scout lateral).

On `tag`, `nd`, `member`, the root, a section and `bbox`/`bounds` a
malformed attribute reads "malformed attribute on <ELEMENT>"
(`malformed_attr`), while on a `node`, `way` or `relation` start tag it is
reported through `scan_meta_attrs` / `delete_coords` as a bare "malformed
OSC XML" (`xml_err`), naming neither element nor attribute. The CHANGELOG
entry for the trailing-attribute fix promises the first wording generally.

## CMD-074 - renumber could keep SharedNodePins-v1 it already carries

Reported by: the renumber LocationsOnWays item (2026-11-24), spar
session 01a20ea3-2302-7893-b81b-e3d577f46209.

Renumber now keeps `LocationsOnWays` (`Ways::ReencodedWithLocations`) but
drops and warns `pbfhogg.SharedNodePins-v1`, although its way splice
copies field 20 byte for byte and the bitmap's meaning survives: live IDs
map by rank onto one interval and orphans bijectively onto its
complement (`IdSpace` in `renumber/wire_rewrite.rs`), so two ref
positions name one node after renumbering exactly when they did before,
and no node is dropped. `WayMembers-v1` must still go (field 5 is not
written). Keeping pins needs an amendment to ADR-0007's producer rule
(prepass features only on whole-blob copies from one input) and a header
test that tracks pins through the way-ID mapping and expects pins and
members separately. Maintainer decision: is a renumbered prepass file
worth declaring pins on?

## CMD-024 - Polygon ring-area validation runs before antimeridian unwrapping

Reported by: extract (flagged as a surprising policy, not counted as a defect).

Ring-area validation (`extract/polygon.rs`) runs before antimeridian
unwrapping. `[(179,0),(-179,2),(180,1),(179,0)]` passes validation although
its unwrapped geometry is collinear. Conversely, some unwrapped triangles with
nonzero area are refused because their wrapped coordinates have zero area. The
recorded raw-ring refusal policy explains the second case, and the hunter did
not count widening the accepted set as a defect.

## CMD-058 - Export writes LineStrings whose emitted positions all coincide

Reported by: export polygon-degeneracy work [P3].

`write_way_geometry` (`export/geometry.rs`) rejects a LineString only below two
input positions. A way `[(0,0),(0,0)]`, or one whose distinct positions all
round to one seven-decimal position, is written and counted as exported.
RFC 7946 asks for two or more positions, which such a line has, so whether it
is invalid is a policy question; libosmium's `use_nodes::unique` would
collapse it and refuse it. Polygons are already judged on emitted area.

## CMD-038 - export --metadata removes tags that collide with no emitted property

Reported by: export [P2].

`export/properties.rs` reserves all six metadata names whenever `--metadata`
is on, whichever metadata fields exist. For a node without metadata but with
the OSM tag `@user=survey`, plain export keeps the tag, while `--metadata`
removes it and emits no replacement `@user`. This contradicts ADR-0010's
emitted-name rule. If reserving the whole namespace is intended, the
documented rule should say so.

## CMD-050 - Peak memory of the oversize cut on oversized input is unmeasured

Reported by: the CMD-049 implementer (2026-11-19); narrowed by spars
(2026-11-24, 2026-11-25); the last made the cut refuse a block it finds no
plan for on the spot, located at its input.

Every re-encoder cuts an oversized block (`commands/split.rs`), but no run
has measured peak memory on input that triggers the cut: a first cut repeats
the block's string table in every piece, and a queued worker task holds all
of its pieces until the consumer writes them. Next: an input that forces the
cut on many blobs, and the peak recorded in
`reference/performance/cross-command.md`.

Ruled, not open: no byte threshold in `can_add_*` or the
`ensure_*_capacity_local` funnel; repack and degrade refuse an over-limit
block by design; a repaired payload `cat --repair` frames under its input
header is not cut (comment in `cat/repair.rs`); `flush_or_overflow` stays
strict on an opted-in sink; the cut's search is sized by a monotone upper
bound and may miss a partition, ending in the same typed refusal.

## CMD-060 - degrade --unsort may have slowed down, and is slow on planet

Reported by: the CMD-056 follow-up measurement (2026-11-21, plantasjen).

`3ce0176a` changed `degrade --unsort` and `--unsort-intra` to move whole ID
runs (`degrade/mod.rs`). Its denmark A/B was noise, but germany
`perturb-pbf --unsort yes`, interleaved `--iterations 1`, was slower in both
pairs: `9bbd64df` 60.6 s (`7f54472a`) and 63.9 s (`f6699714`) against
`3ce0176a` 65.1 s (`5898d9cf`) and 65.8 s (`b41d44ae`), +8 % and +3 %. In
the second pair the node phase was equal (25.0 s) and the whole gap sat in
the way phase (36.4 s against 38.2 s at 4.6 cores both, the change cell
writing about 110 MB more), so drive state is not ruled out. Separately, a
planet `--unsort yes` cell on `3ce0176a` ran past 12 minutes before it was
stopped, where planet `degrade --strip-locations` and repack run 150 to 165
s (`reference/performance/repack-degrade.md`). No planet `--unsort` baseline
has ever been recorded, so whether that wall is the change, the mode, or the
host is unknown. Next: one planet cell at `9bbd64df` to establish the mode's
baseline, then a matched pair; if the change is the cost, it comes out or
is made cheaper.
