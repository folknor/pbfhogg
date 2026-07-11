# Implementation spec: injected prepass metadata (WayMembers-v1 + SharedNodePins-v1)

Status: spec, 2026-07-11. No code landed. Landing 1 is executable now
(see the Landing-1 ordering note under "Gate"); landings 2-4 are gated on
elivagar's Brick 1 superset screen AND the D9 resolved-refs ratification
(two open cross-repo gates - see "Gate" below). Landing 5's flag-ON bench
row carries two further prerequisites that are NOT the elivagar gate: the
brokkr `--inject-prepass` passthrough brick (Out of scope, named) must land
first, and the planet flag-on run needs an explicit user green-light. The
flag-OFF neutrality and docs/ADR parts of landing 5 have no such block.

## Standing references

- Contract this document is written against:
  [`reference/technical-implementation-spec.md`](../reference/technical-implementation-spec.md).
- Source item and survey (restates the full cross-repo contract; normative
  for the wire format and semantics):
  [`notes/injected-prepass.md`](injected-prepass.md). This spec does not
  restate the contract in full; where this spec and the survey disagree on
  contract facts, the survey wins - where they disagree on implementation
  shape, this spec wins.
- Test placement and tiering:
  [`reference/testing.md`](../reference/testing.md).
- Measurement record: [`reference/performance.md`](../reference/performance.md),
  [`reference/performance-history.md`](../reference/performance-history.md),
  `.brokkr/results.db`.
