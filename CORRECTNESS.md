# pbfhogg correctness notes

Parser/encoder edge cases and data representation limits that are accepted by
design. For intentional behavioral differences from osmium, see
DEVIATIONS.md.

## Non-packed repeated fields (protobuf spec violation)

**Status:** Known, accepted. Will not fix unless a real-world PBF triggers it.

**Context:** The protobuf spec requires decoders to accept both packed and non-packed
encodings for repeated scalar fields. pbfhogg's wire parser (`src/read/wire.rs`) uses
strict `(field_number, wire_type)` pattern matching and only accepts packed encoding
(`WIRE_LEN`, wire type 2) for repeated fields like `keys`, `vals`, `refs`, `memids`,
`dense_ids`, `dense_lats`, `dense_lons`, etc. Non-packed entries (`WIRE_VARINT`, wire
type 0) hit the catch-all `_ => cursor.skip_field(wire_type)?` and are silently skipped.

**Impact:** If a PBF producer emits a repeated field as individual non-packed entries
instead of a single packed blob, all values in that field are silently dropped. For
tag fields (`keys`/`vals`), this means tags are lost. For `refs`, way node references
are lost. For dense node coordinate fields, nodes get zero coordinates.

**Why not fix:** Supporting non-packed encoding adds overhead to the read hot path -
the most performance-critical code in the library. Every approach requires either
heap allocation per element, a branch per iterator `.next()` call, or a fallback
re-parse. The current parser processes 59M elements in 0.31s (parallel) / 1.3s
(pipelined). Even a single extra branch per packed field iteration is measurable at
that scale.

