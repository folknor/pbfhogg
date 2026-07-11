# Injected prepass metadata: WayMembers-v1 + SharedNodePins-v1 (survey)

Status: 2026-07-09 survey. Decisions 1 and 2 were RATIFIED 2026-07-10:
steady state is option (a) - altw moves into the daily loop and the
production merge drops `--locations-on-ways` (recorded in
`reference/pipeline.md`) - and injection is opt-in flag-gated with
sparse parity. Decision 3 (the Brick 1 superset screen) and the D9
refinement both CLOSED 2026-07-11 in pbfhogg's favor - see the dated
entry below. The producer landings are unblocked.

**2026-07-11: the format/reader layer has landed** - `WireBlobHeader`
field-5 parse/encode behind the `parse_waymembers` toggle,
`BlobReader::set_parse_waymembers`, the public `Blob::way_members()` /
`Blob::way_member_count()` / `Way::shared_node_pins()` accessors, the
`pbfhogg.WayMembers-v1` / `pbfhogg.SharedNodePins-v1` `HeaderBlock`
feature-string constants and accessors, and the write-time BlobHeader
size cap in `encode_blob_header_into` (now fallible). All additive and
default-off: no producer emits field 5/20 yet, so every reader/writer
call site still passes `None`/off and output is unchanged.

**2026-07-11, later: BOTH cross-repo gates are CLOSED** (elivagar's
verdicts recorded normatively in their `notes/injected-prepass-spec.md`
"Cross-repo ratifications (2026-07-11)" section, elivagar commit
`0f67f51`; instrumentation `4ceacd1`):

- **Brick 1 superset screen PASSED.** germany locations: superset
  1,092,149 vs needed 886,109 = 1.23x (threshold 1.5x; elivagar bench
  `f2b93719`); denmark 1.16x (`fc55fca5`). Superset semantics stand;
  the relation `tag_expr` filter is never built; the "Membership scan"
  section below needs no amendment. elivagar's Brick 4 still gates the
  real cost in bytes on their side (exact-plan baseline 112.3 MB data +
  14.1 MB index on germany locations).
- **D9 resolved-refs refinement RATIFIED as specified** (pin = shared
  AND resolved). Their length validations are unaffected (both depend
  only on lengths, which D9 never changes); the unrefined semantics
  would have pinned their (0,0) unresolved-sentinel vertices, forcing
  DP simplification to retain garbage - D9 is what they want, not what
  they tolerate; and it matches their node-store path, where pins never
  land on unresolved refs today. The survey's shared-ness definition in
  "Way message field 20" below should be read as narrowed to RESOLVED
  positions; the ring-closure trailing duplicate still mirrors bit 0 by
  construction (it resolves iff position 0 does). The survey-faithful
  fallback (zero-coord `ResolvedEntry` emission on shared orphan runs)
  is declined and closed - nothing on their side ever wants a pinned
  position without a real coordinate.
- **`Blob::way_member_count()` ACCEPTED** (the D8 count-validation
  fold). elivagar will grow a third release-checked hard error
  comparing the encoded `way_count` against the blob's actual decoded
  Way count. Consequence for pbfhogg: the producer-side oracle must
  include a fixture spanning a within-byte count difference (encoded
  vs actual counts that need the same number of bitmap bytes) so their
  compare is exercised end-to-end.

The altw producer (relation-scan fusion, stage 1-4 pin computation, the
`--inject-prepass` flag, sparse parity, oracle, benches) is therefore
UNBLOCKED. Remaining prerequisites are pbfhogg-side only: the brokkr
`--inject-prepass` passthrough brick, and the flag-on planet bench
green-light.

Origin: elivagar's `notes/injected-prepass-spec.md` (H2a + H2b of their
planet-30gb roadmap) specifies a cross-repo contract in which altw computes
two global facts at enrichment time and injects them into the enriched PBF,
so elivagar can delete their runtime derivation. That document is normative
for the interface; this note restates everything a pbfhogg reader needs so
nobody has to reach across repos, then maps the contract onto the actual
current external-join pipeline and records the findings and open decisions.

