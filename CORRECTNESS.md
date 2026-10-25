# pbfhogg correctness notes

Parser/encoder edge cases and data representation limits that are accepted by
design. For intentional behavioral differences from osmium, see
DEVIATIONS.md.

## Repeated scalar fields: one non-empty wire occurrence

**Status:** Packed encoding and a single unpacked value are supported.
Multiple data-bearing occurrences of the same field are rejected.

**Context:** `take_packed` in `src/read/wire.rs` accepts both a
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

**Remaining compatibility limit:** Legal protobuf messages that split an
array across data-bearing occurrences are refused. Supporting them would
require a different representation or an allocated fallback. The frame and
block error contracts, including the byte-level rewriters' handling of
these forms, are described in
[reference/truncation-handling.md](reference/truncation-handling.md).

## Tag iterators end early on an out-of-range string index

**Status:** Known, accepted for `TagIter` and `DenseTagIter`
(`wontfix(type-result-fallible)` in source). Fallible siblings exist, and
`diff`, `diff --format osc` and `digest` use them.

**Context:** Tags and relation member roles are indices into the block's
string table. `str_from_stringtable` (`src/read/block.rs`) bounds-checks every
lookup and returns `StringtableIndexOutOfBounds` on a miss, but the infallible
tag iterators cannot surface it: `TagIter` (`src/read/elements.rs`) and
`DenseTagIter` (`src/read/dense.rs`) implement
`Iterator<Item = (&str, &str)>`, and `get_stringtable_key_value` discards the
error as `None`. A tag with an out-of-range key or value index therefore ends
the element's tag list early, silently dropping it and every tag after it.

`tags_checked()` on `Node`, `Way`, `Relation` and `DenseNode` returns the
fallible sibling (`CheckedTagIter`, `CheckedDenseTagIter`), which yields an
`Err` for the bad tag instead. Element content for `diff` and `digest` is
defined once, in `src/content.rs`, on these checked iterators and on
`RelMember::role`'s `Result`. A corrupt element there is never silently equal
to anything: the block-pair paths compare it as unequal ("modified"), the
streaming path's owned conversion errors, and `digest` refuses the file. The
one exception is diff's byte-equal blob fast path, which reports two
byte-identical blobs as common without decoding either.

Member roles are different at the API level - `RelMember::role` returns a
`Result` - but most command consumers outside diff collapse the error with
`role().unwrap_or("")`: the owned-element conversions and the re-encoding
paths (`cat`, `repack`, `extract`, `tags-filter`, `getid`, `getparents`,
`time-filter`, `altw`, `degrade`, the OSC writer). A bad role index is
rewritten there as an empty role. `inspect` shows it as `<invalid>`.

`validate_groups_strict` (`src/read/block.rs`) does not close the gap: it is a
structural pass that checks array framing and parallel-array lengths without
resolving any string index, because resolving them would mean decoding every
packed tag array up front. A tag array that ends mid-varint, or whose keys
and values disagree in count, is rejected there; a well-formed index that
points past the string table is not.

**Not a performance trade:** the bounds check runs on every lookup either way.
Only the error is thrown away. The constraint is the iterator's item type.

**Impact:** Wrong output rather than an error, but only for input whose string
indices point past the table - a producer bug, not something well-formed PBFs
contain. Random corruption of a zlib blob fails the stream's checksum at
decompression before any index is resolved.

**Where it must not apply:** code that has to refuse corrupt input outright
uses `tags_checked()` and matches on `RelMember::role`'s `Result`, rather than
going through the infallible tag iterators or `role().unwrap_or("")`. Library
users who need the same strictness should do likewise, and treat `tags()` as
best-effort on untrusted input.

**Fix approach for the remaining consumers (if ever needed):** move each
re-encoding path to `tags_checked()` and a propagated role error; or resolve
every index in `validate_groups_strict` behind an opt-in strictness flag.

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
   `WireInfo::parse` (`src/read/wire.rs`). After the field loop, `version == Some(-1)`
   and `changeset == Some(-1)` are mapped to `None`. This covers both the library API
   and all command paths with zero additional overhead - the parse loop is already
   branchy, and two comparisons on values in registers are invisible.

2. **Dense nodes:** Normalized at write/conversion boundaries only. `DenseNodeInfo`
   stores `version: i32` and `changeset: i64` as plain non-optional values decoded
   from packed arrays in the dense node iterator - the tightest loop in the library
   (~8 billion iterations for planet). Changing these to `Option` would add per-element
   overhead on a path where every nanosecond matters. Instead, the four conversion
   sites that bridge dense reads to writes guard against -1:
   - `dense_node_metadata`, `dense_node_raw_metadata` and `read_dense_node` in
     `src/owned.rs`
   - `convert_node` in `src/osc/merge_join.rs`