**Practical risk:** Very low. All major PBF producers (osmium, JOSM, Osmosis,
Planetiler, osmcoastline) use packed encoding. The only known producer that emits
non-packed single-element fields is protobuf-net (C#), which is rarely used for OSM
data. libosmium had the same bug ([libosmium#389](https://github.com/osmcode/libosmium/issues/389))
for years before anyone noticed.

**Fix approach (if ever needed):** For each packed repeated field, add an alternative
match arm for `WIRE_VARINT` that reads a single value. Length-delimited repeated fields
(like string table entries) already work since non-packed and packed use the same wire
type - the fix is only needed for numeric repeated fields (varint, sint32, sint64, etc).
Multiple non-packed entries for the same field should accumulate, not overwrite - repeated
varint fields need to append to a buffer rather than storing a single slice. libosmium's
fix (PR #400) handles only the single-value case; a general fix should handle multiple
non-packed entries. The key performance question is whether checking both wire types in the
hot path is acceptable, or whether a fallback re-parse on finding nothing is better.

**Affected parsers:**
- `WireNode::parse()` - fields 2 (keys), 3 (vals)
- `WireWay::parse()` - fields 2 (keys), 3 (vals), 8 (refs), 9 (lats), 10 (lons)
- `WireRelation::parse()` - fields 2 (keys), 3 (vals), 8 (roles_sid), 9 (memids), 10 (types)
- `WireDenseNodes::parse()` - fields 1 (ids), 8 (lats), 9 (lons), 10 (keys_vals)
- `WireDenseInfo::parse()` - fields 1 (versions), 2 (timestamps), 3 (changesets), 4 (uids), 5 (user_sids), 6 (visibles)

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
   - `dense_node_metadata` and `dense_node_raw_metadata` in `src/commands/mod.rs`
   - `read_dense_node` in `src/commands/sort.rs`
   - `convert_node` in `src/commands/stream_merge.rs`

**Consequence for library users:** Code consuming the public `DenseNodeInfo` API
directly (not through pbfhogg's commands) will still observe raw `-1` values from
Osmosis-generated PBFs. This is documented on the `DenseNodeInfo` struct. Library
users who need to handle Osmosis input should check for `-1` themselves. The tradeoff
is accepted: the dense iterator is too hot to add branches for a single producer's
non-standard encoding.

## Null Island ambiguity in coordinate indexes

**Status:** Fixed in `add-locations-to-ways`. Still open in the geocode
index builder.

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

Both are pinned by `null_island_real_node_is_not_reported_missing` in
`tests/cli_add_locations_to_ways.rs`, which runs on both backends.

Remaining sites:

- Geocode builder Pass 2 filters ways' `(lat, lon)` coords with
  `if lat == 0 && lon == 0 { None }` inside the `way.refs()` filter_map
  (`src/geocode_index/builder/pass2.rs`).

**Impact of the remaining site:** a way vertex at exactly `0.0000000,
0.0000000` - decimicrodegree precision, so within ~11 mm of the
intersection of the prime meridian and the equator - is silently dropped
from a geocode index's geometry. The nearest land is ~570 km away.

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
(`resolve_interpolation_endpoints_mmap` in `src/geocode_index/builder.rs`)
runs after Pass 2 and matches each endpoint against nearby addr points
with the same street name; on a match, it overwrites `start_number` and
`end_number` with the matched house numbers. On failure, both stay at
their initial `0` - the same value a legitimate OSM interpolation way
starting at house number 0 would carry.

**Impact:** A reader distinguishing "unresolved interp way" from "interp
way starting at house 0" sees identical bytes. "0" as a house number is
rare but exists in some regions. Ambiguity affects only interpolation
ways where at least one endpoint fails to match an addr point AND the
legitimate range happens to begin at 0.

**Why not fix:** The clean fix is a `resolved: bool` field persisted into
`InterpWay` and a `FORMAT_VERSION` bump. Unrecorded observations in
production so far; the extra byte of format growth and the regeneration
cost for existing indexes aren't justified until the ambiguity is seen
affecting real output. Fix shape is captured at the struct comment in
`SlimInterpWay`.

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

The builder (`src/geocode_index/builder.rs`) used to silently truncate
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

**Status:** Documented and verifiable. Consumers still trust these fields by
default, by design; `check --index` verifies them on demand, and
`--distrust-metadata` declines them per-run on `tags-filter` and `inspect`.

**Context:** pbfhogg writes two non-standard `BlobHeader` fields that standard
readers skip: `indexdata` (field 2 - element kind, `min_id`, `max_id`, `count`,
and for node blobs a spatial bbox) and `tagdata` (field 4 - the blob's set of
unique tag keys). They exist so a command can classify or reject a blob without
decompressing it, which is where essentially all of this crate's performance on
large files comes from.

Nothing in the format signs, checksums or validates either field, and nothing
requires them to have been written by pbfhogg. They are claims a file makes
about itself. The sortedness boundary above is documented carefully because
`sort` used to *infer* sortedness from indexdata's presence; the kind / count /
range boundary is the separate question of whether the values themselves are
true, and it has consumers that act on it destructively:

- `cat --type <kind>` copies a blob through **raw**, without decoding, when the
  declared kind matches - so a mislabelled blob leaks off-kind elements into the
  output, or vanishes from it entirely.
- `renumber` assigns ID blocks from the declared `count`.
- `getid --invert` and `sort` report a blob's element count from `count` on
  their passthrough paths.
- `tags-filter` and `tags-count` skip whole blobs whose tag index lacks every
  required key - so an index omitting a key its elements carry silently drops
  matches. This is a wrong answer, not a slow one.
- The spatial filter skips node blobs whose declared bbox misses the query
  region.

**Why trust is the default:** decoding every blob to check would delete the
optimization the fields exist to provide. A blob's metadata is checked when a
caller asks, not on every read.

**What exists on the other side of the boundary:**

- `pbfhogg check --index` decompresses every blob, re-derives the metadata from
  the payload with `scan_block_ids` / `scan_block_tags`, and reports each
  disagreement. Those are the same two scanners the writer's *passthrough* path
  uses, so on a re-framed blob the check cannot catch a bug inside the scanner
  itself - only metadata that has since diverged from its payload. Against the
  ordinary write path it is independent: `BlockBuilder` derives its index
  incrementally as elements are added and never rescans what it emitted. It
  never routes on the metadata it is checking - no blob filter,
  no `require_indexdata`, every blob decompressed - so it has no
  `--distrust-metadata` flag: distrust is its only mode. Exit 1 means mismatches
  were found; exit 2 means it could not run.
- `--distrust-metadata` on `tags-filter` and `inspect` declines the hints for
  one run, so the answer depends only on element data.
- `degrade --corrupt-indexdata`, `--corrupt-indexdata-kind` and
  `--corrupt-tagdata` generate the adversarial corpus both of the above are
  tested against. All three write **well-formed** hints through the same
  encoders the honest path uses, so a test built on them exercises a consumer's
  belief rather than the reader's rejection of malformed bytes.

**What `check --index` deliberately does not report:** absence. A blob with no
`indexdata` (every third-party PBF) or no `tagdata` makes no claim; a v1
(26-byte) index has no bbox field at all.

Absence is not the same as unreadability, and the difference is load-bearing.
`BlobIndex::deserialize` returns `None` for an index of an unrecognised length,
version or element kind, and every *reader* is right to treat that as "no index,
decode the blob" - but a verifier that inherits the collapse passes the single
most obviously broken input it can be handed. The verifier therefore carries the
raw field's *presence* separately from its parsed value, and reports unreadable
`indexdata`, unreadable `tagdata`, and a well-formed `tagdata` attached to a
blob whose group mixes element kinds (where the scanner sees only the first
element field, so no key set can be complete while `tags-filter` skips the blob
on the strength of it).

Two checks are containment rather than
equality for the same reason - a stored bbox must *contain* the blob's
coordinates (the scan rounds outward through `granularity` / `lat_offset` /
`lon_offset`, so a wider stored box is sound), and a stored tag key set must
*contain* every key the blob carries (a key listed but absent only costs a
decode, and is reported as a distinct, separate violation). Kind, count and ID
range are exact, because every consumer above treats them as exact.

**Residual:** an allocation sized from unverified metadata must still cope with
being wrong. `check`'s `id_alloc_bound` bounds the ID bitset by the input file's
own size rather than by the claimed `max_id`, and both `verify_ids` and
`check_refs` report an out-of-range ID as an input property instead of panicking.
`repack` bounds its `BlockBuilder` pre-allocation the same way. New code that
sizes anything from `indexdata` must do likewise.
