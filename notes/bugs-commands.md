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
renumber (BUG-020), degrade drop counting (BUG-022), cat decode counters
(BUG-023) and extract coordinate narrowing (BUG-025). Entries are in no
order of importance.

## CMD-002 - OSC tags-filter output can reverse an object's final state

Reported by: osc-parsing [P1].

`write_groups` (`tags_filter/osc.rs`) emits all creates, then all modifies,
then all deletes, whatever the input order. A delete followed by a matching
re-create at the same version becomes a create followed by a delete. Both
changes pass the overlay's version gate, so applying the filtered stream
deletes the object. This breaks the meaning of filtering a change stream. The
filter's output ordering is what is wrong.

## CMD-003 - parse_rfc3339_utc accepts malformed signed components

Reported by: osc-parsing [P2], passthrough-rewrite.

`parse_rfc3339_utc` (`commands/mod.rs`) parses components as signed integers
and checks only upper bounds for day and time. `2026-01-01T-1:00:00Z` is
accepted and stored as `2025-12-31T23:00:00Z`, `2026-01--1T00:00:00Z` also
passes, and leading `+` signs are accepted. The parser feeds header overrides
and OSC metadata (`osc/xml_parse.rs`), so malformed timestamps silently become
different valid ones. This breaks the function's documented strict RFC 3339
contract and the refusal policy for malformed input. Fixed-width components
need ASCII-digit validation.

## CMD-004 - Numeric OSC attributes refuse legal XML character references

Reported by: osc-parsing [P2].

`parse_i64_attr`, `parse_f64_attr` and `meta_int` (`osc/xml_parse.rs`) parse
the raw attribute text before XML normalization, so `id="&#49;"`,
`ref="&#50;"`, `lat="&#49;"` and `version="&#49;"` fail instead of reading as
numbers. Native timestamp parsing has the same problem. This breaks OSC XML
compatibility with no recorded deviation. String attributes already normalize
correctly.

## CMD-005 - OSC re-emission changes metadata values that contain referenced whitespace

Reported by: osc-parsing [P2], apply-merge [P2].

`scan_owned_metadata_into` (`osc/xml_parse.rs`) correctly turns
`timestamp="a&#10;b"` into a string containing a newline. `append_attrs`
(`osc/write.rs`) then uses ordinary escaping for timestamp, changeset, uid and
visible, which leaves newline and tab literal, so reading the output
normalizes them to spaces and the value becomes `a b`. Only `user` gets the
whitespace-preserving escaping. This affects both merge-changes modes and the
shared tags-filter OSC emitter. These fields deliberately accept arbitrary
normalized text for pass-through, so it breaks the preservation contract in
`CORRECTNESS.md`. The writer should use the existing whitespace-preserving
escaping for these fields too.

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

## CMD-008 - Derived OSC can contain values pbfhogg's own OSC reader refuses

Reported by: osc-parsing [P2], diff [P2].

The borrowed node writers (`osc/write.rs`) format unchecked PBF coordinates,
so a new node at latitude 91 produces `lat="91"`, which the OSC parser
refuses. `push_info_attrs` formats timestamps without checking the year, so an
element dated `10000-01-01` gets a five-digit year, while `parse_rfc3339_utc`
requires exactly 20 bytes and four year digits. Derive succeeds, but applying
its output fails. The public delete-timestamp parameter has the same problem,
and values above `i64::MAX` are silently clamped (`diff/derive.rs`). This
breaks the derive/apply roundtrip documented in `docs/cli/commands.md`, even
though metadata differences are excluded from comparison. The producer should
refuse values outside the consumer's representable domain, or the consumer
should accept them.

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

## CMD-042 - External ALTW stage 4 copies its passthrough prefix after the router aborts

Reported by: the stage 3 startup-deadlock item (2026-11-18), from source
during the spar; not reproduced.

Before the ordered pool delivers its first result, stage 4's consumer runs
`before_item(0)` (`altw/external/stage4.rs`), which copies every passthrough
blob ahead of way blob 0, the whole untagged node prefix under
`keep_untagged_nodes`. That loop checks only the invocation token. A router
abort (a stage 3 bucket error, a refused stage 3 spawn) does not trip the
token, so the call keeps copying, gigabytes at planet scale, before it
returns the error. Nothing hangs and the error is right, but failure latency
is a full prefix copy. The fix is to have stage 4 observe `router.is_aborted`
at its passthrough boundaries, at the same per-blob granularity as the token
check.

## CMD-043 - The parallel output writer reports startup before its workers exist

Reported by: the stage 3 startup-deadlock item (2026-11-18), from source
during the spar; not reproduced.

`write/parallel_writer.rs` sends its init success before `run_pool` attempts
its worker spawns. A refused worker spawn is recorded in `FirstFailure`, but
the caller has already been told the writer started, and learns of the
refusal only at a later write or the final flush. In external ALTW stage 4
can therefore sit in the router's readiness wait (stage 3 still making
progress) for a writer that is already dead. A delay, not a deadlock. Init
success should wait until the pool's workers are all born.

## CMD-044 - Most library scopes still spawn with the panicking Scope::spawn

Reported by: the stage 3 startup-deadlock item (2026-11-18).

`threads::Scope::spawn` panics on an OS refusal, and most worker scopes in
the library use it (stage 1, stage 2, sparse ALTW, the read pipeline,
classify, apply-changes, diff, geocode and others; the sweep found about
thirty sites). External ALTW stages 3 and 4 and the ordered pool now use
`try_spawn` and own a refusal; the others were not analysed. A spawn panic
inside a scope whose already-born threads wait on a peer that was never
born, or on a channel the body would have fed, deadlocks the same way the
stage 3 refusal did. The rest escape the call as a panic rather than a typed
`Io` error. Each site needs its own wait topology checked, then `try_spawn`
with the refusal recorded and broadcast before the join. Removing
`Scope::spawn` once no caller needs it would keep new sites from
reintroducing it.