**Consequence for library users:** Code consuming the public `DenseNodeInfo` API
directly (not through pbfhogg's commands) will still observe raw `-1` values from
Osmosis-generated PBFs. This is documented on the `DenseNodeInfo` struct. Library
users who need to handle Osmosis input should check for `-1` themselves. The tradeoff
is accepted: the dense iterator is too hot to add branches for a single producer's
non-standard encoding.

## Null Island ambiguity in coordinate indexes

**Status:** Fixed in `add-locations-to-ways` and in the geocode index
builder.

**Context:** An index that stores node coordinates as `(lat: i32, lon: i32)`
in a zero-initialized backing store is tempted to spell "unset" as `(0, 0)`.
That is Null Island, a real coordinate in the Gulf of Guinea, so the
sentinel collides with data in both directions: a node genuinely at `0, 0`
reports as missing, and a missing node is written into a way as if it were
there. All-zero as an absence sentinel is the same mistake indexdata v3
already corrected by spelling an absent bbox as an inverted box.

Fixed sites:

- `SparseArrayIndex` (`src/commands/altw/sparse.rs`) stores latitude biased
  by 90 degrees, which puts every legal latitude below `2^31` and frees bit
  31 of the low word as an explicit presence flag. The slot is still eight
  bytes and `set_len` still zero-fills, so an untouched slot still reads as
  absent - but no coordinate can set that bit by accident. No bitmap, no
  extra memory, exact across the whole domain.
- External-join stage 2 (`src/commands/altw/external/mod.rs`) counts a slot
  as resolved when its id was *found* in the id-sorted node stream, not when
  the coordinate it found was non-zero. Before this the two backends
  disagreed about the same input.

- The geocode builder's Pass 2 coordinate store
  (`src/geocode_index/builder/pass2.rs`) uses the same `coord_pack`
  encoding as the sparse store, so a street vertex at Null Island is kept
  rather than read as an unwritten slot.

The altw sites are pinned by `null_island_real_node_is_not_reported_missing`
in `cli/tests/cli_add_locations_to_ways.rs`, which runs on both backends; the
geocode site by `null_island_street_vertex_is_not_treated_as_absent` in
`tests/geocode_index.rs`.

**Related:** unresolvable way node references are a hard error in both
`add-locations-to-ways` and `apply-changes --locations-on-ways` rather
than a silent `(0, 0)` fill; pass `--ignore-missing-nodes` to accept the
placeholders (only under that flag does the placeholder ambiguity
reappear, now opt-in). The first withdrew a documented osmium deviation
and the second added one - see `DEVIATIONS.md`.

## Geocode index: interpolation unresolved-endpoint sentinel

**Status:** Known, accepted. Documented in code.

**Context:** `SlimInterpWay` (the in-memory staging struct that flushes to
`interp_ways.bin` as `InterpWay`) stores resolved interpolation house
numbers in `start_number: u32, end_number: u32`. The resolver
(`resolve_interpolation_endpoints_mmap` in `src/geocode_index/builder/interp.rs`)
runs after Pass 2 and matches each endpoint against nearby addr points
with the same street name. Endpoint matching rejects house number 0;
only positive matched numbers overwrite `start_number` and `end_number`.
If either endpoint fails to match, both fields stay at their initial `0`.

**Impact:** A 0 at either end means unresolved, so the way yields no
interpolated number
(`interpolation_resolvable` and `interpolated_number` in
`src/geocode_index/reader.rs`). House number 0 is therefore unsupported as
an interpolation endpoint. "0" as a house number is rare but exists in
some regions.

**Why not fix:** The clean fix is a `resolved: bool` field persisted into
`InterpWay`, allowing zero-valued matches, and a `FORMAT_VERSION` bump.
The format growth and regeneration cost for existing indexes remain
deferred until support for house number 0 is needed. Fix shape is captured
at the struct comment in `SlimInterpWay`.

## Geocode index: u16 on-disk count caps (hard error on overflow)

**Status:** Enforced. Builder hard-errors if any cap is exceeded.

**Context:** The geocode on-disk format uses `u16` count fields in several
places:

- Per-cell entry counts for street / addr / interp segments
  (`street_entries.bin`, `addr_entries.bin`, `interp_entries.bin`,
  read by `SegmentEntryIter` / `U32EntryIter` in
  `src/geocode_index/reader.rs`).
- Per-cell entry counts for admin polygons (`admin_entries.bin`, read
  by `AdminEntryIter`).
- Per-way node counts in `StreetWay.node_count` and
  `InterpWay.node_count` (`src/geocode_index/format.rs`).

The builder (`src/geocode_index/builder/`) used to silently truncate
oversized cells/ways to `u16::MAX`, producing silently-incomplete output.
It now hard-errors at the write site via `u16::try_from(...)`, naming the
offending cell or way in the error message.

**Impact:** A build fails instead of quietly losing data. Unreachable
today in practice:

- Per-cell entry caps are well above observed density at street level
  17 (~150 m cells) and coarse level 14 (~1 km cells in dense urban
  areas).
- Per-way `node_count` is well above the OSM convention cap of 2 000
  refs per way.

**Why hard-error instead of bumping to u32:** The format version change
costs on-disk growth and forces regeneration of existing indexes; a
hard error is a strictly better signal than silent truncation until a
real workload actually hits the cap. If that happens, the error message
names the exact field and the migration path: bump the count to `u32`
and increment `FORMAT_VERSION` in `src/geocode_index/format.rs`.

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

**Fix:** `MAX_BLOB_DATASIZE` (`src/read/blob_wire.rs`), set to 32 MiB (defined
as an alias of `MAX_BLOB_MESSAGE_SIZE` to keep the relationship explicit),
rejects the declared size at the boundary with the typed
`BlobError::DataSizeTooBig` before any allocation. This is **not** a written
format limit: the OSM PBF spec caps only the *uncompressed* block (32 MiB) and
the `BlobHeader` message (64 KiB), and says nothing about a serialized-`Blob`
`datasize` ceiling. The bound is the *de facto interoperability limit* - the
reference reader (OSM-binary, `FileBlockHead`) applies a single 32 MiB
`MAX_BODY_SIZE` directly to `BlobHeader.datasize` and rejects anything at or
above it. Mirroring that reader exactly means every file pbfhogg accepts the
reference reader also accepts. A consequence worth stating: a blob carrying its
content uncompressed in `raw` at exactly the 32 MiB content cap serializes to a
few bytes over 32 MiB (protobuf framing plus the `raw_size` hint), so it too is
rejected by the reference reader - interoperable files cannot carry such a blob.
`MAX_BLOB_DATASIZE` (declared-datasize cap) and `MAX_BLOB_MESSAGE_SIZE`
(decompressed-content cap) denote different things but coincide at 32 MiB
because the reference reader enforces one number on both fields. The check lives
at two funnels covering every read-side site: inline in
`BlobReader::read_blob_header` (the `next` / `next_header_skip_blob` spine) and
in `parse_blob_header_with_index` (the shared parse funnel for `read_raw_frame`,
`read_blob_header_only`, and `HeaderWalker::next_header`).

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
them (`src/commands/cat/mod.rs`, reframe path), and
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

**Residual - precondition on the header claim:** A file whose header claims
`Sort.Type_then_ID` while its blobs are internally unsorted is passed
through undetected. That file already violates its own declared contract;
every reader in the ecosystem (including pbfhogg's `ElementReader` ordering
guarantees) trusts the claim, and pbfhogg's own sorted producers (`sort`,
`repack`, `apply-changes`) uphold it. Trusting an explicit format-level
declaration is categorically different from inferring sortedness from the
mere presence of an implementation-detail sidecar field. No `--verify-blobs`
flag: an input that wants verification simply arrives without the claim and
gets the checked scan by default. Producers were deliberately left
unchanged: indexdata (kind/range/count/bbox) is valid for an unsorted blob,
and every consumer other than sort's former inference only uses it for range
queries - stripping or gating it in `cat`/`PbfWriter` would punish those
consumers to protect an inference sort no longer makes.

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

