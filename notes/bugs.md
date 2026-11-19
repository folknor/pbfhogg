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
`notes/bugs-cli.md`, `notes/bugs-tooling.md` and `notes/bugs-docs.md`. They are as reported, read from source, not reproduced
unless an entry says so, and some hunters marked their own finding as a code
reading or an API hazard rather than a demonstrated wrong result; that label is
kept. "Reported by" is a fact about the reports. Entries are in no order of
importance.

## BUG-002 - Missing BlobHeader datasize becomes zero

Reported by: framing.

`WireBlobHeader::parse` defaults `datasize` to zero and never checks presence,
so an omitted required size becomes a zero-byte payload. Header-only and raw
passthrough paths can accept it while a later decode fails differently.

Defect hunt 2026-11-17 (read-framing, P2): `Frames` (`frame.rs`) then accepts
the manufactured zero, so a header with a type but no datasize becomes a
completed frame with an empty body; after a valid leading header, an extension
frame with no datasize can be silently skipped and the input accepted. The
parser needs to distinguish absent from explicit zero. Libosmium rejects both
missing and zero sizes, while the repository explicitly tests acceptance of
zero, so that separate compatibility difference also needs a ruling.

## BUG-003 - Rebuild paths truncate timestamps and coordinate precision

Reported by: decode, encoding, sort, tags, altw-sparse, osc.

`owned.rs` metadata conversion (`element_metadata_impl`,
`dense_node_metadata_impl`, plain and dense conversions) divides millisecond
timestamps by 1000 and the builder writes default timestamp framing; owned node
conversion narrows nanodegrees to decimicrodegrees under default coordinate
framing. Inputs with finer precision change whenever a command rebuilds instead
of passing through: tags-filter, sort overlap rewrites (passthrough keeps
precision, so the result depends on routing), sparse ALTW filtered retained
nodes (`altw/mod.rs` `process_block`, where node content is promised
unchanged). The PBF-to-OSC adapter (`osc/write.rs`) floors millisecond
timestamps to seconds the same way.

Defect hunt 2026-11-17 (read-block-decoding, passthrough-rewrite): the same
conversion breaks a documented claim. `docs/cli/commands.md` (repack section)
says every inline way coordinate round-trips exactly; both repack rewrite paths
(`repack/mod.rs`) convert to decimicrodegrees, so with granularity 1 a decoded
coordinate of 101 nanodegrees becomes 100, and 1 nanodegree becomes 0.
Metadata also goes through whole-second normalization (`owned.rs`). Both
hunters say the documentation is the wrong side here: "same corpus, different
encoding" needs explicit precision qualifications.

## BUG-006 - Rebuild paths drop inline way locations

Reported by: encoding, sort, spatial.

Extract, tags-filter and the sort sweep (borrowed way output and `read_way`
owned demotion) encode ways without their locations, so in sort whether a way
keeps coordinates depends on overlap routing. Some callers classify output as
re-encoded without enrichments, so the hunters flag it for judgement against
each command's promise. `notes/verification.md` VER-135 describes sort's drop
as required and lacking a real-data witness.

## BUG-007 - add_owned_way silently drops mismatched locations

Reported by: encoding.

`add_owned_way` (`owned.rs`) falls back to a plain way when a nonempty
locations vector disagrees with refs; the direct builder location API refuses
the mismatch. The fallback can hide an internal transformation error.

## BUG-008 - Public external_join bypasses missing-node enforcement

Reported by: altw-sparse, altw-external.

`external_join` (`altw/external/mod.rs`) publishes after `external_join_in`
without the command's missing-location policy and has no `ignore_missing_nodes`
parameter, so library callers can publish zero placeholders the default command
rejects. Separately, the command knows the resolved count when stage 2 returns
but enforces the strict policy only after encoding, assembly, compression and
writing; passing the policy into orchestration would fail right after the join.

Defect hunt 2026-11-17 (altw, P1): a sorted, indexed input with a way
referencing an absent node returns success through this public API and
publishes `(0, 0)`, with no signature option authorizing placeholders; the
enforcing path is `add_locations_to_ways_to` (`altw/mod.rs`). Claim named: the
publication guarantee in `CORRECTNESS.md`. The hunter says backend selection
and publication policy should have one owner.

## BUG-009 - Duplicate node IDs resolve to scheduling-dependent coordinates

Reported by: altw-sparse, altw-external, apply-locations.