- Failure ledger consulted:
  [`notes/altw-optimization-history.md`](altw-optimization-history.md)
  (lesson 1: desk estimates on this path are systematically optimistic;
  lesson 5: both 16-byte record widenings regressed and are refuted -
  this spec's two zero-widening escapes exist because of that ledger).
- Standing decisions honored: `decisions/0002-negative-ids-rejected-project-wide.md`
  (negative way/node ids never enter the bitmaps),
  `decisions/0005-latent-invariant-debug-asserts.md` (the tightened
  `BucketLayout` assert stays a release assert - it guards a silent-
  truncation class), `CORRECTNESS.md` "Null Island ambiguity" (the packed
  lat design deliberately preserves the existing (0,0) sentinel semantics,
  see D3). No documented deviation in `DEVIATIONS.md` is touched.
- Ratified upstream decisions (recorded in
  [`reference/pipeline.md`](../reference/pipeline.md)): steady state is
  daily re-enrichment (option a), so apply-changes needs NO field-5/20
  maintenance; injection is opt-in flag-gated with sparse parity.

## Gate

**Do not land landings 2-5 before elivagar's Brick 1 superset screen
passes** (superset <= 1.5x needed_ways on germany locations). If it fails,
the contract gains a relation `tag_expr` filter and section "Membership
scan" below changes: the fused relation scan takes a compiled
`crate::tag_expr` expression instead of the hardcoded
`type=multipolygon|boundary` check. That extension point is named here so
the failure mode is a bounded amendment, not a rewrite; the filter is NOT
built speculatively.

**Second open cross-repo gate (D9 resolved-refs refinement).** The Gate is
NOT the only open cross-repo decision (correction folded from R1 / R2 point
2). D9 narrows the field-20 pin definition from the survey's "every position
holding a shared id" to "shared AND resolved" - a contract-fact divergence
(see D9). Landings 3-5 must not encode the narrowed semantics until elivagar
ratifies the refinement and confirms their length validations are
unaffected. Until then D9 is an open gate on the same footing as Brick 1,
with a survey-faithful fallback named in D9 if elivagar declines.

Landing 1 (format/reader layer) is invariant under both gates - the field
numbers, preamble, and accessor surface do not change if either screen fails.

**Landing-1 ordering vs the survey (correction folded from R2 point 3).**
The survey says "Do not start pbfhogg format work before this screen passes"
(Brick 1). Landing 1 is a deliberate, documented exception: it lands only
additive, default-OFF format/reader plumbing (new field numbers parsed only
under a toggle that defaults off, an all-`None` writer parameter) that is
inert until a producer (landing 2+) emits field 5/20. Nothing an unenriched
reader or writer does changes. Because the spec loses to the survey on
contract facts, this ordering exception is called out explicitly here so
implementers receive ONE ordering, not two: land landing 1 now; hold
landings 2-5 behind both gates. If the reviewer of this spec disagrees, the
fallback is to hold landing 1 too - it costs only sequencing, not rework.

## Scope and stopping rule

In scope: pbfhogg's Brick 2 in full - field-5/field-20 parse + encode,
public accessors, `parse_waymembers` toggle, altw producer in BOTH external
and sparse modes behind `--inject-prepass`, feature strings, the oracle
roundtrip test, the flag-hygiene rule + pinning test, benches and record
updates, ADR.

Out of scope (separate items, named, not deferrals):

- elivagar Bricks 1, 4, 5 (their repo, their gates).
- Brick 3 / Brick 6 data re-enrichment runs and their on-disk-growth
  readings (operational, driven after this spec lands).
- brokkr plumbing: an `--inject-prepass` passthrough flag (modeled on
  `--direct-io`) so enriched runs become their own benchable variant.
  Cross-repo brick in `~/Programs/brokkr`; required before the flag-on
  planet verdict can be recorded (see "Benches" below).
- `notes/altw-external.md` L1 (refcount BlobHeader extension) - explicitly
  a follow-up on the rails landing 1 builds; not built here.
- Incremental field-5 maintenance in apply-changes - ratified unnecessary
  by the daily-loop decision.

## Survey of the ground (what exists, what carries load)

Read-side:

- `src/read/blob_wire.rs`: `WireBlobHeader { blob_type, datasize,
  indexdata, tagdata }` with `parse(data, parse_tagdata, parse_indexdata)`.
  Fields 2 (indexdata) and 4 (tagdata) are toggle-gated; unknown fields are
  wire-skipped. `MAX_BLOB_HEADER_SIZE = 64 KiB` lives here.
- `src/read/blob.rs`: `BlobReader` holds `parse_tagdata: bool` (default
  false) and `parse_indexdata: bool` (default true) with `pub(crate)`
  setters; `Blob::index()` and `Blob::tag_index()` are `pub(crate)` -
  `Blob::way_members()` will be the first PUBLIC per-blob metadata
  accessor. `BlobReader` has four construction sites that initialize the
  toggle fields (`new`, `from_map`-style constructors, `new_seekable`).
- `src/read/wire.rs`: `WireWay` captures fields 1/2/3/4/8/9/10 as borrowed
  slices, everything else skipped. Field 20 is a one-arm addition -
  zero-copy, no toggle needed (no allocation, unlike header tagdata).
- `src/read/elements.rs`: `Way` wraps `WireWay`; accessor pattern
  (`refs()`, `node_locations()`) is the model for `shared_node_pins()`.
- `src/read/block.rs`: `HeaderBlock::LOCATIONS_ON_WAYS` +
  `has_locations_on_ways()` are the model for the two new feature-string
  constants and accessors.

Write-side:

- `src/write/framing.rs`: `encode_blob_header_into(blob_type, datasize,
  indexdata, tagdata, buf)` is the single BlobHeader encoder;
  `frame_blob_into` is the single framing chokepoint (sync and pipelined
  paths both land here). There is currently NO 64 KiB cap check on the
  encoded header - only a u32 overflow check. The contract requires a
  write-time hard failure; this spec adds it (D5).
- `src/write/writer.rs`: `PbfWriter::write_primitive_block_owned(block_bytes,
  index, tagdata)` is the call every altw output block funnels through.
- `src/write/block_builder.rs`: `pub(crate) type OwnedBlock = (Vec<u8>,
  BlobIndex, Option<Vec<u8>>)` - the worker-to-consumer vehicle, used in
  ~24 files (all commands + block_builder + owned.rs).
- `src/write/header_builder.rs`: `HeaderBuilder::from_header` does NOT copy
  input `optional_features` - every rewriting command therefore drops the
  enrichment flags by construction today. This is the accidental safety
  section 8 of the survey documents; landing 4 pins it with a test so it
  cannot regress silently.

ALTW external (`src/commands/altw/external/`):

- `mod.rs`: `external_join` orchestrates; `BucketLayout::new` asserts
  `bucket_width <= u32::MAX`; `IdRecord { local_node_id: u32, blob_idx:
  u32, blob_local_slot: u32 }` (12 bytes); `ResolvedEntry { slot_pos, lat:
  i32, lon: i32 }` (12 bytes on disk). The relation scan already runs
  overlapped with stage 1 in a `thread::scope`, gated on
  `!keep_untagged_nodes`, and completes before stage 4 starts.
- `relation_scan.rs`: `collect_relation_member_node_ids_indexed` preads
  relation blobs only via `BlobMeta`, decodes full `PrimitiveBlock`s,
  iterates members. The fused way-member collection is one extra tag check
  + one extra `IdSet` in the same loop.
- `stage1.rs`: `stage1_pass_a` gets `|way_id, refs|` per way from
  `scan_way_refs` - first/last ref and ref count are in hand exactly where
  the closure flag must be decided.
- `stage2.rs`: `prepare_bucket` sorts records by `local_node_id`
  (`sort_unstable_by_key`); the merge walk computes `global_id = bucket_lo
  + local_node_id` and emits one `ResolvedEntry` per record. All global
  occurrences of a node id are consecutive in exactly one bucket - the
  run-length property the pins ride on. **Hazard found by this survey**:
  the bit-31 closure flag corrupts BOTH the sort key and `global_id`
  unless masked; see D1.
- `stage3.rs` + `coord_payloads.rs`: scatter into 8-byte `(lat, lon)`
  slots, then `encode_blob_payload_from_record` delta-varint-encodes per
  way (2*N zigzag varints, no per-way framing). Both the fully-contained
  worker path and the straddler path converge on that one encoder. The
  decoder is `stage4.rs::reframe_way_blob_with_locations`'s payload
  cursor, which already hard-errors on trailing bytes - the natural
  version-skew canary for the v2 framing (D4).
- `stage4.rs`: way blobs are ALWAYS reframed (never passthrough);
  `reframe_way_blob_with_locations` walks each way's fields to read the id
  and copies `way_bytes` VERBATIM before appending fields 9/10. **Lateral
  finding (pre-existing bug)**: on input that already carries fields 9/10
  (re-enriching an enriched file), the verbatim copy duplicates them;
  packed repeated fields concatenate under conforming protobuf decoders,
  so a second enrichment produces 2x coordinate counts for osmium-class
  readers (pbfhogg's own `WireWay::parse` is last-wins and hides it). The
  sparse reframe (`altw/reframe.rs::splice_way_locations`) strips 9/10
  correctly. Fixed here because field 20 needs the identical strip
  mechanism anyway (D6). `assemble_block` already hard-errors on a way in
  a non-way-indexed blob - no new guard needed on the external side.

ALTW sparse (`src/commands/altw/`):

- `mod.rs::add_locations_to_ways`: pass 0 `collect_way_referenced_node_ids`
  already walks every way ref (the occurrence stream pins need); relation
  member scan `collect_relation_member_node_ids` runs before pass 2. Pass
  2 dispatches to `passthrough.rs` (indexdata present; ways always
  reframed via `altw/reframe.rs`) or `write_output_decode_all` (no
  indexdata; ways go through `BlockBuilder`).
- `altw/reframe.rs::splice_way_locations`: decodes field 8 refs (so
  per-ref pin probes are free lookups), strips 9/10, appends fresh 9/10.
  Way order within the blob = file order = field-5 bit order.

Tests: `tests/cli_add_locations_to_ways.rs` holds the parity canary
`backend_parity_sparse_external_auto` and the fixture builders
(`write_multi_block_test_pbf`, `generate_nodes`, `generate_ways`,
`read_way_locations`). `tests/cli_sort.rs` is the template for the new
`tests/cli_inject_prepass.rs`.

Caller-ordering check (spec rule 8): the priced hot paths were traced
through the actual callers - stage 2's sort and walk both consume
`local_node_id` raw (hence D1), and both stage-3 encode entry points
(worker + straddler) route through `encode_blob_payload_from_record`
(hence the single-function change in D4).

## Design decisions resolved inline

**D1. Closure flag in `IdRecord.local_node_id` bit 31, masked everywhere
downstream.** Stage 1 sets bit 31 on the record of a way's trailing ref
when `refs.len() >= 4 && refs[0] == refs[len-1]`. Consequences the survey
note did not spell out, pinned here:

- `BucketLayout::new` assert tightens from `bucket_width <= u32::MAX` to
  `bucket_width <= 1 << 30`. Rationale: `locate` can return a local offset
  up to `2*bucket_width - 1` in the clamped last bucket, so freeing bit 31
  requires `2*bucket_width - 1 < 2^31`, i.e. `bucket_width <= 2^30`. At
  planet (256 buckets) bucket_width is ~55M, five orders under the new
  bound; the bound is violated only above max_node_id ~2.7e11. The
  `BucketLayout::new` doc comment and the sibling `locate` comment both
  currently state "asserts `bucket_width <= u32::MAX`" and must be updated to
  the new bound when the assert tightens (nit folded from R1).
- `prepare_bucket` sorts by the MASKED key:
  `sort_unstable_by_key(|r| r.local_node_id & LOCAL_ID_MASK)` where
  `const LOCAL_ID_MASK: u32 = 0x7FFF_FFFF` and
  `const CLOSURE_FLAG: u32 = 0x8000_0000` live next to `IdRecord`.
- The stage-2 merge walk computes `global_id = bucket_lo +
  u64::from(record.local_node_id & LOCAL_ID_MASK)`.
- Flag emission happens unconditionally (a flagged record with injection
  off is never inspected), but masking in stage 2 is also unconditional -
  one `and` per record, branch-free, so the flag-off path stays
  semantically identical and measurably neutral (verified by the flag-off
  benches in landing 5). Rejected alternative: conditional flag emission,
  which would make the scratch format depend on the run flag for no
  benefit.

**D2. Run-at-a-time stage-2 walk computes the pin bit.** Records with equal
masked id are consecutive after the D1 sort, and a run never spans blobs
(all records of an id resolve against exactly one node tuple, consumed
together). Restructure the per-record walk into: find run end (scan
forward while masked id equal), count records WITHOUT the closure flag,
`pin = count >= 2`, resolve the id once, emit one `ResolvedEntry` per
record in the run (flagged records included - the trailing closure ref
gets its coordinates and inherits the run's pin, which is exactly the
"mirrors bit 0" contract behaviour). Orphan runs (no matching tuple) emit
nothing, as today.

**D3. Pin transport: `(lat << 1) | pin` packed into `ResolvedEntry.lat`,
only when injecting.** lat in decimicrodegrees is bounded by |9e8| < 2^30
so the shift is lossless in i32; lon has no spare bit and stays raw. Pack
at `ResolvedEntry` construction in stage 2; unpack in
`encode_blob_payload_from_record` (`pin = packed & 1`, `lat = packed >> 1`
arithmetic). The stage-2 resolved-count check keeps reading the UNPACKED
`tuple.lat/lon` so `missing_locations` semantics are bit-identical to
today (a pinned Null-Island node must not silently flip from "missing" to
"resolved"). Zero-filled slots decode as `(0 >> 1, 0 & 1) = (0, 0)` pin 0,
so the unresolved sentinel is preserved with no extra branch. Packing is
gated on the run-level inject flag so flag-off scratch bytes are
byte-identical to today's.

**D4. coord_payloads framing v2 (inject runs only): per way, `2*N` zigzag
varints followed by exactly `ceil(N/8)` pin-bitmap bytes** (LSB-first, bit
i = ref position i pinned). Always emitted per way when injecting, even
all-zero - deterministic framing, the decoder needs no presence signal;
"field 20 omitted when empty" is decided at stage 4 emission, not in the
scratch format. The format is selected by the run-level flag threaded to
both stage 3 (`encode_blob_payload_from_record`) and stage 4 (payload
cursor); no on-disk version byte is needed because scratch never outlives
the run, and the existing trailing-bytes / truncated-varint integrity
checks in the stage-4 cursor fail loudly on any producer/consumer skew.
Cost: ~+1.5 GB on 54.8 GB planet payloads (+~3% scratch), priced in the
survey's cost map.

**D5. Write-time BlobHeader cap - in the encoder, reject `>=`.**
`encode_blob_header_into` (NOT `frame_blob_into`) gains the cap and returns
`io::Result<()>` instead of `()`: after encoding, `header_buf.len() as u64
< MAX_BLOB_HEADER_SIZE` else `io::Error` naming the blob and sizes.

Two corrections folded from review (R1 bug / R2 point 4), both verified
against code:

- **Layer.** `frame_blob_into` is NOT the single header chokepoint. The
  sync writer path `write_primitive_block_owned` -> `write_framed_blob`
  (`writer.rs`) calls `encode_blob_header_into` directly, and so does
  `reframe_raw_with_index_scratch` (`framing.rs`, merge passthrough) and
  `degrade`. A cap placed only in `frame_blob_into` leaves the sync writer
  (taken whenever the `PbfWriter` pipeline is disabled) and the merge
  passthrough un-capped - exactly the failure D5 exists to prevent. Putting
  the cap (and the field-5 parameter, D7) inside `encode_blob_header_into`
  covers every emitter in one place.
- **Off-by-one.** The reader rejects at `header_size >= MAX_BLOB_HEADER_SIZE`
  (`blob.rs`). A `<=` write check would let a 65,536-byte header be written
  and then rejected on read. The bound is strict: `< MAX_BLOB_HEADER_SIZE`.
  Landing-1 tests assert 65,535 bytes pass and 65,536 bytes error.

Unconditional (one compare per blob) because the cap is a spec invariant
for every header field, not just field 5. A blob whose field-5 bitmap would
overflow the header is a hard write-time failure, never a truncated bitmap -
the contract's size policy. Real densities (8,000-66,500 ways/blob per
`reference/blob-density.md`) sit far under the ~512k-ways/blob overflow
point.

**D6. Way-field stripping in both reframe paths.** Sparse
`splice_way_locations` adds field 20 to its existing `(9 | 10, WIRE_LEN)`
strip arm - unconditional, we own coordinates AND pins. External
`reframe_way_blob_with_locations` keeps the verbatim-copy fast path but
its per-way id walk (which already tags every field) now detects fields
9/10/20; on detection (rare: only re-enrichment input) the way is
re-walked with per-field copy that drops 9/10/20. This fixes the
pre-existing duplicate-9/10 bug at zero cost to the common path (the
detect is a comparison the walk already performs). Unit test pins both
paths with a hand-built enriched way.

**D7. Field-5 payload travels as opaque bytes; `OwnedBlock` becomes a
struct.** Stage 4 / sparse decode workers assemble the full field-5
payload (`0x01`, varint way_count, `ceil(n/8)` bitmap bytes, way order =
file order) and attach it to the output block. The anonymous 3-tuple does
not survive a fourth member legibly; convert:

```rust
pub(crate) struct OwnedBlock {
    pub bytes: Vec<u8>,
    pub index: BlobIndex,
    pub tagdata: Option<Vec<u8>>,
    pub way_members: Option<Vec<u8>>, // full field-5 payload, preamble included
}
```

Mechanical edit across the ~24 consuming files (all existing constructors
set `way_members: None`). This is landing 1 so later landings diff small.
`PbfWriter::write_primitive_block_owned` grows a `way_members:
Option<&[u8]>` parameter, threaded to `frame_blob_into` and
`encode_blob_header_into` (new trailing `way_members: Option<&[u8]>`
parameter, encoded as field 5 verbatim). All non-altw callers pass `None`.

**D8. Reader accessor semantics.** `WireBlobHeader` gains `waymembers:
Option<Box<[u8]>>`, parsed only when the new `parse_waymembers` toggle is
on (field 5 allocates, like tagdata; skipped otherwise).
`BlobReader::set_parse_waymembers(&mut self, enable: bool)` is `pub`
(elivagar's Option-3 read loop is BlobReader-based; the survey's section 9
is why this sits on `BlobReader`, not only `ElementReader`). Default off
at every constructor site. Public accessor:

```rust
impl Blob {
    /// Raw way-member bitmap from BlobHeader field 5 (pbfhogg.WayMembers-v1).
    /// Preamble (version byte + way-count varint) validated and stripped.
    /// None when the field is absent, was not parsed (toggle off), or is
    /// malformed (wrong version, bitmap length != ceil(count/8)).
    pub fn way_members(&self) -> Option<&[u8]>
}
```

Malformed-maps-to-None is deliberate: under the feature flag elivagar
treats absence as corruption and hard-errors, so a malformed preamble
still surfaces as a hard error downstream without complicating the
signature the contract pins.

**Count validation gap (folded from R2 point 1).** The contract's own
hard-error class "field 5 vs the blob's way count" cannot be checked by any
consumer if `way_members()` strips the encoded `way_count` and returns only
`&[u8]`: pbfhogg validates `bitmap.len() == ceil(encoded_count/8)` (a
self-consistency check of the preamble against itself) and then discards the
count, while elivagar decodes the blob and knows only the ACTUAL way count.
The two never meet, so a producer bug where encoded and actual counts differ
within one bitmap byte (e.g. encoded 7, actual 8, both `ceil = 1`) is
undetectable end-to-end. The contract API (`survey`) itself pins
`way_members() -> Option<&[u8]>` with the count stripped, so this is an
internal contradiction in the survey, not a defect this spec introduced.
Resolution, additive and contract-preserving: keep `way_members()` as pinned
AND expose the count so the actual-vs-encoded compare is possible on
elivagar's side -

```rust
impl Blob {
    /// Encoded field-5 way_count (preamble), for cross-checking against the
    /// blob's actual decoded Way count. None under the same conditions as
    /// `way_members()`.
    pub fn way_member_count(&self) -> Option<u32>
}
```

Flag to elivagar as a cross-repo API question (they own the validation). The
landing-4 oracle MUST include a fixture whose encoded and actual way counts
differ within the same bitmap-byte bucket, so the compare is exercised. `Way::shared_node_pins(&self) -> Option<&[u8]>`
returns the raw field-20 bytes (`None` when absent - absence is the legal
"no pins" case); length validation (`ceil(ref_count/8)`) is the consumer's
check per the contract. Feature-string constants and accessors on
`HeaderBlock` beside `LOCATIONS_ON_WAYS`:
`WAY_MEMBERS_V1 = "pbfhogg.WayMembers-v1"`,
`SHARED_NODE_PINS_V1 = "pbfhogg.SharedNodePins-v1"`,
`has_way_members_v1()`, `has_shared_node_pins_v1()`.

**D9. Pins are emitted only for refs that resolved to a location.** The
contract text counts occurrences regardless of node existence; the
external implementation can only pin slots that receive a `ResolvedEntry`,
so a shared-but-absent node id yields pin 0 at every position. Sparse
mirrors this explicitly: `pin_i = shared(id_i) && resolved_i`. This is the
only way the two backends (and the in-test oracle) agree, and it is
semantically right - a (0,0) missing-location slot must not pin DP
simplification.

**Precedence conflict, made explicit (folded from R1 inconsistency / R2
point 2).** The survey text pins the bit at "every position holding a shared
id, including a closed ring's trailing duplicate", regardless of node
existence. D9 narrows that to shared AND resolved - a CONTRACT-FACT
divergence, and the standing precedence rule is "where this spec and the
survey disagree on contract facts, the survey wins." So D9 does not get to
declare "this spec wins" and treat the change as settled: it is a second
open cross-repo gate (see Gate section). On the merits D9 is correct, but
procedurally it must be ratified by elivagar before landings 3-5 encode it.

Two ways to close the gate, decided with elivagar, not here:
(a) elivagar ratifies the resolved-refs refinement and updates the normative
contract - then D9 stands as written; or
(b) elivagar declines - then preserve the survey semantics by emitting a
zero-coordinate `ResolvedEntry` carrying the pin bit for shared orphan runs,
so every position holding a shared id pins even when the node is absent. The
external run walk already visits the run; option (b) is a pin-emit on the
orphan branch, not a redesign.

The landing-4 oracle implements whichever definition is ratified; until
ratified it is written to the option chosen, not hardcoded to D9.

**D10. Sparse occurrence counting rides pass 0.** The set of counted ids
equals the referenced-id set (the closure duplicate's id is always already
present at position 0), so pass 0's per-block ref vector - with the
trailing closure ref dropped per way - feeds both the existing
`referenced` IdSet and a new `shared` IdSet in the sequential reducer:
`if referenced.get(id) { shared.set(id) } else { referenced.set(id) }`.
Order across blocks is irrelevant to the final `shared` set. Negative
refs are skipped (never resolve; D9 masks them). Cost: one extra IdSet
(~node-id-space bitset; sparse's operating scales top out at europe) and
one probe per ref. Sparse membership: the existing relation member scan
grows the same fused `type=multipolygon|boundary` way-id collection as
external's, and runs unconditionally when injecting (node-member set still
only when `!keep_untagged_nodes`).

**D11. Flag surface and input requirements.** Library:

```rust
pub struct AltwOptions {
    pub keep_untagged_nodes: bool,
    pub compression: Compression,
    pub direct_io: bool,
    pub force: bool,
    pub index_type: IndexType,
    pub inject_prepass: bool,
}
pub fn add_locations_to_ways(
    input: &Path, output: &Path, options: &AltwOptions, overrides: &HeaderOverrides,
) -> Result<Stats>
```

(The current 8-positional-bool signature does not survive a ninth
parameter; pre-1.0 breakage is legal and the CLI is the only in-repo
caller besides tests.) CLI: `--inject-prepass` on `AddLocationsToWays`.
Validation: `--inject-prepass` requires indexdata-present input in sparse
mode (hard error otherwise, including under `--force`); external requires
indexed input already. The sparse decode-all fallback and `BlockBuilder`
way path therefore never run under the flag; `decode_one`'s non-way arm
gains a guard that hard-errors if a Way element appears in a
non-way-indexed blob while injecting (mirrors external's `assemble_block`
posture). When injecting, the output header declares `LocationsOnWays` +
both new feature strings; when not injecting, output is byte-identical to
today's **for inputs that do not already carry way fields 9/10/20**.

Qualification folded from R2 point 8: D6 strips stale fields 9/10/20
unconditionally in both reframe paths, which changes flag-off output for an
already-enriched input relative to today (today the external verbatim copy
DUPLICATES 9/10 - the pre-existing bug D6 fixes). So flag-off byte-identity
holds only for inputs without those fields; on locations-bearing input the
flag-off output legitimately differs (and is now correct). The denmark
verify uses ordinary indexed input and cannot exercise this; landing 3 adds
a CLI re-enrichment test over a locations-bearing input asserting single
(not doubled) 9/10 and no stale field 20.

**D12. Membership scan fusion (external).**
`collect_relation_member_node_ids_indexed` is generalized to return

```rust
pub(super) struct RelationScanOutput {
    pub member_node_ids: Option<IdSet>, // when !keep_untagged_nodes (existing)
    pub member_way_ids: Option<IdSet>,  // when inject_prepass
}
```

collected in the same decompress + member iteration (one added tag check:
relation `type` tag equals `multipolygon` or `boundary`; only
`MemberId::Way(id)` with `id >= 0` per ADR-0002). The scan now runs when
`inject_prepass || !keep_untagged_nodes` and stays overlapped with stage 1
(completes before stage 4 needs it, as today). The member-way `IdSet` is
~200 MB resident at planet through stage 4 (renumber's way bitset is the
precedent), against stage 4's ~12 GB peak.

## Landings

Each landing is one coherent keep/revert unit; `brokkr check` is green at
every boundary. Commit first, then measure; numbers recorded against the
commit hash.

### Landing 1 - format/reader layer (gate-free, land now)

Bricks:

1. `OwnedBlock` tuple-to-struct conversion with `way_members: None`
   everywhere (D7). Pure mechanical refactor, no behaviour change.
2. `encode_blob_header_into` grows the `way_members` parameter and the D5
   cap, returning `io::Result<()>`; field 5 encoded when `Some`. ALL four
   call sites update, not the two the survey named (correction folded from
   R1 gap - verified against code): `frame_blob_into` and the sync
   `write_framed_blob` (both in the write path), `reframe_raw_with_index_scratch`
   (merge passthrough, `framing.rs`), and `degrade` (`degrade/mod.rs`). The
   `way_members` value threads from `write_primitive_block_owned` through
   BOTH its branches (pipelined `frame_blob_into` and sync `write_framed_blob`);
   passthrough/reframe callers pass `None`.
3. `WireBlobHeader.waymembers` + `parse_waymembers` toggle +
   `BlobReader::set_parse_waymembers` (pub) + `Blob::way_members()` +
   `Blob::way_member_count()` with preamble validation (D8). `WireBlobHeader::parse`
   gains a fourth `parse_waymembers` param; its non-`BlobReader` caller
   `parse_blob_header_with_index` (`blob_wire.rs`, currently `parse(_, true,
   true)`) passes `false`. Fix the stale `WireBlobHeader` doc comment while
   here (it omits field 4 tagdata; add field 5).
4. `WireWay` field-20 arm + `Way::shared_node_pins()` (D8).
5. `HeaderBlock` feature constants + accessors (D8).
6. Tests (inline unit, per `reference/testing.md` placement):
   - field-5 encode-then-parse roundtrip through
     `encode_blob_header_into` / `WireBlobHeader::parse`, toggle on;
   - toggle off yields `None` on the same bytes;
   - malformed preambles (bad version, short bitmap, count mismatch)
     yield `None`;
   - `way_member_count()` returns the encoded count on a valid preamble;
   - a 65,535-byte header encodes successfully and a 65,536-byte header
     errors at `encode_blob_header_into` with the cap in the message (strict
     `<` boundary matching the reader's `>=` reject, D5);
   - `WireWay` parses field 20 alongside 8/9/10; absent field yields
     `None`.
   Stable-allowlist addition (doc edit in `reference/testing.md`):
   `Blob::way_members`, `Blob::way_member_count`, `Way::shared_node_pins`,
   `BlobReader::set_parse_waymembers`, the two `HeaderBlock` accessors.

Gates (landing 1):

- `brokkr check` - clippy + full tier-1 suite; the landing touches wire
  encoding, so this is mandatory.
- `brokkr check --profile full` - the ignored roundtrip suite; reader and
  writer chokepoints changed, and re-encoding a whole PBF and reading it
  back is the check no smaller test makes.
- `brokkr read --dataset denmark --bench` - neutrality of the added parse
  arms (one match arm in `WireBlobHeader::parse` behind a default-off
  toggle, one in `WireWay::parse`). Denmark is sufficient: the cost is
  per-field-tag dispatch, identical per blob at any scale, so a regression
  large enough to matter shows at denmark; a planet read bench would answer
  the same question for 100x the cost. Verdict against the current denmark
  read numbers in `reference/performance.md`; this landing claims
  neutrality, so an unchanged result is the deliverable.
- `brokkr verify cat --dataset denmark --variant indexed` - cheapest
  external cross-check that the writer chokepoint still produces
  osmium-readable output with all-`None` metadata (zero diffs, parity
  exceptions per `reference/osmium-parity.md`). Denmark indexed because
  `cat` passthrough exercises exactly the reframed-header path.

### Landing 2 - external producer + flag (gated on Brick 1 screen)

Bricks:

1. `AltwOptions` + CLI `--inject-prepass` + validations (D11). Sparse +
   inject hard-errors at this landing boundary with "sparse support lands
   next" wording (landing 3 removes it; both landings live in this spec,
   so nothing is deferred out of the item).
2. Relation scan fusion (D12); scan condition widened to
   `inject_prepass || !keep_untagged_nodes`.
3. Stage 1: closure flag emission + `BucketLayout` assert tighten +
   mask constants (D1).
4. Stage 2: masked sort key, masked global id, run-at-a-time walk with
   pin computation (D2), packed-lat emission when injecting (D3).
5. Stage 3 / coord_payloads: unpack + v2 framing when injecting (D4),
   flag threaded through `IntegratedInputs` and the router's straddler
   encode.
6. Stage 4: payload cursor consumes pin bytes; field-20 splice when any
   bit set; strip-on-detect rewalk for fields 9/10/20 (D6, unconditional);
   member-set probe per way builds the field-5 payload into
   `OwnedBlock.way_members` (every way blob, all-zero included); output
   header gains both feature strings when injecting.
7. Sidecar counters (`altw_member_ways`, `altw_pinned_refs`,
   `altw_field5_bytes`, `altw_field20_ways_emitted`) - instrumentation
   only, not CHANGELOG material.
8. Tests: unit tests for run-length pin cases in stage 2 (shared across
   ways, within-way repeat, detached closed ring pins nothing, closure
   trailing ref mirrors bit 0, orphan run emits nothing); D6 strip test;
   CLI tier-1 contract in new `tests/cli_inject_prepass.rs` (external
   backend): enriched output declares all three feature strings, every way
   blob answers `way_members()`, a road-grid fixture yields expected pins,
   a detached building ring omits field 20.

Gates (landing 2):

- `brokkr check`.
- `brokkr check --profile full` (reader/writer + roundtrip surfaces
  touched).
- `brokkr verify add-locations-to-ways --dataset denmark --variant indexed --mode external` -
  flag-off element output unchanged against the osmium reference, zero
  diffs. Denmark indexed answers this: the flag-off path is
  branch-guarded, and any accidental semantic leak (mask bug, packed-lat
  leak) corrupts coordinates at any scale, which denmark exposes.
- `brokkr add-locations-to-ways --dataset denmark --index-type external --bench 1` -
  smoke + wall sanity on the smallest indexed dataset before the europe
  neutrality read in landing 5.

### Landing 3 - sparse producer (gated on Brick 1 screen)

Bricks:

1. Pass 0 `shared` IdSet + closure-exclusion in the per-block ref vector
   (D10).
2. Sparse relation scan: fused way-member collection, unconditional under
   inject (D10).
3. `splice_way_locations`: field-20 strip arm (D6), per-ref
   `shared && resolved` pin bitmap (D9), field-20 splice when nonzero;
   per-blob field-5 assembly in `decode_one`; feature strings via
   `build_schedule`'s header configure.
4. Remove the landing-2 sparse hard error; keep the indexdata requirement
   and the decode-all/`--force` rejection (D11), plus the way-in-non-way-
   blob guard.
5. Tests: tier-1 sparse twin of the landing-2 CLI contract;
   `backend_parity_inject_prepass` in `tests/cli_inject_prepass.rs`
   (tier 2) - run sparse, external, auto over the multi-block fixture with
   the flag, assert element equivalence AND per-blob `way_members()`
   bitmap equality AND per-way `shared_node_pins()` equality across all
   three outputs.

Gates (landing 3):

- `brokkr check`.
- `brokkr verify add-locations-to-ways --dataset denmark --variant indexed --mode sparse` -
  flag-off sparse output unchanged, zero diffs. Denmark for the same
  reason as landing 2.
- `brokkr test cli_inject_prepass backend_parity_inject_prepass` - three
  independent implementations (sparse, external, in-test fixtures) now
  check each other.

### Landing 4 - oracle roundtrip + flag hygiene (gated on Brick 1 screen)

Bricks:

1. Oracle roundtrip test (Brick 2's contract gate) in
   `tests/cli_inject_prepass.rs`, tier 1 sized: build a fixture with known
   topology (junctions, within-way repeats, closed rings, a ring sharing
   an edge with a road, mp/boundary/other relations, refs to absent
   nodes), run `add-locations-to-ways --inject-prepass` (both backends),
   read back with `BlobReader` + `set_parse_waymembers(true)`, recompute
   membership and pins from the fixture's raw elements with an independent
   in-test implementation of the contract semantics (whichever D9 option is
   ratified), compare bitmaps bit-for-bit. The topology MUST include a way
   blob whose encoded field-5 `way_count` and actual decoded Way count could
   differ within one bitmap byte (per D8's count-gap fold), so
   `way_member_count()` vs the decoded count is exercised, not just
   `way_members().len()`.
2. Flag-hygiene rule, stated in code where it lives: a doc comment on
   `HeaderBuilder::from_header` recording that optional_features are
   deliberately not copied, and that any command rewriting way payloads or
   way-blob headers without maintaining fields 5/20 must not emit the two
   feature strings (survey section 8's rule, now load-bearing).
3. Flag-hygiene sweep test (tier 2, same file): enrich a fixture with
   `--inject-prepass`, then run every rewriting command over it and assert
   the output header carries NEITHER feature string. The inventory is NOT
   the eight commands the survey listed - it is the COMPLETE set of
   `warn_locations_on_ways_loss` callers (correction folded from R2 point 7;
   that warn fn is the canonical "this command rewrites and may drop
   way-level metadata" marker, so its caller set is the natural sweep
   generator). Verified caller set today: `repack`, `sort`, `renumber`,
   `getparents`, `degrade`, `time-filter`, `apply-changes`, `tags-filter`
   (PBF and OSC paths), `getid` (its three call sites), `cat` (mod, dedupe),
   and `extract` (complete, smart, simple, multi). The earlier list omitted
   `renumber`, `getparents`, `degrade`, multi-extract, and `cat` dedupe -
   all of which rewrite output and must be swept. Drive each command in the
   modes that produce output; a mode that cannot carry way blobs is exempt
   and noted, not silently skipped. This pins the accidental safety so a
   future header-preserving change fails loudly here instead of shipping
   malformed enrichments.
4. Extend `warn_locations_on_ways_loss` in `src/commands/mod.rs` to also
   warn when the input declares either new feature string (same posture:
   silent strip is legal, silent-and-unannounced is not).

Gates (landing 4):

- `brokkr check` (the oracle test is tier 1; the sweep is tier 2, run via
  `brokkr check --profile <cmd>` sweeps or explicitly with
  `brokkr test cli_inject_prepass <name>`).
- `brokkr test cli_inject_prepass inject_prepass_oracle_roundtrip`
- `brokkr test cli_inject_prepass rewriting_commands_drop_enrichment_flags`

### Landing 5 - benches, records, docs (gated on Brick 1 screen)

Benchmark discipline: all landings 2-4 committed first; baselines captured
via `--commit` from the same branch. The pre-change baseline the verdicts
are read against: planet external **546.0 s** (`7fd04130`, plantasjen; the
survey's budget anchor - re-capture at the current HEAD-1 with `--commit`
since `a0a2e3b` and later landed on adjacent paths); europe external
233-271 s depending on compression per `reference/performance.md`.

Gate-command completeness (folded from R2 point 6): the commands below pin
`--variant indexed` explicitly (external requires indexed input; a run
without the variant is underspecified per
`reference/technical-implementation-spec.md`), split the two compression
modes into two copy-pasteable commands rather than "under BOTH", and mark
the one unavoidable placeholder. The `--commit <ref>` cell cannot be a
literal until landings 2-4 have hashes; fill the ref of the commit
immediately preceding landing 2 at bench time - it is a fill-in, not a
missing decision.

1. Flag-OFF neutrality (the 3%-by-construction claim still gets measured -
   lesson 1):
   - `brokkr add-locations-to-ways --dataset europe --variant indexed --index-type external --bench`
     vs `brokkr add-locations-to-ways --dataset europe --variant indexed --index-type external --bench --commit <pre-landing-2 ref>`
     (group HEAD cells and worktree cells per the build-thrash rule).
     Europe first because it is the cheapest input where the stage-1/2/3
     hot loops run at representative record volumes (~4.7B records);
     denmark cannot expose a per-record cost.
   - Planet flag-off run only if europe shows any drift beyond noise
     bounds per `reference/performance.md` reading rules. Planet is an
     explicit user decision; this spec requests it only on a europe red
     flag.
   Keep/revert bound: flag-off europe within noise of baseline; anything
   beyond noise is a revert of the offending landing, not a budget spend -
   the budget belongs to the flag-on run.
2. Flag-ON price (requires the named brokkr `--inject-prepass` passthrough
   brick; blocked until it lands):
   - `brokkr add-locations-to-ways --dataset europe --variant indexed --index-type external --inject-prepass --compression zlib:6 --bench`
   - `brokkr add-locations-to-ways --dataset europe --variant indexed --index-type external --inject-prepass --compression zstd:1 --bench`
     (two commands, one per compression mode - writer-ceiling diagnostic, the
     added field-20/field-5 bytes land in the compression path).
   - `brokkr add-locations-to-ways --dataset denmark --variant indexed --index-type sparse --inject-prepass --bench`
     for a sparse sanity wall number (sparse has no standing planet budget).
   - Planet flag-on run when the user green-lights: budget is the standing
     **external <= 3% bound** (~16 s against the re-captured planet
     baseline). Keep/revert verdict read against that bound; peak anon RSS
     from `brokkr sidecar <UUID> --human` recorded alongside (stage-4
     planet peak ~12 GB is the reference; the member IdSet adds ~200 MB).
3. Records: new numbers into `reference/performance.md` (flag-on as its
   own row, honest price per the ratified decision); superseded baseline +
   arc narrative into `reference/performance-history.md`; on-disk growth
   of the enriched output recorded per dataset when Brick 3 runs (out of
   scope here, named).
4. ADR: `decisions/0007-injected-prepass-wire-extensions.md` - records why
   pbfhogg carries private wire extensions (BlobHeader field 5, Way field
   20), the superset semantics, presence = validity, the opt-in flag
   posture, the D9 resolved-refs refinement, and the hygiene rule. The
   landing establishes new architecture; the why is captured while fresh.
5. `CHANGELOG.md`: new capability entry (`--inject-prepass`, the two
   feature strings, public accessors `Blob::way_members` /
   `Way::shared_node_pins` / `BlobReader::set_parse_waymembers`, the
   `add_locations_to_ways` signature change to `AltwOptions` - a breaking
   library change). The `OwnedBlock` conversion, counters, and scratch
   format are internal; skipped per the CHANGELOG rules.
6. `TODO.md`: close the item; `notes/injected-prepass.md` gains a
   status line pointing at this spec and the ADR.

## Target artifacts (signatures pinned)

Read side (landing 1):

```rust
// blob_wire.rs
pub(crate) struct WireBlobHeader {
    pub blob_type: BlobKind,
    pub datasize: i32,
    pub indexdata: Option<[u8; INDEX_SIZE]>,
    pub tagdata: Option<Box<[u8]>>,
    pub waymembers: Option<Box<[u8]>>,           // field 5, raw incl. preamble
}
impl WireBlobHeader {
    pub fn parse(data: &[u8], parse_tagdata: bool, parse_indexdata: bool,
                 parse_waymembers: bool) -> Result<Self>;
}
// blob.rs
impl<R: Read + Send> BlobReader<R> {
    pub fn set_parse_waymembers(&mut self, enable: bool);
}
impl Blob {
    pub fn way_members(&self) -> Option<&[u8]>;       // D8 semantics
    pub fn way_member_count(&self) -> Option<u32>;    // D8 count-gap fold
}
// wire.rs
pub(crate) struct WireWay<'a> { /* existing */ pub pins_data: Option<&'a [u8]> }
// elements.rs
impl<'a> Way<'a> {
    pub fn shared_node_pins(&self) -> Option<&'a [u8]>;
}
// block.rs
impl HeaderBlock {
    pub const WAY_MEMBERS_V1: &str = "pbfhogg.WayMembers-v1";
    pub const SHARED_NODE_PINS_V1: &str = "pbfhogg.SharedNodePins-v1";
    pub fn has_way_members_v1(&self) -> bool;
    pub fn has_shared_node_pins_v1(&self) -> bool;
}
```

Write side (landing 1):

```rust
// block_builder.rs - D7 struct OwnedBlock
// framing.rs - returns Result now (D5 cap lives here, covers every emitter)
pub(crate) fn encode_blob_header_into(blob_type: &str, datasize: i32,
    indexdata: Option<&[u8]>, tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>, buf: &mut Vec<u8>) -> io::Result<()>;
// writer.rs
pub(crate) fn write_primitive_block_owned(&mut self, block_bytes: Vec<u8>,
    index: BlobIndex, tagdata: Option<&[u8]>,
    way_members: Option<&[u8]>) -> io::Result<()>;
```

Producer side (landings 2-3): `AltwOptions` (D11), `RelationScanOutput`
(D12), `CLOSURE_FLAG` / `LOCAL_ID_MASK` constants beside `IdRecord`,
`inject: bool` threaded through `Stage1Output` consumers,
`IntegratedInputs`, `ConcurrentBlobLocationRouter::new`, and
`stage4_assembly` / `write_output_passthrough` signatures. Field-5 payload
layout (both producers): `[0x01][varint way_count][ceil(way_count/8) bytes,
LSB-first, bit i = blob's i-th Way element in file order]`.

## Data flow (external, inject on)

```
relation blobs --(fused scan, overlapped w/ stage 1)--> member way IdSet ----+
way refs --stage1--> IdRecord (+bit-31 closure flag on trailing ring ref)    |
        --stage2--> masked sort -> run walk -> pin=count(unflagged)>=2       |
                    -> ResolvedEntry(lat<<1|pin, lon)                        |
        --stage3--> unpack -> coord_payloads v2 (2N varints + ceil(N/8) pins)|
        --stage4--> reframe: splice 9/10 (+ 20 when nonzero, strip stale     |
                    9/10/20) ; probe way id ------------------------------->-+
                    -> OwnedBlock{ bytes, index, tagdata:None,
                                   way_members:Some(field5 payload) }
        --writer--> BlobHeader fields 2,3,4,5 ; 64 KiB cap assert
header: LocationsOnWays + pbfhogg.WayMembers-v1 + pbfhogg.SharedNodePins-v1
```

## Risks and their bricks

- **Desk optimism (ledger lesson 1).** Every per-record cost above is
  branch-guarded or branch-free, but the verdict is the landing-5 bench
  matrix, not this document. The flag-off europe A/B is mandatory before
  the planet ask.
- **Record widening (ledger lesson 5).** Both scratch records stay 12
  bytes by construction (D1, D3); any future amendment that widens either
  is refuted by the ledger unless it brings new measurements.
- **Version skew between stage 3 and stage 4 payload framing.** Covered by
  the existing truncated-varint / trailing-bytes hard errors plus the
  run-level flag being a single value threaded from `external_join` - the
  two sides cannot disagree within a run (D4).
- **Sort-key corruption via the closure flag.** The D1 masking is the
  brick; the stage-2 unit tests in landing 2 include a flagged record
  ordering case specifically.
- **Half-usable flag between landings 2 and 3 (folded from R1 smell,
  minor).** Landing 2 ships `--inject-prepass` as a documented CLI flag that
  hard-errors on sparse (the default `--index-type`), working only under
  `--index-type external`. Both landings live in this spec so nothing is
  deferred out of the item, but if landing 3 slips the flag is half-usable.
  Mitigation: the landing-2 hard error names "sparse support lands next", and
  the two landings are adjacent keep/revert units - do not release a version
  boundary between them.
- **Header overflow on dense way blobs.** D5 hard error; adversarial unit
  test builds a synthetic > 512k-way header payload and asserts the error.
- **Enriched output is osmium-incompatible (folded from R2 point 5).** The
  survey says standard readers "skip both fields as unknown, paying only a
  wire-skip cost." True for a reader that accepts the header - but libosmium
  2.23 has a signed-char sign-extension bug that rejects ANY BlobHeader over
  127 bytes (`framing.rs` documents it, filed as libosmium issue 405).
  Field-5 headers run ~1 KB (Geofabrik) to ~8.3 KB (planet), so an enriched
  blob is rejected outright by affected osmium versions - the whole blob, not
  just the unknown field. Consequences: (1) the wire-skip framing holds only
  for readers that accept large headers (pbfhogg, nidhogg ingest), and the
  enriched file must be documented as osmium-incompatible (this is an
  amplification of the EXISTING indexdata/tagdata incompatibility, not new);
  (2) the flag-ON path cannot be cross-checked with `brokkr verify` against
  osmium - a flag-on external gate must read back with pbfhogg's own reader
  (or nidhogg), not osmium. The landing-1/2 osmium verifies stay valid
  because they exercise all-`None` / flag-off output whose headers stay
  small; only the flag-on enriched output is affected.

## Review reconciliation (2026-07-11)

Two reviews (R1 opus, R2 codex gpt-5.6) were validated against code and
folded. Every finding was accepted; the folds are inline at the decisions
they touch, cross-referenced below.

Accepted and folded:

- **D5 cap layer + off-by-one** (R1 bug + R2 point 4, consolidated). Cap
  moved into `encode_blob_header_into` (returns `Result`), reject `>=
  MAX_BLOB_HEADER_SIZE`. Verified: sync `write_framed_blob` and
  `reframe_raw_with_index_scratch` call the encoder directly, bypassing
  `frame_blob_into`; reader rejects at `>=` (`blob.rs`). See D5, Landing 1
  brick 2.
- **Undercounted call sites** (R1 gap). The four `encode_blob_header_into`
  callers and the extra `WireBlobHeader::parse` caller
  (`parse_blob_header_with_index`) are now named. Verified by grep. (R1 said
  "five callers"; the accurate `encode_blob_header_into` count is four -
  `frame_blob_into`, sync `write_framed_blob`, `reframe_raw_with_index_scratch`,
  `degrade` - plus the one `parse` caller. Substance holds.) See Landing 1
  brick 2.
- **D9 vs precedence rule / second open gate** (R1 inconsistency + R1 gate
  gap + R2 point 2, consolidated). D9 is now flagged as an open cross-repo
  gate with a survey-faithful fallback (zero-coord ResolvedEntry). See Gate,
  D9.
- **Field-5 count-validation gap** (R2 point 1). `way_member_count()` added
  so elivagar can compare encoded vs actual counts; oracle fixture must span
  a within-byte count difference. See D8, Landing 1/4.
- **Landing-1 vs survey ordering** (R2 point 3). Explicit exception recorded
  with rationale (additive, default-off, inert). See Gate.
- **osmium incompatibility of enriched output** (R2 point 5). See Risks.
- **Landing-5 copy-pasteable commands + "only remaining gate" overclaim**
  (R2 point 6). Variants pinned, compression split, prerequisites listed.
  See Status, Landing 5.
- **Flag-hygiene sweep inventory** (R2 point 7). Regenerated from the full
  `warn_locations_on_ways_loss` caller set. See Landing 4 brick 3.
- **Flag-off byte-identity overclaim** (R2 point 8). Qualified to inputs
  without fields 9/10/20; re-enrichment test added. See D11, Landing 3.
- **Half-usable flag between landings 2-3** (R1 smell). See Risks.
- **Nits** (R1): `BucketLayout` doc comment update (folded into D1);
  `WireBlobHeader` doc comment fix (folded into Landing 1 brick 3). The
  survey line-drift nit (`commands/mod.rs` 223 vs 225) is informational -
  this spec avoids source line numbers by policy, so nothing to change here.

Rejected: none. R1's "What checks out" section (12-byte record widths,
packed-lat arithmetic including negative lat, the D1 bit-31 bound,
`OwnedBlock` tuple shape, `WireWay` field set, `HeaderBlock::LOCATIONS_ON_WAYS`
model, `HeaderBuilder::from_header` non-copy) was re-verified and stands; no
change needed. The only reviewer inaccuracy is R1's caller count (five vs
four), corrected above without affecting the fix.
