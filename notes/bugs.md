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

## BUG-001 - Blob envelope parsers ignore wire type

Reported by: framing.

`WireBlob::parse` (`blob_wire.rs`) and `parse_raw_envelope` (`decompress.rs`)
dispatch known fields by number without checking wire type: a wrong-typed
payload field is read as length-delimited anyway and `raw_size` is read as a
varint regardless of its tag. Unlike `WireBlobHeader`, these can reinterpret
malformed envelope bytes instead of rejecting or skipping them.

Defect hunt 2026-11-17 (read-framing, read-block-decoding): rated P1 by
read-framing. Concrete trigger: an ordinary raw Blob whose payload tag changes
from `0x0a` to `0x0e` (reserved wire type 6) is still consumed and, if the
payload is a valid PrimitiveBlock, decodes successfully; other wrong wire types
can consume bytes across real field boundaries. The framing layer cannot catch
it because the declared extent stays correct. The same pattern is in
`WireHeaderBlock::parse` (`block.rs`): appending `87 02 00` (field 32, invalid
wire type 7) yields replication timestamp 0, and `82 02 00` (empty
length-delimited field 32) becomes a timestamp instead of being skipped. The
element parsers already match on field and wire type together. Claim named:
protobuf decoding and the input-refusal contract. read-framing proposes one
borrowed Blob-envelope parser shared by owned Blob construction, direct
decompression and payload comparison, separating structural from
post-decompression validation.

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

## BUG-004 - Out-of-range tag SIDs silently shorten tag lists

Reported by: decode, encoding, merge, tags.

`TagIter::next` (`elements.rs`) and `DenseTagIter` end iteration at an
out-of-range string index, and structural validation does not check SID bounds.
Rebuild and Owned-conversion paths (filters, extract, sort sweep encoding) use
`tags()`, so malformed indices can produce successfully written elements with
fewer tags. Checked iterators exist. The tags hunter describes this end-of-list
handling as existing behaviour a rewrite must keep; the decode, encoding and
merge hunters call it a silent wrong answer. Both readings are recorded.

Defect hunt 2026-11-17: reported independently by read-block-decoding (P1),
write-encoding (P2), apply-merge (P2), id-filter-selection (P1), export (P2),
passthrough-rewrite and geocode-builder (P2). All of them cite the
`CORRECTNESS.md` entry that documents the iterator limitation and all call that
acceptance incompatible with the refusal-over-silent-wrong-answer contract;
the wrong boundary is each command consuming the infallible iterator while
checked accessors exist. Consumers named:
- repack writer and owned paths (`repack/mod.rs`), against repack's explicit
  tag round-trip promise in `docs/cli/commands.md`;
- cat cleaning path (`cat/mod.rs`) and degrade rewrites; cat's
  unclassifiable-block fallback already uses the checked accessors;
- owned conversions (`owned.rs`), used by blob_merge cursor demotion;
- blob_merge sweep encoder (`blob_merge/sweep.rs`), affecting sort and
  cat --dedupe;
- getid, getparents, time-filter and tags-filter: a way with
  `highway=primary`, a bad index, then `name=Main` is missed by a `name=Main`
  filter and written with only its first tag by a `highway=primary` filter;
- export (`export/mod.rs`): a bad first tag drops a tagged element as
  untagged; `building=yes`, bad tag, `area=no` becomes a Polygon because
  classification never reaches the override, breaking "area=no always
  disqualifies";
- geocode builder node processing (`pass2.rs`) and relation selection
  (`pass1.rs`): an address loses postcode or place fields after the bad pair;
  Pass 1.5 already refuses the same corruption in way tags.
The proposed fix is fallible tag iteration carried through owned conversion,
matching and builder intake.

## BUG-005 - Diff clipping can hide elements beyond a false index range

Reported by: decode, merge.

A carried frame is clipped at successive unit boundaries (`merge_join.rs`
`clip`) and stops being carried once its claimed last key is passed
(`diff/units.rs` carry selection). Elements beyond an understated claim belong
to no unit, and the later claimed-maximum check only sees the clipped run.

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

## BUG-010 - Negative IDs are dropped by IdSet-based paths

Reported by: altw-sparse, tags, spatial.

`IdSet::set` ignores negative values. Sparse ALTW reference collection and
coordinate population discard negative IDs although `osm_id.rs` defines signed
ordering and external's diagnostic implies sparse can join them; tags-filter
reference inclusion records matches through `IdSet`, so a matching negative-ID
element survives `-R` but disappears with reference inclusion; extract
membership (`IdSet` and multi-region membership) discards negatives. Not
reproduced.

Defect hunt 2026-11-17 (extract P1, id-filter-selection P2, cli-binary):
- Extract: in-region node `1`, out-of-region node `-1`, way `10` referencing
  `[1,-1]`; complete-ways selects the way, discards `-1` while collecting
  dependencies, and publishes the way without a node it references
  (`extract/smart.rs` dependency collection, `extract/common.rs` membership
  emission); smart inherits it. Simple is inconsistent: an in-region negative
  node disappears through filtered emission but survives in a verified
  raw-copy blob. Claims named: complete-ways completeness and the stated
  rejection policy; `IdSet` keeps its own contract and extraction fails to
  enforce its precondition.
- getid: `parse_ids_from_pbf` (`getid/mod.rs`) silently drops negatives from
  an `--id-osm-file` source, so a mixed source succeeds with a shortened
  request; reference expansion also discards negative relation seeds and
  negative way references. `DEVIATIONS.md` acknowledges these and declines
  further rejection; the hunter reads that as accurate but contradicting the
  summary contract.
See also DOC-001 in `notes/bugs-docs.md` for the blanket "rejected
project-wide" wording.

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
non-monotonic for inspect, also affecting `data.objects_ordered`.

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

## BUG-015 - OSC re-emission drops explicit empty metadata

Reported by: osc.

`OwnedMetadata::append_attrs` tests string emptiness and parsing keeps no
presence bits, so an explicit `user=""` is omitted on output.

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

## BUG-025 - Extract strategies accept different extreme coordinates

Reported by: spatial.

Simple's raw node scanner uses checked scaling; columnar and element accessors
use wrapping arithmetic and narrowing, so extreme framing values fail in one
strategy and produce coordinates in another.

Defect hunt 2026-11-17 (extract P1, geocode-builder P1) shows the narrowing
publishes wrong locations:
- Extract: complete-ways, smart and multi-extract classify nodes through the
  unchecked columnar conversion (`read/columnar.rs`, called from
  `extract/smart.rs` pass 1). A node with `lat_offset = 429496729600` and raw
  latitude and longitude zero is at 429.4967296 degrees; dividing by 100
  gives 4294967296, which narrows to 0. An extraction around `(0,0)` selects
  it and the write pass (`extract/common.rs`) writes it at `(0,0)`. Truthful
  indexdata without a bbox suffices. Sorted single-extract simple already
  refuses this through its checked scanner.
- Geocode builder: Pass 2 (`builder/pass2.rs`) calls `decimicro_lat()` and
  `decimicro_lon()`, which narrow with `as i32` (`read/elements.rs`), before
  the domain check. A raw latitude of `4_294_967_296` becomes 0, which
  `pack_coord` accepts, so an addressed node becomes an address at latitude
  zero. This breaks the promise in `docs/cli/commands.md` that out-of-domain
  nodes fail the build.
Both hunters say the general accessors document their permissive behaviour
and the consumer is the wrong end: it needs checked scaling and narrowing
before domain validation.

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