Sparse ALTW stores duplicate IDs with relaxed atomics, last store wins (within
a blob the later tuple, across parallel blobs whichever runs last); external
stage 2 matching takes the first tuple; equal IDs pass the ascending checks. So
backends disagree and sparse is nondeterministic. apply-changes location mode
merges worker coordinate maps in slot order, not source order, while the order
guard permits equal boundary IDs (`drain.rs`). Not reproduced.

Defect hunt 2026-11-17 (altw, P1): two records for node 1 at different
coordinates followed by a way referencing node 1 produce different inline
coordinates by engine and possibly worker timing; exact indexdata does not
prevent it. Refusals also diverge: if the first record is valid and the second
is outside the domain but fits `i32`, sparse refuses the second insertion,
while external resolves against the first and never domain-checks the second.
Claim named: agreement between index strategies, including the explicit
requirement in `altw/external/stage2.rs` that domain refusals agree. The
hunter asks for a common refusal policy for conflicting coordinates.

## BUG-011 - Node-statistics histograms depend on worker scheduling

Reported by: check.

`WorkerAccum` (`inspect/node_stats.rs`) carries a partial 128-coordinate window
across whichever blobs a worker receives, so adjacency, buckets and window count
vary with scheduling; the parity test excludes histogram equality. Fixing needs
a contract decision.

Defect hunt 2026-11-17 (check-inspect, P2): witness - two blobs of 64 nodes,
the first at latitude 0 throughout, the second at 900,000,000 throughout. In
file order they form one 128-value window needing 30 bits; on separate workers
they become two constant windows needing zero bits. Histogram, block count and
compression estimate change with scheduling or `-j`. The comment claiming the
histogram shape is preserved is false. Proposed: order coordinate batches
before forming windows.

## BUG-012 - inspect and check disagree on negative-ID order

Reported by: check.

Inspect's `TypeIdRange` (`inspect/types.rs`) compares signed numerically; ID
checking uses canonical OSM order, so `0, -1, -2, 1` is ordered for check but
non-monotonic for inspect, also affecting `data.objects_ordered`. Under
ADR-0028 the `--id-ranges` monotonicity is a numeric report field (and
`docs/cli/commands.md` now says so), but `objects_ordered` claims OSM order
and must judge it canonically; that half is the bug. The `check --locations`
capped detail list, ordered by signed numeric ID, is a diagnostic order the
ADR does not govern.

Defect hunt 2026-11-17 (check-inspect, P2) restates this as part of its
`data.objects_ordered` finding; the other half of that finding is a separate
mechanism, filed as CMD-030 in `notes/bugs-commands.md`.

## BUG-013 - PBF-to-OSC emission drops visibility

Reported by: osc.

`push_info_attrs` (`osc/write.rs`) takes no visibility argument, so a created
or modified element with `visible=false` loses the attribute.

## BUG-014 - OSC attribute helpers skip trailing malformed attributes

Reported by: osc.

The attribute helpers (`xml_parse.rs`) return after finding the requested field,
so `<nd ref="1" broken/>` leaves `broken` unexamined; quick-xml start-event
construction does not validate attributes itself.

Defect hunt 2026-11-17 (osc-parsing, P2): `<member type="node" ref="1"
role="outer" role=broken/>` is accepted because every lookup stops before the
malformed trailing attribute; tags and node references have equivalent holes,
and re-emission silently removes the offending tail. Claim named: the
malformed-role refusal contract in `CORRECTNESS.md`. The parser needs to
validate the complete child attribute list.

## BUG-016 - apply-changes applies older OSC versions over newer base versions

Reported by: apply-core.

By inspection, a base node at version 10 with one OSC modify at version 9 is
replaced by version 9, and an older delete removes it: `KindSeeks`
(`classify.rs`) matches by ID only and the rewrite applies the action without
comparing base metadata; the ingest regression gate compares changes within the
overlay only. The vendored osmium `command_apply_changes.cpp` merges by
descending version and keeps the first per ID, retaining version 10. The sorted
delete representation has already discarded the metadata needed.

Defect hunt 2026-11-17: the hunters disagree on which side is wrong.
- apply-merge (P1): base node 1 at version 9 plus a single OSC modify at
  version 8 outputs version 8; a version-8 delete removes the version-9
  object. Rewrite dispatch (`rewrite_block.rs`) replaces or deletes by
  identity; version checks run during OSC accumulation only; `SortedDiff`
  (`osc/sorted.rs`) stores deletes as IDs alone. Claim named: the
  `--allow-version-regression` contract in `docs/cli/commands.md`, since a
  downgrade succeeds with the flag off. The finished overlay must keep version
  information, including tombstones, for the base merge.
- osc-parsing and diff (both flagged out of scope): the implementation matches
  its overlay-specific API documentation; the CLI wording overstates the
  protection and should limit it to ordering within the change stream.