Companion docs:

- [altw-external.md](altw-external.md) - live leads for the external join;
  L1/L2 (BlobHeader extensions) are the direct ancestors of this work.
- [altw-optimization-history.md](altw-optimization-history.md) - the
  measured lessons this note's risk analysis leans on.
- [../reference/blob-density.md](../reference/blob-density.md) - element
  density figures used for the field-5 size policy.
- `reference/performance-history.md` "Pipelined-reader decode-admission
  bound" - the landed elivagar-reported backpressure fix (`a0a2e3b`);
  consistency notes in section 9. (The plan doc
  `notes/pipelined-reader-decode-backpressure.md` was retired after
  validation.)

## 1. What elivagar re-derives today, and why it moves here

elivagar's tilegen reads the altw-enriched PBF and re-derives two global
facts from it on every run:

1. **Relation plan.** A prepass reads every relation blob, matches
   `type=multipolygon` / `type=boundary` relations against its shortbread
   config, and collects member way ids into an `FxHashSet<i64>`. The way
   pass consults it per way to decide whether a tagless way must still feed
   the way index. On enriched (locations-on-ways) input the way blocks
   arrive almost immediately, so the join is nearly all stall: 6.3 s on
   germany locations, minutes projected at planet, plus a ~1 GB planet-scale
   id set held for the whole way phase.
2. **Shared-node pins.** DP simplification must not move junction vertices
   independently in ways that share them (visible gaps otherwise). The
   exact global answer (a second full way-blob read + external merge-sort
   of every ref) is disabled by default on cost; production uses a
   block-local approximation that misses cross-block junctions - a standing
   quality compromise.

Both facts are computable during altw, which already streams every way and
every relation once. The contract injects them through pbfhogg's existing
metadata channels and elivagar deletes the runtime derivation on the
enriched path (their runtime fallbacks stay, for raw Geofabrik input).

Two consumers read the enriched file: elivagar tilegen and nidhogg's PBF
ingest. There is ONE shared enriched file carrying both fields; standard
readers (osmium, nidhogg ingest) skip both fields as unknown, paying only a
wire-skip cost.

## 2. The contract (restated; normative source is the elivagar spec)

### Header feature flags

altw output declares, in `HeaderBlock.optional_features`, alongside
`LocationsOnWays`:

- `pbfhogg.WayMembers-v1` - every OSMData way blob carries BlobHeader
  field 5 (way-member bitmap), and it is trustworthy.
- `pbfhogg.SharedNodePins-v1` - every way element carries exact shared-node
  pin data (field 20, possibly omitted when empty), and it is exact.

