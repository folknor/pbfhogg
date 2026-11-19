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
(BUG-004), apply-changes base version precedence (BUG-016), plain nodes in
renumber (BUG-020), degrade drop counting (BUG-022) and cat decode counters
(BUG-023). Entries are in no
order of importance.

## CMD-006 - load_all_diffs silently skips files it cannot stat

Reported by: osc-parsing [P2].

The directory scan (`osc/parse.rs`) uses `Path::is_file()`, which returns
false when the metadata lookup fails. A directory that permits listing names
but denies searching its children can hold numeric OSC files and still yield
an empty overlay with "Loaded 0 diffs". A metadata failure on one entry can
silently omit just that diff. This breaks the loader's promise to load the
matching files and its explicit protection against successful zero-diff
mistakes. Metadata failures need to propagate, separately from skipping
directories.

## CMD-007 - OSC output can contain characters XML forbids

Reported by: osc-parsing [P2], diff [P2].

`push_attribute_escaped` (`osc/write.rs`) handles XML punctuation and
whitespace, but neither it nor quick-xml's writer rejects XML 1.0 forbidden
code points, and PBF string validation checks UTF-8 only. A PBF tag containing
NUL or U+0001 reaches `write_checked_tags_xml` and is written literally, from
diff derive, tags-filter or any other PBF-to-OSC path. These characters cannot
appear in XML 1.0 even as character references, so conforming readers reject
the file. This breaks the promise that `--format osc` writes OSC XML. The
shared writer should refuse unrepresentable strings before publication.

## CMD-009 - --increment-version stops incrementing at i32::MAX

Reported by: osc-parsing [P2], diff [P2].

`write_delete_element` (`diff/derive.rs`) uses `saturating_add(1)`. Deleting
an element at version 2147483647 with the flag emits the same version and
reports success, despite the flag's documented promise to bump the version by
one. An unrepresentable increment should be refused.

## CMD-011 - Unindexed gzip derive ignores the -j worker cap

Reported by: diff [P2].

`assemble_osc` (`diff/derive.rs`) sizes its gzip workers from host parallelism
and never receives the requested `Parallelism`. When either input is
unindexed, `diff --format osc -j 1 -o changes.osc.gz` can start a compression
pool that uses every reported core. This contradicts the documented rule in
`docs/cli/commands.md` that `N` caps worker pools at `N`. These are
compression workers, not the separately exempted reader or drain threads.

## CMD-014 - ALTW refuses legal non-default way coordinate framing without documenting it

Reported by: altw [P2].

`check_default_coord_framing` (`altw/reframe.rs`), used by both indexed
engines, rejects any way block whose granularity is not 100 or whose
coordinate offsets are nonzero. An ordinary indexed way block with
`granularity = 1000` fails even when every referenced node exists, while the
forced decode-all sparse path (no indexdata) accepts it. This contradicts the
sparse input coverage documented in `docs/cli/commands.md`, and the
deliberate protobuf restriction is missing from `DEVIATIONS.md`. The refusal
does prevent wrong coordinates. The hunter says to either convert the framing
or document the refusal.

## CMD-015 - The ALTW way reframers drop an empty but present StringTable

Reported by: altw [P2].

The sparse reframer (`altw/reframe.rs`) and the external one
(`altw/external/stage4.rs`) both write the table with `encode_bytes_field`,
which omits empty payloads. An empty but present StringTable, with a tagless
way and honest indexdata, passes and is published without the required
`PrimitiveBlock.stringtable` field. A later ALTW run then fails its own
missing-StringTable check in `reframe.rs`. This breaks the PBF output
contract. Whether empty tables should be accepted or refused, turning presence
into absence and publishing is wrong.

## CMD-016 - External ALTW statistics leave out whole dropped node blobs

Reported by: altw [P2].

When stage 4 (`altw/external/stage4.rs`) finds an entirely untagged node blob
with no relation members, it continues without updating node statistics. A
file of only such nodes plus ways that reference them can report
`0 nodes read, 0 written, 0 dropped`, although coordinates were resolved from
those nodes and every node record was removed. Sparse reports the dropped
nodes. This breaks the meaning of `Stats::nodes_dropped` and of the printed
summary.

## CMD-017 - wait_ready emits a WAIT span before confirming it parked

Reported by: altw (lateral observation).

`wait_ready` (`altw/external/coord_payloads.rs`) starts its `WAIT_*` span
before confirming a Condvar park, so a publication racing the first check
emits a wait span without blocking. The nanosecond stall counter is correctly
gated, but the marker span is not. The repository's convention is to emit
`WAIT_*` pairs only on genuine blocks.

## CMD-018 - renumber rejects empty ways its own builder writes

Reported by: sort-renumber [P2].

