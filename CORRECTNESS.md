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

## Null Island ambiguity in dense mmap index

**Status:** Known, accepted. Documented in code at every affected site.

**Context:** Every index that stores node coordinates as `(lat: i32, lon: i32)`
pairs in a zero-initialized backing store uses `(0, 0)` as the "unset"
sentinel:

- `DenseMmapIndex::get` treats `packed == 0` as absent
  (`src/commands/add_locations_to_ways.rs`, `DenseMmapIndex::get`).
- `SparseArrayIndex::get_at_offset` treats `lat == 0 && lon == 0` as absent
  (`src/commands/add_locations_to_ways.rs`, `SparseArrayIndex::get_at_offset`).
- External-join stage 2 counts an entry as resolved only when
  `lat != 0 || lon != 0` (`src/commands/altw/stage2.rs`, `is_resolved`), and
  `Stats.missing_locations = total_slots - resolved_count`.
- Geocode builder Pass 2 filters ways' `(lat, lon)` coords with
  `if lat == 0 && lon == 0 { None }` inside the `way.refs()` filter_map
  (`src/geocode_index/builder.rs`, the `coords` collection in the
  per-way handler).

All sites therefore treat a node at exactly `0.0000000, 0.0000000` (Null
Island) as missing, with identical user-visible behavior. Each site carries
a `KNOWN LIMITATION` comment cross-linking to the others so a future
sentinel-contract change covers all of them at once.

**Impact:** Ways referencing nodes at exactly `(0, 0)` - decimicrodegree
precision, so within ~11 mm of the intersection of the prime meridian and
equator - will not have locations added. In the geocode builder, streets or
address ways with a node at exactly `(0, 0)` will have that coordinate
silently dropped from their geometry. This affects zero real-world nodes.
The nearest land is ~570 km away (Gulf of Guinea).

**Why not fix:** Fixing requires either a separate occupancy bitmap (1 bit
per node, ~550 MB at planet scale) or reserving an impossible sentinel
with explicit valid-bit tracking. Both add memory overhead and complexity
for a case that affects no real data.

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