elivagar treats the flags as authoritative and validates with
RELEASE-checked errors (not `debug_assert!`). Hard-error classes on their
side: field 5 absent on a way blob under `WayMembers-v1` (presence =
validity; an all-zero bitmap is still emitted, so absence is unambiguously
corruption); any present-but-wrong-length bitmap (field 5 vs the blob's way
count, field 20 vs the way's ref count); location count != ref count on a
pinned way; either flag present WITHOUT `LocationsOnWays` (altw never
produces that combination). Field-20 absence is NOT corruption - omission
legitimately means "no pins". Consequence for us: the writer must be exact,
not best-effort; there is no lenient reader downstream.

### BlobHeader field 5: way-member bitmap

protobuf field 5, wire type 2 (len-delimited bytes), on OSMData way blobs
only. Layout:

```
byte 0        version, 0x01
varint        way_count (number of Way elements in the blob)
ceil(n/8) B   bitmap, LSB-first: bit i = way at position i among the
              blob's Way elements in file order is a member way
```

Bit i set means: way i is referenced as a Way-type member by at least one
relation tagged `type=multipolygon` or `type=boundary`. **Superset
semantics** - no shortbread matching in pbfhogg; the cross-repo coupling
stays zero. Blobs where no way is a member still carry the field.

Size policy: the 64 KiB `MAX_BLOB_HEADER_SIZE` cap is hard. Field-5 size is
set by altw's per-blob way density. altw asserts the encoded BlobHeader
length against the cap when writing field 5; a blob that would overflow is
a hard write-time failure (split or reject), never a truncated bitmap.
Real densities (see [blob-density.md](../reference/blob-density.md)): way
blobs are ~8,000 ways/blob on Geofabrik input (~1 KB bitmap) and ~66,500
ways/blob on osm.org planet input (~8.3 KB) - both far under budget. The
header overflows only above ~512k ways/blob.

### Way message field 20: shared-node pin bitmap

protobuf field 20, wire type 2, inside the Way message alongside refs (8) /
lat (9) / lon (10). Bitmap, LSB-first, bit i = the node at ref position i
is a shared node. Length exactly `ceil(ref_count/8)` when present. OMITTED
when no bit is set. Field 20 is outside the range used by osmformat.proto
and osmium; standard readers skip it.

Shared-ness, exact definition (computed over ALL ways of the input): for
each way take its refs, minus the trailing ref when `len >= 4 && first ==
last` (ring-closure duplicate). Count occurrences of each node id across
all these slices, all ways, including repeats within one way. Total count
>= 2 = shared. The bitmap sets the bit at EVERY position holding a shared
id, including a closed ring's trailing duplicate (it mirrors bit 0 by
construction).

Two deliberate divergences from elivagar's block-local semantics, both
accepted on their side: (1) ways with <= 2 refs CONTRIBUTE occurrences
(block-local excluded them); (2) the ring-closure self-count is gone
(closing duplicate skipped when counting). Consequence: a detached closed
ring (typical building) pins nothing and omits field 20.

### pbfhogg public API consumed by elivagar

- `Blob::way_members(&self) -> Option<&[u8]>` - raw bitmap bytes of header
  field 5, version + count preamble stripped and validated; `None` when
  absent. Public (today's `Blob::index()` is `pub(crate)`; this becomes the
  first public per-blob metadata accessor).
- A new opt-in parse toggle (`parse_waymembers`, modeled on
  `parse_tagdata`) threaded through `BlobReader::new`, so read paths that
  do not want field 5 pay nothing. elivagar sets it ON exactly when the
  header declares `pbfhogg.WayMembers-v1` - and sets it on `BlobReader`
  directly, because their locations-path read loop is BlobReader-based
  (section 9).
- `Way::shared_node_pins(&self) -> Option<&[u8]>` - the field-20 bitmap,
  `None` when omitted.
- `HeaderBuilder` grows nothing structurally; altw appends the two feature
  strings via the existing `optional_feature`.

### altw computation, pinned at algorithm level by the contract

- Membership: a relation-blob pre-scan before any way blob is written;
  member way ids of mp/boundary relations into an `IdSet`; pass 2 / stage 4
  reads the set when building each way blob's field 5.
- Shared counting: external mode rides the join that already brings every
  ref occurrence of a node id together (see section 3 - the contract text
  says "double radix permutation", which is stale; the property survives).
  Sparse mode: an occurrence-count table. Either way the pin bits arrive in
  way order for field-20 emission.
- Any pass-2 fast path that would skip re-encoding way payloads is
  incompatible with the pins flag and must be disabled or made pin-aware
  when pins are requested. (In external mode this is moot: way blobs are
  always reframed.)

### Bricks and gates (cross-repo ordering)

