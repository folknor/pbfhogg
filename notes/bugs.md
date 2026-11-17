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
for the scope names). They are as reported, read from source, not reproduced
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

## BUG-002 - Missing BlobHeader datasize becomes zero

Reported by: framing.

`WireBlobHeader::parse` defaults `datasize` to zero and never checks presence,
so an omitted required size becomes a zero-byte payload. Header-only and raw
passthrough paths can accept it while a later decode fails differently.

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

## BUG-004 - Out-of-range tag SIDs silently shorten tag lists

Reported by: decode, encoding, merge, tags.

`TagIter::next` (`elements.rs`) and `DenseTagIter` end iteration at an
out-of-range string index, and structural validation does not check SID bounds.
Rebuild and Owned-conversion paths (filters, extract, sort sweep encoding) use
`tags()`, so malformed indices can produce successfully written elements with
fewer tags. Checked iterators exist. The tags hunter describes this end-of-list
handling as existing behaviour a rewrite must keep; the decode, encoding and
merge hunters call it a silent wrong answer. Both readings are recorded.

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

## BUG-009 - Duplicate node IDs resolve to scheduling-dependent coordinates

Reported by: altw-sparse, altw-external, apply-locations.

Sparse ALTW stores duplicate IDs with relaxed atomics, last store wins (within
a blob the later tuple, across parallel blobs whichever runs last); external
stage 2 matching takes the first tuple; equal IDs pass the ascending checks. So
backends disagree and sparse is nondeterministic. apply-changes location mode
merges worker coordinate maps in slot order, not source order, while the order
guard permits equal boundary IDs (`drain.rs`). Not reproduced.

## BUG-010 - Negative IDs are dropped by IdSet-based paths

Reported by: altw-sparse, tags, spatial.

`IdSet::set` ignores negative values. Sparse ALTW reference collection and
coordinate population discard negative IDs although `osm_id.rs` defines signed
ordering and external's diagnostic implies sparse can join them; tags-filter
reference inclusion records matches through `IdSet`, so a matching negative-ID
element survives `-R` but disappears with reference inclusion; extract
membership (`IdSet` and multi-region membership) discards negatives. Not
reproduced.

## BUG-011 - Node-statistics histograms depend on worker scheduling

Reported by: check.

`WorkerAccum` (`inspect/node_stats.rs`) carries a partial 128-coordinate window
across whichever blobs a worker receives, so adjacency, buckets and window count
vary with scheduling; the parity test excludes histogram equality. Fixing needs
a contract decision.

## BUG-012 - inspect and check disagree on negative-ID order

Reported by: check.

Inspect's `TypeIdRange` (`inspect/types.rs`) compares signed numerically; ID
checking uses canonical OSM order, so `0, -1, -2, 1` is ordered for check but
non-monotonic for inspect, also affecting `data.objects_ordered`.

## BUG-013 - PBF-to-OSC emission drops visibility

Reported by: osc.

`push_info_attrs` (`osc/write.rs`) takes no visibility argument, so a created
or modified element with `visible=false` loses the attribute.

## BUG-014 - OSC attribute helpers skip trailing malformed attributes

Reported by: osc.

The attribute helpers (`xml_parse.rs`) return after finding the requested field,
so `<nd ref="1" broken/>` leaves `broken` unexamined; quick-xml start-event
construction does not validate attributes itself.

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

## BUG-023 - cat's blobs_decoded hides decompression

Reported by: copy.

Unindexed plain cat decompresses ordinary blobs to generate indexdata but counts
them as passthrough; `blobs_decoded` counts only rebuilt unclassifiable blobs.

## BUG-024 - Export keeps decoding after a write failure

Reported by: spatial.

The export callback records `first_err` and later callbacks return early, but
sequential `for_each` still reads and decompresses the rest of the input, so a
broken pipe can waste a planet traversal.

## BUG-025 - Extract strategies accept different extreme coordinates

Reported by: spatial.

Simple's raw node scanner uses checked scaling; columnar and element accessors
use wrapping arithmetic and narrowing, so extreme framing values fail in one
strategy and produce coordinates in another.

## BUG-026 - Geocode candidate and ranked APIs break equal-area ties differently

Reported by: geocode.

`Candidates::into_result()` keeps the first equal-area admin match; ranked lookup
picks the lower polygon index; an index with equal-area polygons in reverse
order makes them disagree. Code reading, not executed.

## BUG-027 - Geocode building placement is a vertex mean

Reported by: geocode.

Building address placement averages resolved way coordinates including the
repeated closing vertex, can fall outside a concave building, and averages near
longitude zero across the antimeridian. Flagged to preserve during rewrites
unless placement semantics change.

## BUG-028 - Geocode query bench can be optimized away and measures warm uniform points

Reported by: geocode.

`crates/bench/src/geocode.rs` measures serial queries over a repeated, warmed,
uniform-in-bbox set, so it says nothing about cold pages, address-heavy load or
CLI throughput, and its optimizer sink consumes only `admin.len()`.

## BUG-029 - Scan helpers that look complete but are not

Reported by: indexdata.

`blob_filter_check` (`scan/classify.rs`) accepts a `BlobFilter` but ignores its
bbox; `parallel_scan_blobs_raw` does not enforce the schedule's kind claim.
Callers compensate today; the hunter calls these API hazards, not demonstrated
wrong results.