- verify (P2): reads the overlay-only gate as pbfhogg's documented rule
  (`DEVIATIONS.md`) and reports that `pbfhogg-verify` apply-changes
  (`checks/apply_changes.rs`) fails that correct result, because it tolerates
  only osmium-only objects finally deleted by the OSC; equal-version changes
  with differing timestamps fail the same way. It calls the delete-only
  exception in `reference/verify.md` wrong against `DEVIATIONS.md`, says the
  same assumption affects the osmium roundtrips in
  `checks/derive_changes.rs`, and asks for an independent precedence model
  instead of unconditional equality with osmium.

## BUG-017 - apply-changes direct reads do not reach the bulk reads

Reported by: apply-core.

`direct_reads` reaches the initial `InputFront`, but the scanner opens a
buffered walker (`scanner.rs`) and the worker pool opens a plain file
(`streaming.rs`), so the heavy reads stay buffered.

## BUG-018 - renumber direct reads do not reach the bulk reads

Reported by: sort.

The initial header read gets the flag, but payload workers use `pin.open()` and
schedule construction opens the ordinary header walker (`renumber/mod.rs` input
setup).

## BUG-019 - apply-changes blob-size percentiles sample passthrough only

Reported by: apply-core.

Samples are inserted only in the two passthrough arms; rewritten and create
frames never contribute, though `stats.rs` labels the result blob sizes.

## BUG-020 - renumber rejects plain node encoding

Reported by: sort.

`single_dense` (`wire_rewrite.rs`) requires `DenseNodes`, rejecting plain
nodes, including valid sorted output the library's partial-metadata encoding
produces.

Defect hunt 2026-11-17 (sort-renumber, P2): a sorted, honestly indexed block
with one plain node fails renumber, and the error wrongly calls the
single-kind group mixed. `BlockBuilder::add_node_partial` deliberately writes
plain nodes when dense encoding cannot preserve metadata presence, for example
metadata with an explicit UID of zero and no version. Claim named: renumber
renumbers all element IDs with no dense-only restriction. The hunter's
structural note: renumber's rewrite layer needs to cover every element
representation the library produces.

## BUG-021 - renumber turns explicit zlib:6 into zlib:1

Reported by: sort.

Compression selection (`renumber/mod.rs`) maps explicit `zlib:6` to `zlib:1`;
disclosed in CLI source but an observable trap for any pipeline rewrite.

## BUG-022 - degrade does not reliably drop N unique IDs

Reported by: copy.

`select_drop_sets` (`degrade/mod.rs`) counts occurrences and admits repeated
identical `DropKey`s into its heaps, then inserts into hash sets, so two
versions of one ID take two slots but select one ID; it can accept N above the
distinct count and still report N dropped. Marked "likely bug".

Defect hunt 2026-11-17 (passthrough-rewrite): with two versions of node 1,
requesting one removal deletes both records, because rewriting removes every
occurrence of a selected ID; requesting two from an input holding only those
two versions passes the size check but selects one ID. Neither the CLI's
"exactly N elements" nor `Reencode`'s "exactly this many unique element IDs"
holds, and the reported dropped count is just N. Selection must use the
identity that removal and counting use.

## BUG-023 - cat's blobs_decoded hides decompression

Reported by: copy.

Unindexed plain cat decompresses ordinary blobs to generate indexdata but counts
them as passthrough; `blobs_decoded` counts only rebuilt unclassifiable blobs.

Defect hunt 2026-11-17 (passthrough-rewrite): the repair path has the same
gap. `repair_passthrough` (`cat/mod.rs`) validates every data blob, but sets
`blobs_decoded` only from blobs rebuilt into single-kind blocks, and
`Outcome::Rewritten` increments neither decoded nor passthrough counts. One
repaired, classifiable blob can report `0 blobs passed through` then
`Repaired 1 input blobs`, with `cat_blobs_decoded = 0`, against
`CatStats::blobs_decoded`'s documented meaning.

## BUG-024 - Export keeps decoding after a write failure

Reported by: spatial.

The export callback records `first_err` and later callbacks return early, but
sequential `for_each` still reads and decompresses the rest of the input, so a
broken pipe can waste a planet traversal.

Defect hunt 2026-11-17 (export) observed the same behaviour and notes it is
already recorded as deferred work.

## BUG-026 - Geocode candidate and ranked APIs break equal-area ties differently

Reported by: geocode.

`Candidates::into_result()` keeps the first equal-area admin match; ranked lookup
picks the lower polygon index; an index with equal-area polygons in reverse
order makes them disagree. Code reading, not executed.