1. **Brick 1 (elivagar, first)** - instrument the superset price: emit
   `relation_plan_superset_ways` (= members of ALL mp/boundary relations,
   exactly what field 5 will contain) beside `relation_plan_needed_ways`.
   Proceed threshold: superset <= 1.5x needed on germany locations. Above
   it, the contract gains a relation tag filter (altw takes a `tag_expr`
   CLI argument derived from shortbread's relation matchers) - a durable
   cross-repo coupling the design explicitly wants to avoid; it is built
   only if the measured inflation forces it. The count ratio is an early
   screen; elivagar's Brick 4 gates the real cost in bytes.
   **Do not start pbfhogg format work before this screen passes.**
2. **Brick 2 (pbfhogg)** - the paired change this note precedes: field-5
   parse/encode, field-20 accessor, `Blob::way_members()`, altw relation
   pre-scan + shared-bit join, header feature strings, and a roundtrip
   test: altw a fixture, read back bitmaps and pins, compare against an
   in-test oracle that recomputes both from the fixture's raw elements.
   Gated by pbfhogg's own suite.
3. **Brick 3 (data)** - re-enrich denmark, germany, norway on plantasjen.
   Gate: the CURRENT elivagar must run the new files unchanged (the
   injection is additive). On-disk growth is recorded per dataset (absolute
   + percent) as the honesty-clause reading of the cost every reader pays.
4. **Bricks 4/5 (elivagar)** - consume membership, then pins + teardown of
   their global prepass. Their gates, not ours.
5. **Brick 6** - re-enrich north-america, planet-slope reading.

## 3. Where the contract lands in the current external pipeline

The elivagar spec's algorithm sketch predates A1 (rankless node-ID bucketed
join, landed 2026-04-25): there is no rank machinery and no double radix
permutation anymore. The property the contract needs survives in the A1
shape, and the paired spec must be written against it:

