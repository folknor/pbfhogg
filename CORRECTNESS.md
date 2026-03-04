# pbfhogg correctness notes

Known deviations from specifications and edge cases that are accepted by design.

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

**Why not fix:** Supporting non-packed encoding adds overhead to the read hot path —
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
type — the fix is only needed for numeric repeated fields (varint, sint32, sint64, etc).
Multiple non-packed entries for the same field should accumulate, not overwrite — repeated
varint fields need to append to a buffer rather than storing a single slice. libosmium's
fix (PR #400) handles only the single-value case; a general fix should handle multiple
non-packed entries. The key performance question is whether checking both wire types in the
hot path is acceptable, or whether a fallback re-parse on finding nothing is better.

**Affected parsers:**
- `WireNode::parse()` — fields 2 (keys), 3 (vals)
- `WireWay::parse()` — fields 2 (keys), 3 (vals), 8 (refs), 9 (lats), 10 (lons)
- `WireRelation::parse()` — fields 2 (keys), 3 (vals), 8 (roles_sid), 9 (memids), 10 (types)
- `WireDenseNodes::parse()` — fields 1 (ids), 8 (lats), 9 (lons), 10 (keys_vals)
- `WireDenseInfo::parse()` — fields 1 (versions), 2 (timestamps), 3 (changesets), 4 (uids), 5 (user_sids), 6 (visibles)

## Null Island ambiguity in dense mmap index

**Status:** Known, accepted. Documented in code.

**Context:** `DenseMmapIndex` (used by `add-locations-to-ways`) stores node coordinates
as `(lat: i32, lon: i32)` pairs in a direct-addressed array, using `(0, 0)` as the
"unset" sentinel. This means a node at exactly `0.0000000, 0.0000000` (Null Island)
is treated as missing.

**Impact:** Ways referencing nodes at exactly `(0, 0)` — decimicrodegree precision,
so within ~11mm of the intersection of the prime meridian and equator — will not
have locations added. This affects zero real-world nodes. The nearest land is ~570 km
away (Gulf of Guinea).

**Why not fix:** Fixing requires either a separate occupancy bitmap (1 bit per node,
~550 MB at planet scale) or reserving an impossible sentinel with explicit valid-bit
tracking. Both add memory overhead and complexity for a case that affects no real data.
