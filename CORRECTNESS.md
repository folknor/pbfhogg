# pbfhogg correctness notes

Parser/encoder edge cases and data representation limits that are accepted by
design. For intentional behavioral differences from osmium, see
DEVIATIONS.md.

## Repeated scalar fields: one non-empty wire occurrence

**Status:** Packed encoding and a single unpacked value are supported.
Multiple data-bearing occurrences of the same field are rejected.

**Context:** `take_packed` in `crates/lib/src/read/wire.rs` accepts both a
length-delimited packed segment and an individually tagged varint. A single
unpacked value is retained as its original varint byte slice, so accepting it
requires no per-element allocation or change to the packed iterators. This
applies to the repeated scalar fields in `WireNode`, `WireWay`,
`WireRelation`, `WireDenseNodes` and `WireDenseInfo`.

Protobuf also permits several occurrences, with concatenation semantics.
The borrowed-slice representation cannot join discontiguous segments, so a
second non-empty occurrence is a hard error naming the field. Empty packed
segments are ignored in either position. A field written as several
unpacked values therefore errors rather than silently dropping values.

The singular embedded metadata messages follow the same rule
(`take_message` in `crates/lib/src/read/wire.rs`): an element's `Info` and a dense
group's `DenseInfo` may appear once with data, plus any number of empty
occurrences. Protobuf merges repeated occurrences of a singular message, which
one borrowed slice cannot express; keeping the last occurrence, as the parsers
once did, dropped fields only the earlier one set and let a malformed earlier
`Info` pass block validation unseen. A second data-bearing occurrence is
therefore a hard error naming the field. A block's `PrimitiveBlock.stringtable`
takes the same rule (`take_string_table`), shared by the decoder, the
indexdata scanner (`blob_meta::scan`), renumber's wire splice and both
add-locations-to-ways way reframers, so every walk that selects a table
selects the same one; keeping the last table had resolved tags against the
wrong strings. The `PrimitiveGroup.dense` message itself is stricter and
refuses any repeat (`WireGroup::dense`), and so does the header's
`HeaderBlock.bbox` (`WireHeaderBlock::parse`), whose one occurrence must also
carry all four required coordinates; DEVIATIONS.md "PBF decoding: the header
bbox appears at most once, complete" records why.

**Remaining compatibility limit:** Legal protobuf messages that split an
array across data-bearing occurrences, or a metadata message across
data-bearing occurrences, are refused. Supporting them would
require a different representation or an allocated fallback. The frame and
block error contracts, including the byte-level rewriters' handling of
these forms, are described in
[reference/truncation-handling.md](reference/truncation-handling.md).

## Tag string indices are resolved at block construction

**Status:** Fixed. A tag key or value string index past the block's string
table is refused when the block is built.

**Context:** Tags and relation member roles are indices into the block's
string table. `validate_block` (`crates/lib/src/read/block.rs`) bounds every
plain `keys` / `vals` index and every dense `keys_vals` key and value index
against the table, in the same scan that checks their width, and refuses one
past it as `StringtableIndexOutOfBounds`. Every `PrimitiveBlock` and every
validator projection goes through it, so on an accepted block every tag
lookup resolves: `TagIter` and `DenseTagIter` treat a failed lookup as a
broken construction invariant and panic in release, and there is no
fallible tag iterator. Earlier, an out-of-range index ended the infallible
iterators' list early, so repack, cat, degrade, sort, the filters, extract,
export, the geocode builder and ALTW could write an element with fewer tags
or select on a shortened list; that is now a refusal of the block. The raw
blob metadata scanner (`crates/lib/src/blob_meta/scan.rs`) applies the same
key and value bound, so indexdata generation and `check --index` refuse what
a decode refuses.

The claim covers accepted blocks and validator projections, not every byte
of a file: a blob copied without decoding (raw passthrough, diff's
byte-equal blob fast path) is not checked.

Member roles and user names are not resolved at construction.
`RelMember::role` and the `user` accessors return a `Result`, and every path
that writes them propagates it: the re-encoding commands (through the public
`Relation::members_into`, and `element_metadata` and `dense_node_metadata` in
`crates/lib/src/owned.rs`, which the owned-element conversions share), the
`bench-write` instrument and the OSC writer refuse a bad role or user index
with an error naming the element, rather than writing an empty string.
`inspect` shows a bad role as `<invalid>`. `build-geocode-index` refuses a bad
role on a way member of a boundary candidate, since the role decides whether
that way is an outer ring, a hole or auxiliary; it does not read roles of node
or relation members or of relations it does not select, so a bad index there
passes. The check is on the decoded index, after the documented signed-32
narrowing of the varint. Element content for
`diff` and `digest` (`crates/lib/src/content.rs`) resolves roles through
that `Result`: a bad role is never silently equal to anything.

`apply-changes` copies the surviving elements of a blob it rewrites with their
string indices unresolved, into an output block whose string table starts as
the source block's. A tag index past the source table is refused when the
source block is built; the raw copy checks every role and user index it
copies against the source block's table and refuses one past it, naming the
element, the field and the index: such an index would otherwise name one of
the strings the diff's own elements add behind the copied table. It also
refuses a rewritten block whose string table does not begin with the empty
string. Blobs it copies unchanged are not decoded and not checked. When an
output block overflowed the blob limit and is written again, its survivors'
blocks hold only the source strings they use, and each copied index is
translated to its slot there; it is checked against the source block's table
first, with the same refusals.