`encode_way` (`write/encode.rs`) omits field 8 when `refs` is empty, but
`reframe_way` (`renumber/wire_rewrite.rs`) requires the field and returns
`reframe_ways: no refs field`. A sorted, indexed file produced with
`add_way(..., &[], ...)` cannot be renumbered. renumber is also inconsistent
with itself: it accepts an explicitly present empty refs field, its
replacement encoder then omits the field, and so its successful output fails a
second renumber. An absent repeated field must mean an empty list, as the
producer already assumes.

## CMD-019 - sort orders history versions only when blob layout forces a rewrite

Reported by: sort-renumber [P2].

`sort/mod.rs` and the parity table claim equivalence with osmium sort, and the
vendored `osmium-sort.md` orders equal IDs by ascending version. Take one
isolated history blob holding `way 7/version 2` then `way 7/version 1`, under
a header without the sorted claim. Pass 1 compares only IDs
(`blob_meta/scan.rs`), finds no descent and leaves the blob eligible for
passthrough, so sort succeeds with the versions still descending.
`--distrust-metadata` does not fix this. If the records sit in overlapping
blobs, or an unrelated ID inversion forces a rewrite, the sweep
(`blob_merge.rs`, `blob_merge/sweep.rs`) emits version 1 before version 2. The
result depends on blob boundaries and unrelated disorder, because
classification uses a weaker ordering than the rewrite path. The statement in
`reference/indexdata-trust.md` that sort does not reorder versions is also
wrong, since it sometimes does.

## CMD-020 - Default tags-filter treats every occurrence of a matched ID as a match

Reported by: id-filter-selection [P2].

Pass 1 stores matching IDs in sets, and `filter_block_pass2`
(`tags_filter/mod.rs`) then treats every occurrence of a stored ID as a direct
match without examining its tags. A sorted history input with
`n1/v1 amenity=cafe` and `n1/v2 amenity=school`, and no references, outputs
both versions for `n/amenity=cafe`, both counted as directly matched, and
`--remove-tags` keeps the non-matching version's tags. For ways: if matching
`w10/v1` references `n1` and non-matching `w10/v2` references `n2`, both ways
are written but dependency collection gathers only `n1`. The CLI contract in
`docs/cli/commands.md` states no uniqueness or history restriction, while
osmium explicitly limits correct history filtering to `-R`. The hunter asks
for either a refusal with a documented restriction or occurrence identity kept
throughout. This does not require adding history support.

## CMD-021 - time-filter treats an absent dense timestamp as the epoch

Reported by: id-filter-selection [P2].

`dense_timestamp` (`time_filter/mod.rs`) checks whether the group has a
timestamp column and then returns the slot's value, without checking whether
that node claims a timestamp. A dense node with an absent or sentinel version
and timestamp zero is unclaimed padding under the metadata rules in
`CORRECTNESS.md`, and `DenseNodeInfo::claims` (`read/dense.rs`) correctly
reports no timestamp for it. Even so, a sorted snapshot containing that slot
passes `time-filter` at cutoff 250 and emits the node as predating the cutoff.
The documented claim is that elements without timestamps are refused rather
than treated as zero. The accessor needs per-slot presence semantics.

## CMD-022 - extract --directory can publish outside the override root

Reported by: extract [P2].

Rebasing (`extract/mod.rs`) keeps relative paths verbatim and joins them under
the override. With `-d /work/results`, an output named `../escaped.pbf`
becomes `/work/results/../escaped.pbf` and is written to `/work/escaped.pbf`.
Absolute paths under the old root can also keep escaping `..` suffixes. This
contradicts the explicit guarantee in `DEVIATIONS.md` that every configured
output stays under the override root. Normal output validation does not catch
this, and with overwrite enabled it can replace a file outside the root.

## CMD-023 - Absolute extract outputs lose subdirectories when the config path is relative

Reported by: extract [P2].

The configured root (`extract/mod.rs`) stays relative when the config argument
is relative, so rebasing compares an absolute output against a relative root
with `strip_prefix`, which always fails. From `/work`, a config
`cfg/extract.json` with `"directory":"out"` and output
`/work/cfg/out/part/a.pbf`, rebased under `/new`, produces `/new/a.pbf`
instead of the documented `/new/part/a.pbf`. This violates the promised
preservation of subpaths. It can also turn distinct destinations into
duplicate filenames and cause an otherwise valid config to be refused.

## CMD-024 - Polygon ring-area validation runs before antimeridian unwrapping

Reported by: extract (flagged as a surprising policy, not counted as a defect).

Ring-area validation (`extract/polygon.rs`) runs before antimeridian
unwrapping. `[(179,0),(-179,2),(180,1),(179,0)]` passes validation although
its unwrapped geometry is collinear. Conversely, some unwrapped triangles with
nonzero area are refused because their wrapped coordinates have zero area. The
recorded raw-ring refusal policy explains the second case, and the hunter did
not count widening the accepted set as a defect.