Defect hunt 2026-11-17 (geocode-reader, P2): `Candidates::into_result`
(`reader.rs`) explicitly claims to match `query()` semantics. With two
containing same-level polygons of equal stored area presented as `[1, 0]`,
`query()` selects polygon 0 and `candidates().into_result(&reader)` selects
polygon 1. This occurs with valid index data, and equal stored areas also
arise through f32 rounding. `AdminMatch` discards polygon identity, so sharing
the ranking needs that identity kept through collection.

## BUG-027 - Geocode building placement is a vertex mean

Reported by: geocode.

Building address placement averages resolved way coordinates including the
repeated closing vertex, can fall outside a concave building, and averages near
longitude zero across the antimeridian. Flagged to preserve during rewrites
unless placement semantics change.

Defect hunt 2026-11-17 (geocode-builder) observed the antimeridian case
(`builder/pass2.rs` arithmetic longitude mean) and classes it as a surprising
consequence of the documented mean-of-nodes policy, not a contract defect.

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

## BUG-029 - Scan helpers that look complete but are not

Reported by: indexdata.

`blob_filter_check` (`scan/classify.rs`) accepts a `BlobFilter` but ignores its
bbox; `parallel_scan_blobs_raw` does not enforce the schedule's kind claim.
Callers compensate today; the hunter calls these API hazards, not demonstrated
wrong results.

## BUG-030 - inspect's monotonicity answer depends on whether the file is indexed

Reported by: the negative-ID registry (`crates/cli/tests/cli_negative_ids.rs`).

The index-only report judges `id_ranges.<kind>.monotonic` from consecutive
blob ID ranges, which cannot see order inside a blob; the decoding report
judges the elements. On the mixed-sign graph the relation blobs `[-1, -7, 1]`,
`[7, 9, 11]` have ascending ranges, so `inspect --id-ranges` says monotonic
on the indexed file and not monotonic on the same file unindexed. Positive
input can show it too (an indexed blob internally out of order under
ascending ranges). Held as a known gap by the registry case `inspect_report`
(indexed framing). Neighbour of BUG-012, which is about which order the
answer means, not about the two paths disagreeing.

## BUG-031 - check --ids --full duplicate details depend on completion order

Reported by: the codex spar of the check negative-ID work.

The duplicate pass attributes a duplicate to whichever blob reaches the
set second: the atomic setters in the workers, and `settle_overflow` in
the phase's merge, which takes results in completion order
(`check/verify_ids.rs`). Totals are exact, but the order of the stored
violations is not, so `--max-errors` can keep a different finding from run
to run, and even the uncapped list can be ordered differently. Witness:
node blobs `[-1] | [-2] | [-1]`, one per blob. Both schedules report two
violations, `Duplicate -1` and `NonMonotonic -1 after -2`, but in either
order, and `--max-errors 1` keeps whichever came first. Positive IDs above
the allocation and atomic duplicates already behaved this way; since
negatives took the same path they do too. Fix: give duplicates a fixed place
in report order and pick their examples by canonical ID, or settle every
occurrence in one ordered consumer. Neighbour of PERF-048, which plans a
redesign of the same per-blob retention.

## BUG-032 - Re-encoding commands write wrapped node coordinates

Reported by: the codex spar of the extract and geocode coordinate work.

Extract and the geocode builder now convert every node coordinate exactly
(`decimicro_lat_checked` / `decimicro_lon_checked`, the checked columnar
decode, `nano_lat_exact`) and refuse one that does not fit an `i32`
decimicrodegree. The OSC node writers derive uses do the same, and also
refuse a coordinate outside the geographic domain. Every other command that
decodes a node and writes it again still uses the permissive
`decimicro_lat` / `decimicro_lon`, which wrap modulo 2^64 and narrow with
`as i32`: cat and repack re-encode, sort, degrade, tags-filter,
time-filter, getid, getparents, apply-changes element writes and altw stage 4 among them. A block with
`lat_offset = 429496729600` and a raw latitude of 0 (429.4967296 degrees)
is written at latitude 0, a silent wrong answer where a refusal is owed.
Inline way locations (`WayNodeLocation`) wrap the same way before
construction. The fix is to make the ordinary accessors exact and fallible
library-wide, with the wrapping forms renamed as deliberate, and give each
consumer an error path; inspect --nodes is documented to agree with the
permissive walk and needs its own decision. It is a measured-path change
for every node-decoding command, which is why it did not land with the
extract and geocode fix. Once a permissive re-encoder has written the
wrapped value, no downstream check can see the original.
