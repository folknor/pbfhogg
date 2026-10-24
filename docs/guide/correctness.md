# pbfhogg correctness notes

Parser/encoder edge cases and data representation limits that are accepted by
design. For intentional behavioral differences from osmium, see
[Deviations](/cli/deviations).

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
  `inspect --extended` metadata census and `export --metadata`. `Info::claimed_version()` and
  `DenseNodeInfo::claimed_version()` are the public form of the rule; `version()`
  still returns the raw value.
- **The other fields of an `Info`** keep their own presence, zeros included.
- **The other fields of a `DenseInfo` slot** depend on its version, because a
  packed column cannot mark one slot absent. Version 1 or more: every field is
  present as decoded. Negative: no metadata (the Osmosis rule above). Version 0:
  a field counts only with a value no padding slot holds - non-zero timestamp,
  positive changeset and uid, a non-empty user, `visible` false - the same tests
  as libosmium's `detect_available_metadata`. So the padding slot prints nothing,
  and a real versionless node keeps its non-zero fields. A zero field in such a
  slot (an anonymous `uid` 0, say) is dropped: on the wire it cannot be told
  apart from padding.

Re-encoding is unaffected: the conversions that feed a rewrite keep a version-0
slot and its fields as they are, and drop only `-1` slots.

## Null Island ambiguity in coordinate indexes

**Status:** Fixed. This class is closed - no index in the crate spells absence
as an all-zero coordinate any more.

**Context:** An index that stores node coordinates as `(lat: i32, lon: i32)` in a
zero-initialized backing store is tempted to spell "unset" as `(0, 0)`. That is
Null Island, a real coordinate in the Gulf of Guinea, so the sentinel collides
with data: a node genuinely at `0, 0` reports as missing, and a missing node is
written into a way as if it were there.

`add-locations-to-ways` and the geocode index builder no longer do this. Both
rank-indexed coordinate stores share one encoding: latitude biased by 90
degrees, which puts every legal latitude below `2^31` and leaves bit 31 of the
low word free as an explicit presence flag, with longitude verbatim in the high
word. An untouched slot still reads as absent because the backing store
zero-fills, but no coordinate can set that bit by accident, and a coordinate
outside the geographic domain is refused rather than stored as some other
place. The encoding is defined once and used by both callers, so the two cannot
drift apart; its layout, domain and Null Island behaviour are pinned by unit
tests beside it, and the end-to-end behaviour by
`null_island_real_node_is_not_reported_missing` and
`null_island_street_vertex_is_not_treated_as_absent`.

The `external` ALTW backend reaches the same answer differently: it counts a
reference as resolved when its id was *found* in the node stream, not when the
coordinate it found was non-zero. All three paths therefore agree about a node
at Null Island.

The one remaining all-zero sentinel of this shape, the blob index's "no bbox",
was also retired: `(0, 0, 0, 0)` is the legal bbox of a single-node blob at Null
Island, so index v3 spells absence as an inverted box that no well-formed bbox
can equal. v2 files keep their original reading, because that is what their
writer meant.

**Related:** unresolvable way node references are now a hard error rather than a
silent `(0, 0)` fill; pass `--ignore-missing-nodes` to accept the placeholders.
That filler is still `(0, 0)`, but it is opt-in, counted, and gated by a
post-write check, so it is a deliberate output rather than an ambiguous
sentinel. See [Deviations](/cli/deviations).

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
about itself. Sortedness is a separate boundary: `sort` keys its trust on the
header's `Sort.Type_then_ID` claim, never on indexdata's presence. The kind /
count / range boundary is the question of whether the values themselves are
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