The owned OSC sinks (`merge-changes` and `tags-filter --input-kind osc`)
deliberately parse version but pass timestamp, changeset, uid and visible through
as normalized XML attribute text without native-value validation. They do not
preserve raw entity spellings or raw attribute whitespace. The apply-changes
overlay retains its native numeric metadata parsing and treats only literal
`false` as hidden. Malformed or missing member roles become empty in all sinks.
Unifying these policies would reject accepted input or add per-element work.
A delete remains pending until a re-create is accepted; a rejected stale
re-create must not erase it.

Every OSC producer (`merge-changes`, `tags-filter --input-kind osc`,
`diff --format osc`) frames its output by name through
`pbfhogg::osc::output_is_gzip`: `.osc` is plain, `.osc.gz` is gzip, anything
else is refused before any work. Changing that rule changes the output
transport contract.
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

## Sidecar comparison contracts

The historical names `merge_scanner_to_drain_bytes_high_water` and
`merge_scanner_to_workers_bytes_high_water` deliberately describe the existing
largest-item estimates, not sampled queue occupancy. Renaming them breaks
brokkr comparisons across runs. All OSC parser callers emit
`merge_changes_decompress_ns` once per input file, with that file's time rather
than an accumulated command total, including the apply-changes overlay and
OSC tags-filter. There are no command-specific replacement counters.
