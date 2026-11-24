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

## CMD-052 - OSC readers accept undeclared entities and a misplaced DOCTYPE

Reported by: the XML 1.0 input character check (spar with codex).

The shared driver (`drive_osc`, `osc/xml_parse.rs`) now holds every event to
XML 1.0 `Char`, but other well-formedness rules still pass where no sink
looks. An undeclared named entity is refused only in an attribute a sink
normalizes: `<osmChange x="&x;"/>` (root attributes are never read) and
`<osmChange>&x;</osmChange>` (a named `GeneralRef` is ignored) are accepted,
while Expat refuses both. A DOCTYPE after the root (`<osmChange/><!DOCTYPE
osmChange>`) and a DOCTYPE with a malformed external identifier
(`<!DOCTYPE osmChange SYSTEM>`) are accepted too; quick-xml does not check
either. None can put a wrong value into a change, so this is parity, not a
silent wrong answer. A fix would refuse every non-predefined named
`GeneralRef`, scan the remaining attributes of each start tag for `&name;`
outside the five predefined names, and refuse a DOCTYPE outside the prolog.

## CMD-061 - OSC readers accept attribute lexical errors quick-xml does not check

Reported by: the OSC trailing-attribute fix (spar with codex).

Every admitted OSC start tag now has its attribute list walked to the end
with quick-xml 0.42's checked iterator, which refuses a missing `=`, an
unquoted or unterminated value and a duplicated key. It does not require
whitespace between attributes (`<tag k="a"v="b"/>` reads as `k` and `v`)
or check attribute name characters; Expat refuses both. Parity, not a wrong
value, unless a producer relies on the run-together form. A fix belongs in
the driver's per-event byte scan (`check_event_chars`), which already walks
every start tag, and must be measured on planet merge-changes and OSC
tags-filter.

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
`apply_changes/overflow.rs` and `stream_output.rs::settle`).

What recovery does not cure, each a loud refusal before publication. First,
a surviving base element is always written with its source block's whole
string table, so a survivor that grew past its source bytes (refreshed
`--locations-on-ways` coordinates on a long way), beside a large dead string
in the table, fails alone where its own strings would fit. The same seed
makes a blob alternating survivors and large OSC values fan out into many
table-sized frames; past `APPLY_CHANGES_OVERFLOW_REPLAY_BYTES` (256 MiB of
frames per blob) the worker refuses. Rebuilding survivors' tables from the
strings they use (remap their raw indices) on replay closes both. The
budget also refuses legitimate output: more than 256 MiB of large values
changed in one base blob, or one large role shared by many relation creates
that must be cut apart. Closing that means streaming a worker's recovered
frames to the drain instead of holding them until the rewrite ends. Second,
halving misses a partition that exists only because string index widths
shrink when two elements share a table, and `add_node_partial` writes a
metadata-less node dense, two bytes larger than plain, so a node at exactly
the payload limit fails. Both need blocks within bytes of 32 MiB.

## CMD-050 - Re-encoding commands cut blocks only by element count

Reported by: the CMD-049 implementer (2026-11-19); not verified per command.

apply-changes now cuts an output block early when
`BlockBuilder::content_over_payload_limit()` says its string content plus
serialized groups would pass the blob payload limit, and recovers an
overflowing block by replaying it in smaller pieces. The other `BlockBuilder`
users (cat re-encode, sort, repack, extract) still cut only by element count,
so two adjacent elements with large tag values can land in one block that the
writer then refuses as too large. The input is valid; the command fails. The
helper is available to them; adopting it needs a matched measurement per
command, since the check runs per added element.

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