## CMD-025 - degrade's corruption modes can publish an invalid PBF frame

Reported by: passthrough-rewrite.

`reframe_raw` (`degrade/mod.rs`) appends replacement hints and checks only
that the resulting header length fits `u32`, not that it stays under the PBF
header limit of 65,536 bytes. A valid header near the limit grows past it when
`--corrupt-tagdata` adds a hint, or when index corruption replaces a 26-byte
v1 index with a 42-byte v2 index. `write_raw_owned` trusts the framing it is
given, so the command can successfully publish a file its own reader refuses.
Deliberately false hints are the requested degradation, but invalid framing is
not. cat's reindex implementation already has the size gate this needs.

## CMD-026 - degrade --unsort at cap 1 does not produce overlapping blobs

Reported by: passthrough-rewrite.

With unique ascending IDs and cap 1, swapping the first pair
(`degrade/mod.rs`) produces singleton blobs `[2]`, `[1]`, `[3]`, whose ranges
are disjoint. The merge planner sorts entries by first ID before detecting
overlap, so `sort` can restore order by permuting blobs without rewriting an
overlap run. The test helper in `crates/cli/tests/cli_degrade.rs` hides this
because it checks only `previous.max >= next.min`, which counts descending
disjoint ranges as overlapping. The command does create disorder, but the
advertised benchmark shape and its test are wrong at cap 1.

## CMD-027 - A large inspect --blocks limit panics

Reported by: check-inspect [P1].

`inspect/report.rs` computes `limit * 2` without an overflow check. In a
64-bit release build, `--blocks 9223372036854775808` makes the product zero,
so with any data block present truncation is selected and the following huge
slice panics. With overflow checks on, the multiplication itself panics. This
breaks the flag's limiting behaviour and the refusal contract. The JSON
renderer already uses an overflow-safe comparison.

## CMD-028 - inspect --show drops present zero metadata and never prints visibility

Reported by: check-inspect [P2], cli-binary [P2].

`write_info` (`inspect/show_element.rs`) drops present timestamps, changesets
and UIDs when their value is zero, and `write_user` drops a present empty
user. The dense formatter also discards zero values after
`DenseNodeInfo::claims()` has established that they are present. Neither
formatter prints visibility, including `visible=false`. As a result, absent
and explicit-zero metadata display identically, and deleted history objects
lose their deletion state. This contradicts `CORRECTNESS.md`, which preserves
presence and zeros in printing outputs, and the `show_element_to` contract to
write all metadata. The formatter is wrong where decoded claims become text.

## CMD-029 - Default inspect JSON reports tagged_nodes 0 without measuring it

Reported by: check-inspect [P2].

The index-only scan (`inspect/scan.rs`) accumulates element counts but never
counts tagged nodes, yet `inspect/json.rs` always publishes
`"tagged_nodes": 0`. An honest indexed file with tagged nodes reports zero by
default and a positive count under `--distrust-metadata`. This breaks the
field's meaning without needing corrupt metadata. An unmeasured count should
be represented as unavailable, separately from a measured zero.

## CMD-030 - data.objects_ordered rejects a mixed-kind blob in canonical order

Reported by: check-inspect [P2].

`inspect/scan.rs` derives element ordering partly from the block-layout
summary, which declares every mixed-kind blob nonstandard. A legal block
holding a node group followed by a way group therefore reports unordered
objects, although its elements are in canonical order and `check --ids`
passes them. This breaks the field's stated meaning (`inspect/keys.rs`).
Element ordering needs decoded group-order evidence, because block-layout
classification answers a different question. The signed-comparator half of
the same finding is BUG-012 in `notes/bugs.md`.

## CMD-031 - inspect timestamp formatting drops milliseconds

Reported by: check-inspect [P2].

`format_timestamp` (`inspect/format.rs`) accepts milliseconds but discards the
fraction by division toward zero. With `date_granularity=1`, 1500 ms displays
as `1970-01-01T00:00:01Z` and -1 ms becomes the epoch. This affects the text
report, JSON and timestamp `--get` values. The census explicitly operates in
milliseconds (`CORRECTNESS.md`), so the formatted range should keep that
resolution. The export hunter separately noted that export timestamps also
silently lose their subsecond part.

## CMD-032 - inspect's empty-state sentinels hide extreme values

Reported by: check-inspect [P3].

`ExtendedStats::has_timestamps` (`inspect/types.rs`) treats
`min_timestamp == i64::MAX` as absence, so a sole timestamp of `i64::MAX`
with date granularity 1 counts as metadata but produces no timestamp range.
`DataBbox::has_data` has the same collision when every decoded latitude equals
`i64::MAX`: the nodes are counted but the bbox disappears. These values reach
inspect through the reader's accepted arithmetic. Explicit optional extrema
would keep "no observation" distinct from "extreme observation".

