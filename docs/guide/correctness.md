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
sentinel. See [Deviations](./deviations).