## Osmosis -1 sentinel for absent metadata

**Status:** Fixed. Normalization split across parse-time and write-time boundaries.

**Context:** Osmosis writes `-1` for version and changeset when metadata is absent
([libosmium#247](https://github.com/osmcode/libosmium/issues/247)). The protobuf
default for these fields is 0, but Osmosis explicitly encodes -1 as a sentinel
meaning "no data." Without normalization, pbfhogg round-trips `-1` as a real version
number, which is semantically wrong - downstream tools may interpret it as a genuine
historical version.

**Fix strategy - two-tier normalization:**

1. **Non-dense elements (Node, Way, Relation):** Normalized at parse time in
   `WireInfo::parse` (`crates/lib/src/read/wire.rs`). After the field loop, `version == Some(-1)`
   and `changeset == Some(-1)` are mapped to `None`. This covers both the library API
   and all command paths with zero additional overhead - the parse loop is already
   branchy, and two comparisons on values in registers are invisible.

2. **Dense nodes:** Read at the claim and conversion boundaries only. `DenseNodeInfo`
   stores `version: i32` and `changeset: i64` as plain non-optional values decoded
   from packed arrays in the dense node iterator - the tightest loop in the library
   (~8 billion iterations for planet). Changing these to `Option` would add per-element
   overhead on a path where every nanosecond matters. Instead, the sites that
   bridge dense reads to outputs read -1 as libosmium does (version -1 is no
   version, changeset -1 is 0), and keep the slot's other fields:
   - `dense_node_metadata`, `dense_node_raw_metadata` and `read_dense_node` in
     `crates/lib/src/owned.rs` write both as 0 (a slot is never dropped for
     its version)
   - every printing output, through `DenseNodeInfo::claims`

**Consequence for library users:** Code consuming the public `DenseNodeInfo` API
directly (not through pbfhogg's commands) will still observe raw `-1` values from
Osmosis-generated PBFs. This is documented on the `DenseNodeInfo` struct. Library
users who need to handle Osmosis input should check for `-1` themselves, or use
`claimed_version()` (below). The tradeoff is accepted: the dense iterator is too
hot to add branches for a single producer's non-standard encoding.

### Version 0 is not a version claim either

OSM versions start at 1, and the writers spell "no version" as 0. pbfhogg's
`BlockBuilder` stores version 0 with zeroed fields in the `DenseInfo` slot of a
node added without metadata to a block whose other nodes carry it (the arrays
must stay aligned), and writes version 0 for a versionless element that carries
other metadata (an apply-changes OSC element with a timestamp but no version).
libosmium does the same: it writes 0 for any versionless object, reads both 0 and
-1 back as its own "no version", and its XML writer omits a zero version. The
encoding is left alone; outputs that print metadata read it instead:

- **A version is printed only when it is 1 or more**, on `Info` and `DenseInfo`
  alike: OSC from `diff --format osc` (creates, modifies and deletes, so a
  metadata-less node no longer deletes as `version="0"`, or `version="1"` under
  `--increment-version`), the text diff's `vN`, `inspect --show`, the
  `inspect --extended` metadata census (which counts a version only when it is
  1 or more) and `export --metadata`. `Info::claimed_version()` and
  `DenseNodeInfo::claimed_version()` are the public form of the rule; `version()`
  still returns the raw value.
- **The other fields of an `Info`** keep their own presence, zeros included.
- **The other fields of a `DenseInfo` slot** depend on the group's columns and
  the slot's version, because a packed column cannot mark one slot absent. A
  column the group lacks is never claimed (libosmium writes any subset of
  columns); `visible` reads true without its column. Version 1 or more: every
  present field is claimed as decoded, a changeset of -1 as 0. Below 1 (0, the
  Osmosis -1, or any other negative value; the slot is never dropped):
  a field counts only with a value no padding slot holds - non-zero timestamp,
  positive changeset and uid, a non-empty user, `visible` false (the first four
  are the value tests of libosmium's `detect_available_metadata`). So the
  padding slot prints nothing, and a real versionless node keeps its non-zero
  fields. A zero field in such a slot (an anonymous `uid` 0, say) is dropped: on
  the wire it cannot be told apart from padding.

Re-encoding keeps what these rules read: the conversions that feed a rewrite
keep every dense slot, its fields and its group's columns (a complete group is
written with all six), writing a `-1` version or changeset as 0, so the
output claims what the input did, apart from sub-second timestamps. At the
default `date_granularity` of 1000 the stored timestamp is copied verbatim,
so even one whose millisecond scaling overflows reads back as the same
refused claim. Only a block at another granularity is converted: scaled
exactly, rounded down to whole seconds, refused where the scaling overflows
or the rounded second no longer fits i64 milliseconds
(`output_timestamp_seconds` in `crates/lib/src/owned.rs`). Rounding can
turn a versionless slot's non-zero claim into the 0 that reads as padding;
such a node is written plain with an explicit 0, so the claim survives.
`--clean version` does the same for a versioned slot: the columns it keeps
are claims zeros included, so a node holding a kept 0 timestamp, a
changeset or uid of 0 or below, or an empty user is written plain with
those fields explicit; a cleaned field is claimed by neither encoding
(held as padding when dense, omitted when plain), and `visible` true is
not kept once the version goes (`DenseMetadata::explicit` in
`crates/lib/src/write/block_builder.rs`, set by the conversions and by
`clean_metadata`). Coordinates of a rebuilt block are truncated toward zero to
decimicrodegrees under the same rule; docs/cli/commands.md "PBF rebuild
precision" states the user-facing contract. A plain `Info` is
re-encoded with each field present exactly when it was decoded present
(`element_metadata` in `crates/lib/src/owned.rs`): the `-1` version or
changeset stays absent, the other fields stay as carried. A node is written
dense with all six columns when its metadata is complete with a version of 1
or more; a partial one is written dense, with exactly its present fields as
columns, only when the slot rules above read it back as the plain `Info`
reads it and its run is smaller dense (`partial_columns` and
`resolve_optional_run` in `crates/lib/src/write/block_builder.rs`). So a
plain version-0 node with an explicit zero field stays a plain node.

The `inspect --extended` metadata census follows only the version rule above.
It counts, it does not print, so its other fields follow the next section.

## Metadata coverage in inspect --extended

The census ("All objects have" / "Some objects have", JSON `metadata`, `-g
metadata.*`) counts the objects whose field carries an available value, as
`osmium fileinfo -e` does through libosmium's `detect_available_metadata`. Each
field is judged on its own value, on plain `Info` and every `DenseInfo` slot
alike and whatever the version: version 1 or more, timestamp non-zero, changeset
and uid positive, a user name that resolves to a non-empty string. Field
presence does not count: a plain `Info` carrying uid 0 or an empty user name has
no uid or user here, although the printing outputs above show them. The Osmosis
`-1` slot is not skipped: its version fails the test by value, and a real
timestamp, changeset, uid or user it carries still counts, as in libosmium.
The data timestamp range is fed by every non-zero timestamp the census counts.
Each timestamp is scaled to milliseconds exactly: one whose scaling by the
block's `date_granularity` overflows `i64` refuses the blob, as every output
judging a claimed timestamp refuses it, rather than wrapping to 0 (left out
of the coverage) or to an arbitrary date in the range. Dense delta sums still
wrap, as every reader reconstructs them; only the scaling is checked.

Boundaries against `fileinfo`, all deliberate:

- **Unresolvable user index:** an index past the string table counts as no
  user (libosmium refuses the file). "No" therefore means "not established":
  the census cannot tell empty names from broken references.
- **Timestamps are judged in milliseconds:** a sub-second non-zero timestamp
  counts, and the printed range keeps the exact milliseconds (`.SSS` after
  the second holding the instant) when an extreme is not a whole second; libosmium truncates to whole
  seconds first.
- **User names:** "non-empty" is byte length; a name starting with NUL counts
  (libosmium tests the first byte).
- **Values libosmium refuses** (version below -1, changeset below -1 or at
  least 2^32-1) are accepted by pbfhogg and judged by the same tests.
- **Empty input:** with no objects, "All objects have" is `(none)`; fileinfo
  reports every field.
- **Timestamp range** excludes zero; fileinfo's first timestamp becomes the
  epoch when any object lacks one.

## Null Island ambiguity in coordinate indexes

**Status:** Fixed. This class is closed - no index in the crate spells absence
as an all-zero coordinate any more.

**Context:** An index that stores node coordinates as `(lat: i32, lon: i32)`
in a zero-initialized backing store is tempted to spell "unset" as `(0, 0)`.
That is Null Island, a real coordinate in the Gulf of Guinea, so the
sentinel collides with data in both directions: a node genuinely at `0, 0`
reports as missing, and a missing node is written into a way as if it were
there.

Every flat, rank-indexed coordinate store in the crate (the
`add-locations-to-ways` sparse store and the geocode builder's Pass 2 store
among them) shares one slot encoding, defined once in `crates/lib/src/coord_pack.rs`:
latitude biased by 90 degrees, which puts every legal latitude below `2^31`
and leaves bit 31 of the low word free as an explicit presence flag, with
longitude verbatim in the high word. An untouched slot still reads as absent
because the backing store zero-fills, but no coordinate can set that bit by
accident, and a coordinate outside the geographic domain is refused rather
than stored as some other place. No bitmap, no extra memory, exact across the
whole domain. Because the encoding is defined once, its users cannot drift
apart; its layout, domain and Null Island behaviour are pinned by unit tests
beside it.

The `external` ALTW backend reaches the same answer differently: stage 2
counts a reference as resolved when its id was *found* in the id-sorted node
stream, not when the coordinate it found was non-zero. Before this the two
backends disagreed about the same input.

The end-to-end behaviour is pinned by
`null_island_real_node_is_not_reported_missing` in
`crates/cli/tests/cli_add_locations_to_ways.rs`, which runs on both backends, and by
`null_island_street_vertex_is_not_treated_as_absent` in
`crates/lib/tests/geocode_index.rs`.

The one remaining all-zero sentinel of this shape, the blob index's "no
bbox", was also retired: `(0, 0, 0, 0)` is the legal bbox of a single-node
blob at Null Island, so index v3 spells absence as an inverted box that no
well-formed bbox can equal. v2 files keep their original reading, because
that is what their writer meant.

**Related:** unresolvable way node references are a hard error in both
`add-locations-to-ways` and `apply-changes --locations-on-ways` rather
than a silent `(0, 0)` fill; pass `--ignore-missing-nodes` to accept the
placeholders (only under that flag does the placeholder ambiguity
reappear, now opt-in). That filler is opt-in, counted, and gated by a
check that refuses before anything is published (add-locations-to-ways
counting references, the sparse index after its write and the external
join right after its node join, before it encodes any way; the library
reaches either backend only through `add_locations_to_ways`, so no entry
point skips the check; apply-changes at its node-to-way
barrier, counting distinct nodes), so it is a deliberate output rather than
an ambiguous sentinel. The first withdrew a documented osmium deviation
and the second added one - see `DEVIATIONS.md`.

## Geocode index: interpolation unresolved-endpoint sentinel

**Status:** Known, accepted. Documented in code.

**Context:** `SlimInterpWay` (the in-memory staging struct that flushes to
`interp_ways.bin` as `InterpWay`) stores resolved interpolation house
numbers in `start_number: u32, end_number: u32`. The resolver
(`resolve_interpolation_ways_mmap` in `crates/lib/src/geocode_index/builder/interp.rs`)
runs after Pass 2 and takes each node's number from the addr points on
the way's street or place at exactly that node's coordinate (ADR-0024), splitting
the way into one record per section between address nodes.
Matching rejects house number 0;
only positive matched numbers overwrite `start_number` and `end_number`.
If any part of the way fails, it stays one record with both fields at
their initial `0` (an end with no match, disagreeing coincident numbers,
a number that is not a positive decimal, equal or wrong-parity numbers,
no single street or place, a missing node, an unsupported `addr:interpolation`
mode).

**Impact:** A 0 at either end means unresolved. Since format version 10
an unresolved way is counted in the build diagnostics
(`geocode.pass2.interp_unresolved`) but written to no interpolation record
or cell entry, so it can never answer; the reader still applies the same
zero rule to any record it reads (`InterpolationType::answers` in
`crates/lib/src/geocode_index/format.rs`). House number 0 is therefore
unsupported as an interpolation endpoint. "0" as a house number is rare but
exists in some regions.

**Why not fix:** The clean fix is a `resolved: bool` field persisted into
`InterpWay`, allowing zero-valued matches, and a `FORMAT_VERSION` bump.
Format version 5 (`addr:place`, numeric steps) did not take it: no
workload has needed house number 0 as an endpoint, so the field waits for
one. Fix shape is captured
at the struct comment in `SlimInterpWay`.

## Geocode index: u16 on-disk count caps (hard error on overflow)

**Status:** Enforced. Builder hard-errors if any cap is exceeded.

**Context:** The geocode on-disk format uses `u16` count fields in several
places:

- Per-cell entry counts for street / addr / interp segments
  (`street_entries.bin`, `addr_entries.bin`, `interp_entries.bin`) and
  for admin polygons (`admin_entries.bin`). Every entry list shares one
  framing, owned by `crates/lib/src/geocode_index/format.rs`: a `ListCount` (u16)
  prefix read by `Table::list`, with list sizes from `list_byte_len`.
- Per-way node counts in `StreetWay.node_count` and
  `InterpWay.node_count` (`crates/lib/src/geocode_index/format.rs`).

The builder (`crates/lib/src/geocode_index/builder/`) used to silently truncate
oversized cells/ways to `u16::MAX`, producing silently-incomplete output.
It now hard-errors at the write site via `u16::try_from(...)`, naming the
offending cell or way in the error message.

**Impact:** A build fails instead of quietly losing data. Unreachable
today in practice:

- Per-cell entry caps are well above observed density at street level
  17 (cells 46 to 83 m wide) and coarse level 14 (367 to 663 m), even
  in dense urban areas.
- Per-way `node_count` is well above the OSM convention cap of 2 000
  refs per way.

**Why hard-error instead of bumping to u32:** The format version change
costs on-disk growth and forces regeneration of existing indexes; a
hard error is a strictly better signal than silent truncation until a
real workload actually hits the cap. If that happens, the error message
names the exact field and the migration path: bump the count to `u32`
and increment `FORMAT_VERSION` in `crates/lib/src/geocode_index/format.rs`.

## `BlobHeader.datasize` cap (hard error before allocation)

**Status:** Enforced. Reader hard-errors before allocating an oversized
compressed body.

**Context:** `BlobHeader.datasize` is the producer-declared on-wire length of
the serialized (usually compressed) `Blob` message that follows the header -
the number of bytes a reader preallocates and reads *before* any decompression.
The existing `MAX_BLOB_MESSAGE_SIZE` (32 MiB) caps the *decompressed* block
content, but it only fires after the compressed body has already been read into
memory. A hostile or corrupt `datasize` therefore drove the pre-decompression
body allocation (`Vec::with_capacity(datasize)` in `BlobReader::next`,
`vec![0u8; frame_len]` in `read_raw_frame`, and consumer `pread_data` calls
against `HeaderWalker` metadata) to an arbitrary size ahead of the 32 MiB
guard - a memory-amplification vector on every command that reads PBF input.

**Fix:** `MAX_BLOB_DATASIZE` (`crates/lib/src/read/blob_wire.rs`), set to 32 MiB (defined
as an alias of `MAX_BLOB_MESSAGE_SIZE` to keep the relationship explicit),
rejects the declared size at the boundary with the typed
`BlobError::DataSizeTooBig` before any allocation. This is **not** a written
format limit: the OSM PBF spec caps only the *uncompressed* block (32 MiB) and
the `BlobHeader` message (64 KiB), and says nothing about a serialized-`Blob`
`datasize` ceiling. The bound is the *de facto interoperability limit* - the
reference reader (OSM-binary, `FileBlockHead`) applies a single 32 MiB
`MAX_BODY_SIZE` directly to `BlobHeader.datasize` and rejects anything at or
above it. Mirroring that reader exactly means pbfhogg accepts no declared size
above what the reference reader accepts. The lower bound matches libosmium: an
absent or zero `datasize` is refused for every blob type. A consequence worth stating: a blob carrying its
content uncompressed in `raw` at exactly the 32 MiB content cap serializes to a
few bytes over 32 MiB (protobuf framing plus the `raw_size` hint), so it too is
rejected by the reference reader - interoperable files cannot carry such a blob.
`MAX_BLOB_DATASIZE` (declared-datasize cap) and `MAX_BLOB_MESSAGE_SIZE`
(decompressed-content cap) denote different things but coincide at 32 MiB
because the reference reader enforces one number on both fields.
`Frames::read_header` in `crates/lib/src/read/frame.rs` checks the declared
size once per frame before exposing its payload length. Both `BlobReader`
and `HeaderWalker` use this state machine for frame reads and skips.

**Impact:** Well-formed files are byte-identical - the guard only trips at the
cap, which no interoperable blob reaches. The single observable change is a
typed `DataSizeTooBig` error on an adversarial or corrupt declared datasize, in
place of an outsized allocation followed (for a payload that never arrives) by a
truncation error.

**Why hard-error instead of clamping:** Matches the repo's
hard-error-over-silent-corruption posture (`reference/truncation-handling.md`)
and the sibling `MAX_BLOB_HEADER_SIZE` / `HeaderTooBig` cap. A declared size at
or past the reference reader's own bound is a corrupt or hostile (or simply
non-interoperable) input; refusing it is strictly better than reading a partial
body or aborting on a failed allocation.

## `sort`: intra-blob disorder

**Status:** Fixed for every input that does not declare `Sort.Type_then_ID`
(non-indexed AND indexed). For inputs that do declare it, intra-blob
sortedness is a precondition of the header claim itself, documented below.

**Context:** `pbfhogg sort` is a blob-level permutation sort: pass 1 indexes
each blob's `(element_type, min_id, max_id)`, pass 2 raw-passes-through blobs
whose ranges do not overlap a neighbour and decode-merges the ones that do.
Both passes assume every blob is *internally* sorted - nothing decoded the
elements to check. A file whose blobs are internally UNSORTED but whose
`(min_id, max_id)` ranges are disjoint therefore slipped past the overlap
check: `sort` emitted a byte-identical copy stamped `Sort.Type_then_ID`,
silently corrupting the sorted invariant. `degrade --unsort-intra` produces
exactly this shape (one internal ID inversion per kind, no cross-blob range
overlap); composed with `--strip-indexdata` it yields the non-indexed variant
that reaches the pass-1 fallback.

**Fix:** Pass 1's payload fallback already preads, decompresses, and scans
every element ID to derive the blob's `(min_id, max_id)`. `scan_block_ids`
grew a checked twin (`scan_block_ids_checked`) that tracks intra-blob
monotonicity during that same scan - one canonical-OSM-order compare per
element, so effectively free where the payload is decoded anyway. A blob
found internally out of order is flagged and routed into pass 2's decode +
re-encode path exactly as an overlapping blob would be, so the sweep-merge
reorders its elements and the output is genuinely sorted. Sorting is the
command's job, so detected disorder is *handled*, not errored. The
monotonicity check uses `osm_id_cmp`, so blobs in canonical negative-ID
order (`-1, -2, -3, ...`) are not false-flagged.

**Which blobs get the payload check - keyed on the header claim, not on
indexdata:** Indexdata presence does NOT imply an internally sorted payload.
`cat` attaches indexdata to arbitrary third-party blobs without reordering
them (`crates/lib/src/commands/cat/mod.rs`, reframe path), and
`PbfWriter::write_primitive_block` indexes caller-provided blocks as-is - so
"indexdata is pbfhogg-written, hence sorted" is false: an unsorted
non-indexed file piped through `cat` yields indexed-but-unsorted blobs. Pass
1 therefore keys its trust on the input header's `Sort.Type_then_ID` claim,
the same format-level contract `ElementReader` keys its ordering guarantees
on:

- **Input declares `Sort.Type_then_ID` + blob has indexdata:** header-only
  classification, payload never decoded. This preserves the passthrough
  design (on declared-sorted input ~94% of wall time is already the
  writer-side `copy_file_range`; a mandatory decode would defeat the point).
- **Everything else** (no indexdata, or indexdata without the header claim):
  pread + decompress + checked ID scan. For the indexed-unclaimed case sort
  prints a one-line stderr notice that pass 1 is decoding payloads to verify
  intra-blob order; the freshly scanned index (not the stored indexdata) is
  used for range analysis. `degrade --unsort` / `--unsort-intra` clear the
  sorted claim, so their output - indexed or not - is detected and repaired,
  not passed through.

**`cat --dedupe` shares this pass 1** (`crates/lib/src/commands/blob_merge.rs`). Every
dedupe input must claim `Sort.Type_then_ID`, so only its blobs without
indexdata take the checked scan, and one found internally out of order is
rewritten through the sweep like an overlapping blob - which also drops any
exact duplicates inside it - rather than copied through under the sorted
stamp.

**The sweep reads a believed blob in order and refuses a descent.** The
overlap sweep (`crates/lib/src/commands/blob_merge/sweep.rs`) merges borrowed cursors
and emits each blob's elements in the order it reads them, so a blob placed
in an overlap run on its believed claim whose IDs descend internally is
refused as `Unsorted` when the descent is read, where the earlier owned-heap
sweep silently reordered it. Only a blob pass 1 scanned and flagged
internally unsorted (or restructured) is sorted before it is merged. Of two
copies equal in (type, id, version) the one from the earliest input, then
the earliest frame in that input, survives; a dropped copy is still
validated (its user name and member roles resolved) as a written one is.

**Residual - precondition on the header claim, and its repair:** By default
a file whose header claims `Sort.Type_then_ID` while its blobs are
internally unsorted is passed through undetected. That file already violates
its own declared contract; pbfhogg's own sorted producers (`sort`, `repack`,
`apply-changes`) uphold it, and trusting an explicit format-level declaration
is categorically different from inferring sortedness from the mere presence
of an implementation-detail sidecar field. `ElementReader` does not check the
claim either (it used to `debug_assert` node order, which panicked debug
builds on a lying file and checked nothing in release); the commands whose
answer depends on order check it on the elements they already visit
(apply-changes, `add-locations-to-ways --index-type external`, time-filter,
renumber, digest) and refuse the file as `Unsorted`. Such a file cannot be
expected to arrive without its claim, so the advice those refusals give is
`sort --distrust-metadata`, which takes the checked scan whatever the header
claims and rewrites the disordered blobs (reference/indexdata-trust.md, "The
remedy for `Unsorted`"); plain `sort` keeps the passthrough fast path.
Producers were deliberately left unchanged: indexdata (kind/range/count/bbox)
is valid for an unsorted blob, and every consumer other than sort's former
inference only uses it for range queries - stripping or gating it in
`cat`/`PbfWriter` would punish those consumers to protect an inference sort
no longer makes.

**Versions of one ID.** `sort` orders records of one ID by ascending version,
as osmium sort does (records of one ID and version keep input order; osmium
also orders those by timestamp, DEVIATIONS.md). The sweep writes every group
in that order, so a scanned blob must be held to the same order or the
answer would depend on whether the blob overlaps another: pass 1's ID walk
marks a blob whose IDs repeat without descending, and only for such a blob
`sort` reads the versions (`blob_meta::versions_ascend`) and rewrites the
blob when one descends. Input without a repeated ID pays one more branch
per element and nothing else. A version column pass 1 cannot read exactly
routes the blob to the rewrite, whose decoder decides. `cat --dedupe` does
not ask: it passes a lone ID-monotone blob through as stored. A believed blob
under the header claim is not decoded, so `sort` takes its versions to
ascend; the claim's name does not say so, and `--distrust-metadata` checks
it.

## Blob sidecar metadata: `indexdata` and `tagdata` are a trust boundary

**Status:** Documented and verifiable. Consumers believe these fields by
default, by design; `check --index` verifies them on demand.

The trust model - which claims are believed, what checks them in passing,
what verifies them, what a false one costs per command, and what a missing
or partial index does - is stated once, in
[reference/indexdata-trust.md](reference/indexdata-trust.md). The decision is
ADR-0013.

## OSC compatibility and emission contracts

Input detection for `tags-filter` deliberately reads only the first four bytes.
Gzip magic or an initial `<` selects OSC; a positive big-endian header size below
1000 selects PBF. If sniffing fails, `.osc`, `.osc.gz` and `.osc.bz2` select OSC;
every other extension selects PBF. This is detection, not a promise of bzip2
decoding. No BOM or whitespace skipping, or new ambiguous-format refusal, is
part of this contract. The help text retains the extension-default description.

Every OSC consumer enforces one element hierarchy, in one driver: the root is
`<osmChange>`; under it, `<create>` / `<modify>` / `<delete>` sections and
unrecognised leaf elements (ignored; any element inside one is refused);
sections contain only `node`, `way` and `relation`; an object contains only
`tag`, `nd` (ways), `member` (relations) and `bbox` / `bounds` (ways and
relations, ignored); those children contain no elements; nothing follows the
root. Anything else is refused, naming the misplaced element. This follows
libosmium's section and object-child structure; the root-level differences are
in DEVIATIONS.md. Text and comments are ignored wherever they appear, except
for the character check below.

The PBF-to-OSC producer (`diff --format osc`) writes only values the overlay's
native parsing accepts, and refuses an element it cannot write. Node
coordinates come from the exact accessors. A value that does not fit `i32`
decimicrodegrees is a wire-format refusal, and one outside +-90 / +-180
degrees on the very `i32` printed is invalid data. Timestamps are scaled
exactly, and a scaling that overflows `i64` milliseconds is a wire-format
refusal. The scaling is the claim rules' own (every metadata output, export,
`digest --form full`, `inspect --show` and time-filter among them, reads the
same exact claim), so an overflow is never read as a wrap to zero, which a
versionless dense slot would take for padding. Timestamps print through `format_rfc3339_utc`,
whose domain is the parser's: years 0000 to 9999. The delete timestamp
override is held to the same domain as an invalid argument before either
input is opened. Deleted elements print only ID, version and the override, so
their old values are not judged.

Every OSC producer, derive and the re-emitters alike, writes only characters
XML 1.0's `Char` production admits. PBF strings are validated as UTF-8 only,
so a tag key or value, member role, user or other metadata text can
hold a C0 control other than tab, LF and CR, or U+FFFE / U+FFFF. None of
those can appear in an XML 1.0 document even as a reference. The one
user-text escape boundary (`osc::write::check_xml_chars`, behind both escape
helpers) refuses such a value as invalid data, naming the element and
attribute, before anything is published. Tab, LF and CR are written as
references. For PBF input this is the only check; for OSC input it is not
reached, because the shared reader refuses first.

Every OSC reader holds the whole input document to XML 1.0's `Char`
production, as libosmium's Expat does, on the one driver every route shares
(`check_event_chars` in `osc/xml_parse.rs`), checking each event before it is
dispatched. Earlier events, earlier parts of the same change included, may
already have reached a sink; the run still fails and publishes nothing, and no
filter predicate or simplify dedupe decides whether the refusal happens. A forbidden scalar written literally
anywhere (attribute, text, comment, CDATA, processing instruction, DOCTYPE),
or a character reference in an attribute value or text whose decoded value is
not a `Char`, refuses the document as invalid data. That covers attributes no
command reads (root, section, unknown) and changes a filter or simplify would
discard. A malformed numeric reference is refused the same way.
Reference-looking text that is not a reference stays legal: `&amp;#1;`, and
`&#1;` inside a comment, CDATA or processing instruction. A DOCTYPE carrying an
internal subset is refused outright (see DEVIATIONS.md); an external
identifier is never loaded. Input is decoded as UTF-8 whatever its
declaration says, and the check is on those scalars.

The same driver also holds the lexical rules quick-xml leaves unchecked and
Expat enforces. A reference in an attribute value (read or not) or in text
must be a valid character reference or name one of the five predefined
entities; an undeclared `&x;`, a bare `&` and a literal `<` in an attribute
value are refused, even where the document names an external DTD (see
DEVIATIONS.md). Attributes must be separated by whitespace, and an attribute
name no reader matched, or the name of an unrecognised root-level element,
must be an XML `Name` (Fifth Edition tables, a little wider than Expat's). A
DOCTYPE must be spelled `<!DOCTYPE`, come before the root, appear once, and
be a `Name` with an optional well-formed external identifier; its name need
not match the root. The XML declaration must be the first thing in the
document (after a BOM). Outside the root only whitespace, comments and
processing instructions may appear; `--` inside a comment, `]]>` in text and
a processing-instruction target that is not a `Name` or is `xml` in any case
are refused. Not checked: the declaration's own pseudo-attributes.

The re-emitting OSC paths (`merge-changes`, with or without `--simplify`, and
`tags-filter --input-kind osc`) deliberately parse version but pass timestamp,
changeset, uid and visible through as normalized XML attribute text without
native-value validation. They do not preserve raw entity spellings or raw
attribute whitespace, and all of them write the same normalized bytes for the
same element. The apply-changes overlay retains its native numeric metadata
parsing and treats only `false` as hidden, compared after the same XML
attribute normalization the re-emitters apply, so an entity-spelled `false` is
hidden whether a change is applied directly or after re-emission (`False` is
visible on both routes). apply-changes writes a snapshot, so a hidden create
or modify is recorded as a versioned delete of its object (its version claim
reaches the version gates like any delete's; a hidden node needs no
coordinates); the public overlay parser still stages it as a hidden upsert,
location required. The re-emitters accept a hidden node without coordinates
under the delete-section location rule (both or neither, each usable) and
keep its section and `visible`, so a retained coordinate-less hidden record
carries the same action, identity, version claim and visibility through
re-emission.
osmium apply-changes in snapshot mode likewise writes the winning record only
when it is visible. An empty `visible=""` is the attribute absent for
every consumer: it is no visibility claim and does not by itself give an
element metadata, so a change claims the same thing applied directly and after
re-emission. Every other metadata attribute a retained change was read with is
written back when present, empty included: an empty `user=""` stays a
blank-user claim (apply records it as a user distinct from none), and an empty
`uid`, `changeset` or `timestamp` is re-emitted, so apply refuses it after
re-emission exactly as it does directly. Applying the overlay's native metadata
validation to the re-emitting paths would reject attribute text they currently
preserve. A change the re-emitters discard (an earlier change simplify replaces,
a create or modify the filter does not keep) takes its unvalidated attribute
text with it. Every
`tag`, `nd` and `member` is read by one parser per child kind, whatever the
command: a missing role reads as empty, and a malformed role, an unknown member
type, or a missing `k`, `v` or `ref` is refused. Each child's attribute list is
read to its end, so an attribute quick-xml's checked iterator reports malformed
(no `=`, an unquoted or unterminated value) or duplicated is refused wherever it
sits, after the attributes read included; so is one on a start tag no sink reads
(the root, a section, `<bbox>` or `<bounds>`, an unrecognised root-level
element). Object start tags are read to the end by their metadata scan. The
lexical rules that iterator does not check are held as described above. The
children of an object in
`<delete>` get that same parse in the driver, so every command refuses the same
malformed delete; apply-changes then keeps only the delete's identity and
version, while the re-emitters keep and write the delete's body, as osmium
does.
A delete remains pending until a re-create is accepted; a rejected stale
re-create must not erase it.

Every OSC producer (`merge-changes`, `tags-filter --input-kind osc`,
`diff --format osc`) frames its output by name through
`pbfhogg::osc::output_is_gzip`: `.osc` is plain, `.osc.gz` is gzip, anything
else is refused before any work. Changing that rule changes the output
transport contract.
A re-emitter that keeps a change stream (default `merge-changes`,
`tags-filter --input-kind osc`) writes the changes it keeps in input order.
Apply admits an equal version and lets the later change win, so grouping by
action would put a same-version re-create before the delete it follows and
leave the object deleted. Only `--simplify`, which keeps one change per
object, may regroup.
Integer decimicrodegree conversion for diff text and OSC output deliberately
divides by `1e7`; multiplication can change rounding and therefore output bytes.
`diff --format osc -q` without `-o` deliberately errors with
`--output is required with --format osc`: the output requirement precedes
text-only flag validation.

## Renumber terminal output and hot path

Renumber deliberately writes no tagdata. It is a terminal step whose output is
not tag-filtered. Its payload-only pread and byte rewrite do not read tagdata
headers, parse key lists, or bounds-check copied key string indices. Corrupt
indices pass through as on the read side. Adding those operations changes the
historical output or adds per-blob or per-tag work to the measured hot path.
The one exception is off that path: when a rewritten block cannot be cut to
fit with its whole string table in every piece, the re-encoding cut
(`commands/split/reencode.rs`, shared with the history time-filter and the
sort / `cat --dedupe` sweep) reads every string reference of the block and
gives each piece a table of only the strings it uses, and only when every
element field is one the format defines and every index lies within the
table. Otherwise every piece keeps the whole table and every index as it is,
corrupt ones included.

The byte rewrite takes every element representation the library writes. A
node group is one `DenseNodes` or plain `Node` messages (written where a
dense slot cannot keep the metadata fields present); each is renumbered in
its own form, so plain stays plain, and both share the blob's order check
and numbering. A plain node's `id` must be one varint, as for ways and
relations. A group with no element field is zero nodes. A group holding
both plain and dense nodes is refused: it carries no indexdata, and `cat`
rebuilds it. A way with no `refs` field has an empty list, as the encoders
write it, and an explicitly empty `refs` comes out in that same form, so
renumber output renumbers again.

The rewrite emits only the fields it interprets, by one rule for node, way and
relation blobs: the string table, the element fields of each group, and the
block's granularity, date granularity and offsets. Every other PrimitiveBlock
or PrimitiveGroup field, the obsolete group `changesets` (field 5) and unknown
numbers alike, is dropped, as osmium renumber's re-encode drops them and as
every reader skips them. Keeping one verbatim could carry element IDs renumber
cannot rewrite, and a block-wide field would repeat into every piece of a cut
block. An element field of the blob's own kind that is not length-delimited is
refused, not skipped. Fields inside an element message are copied without
interpretation, so an extension field there that names an element ID keeps
the old ID; such a field also keeps a cut block's whole string table, since
it might name a string the cut cannot remap.

## Sidecar comparison contracts

The historical names `merge_scanner_to_drain_bytes_high_water` and
`merge_scanner_to_workers_bytes_high_water` deliberately describe the existing
largest-item estimates, not sampled queue occupancy. Renaming them breaks
brokkr comparisons across runs. All OSC parser callers emit
`merge_changes_decompress_ns` once per input file, with that file's time rather
than an accumulated command total, including the apply-changes overlay and
OSC tags-filter. There are no command-specific replacement counters.

## Coordinate domain checks

The general PBF readers and dense-node iterators expose coordinates without
checking geographic ranges. Their coordinate accessors (`decimicro_lat`,
`nano_lat` and kin on `Node`, `DenseNode` and inline way locations), the
node scanner and the columnar decode check only the representation,
exactly: a coordinate whose exact value does not fit the accessor's type
(`i32` decimicrodegrees, `i64` nanodegrees) is an error, never wrapped.
Every command that writes, compares or digests a decoded node goes through
these checked conversions (extract, cat, repack, sort and `cat --dedupe`,
degrade, tags-filter, time-filter, getid, getparents, apply-changes,
add-locations-to-ways, diff, digest and derived OSC), so none places or
compares a node at a wrapped coordinate. A blob copied without decoding is
not judged. `inspect --nodes` and its extended bbox alone keep the wrapping
conversion, as a report of what the stored values wrap to. Bbox parsing,
location joins, `check --locations`, the geocode builder and export have
separate domain checks; export judges each position on the seven-decimal
text it writes and skips and counts a node or way with one outside the
domain. This is a deliberate boundary, not a promise that reading a
node validates its geographic position.

The geocode builder admits every node by the rule `check --locations` uses:
the exact nanodegree value, before truncation, within +-90 latitude and
+-180 longitude inclusive; anything else fails the build.

Location joins in add-locations-to-ways and apply-changes judge the
truncated decimicrodegree value. An input less than 100 nanodegrees beyond
+-90 latitude or +-180 longitude can therefore be written at the limit,
while `check --locations` rejects the exact nanodegree input. The written
coordinate is valid. Exact join-time checking would require nanodegrees or
a remainder flag on the scanner's hot path; retain truncation unless a real
producer demonstrates a need for the extra information.
