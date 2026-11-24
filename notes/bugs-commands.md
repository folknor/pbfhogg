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

## CMD-053 - renumber's group policy for non-element fields differs by kind

Reported by: the renumber representation item (2026-11-20), while sparring
plain-node support; not reproduced.

`renumber/wire_rewrite.rs` rebuilds node groups (dense and plain) and
relation groups from their element messages only, silently dropping every
other `PrimitiveGroup` field, including field 5 (`changesets`, a known
message the index scanner ignores), while way groups copy such fields
through verbatim. No producer pbfhogg reads writes them, but the three
rewriters disagree on one input shape. Pick one rule (refuse a `changesets`
field in any data blob, or keep unknown fields everywhere) and apply it to
all three.

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

## CMD-039 - check --refs passes an unreadable leading header payload

Reported by: shared-primitives (the hunter explicitly does not count this as a
confirmed defect).

`consume_leading_header` (`scan/classify.rs`) checks the frame's type but
never decodes its body, and `check_refs` (`check/refs.rs`) does no separate
header decode. A correctly framed `OSMHeader` with the one-byte payload `0x80`
and no data blobs reaches a successful empty reference report. `--refs`
promises only referential integrity, so its success does not certify that the
header is readable.

## CMD-041 - renumber still refuses a few blocks its rewrite grows past the limit

Reported by: the negative-ID renumber item (2026-11-18); narrowed
2026-11-24 after the split path landed (`renumber/split.rs`: a rewritten
block over the payload limit is cut between elements by an exact
cut-point search, each piece with its own index).

What the split does not cure, each a loud refusal before publication.
First, every piece repeats the input block's whole string table, so one way
or relation that fits with only its own strings but not beside the whole
table is refused (remapping a piece's string indices would close it).
Second, a `DenseNodes` message is indivisible: a one-group node block within
about ten bytes of 32 MiB whose first new ID is wider than its old one
outgrows the limit (cutting it means rebasing every delta column at a node
boundary). Third, the limit is checked on raw bytes; a near-limit block
whose zlib or zstd body expands to 32 MiB is refused by the writer, and
renumber cannot retry because the writer frames off the consumer thread.
Each of these refusals prints the library's "fewer elements per blob" text,
and renumber has no such flag (`element_cap_advice` in the CLI answers only
repack and degrade).

## CMD-049 - apply-changes still refuses a few elements that fit a block

Reported by: the derive OSC domain item (2026-11-19); narrowed 2026-11-19
after overflow recovery landed (an overflowing output block is re-cut,
`apply_changes/overflow.rs` and `stream_output.rs::settle`), and again
2026-11-24 after the replay's blocks began carrying only the strings their
survivors use, with the whole-table replay as fallback.

What recovery does not cure, each a loud refusal before publication. The
replay budget (`APPLY_CHANGES_OVERFLOW_REPLAY_BYTES`, 256 MiB of frames per
blob, per strategy) refuses legitimate output: more than 256 MiB of large
values changed in one base blob, or one large role shared by many relation
creates that must be cut apart (each piece repeats the role). Closing that
means streaming a worker's recovered frames to the drain instead of holding
them until the rewrite ends; `DrainItem::Rewritten` carries one blob's
frames as one item today, so this needs an ordered frame stream or a spill
behind that item, delivered without letting later sequences block the next
one. Second, halving misses a partition that exists only because string
index widths shrink when two elements share a table, and `add_node_partial`
writes a metadata-less node dense, which the entry's reporter put at two
bytes larger than plain (not measured), so a node at exactly the payload
limit fails. Both need blocks within bytes of 32 MiB.

## CMD-050 - History time-filter and the sort/dedupe sweep can refuse frameable input

Reported by: the CMD-049 implementer (2026-11-19); narrowed by its spar
(2026-11-24).

Both pack records from many input blobs into one `BlockBuilder` with no
user-set cap, cut by count only, so input whose blobs all frame can build an
output block past the 32 MiB payload limit at the default 8000 cap: for
example 1200 selected relations of 10000 members each (about 36 MB of member
arrays) spread over input blobs of 600. The writer refuses it, typed, exit 2,
nothing published, and no flag or repack of the input helps. Fix shape:
recover on the typed payload or body overflow and re-emit the block's records
in smaller pieces, as apply-changes does. Neither path retains them today:
history time-filter drops each record once written, so it must hold the
current output block's records and catch the overflow before the block
reaches the writer pipeline; the sweep drains one ID group at a time while a
block spans many, and retaining `Held` records can extend decoded source
blobs' lifetimes, which the memory design must account for. Cover the final
flush too.

Ruled, not open: no byte threshold in `can_add_*` or the
`ensure_*_capacity_local` funnel (any threshold is beaten by the next
element, `content_len` is a lower bound, and repack's and degrade's layouts
are count rules the verifier and the unsort modes depend on); repack and
degrade refuse an over-limit block by design. Known limitation, below the
fix above: input-blob-local re-encoders (cat re-encode, snapshot
time-filter, tags-filter, extract, getid, getparents) have no cross-blob
growth, but carry no byte guarantee either: a rebuilt string table can give
common strings wider indices, and a codec change can push an unchanged
payload's body past the limit.

## CMD-066 - add-locations-to-ways writes one block per way blob, however large

Reported by: the CMD-050 spar (2026-11-24).

Both indexed engines (`altw/passthrough.rs` `decode_one`, and
`altw/external/stage4.rs`) emit the reframed way payload as one block per
input way blob, and the unindexed decode-all fallback (`altw/mod.rs`) adds
locations through a count-cut `BlockBuilder`. Adding coordinates grows a
block: 8000 ways of 1200 refs with metre-scale deltas carry about 38 MB of
location arrays alone, so a frameable input blob comes out refused (exit 2)
with no cap flag to lower. Fix shape: cut the finished payload between ways,
reusing renumber's wire partition (`renumber/split.rs`), deriving each
piece's ID range from the ways it holds (IDs keep their gaps), slicing the
`WayMembers` bitmap and per-way pin data with them; no extra scan for a
block that frames. The guarantee is scoped to pieces that fit with the
repeated string table, and body-limit and `HeaderWayMembers` overflows need
the same treatment if the fix promises recovery generally.

## CMD-067 - The oversize refusal advises fewer elements per blob where no flag exists

Reported by: the spar accounting of the block-size items (2026-11-24), from
the CMD-050 and CMD-049 scouts' laterals.

Every `BlobTooLarge` refusal (`OversizePart` messages in `error.rs`) ends in
"fewer elements per blob shrink it", and the CLI's `element_cap_advice`
(`crates/cli/src/lib.rs`) adds a flag only for repack and degrade. On every
other command that can refuse an oversize output block (time-filter, sort,
`cat --dedupe`, add-locations-to-ways, cat re-encode, tags-filter, extract,
getid, getparents, apply-changes) the user is told to use fewer elements per
blob and has no way to. apply-changes makes it worse when its overflow replay
budget (`APPLY_CHANGES_OVERFLOW_REPLAY_BYTES`) runs out: the replay returns
the first block's original oversize error (`apply_changes/overflow.rs`), so
the message names neither the budget nor anything the user can change.
renumber's case is recorded in its own entry above. A fix would keep the
library text free of advice the caller cannot act on (the CLI already adds
the flag where one exists) and give the budget refusal its own wording.

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