- **Membership scan**: `external/relation_scan.rs:22`
  (`collect_relation_member_node_ids_indexed`) already preads only relation
  blobs via the `BlobMeta` table, builds full `PrimitiveBlock`s, and
  iterates members. Fusing in a relation `type` tag check and collecting
  `MemberId::Way` ids into a second `IdSet` is the same decompress, same
  iteration, one extra branch. Today the scan fires only when
  `keep_untagged_nodes=false`; with injection on it must run
  unconditionally. It runs overlapped with stages 1/2 and only needs to
  complete before stage 4 emits its first way blob header - wall cost is
  approximately zero. The member `IdSet` over way-id space is ~200 MB
  resident through stage 4 (renumber's way bitset is the precedent),
  against stage 4's ~12 GB planet peak.
- **Shared-ness detection**: stage 2 sorts each bucket's `IdRecord`s by
  `local_node_id` before the merge walk (`stage2.rs`, `prepare_bucket`
  sort + the walk at `stage2_node_join`), and bucketing is by node id, so
  all global occurrences of a node id are consecutive at `record_ptr` in
  exactly one bucket. Shared = run-length of counting records >= 2, i.e.
  one comparison against the previous record in a loop that is already
  memory-bound.
- **Ring-closure exclusion**: stage 1 walks refs per way anyway (it emits
  one IdRecord per ref and counts refs for the per-way sidecar). Detecting
  `len >= 4 && first == last` is remembering one id; the closure ref's
  record must still exist (its coordinate gets spliced) but must carry a
  "non-counting" flag so stage 2's run length skips it. It inherits the
  shared bit naturally by being in the same run, which is exactly the
  "mirrors bit 0" behaviour the contract wants.
- **Pin transport stage 2 -> 3**: the pin bit must ride each
  `ResolvedEntry`. See section 4 - do not widen the record.
- **Stage 3**: while encoding each way's delta-varint coord payload, fold
  the per-ref pin bits into a per-way bitmap appended to the
  `coord_payloads` format (our own scratch format; version it).
- **Stage 4**: `reframe_way_blob_with_locations` (stage4.rs) already
  parses way ids and splices packed fields from the payload cursor with a
  trailing-bytes integrity check. Field 20 is a bytes-field splice of the
  payload bitmap when nonzero; field 5 is one member-`IdSet` probe per way
  folded into a per-blob bitmap handed to the framing path (the
  `OwnedBlock` tuple grows an optional way-members member, alongside the
  existing optional tagdata).

Sparse mode (`altw/reframe.rs` path) needs the same two emissions if the
flags are unconditional; see the parity decision in section 6.

## 4. The record-width hazard, and two zero-widening escapes

[altw-optimization-history.md](altw-optimization-history.md) lesson 5: the
12-byte intermediate records are a measured local optimum; both widening
attempts regressed (epoch spill 16-byte: +10% planet; blob-group 16-byte:
+3.6 to +9.4% europe). The slot-bucket stream is the largest scratch stream
(~150 GB at planet). Naively adding a pin byte, or widening either record
to 16 bytes, is the one implementation history says will blow the
regression budget on its own. Two escapes keep both records at 12 bytes:

1. **Stage-1 `IdRecord` closure flag in `local_node_id` bit 31.**
   `BucketLayout::new` asserts `bucket_width <= u32::MAX`; at planet with
   256 buckets the width is ~55M, needing 26 bits (`external/mod.rs` says
   so in its own doc comment). Tighten the constructor assert to
   `bucket_width < 2^31` and bit 31 is free for "do not count me". The
   existing "silent truncation of local_node_id is forbidden" invariant is
   preserved by the tightened assert.
2. **Stage-2 `ResolvedEntry` pin bit packed into lat.** lat in
   decimicrodegrees is bounded by |9e8| < 2^30, so `(lat << 1) | pin` fits
   i32 with a bit to spare (lon is bounded by |1.8e9| and does NOT have a
   spare bit - it must be lat). One shift on write in stage 2, one on read
   in stage 3. The zero-coord unresolved sentinel stays raw `(0, 0)`; no
   pinned real coordinate can collide with it (a pinned lat encodes to an
   odd value).

Both are cheap, both are exactly the kind of desk reasoning lesson 1 warns
about ("desk estimates on this code path are systematically optimistic") -
they must be measured, not trusted.

## 5. Cost map against the regression budget

Standing constraint: **ALTW external must not regress more than 3%.**
Planet external baseline 546.0 s (`7fd04130`, plantasjen) -> budget ~16 s.
Europe 233-271 s depending on compression -> ~7-8 s.

| Component | Where | Desk estimate (planet) |
|---|---|---|
| Membership relation scan (fused, overlapped) | relation_scan | ~0 wall |
| Member IdSet probes + field-5 bitmaps | stage 4 | <1 s CPU across workers |
| Closure flag + first-id tracking | stage 1 | noise |
| Run-length + bit pack/unpack | stages 2/3 | noise-to-small; hot loop |
| Per-way pin bitmaps in coord_payloads | stage 3/4 | +~1.5 GB on 54.8 GB payloads (+~3%), ~+1.5-2 s of stage-4 read floor |
| Field-20/field-5 output bytes | stage 4 writer | small; interacts with the zlib:6 writer ceiling |
| On-disk growth of enriched output | - | field 5 ~150 MB (headers, uncompressed) + field 20 (compressed in-blob); order +1-2% - this is Brick 3's recorded reading |

Total desk estimate: low single-digit seconds at planet, inside budget.
Mandatory caveats: lesson 1 (desk optimism - measure denmark/japan first),
and the writer-ceiling diagnostic (measure keep/revert under both `zlib:6`
and `zstd:1`, since the added bytes land in the compression path).

The "field 20 omitted when empty" case should be genuinely common:
buildings dominate way counts and pin nothing once the closure self-count
is excluded. Road ways will almost all carry it (endpoints are junctions).

## 6. Recommendation: opt-in flag, and the sparse-parity decision

The elivagar spec reads as if altw emits the fields unconditionally. The
paired spec should gate the injection behind a CLI flag (one flag for both,
e.g. `--inject-prepass`, or two). The contract only pins what the feature
strings MEAN when present; a flag-gated producer satisfies it, and brokkr
passes the flag when enriching. This buys:

- The default `altw` path stays byte-identical - the 3% bound holds by
  construction for everyone not running the enrichment, and the enriched
  run becomes its own brokkr variant with its own honest price on the
  record (which is what the elivagar spec's honesty clause wants anyway).
- It scopes the sparse question. Unconditional emission forces sparse (and
  the decode-all fallback) to implement both fields too, or the
  `backend_parity_dense_sparse_external_auto` CLI canary dies.

Sparse-parity recommendation: implement the fields in sparse as well. An
occurrence-count table (or a pair of seen/seen-twice bitsets over the rank
index) is genuinely easy at sparse's scales, and keeping the
backend-parity canary as an oracle for the external implementation is
worth more than the code it costs. The fallback position, if sparse parity
turns out annoying, is to restrict the flag to `--index-type external` and
hard-error on other combinations - but then the parity test cannot cover
the new fields.

The roundtrip test (Brick 2's gate) recomputes both facts from the
fixture's raw elements in-test and compares against the emitted bitmaps;
with sparse parity, `brokkr verify add-locations-to-ways` plus the parity
canary give three independent implementations checking each other.

## 7. Finding: the steady-state pipeline discards the enrichment

`reference/pipeline.md` is explicit: altw runs once at bootstrap; steady
state is `apply-changes --locations-on-ways` daily. And
`apply_changes/rewrite.rs:133` builds a FRESH output header
(`hb.sorted().optional_feature("LocationsOnWays")`) rather than copying the
base's optional_features. So after the first daily diff the enrichment
flags vanish, elivagar falls back to its runtime prepass, and the entire
injected-prepass win exists only on freshly-altw'd files.

This is safe by accident - no hard errors, just stale dead field-5/20
bytes riding along inside passthrough blobs, unparsed because the flag is
gone. But it means the elivagar spec's "once per enrichment" framing
quietly assumes enrichment happens every refresh cycle, which today's
pipeline does not do.

It also cannot be patched by just copying the flags forward:

- Stale field 5 after a diff is a **false-negative membership** risk (a
  new relation makes an existing way a member; its bit is 0). That is
  wrong tiles, the one direction the superset design does not tolerate.
- Stale field 20 is a silent quality regression (missing pins on new and
  changed ways - and on the injected path elivagar skips block-local
  counting entirely, so locally worse than today's approximation).
- Incremental maintenance of field 5 in apply-changes is a real but
  tractable feature: relation blobs are a tiny tail, so recomputing the
  member set per merge is cheap - but changed bits can land in
  otherwise-passthrough way blobs, forcing header reframes on them
  (cat-passthrough-style cost, not CopyRange).
- Incremental field 20 is effectively infeasible: it needs exact global
  ref counts with decrement (a persistent count store over ~2B referenced
  nodes), and a new way can flip a bit inside another way's payload in an
  untouched blob.

Options, in decreasing enrichment freshness:

- (a) Re-run altw after each daily merge (~9-10 min/day at planet; the
  merge could then drop `--locations-on-ways` since altw redoes it).
- (b) Teach apply-changes to maintain field 5 and accept that pins go
  stale until the next full enrichment.
- (c) Accept bootstrap-only benefit; steady state uses elivagar's runtime
  fallbacks.

**This is a product decision that shapes how much Brick 2 complexity is
worth buying, and it must be taken before the paired spec is written.**

**RESOLVED 2026-07-10: option (a) ratified.** Rationale: post-A1
external is ~9 min at planet (546.0 s), the daily loop already carries
a post-merge rebuild of the same magnitude (build-geocode-index ~7
min), and nidhogg had not yet enabled `locations_on_ways` so nothing
downstream migrates. Cost accepted: daily write volume roughly doubles
at planet. Full pipeline shape in `reference/pipeline.md`.

Related asymmetry worth recording: subset-producing commands
(extract/getid/tags-filter) err SAFE if metadata were ever carried through
them - dropping relations only turns true members into false positives
(superset stays superset), and dropping ways only over-pins. Data-adding
commands (apply-changes) err UNSAFE (false negatives). In practice all of
these rebuild headers and drop the fields anyway; see section 8.

## 8. Flag hygiene across rewriting commands

Every re-encoding command (`repack`, `sort`'s overlap path, `extract`,
`getid`, `tags-filter`, `cat --clean`, `apply-changes`) rebuilds
BlobHeaders and Way payloads and therefore drops fields 5/20; since none
of them copy `optional_features` from the input header today, they are all
accidentally safe (output carries no flags, elivagar takes its fallback).
The paired spec must state the rule explicitly so a future
header-preserving change does not create malformed enrichments:

> Any command that rewrites way payloads or way-blob headers without
> maintaining fields 5/20 must not emit `pbfhogg.WayMembers-v1` /
> `pbfhogg.SharedNodePins-v1` in its output header.

Precedent for the posture: `commands/mod.rs:223` already warns when a
command is about to silently strip `LocationsOnWays`. A test pinning the
rule (enriched fixture through each rewriting command; assert no flags in
output) belongs in Brick 2.

## 9. Consistency with the decode-backpressure work

The decode-backpressure fix (elivagar-reported; pipelined-reader
admission gate) landed and was validated 2026-07-10 at commit `a0a2e3b`
- all gates kept; the full record is in
`reference/performance-history.md` "Pipelined-reader decode-admission
bound". elivagar's locations path reads via its own BlobReader-based
bounded loop (Option 3 of that decision). Three touch points:

- The contract's field-5 enforcement point on the elivagar side ("the
  decode worker, right where it calls `blob.way_members()`") lives inside
  that Option-3 loop - which is why the `parse_waymembers` toggle belongs
  on `BlobReader::new`, not only on `ElementReader`.
- ALTW external is untouched by the backpressure fix (pread workers
  throughout), so the two work items do not interact on the 3% budget.
- The sequencing gate this section used to track is cleared: the
  backpressure fix is landed and validated, so Brick 2 no longer risks
  sharing a baseline with an unlanded behaviour change on the same read
  paths. Brick 2 benches should baseline at `a0a2e3b` or later.

## 10. Lineage

This contract is the decision that
[altw-external.md](altw-external.md) L1/L2 ("BlobHeader refcount / node-ID
extensions written by `cat`, consumed by ALTW stage 1") have been blocked
on since April: "does the production pipeline guarantee pbfhogg-produced
inputs, treating header extensions as opaque elsewhere?" The elivagar spec
answers yes for the elivagar-facing direction. If Brick 2 lands the
field-5 plumbing (opt-in header field, write-time cap assert,
`parse_waymembers` toggle), L1's refcount extension becomes a small
follow-up on the same rails.

## 11. Open decisions before the paired spec

1. ~~Steady-state story (section 7, options a/b/c)~~ - **RATIFIED
   2026-07-10: option (a).** altw runs in the daily loop after each
   merge; the production merge drops `--locations-on-ways`; both
   injected fields stay fresh every cycle, so apply-changes needs no
   field-5/20 maintenance (only the section-8 hygiene rule + pinning
   test). Recorded in `reference/pipeline.md`.
2. ~~Opt-in flag shape and sparse parity (section 6)~~ - **RATIFIED
   2026-07-10 as recommended:** flag-gated injection (working name
   `--inject-prepass`), sparse implements both fields to keep the
   backend-parity canary.
3. ~~Wait for elivagar's Brick 1 superset screen (<= 1.5x on germany
   locations)~~ - **PASSED 2026-07-11** (germany 1.23x, denmark 1.16x;
   elivagar commits `4ceacd1` instrumentation / `0f67f51` record). The
   contract does not change; no relation tag filter. Together with the
   same-day D9 ratification (see the status entry at the top), no open
   cross-repo gate remains on the producer landings.