## CMD-033 - A header-only file is indexed or not depending on inspect mode

Reported by: check-inspect [P3], cli-binary [P2].

The index-only path (`inspect/scan.rs`) returns `indexed=true` for a
header-only file, while the decoded path requires at least one data blob. For
the same valid header-only file, `inspect --get indexed` returns `true`,
`inspect --extended --get indexed` returns `false`, and
`inspect --indexed --extended` prints an affirmative probe verdict followed by
a report saying `Indexed: no`. `docs/cli/commands.md` explicitly defines
header-only files as indexed, and the library's full-scan result is the wrong
side. Separately, the extended text renderer (`inspect/report.rs`) omits
metadata coverage entirely when there are no objects, although the
documentation promises `All objects have: (none)`.

## CMD-034 - Export can leave a clockwise ring clockwise through cancellation

Reported by: export [P2].

`export/geometry.rs` computes the shoelace area from absolute longitude and
latitude products. For the clockwise ring `(128, 64)`, `(128, 64.0000001)`,
`(128.0000001, 64)`, `(128, 64)` (lon, lat; ordinary decimicrodegree values),
the calculation subtracts products around 8192 to recover a twice-area around
`1e-14`, which rounds away. The area becomes zero, `reverse` stays false and
the ring stays clockwise. This breaks ADR-0010's unconditional
counterclockwise exterior-ring promise. The shared `geo::signed_area`
(`geo.rs`) already avoids this with a local origin, and export duplicates the
vulnerable calculation.

## CMD-035 - Export accepts degenerate polygons, including ones made by rounding

Reported by: export [P2].

`write_way_geometry` (`export/geometry.rs`) checks the position count and that
there are three distinct unrounded positions, but never rejects zero area. A
closed area-tagged way `[(0,0),(1,1),(2,2),(0,0)]` passes and is counted as an
exported Polygon. A valid granularity-1 triangle
`[(0,0),(0,1e-9),(1e-9,0),(0,0)]` passes the distinct-position check, and then
seven-decimal formatting (`coord_fmt.rs`) collapses every output position to
`[0,0]`. Both break the promise that invalid way geometry is skipped and
counted. Validation needs to judge the coordinates actually emitted.

## CMD-036 - Export emits coordinates outside the GeoJSON domain

Reported by: export [P2].

Point serialization (`export/geometry.rs`) does no coordinate validation, and
way validation checks no geographic bounds. A tagged node at latitude 100 is
exported, and a way containing libosmium's undefined-location values
(214.7483647 on both axes) can be emitted and counted as valid.
`DEVIATIONS.md` promises RFC 7946 coordinate order and validity. The general
reader leaves coordinate values unchecked, so the validation belongs at
export's output boundary.

## CMD-037 - export --metadata can emit timestamps that are not RFC 3339

Reported by: export [P2].

`export/properties.rs` passes every decoded timestamp to `format_epoch_secs`
(`commands/mod.rs`) without checking the calendar range. With the default date
granularity, the raw timestamp `253402300800` produces
`"10000-01-01T00:00:00Z"`, because the formatter's `04` sets only a minimum
width. This violates the RFC 3339 UTC promise in `docs/cli/commands.md`. The
formatter documents its four-digit-year limit, and export does not enforce it.

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

## CMD-040 - Sparse ALTW carries a false sorted header into its output

Reported by: the negative-ID ALTW item (2026-11-18), while sparring auto's
external admission gate; not reproduced.

`altw_output_facts` carries the input's order claim, and the sparse index
does not depend on order, so it never tests that claim. A file stamped
`Sort.Type_then_ID` whose node blob ranges run `[2]` then `[1]` joins under
`--index-type sparse` and publishes an output stamped sorted that is not.
Auto now sees the contradiction in its header walk (the external join's
order check) and runs sparse instead of routing to external's refusal, so
it reaches the same output. The fix belongs where the stamp is written, for
explicit sparse and auto alike: drop the claim, or refuse, when a decoding
pass proves it false.

## CMD-041 - renumber refuses a block its rewrite grows past the raw size cap

Reported by: the negative-ID renumber item (2026-11-18), while sparring the
negative start; not reproduced.

Renumber writes exactly one output block per input block (`pool.rs`
`run_rewrite_stage`), and the writer refuses a block over the raw size cap
(`write/framing.rs`). The rewrite can grow a block: a way or relation ID of
1 is a one-byte varint, a large start (`2^40`) or a negative one (`-1` is
ten bytes) widens every ID, and a way alternating between a live node and an
orphan can widen its ref deltas. An input block close to the cap then fails
with the size error instead of renumbering. It fails loudly and before
publication, and is sign-neutral (a large positive start does it too). The
fix is a split path: cut an oversized rewritten block between elements into
two output blocks with their own indexes.

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
