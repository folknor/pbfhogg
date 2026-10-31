# Indexdata trust model

What pbfhogg believes about a PBF without decoding it, what it checks along
the way, what verifies the rest, and what a false claim costs. The decision
and its rationale are [ADR-0013](../decisions/0013-blob-metadata-is-trusted-by-default-and-verified-on-demand.md);
this page is the current operational statement of it, and every command that
acts on these claims cites it.

## Scope

- **Sidecar claims**: `BlobHeader.indexdata` (field 2: element kind, `min_id`,
  `max_id`, `count`, and for node blobs a spatial bbox) and
  `BlobHeader.tagdata` (field 4: the blob's set of tag keys).
- **Order claims**: the header's `Sort.Type_then_ID`, which promises type
  order across blobs and IDs within each kind in canonical OSM order (`0`,
  then the negative IDs by ascending absolute value, then the positives),
  inside every blob as well as across blobs.
- **Indexing classification**: how a command decides whether a file is
  indexed at all. This is an inference, not a claim (below).

Out of scope: the header's feature declarations (`LocationsOnWays`,
`pbfhogg.WayMembers-v1`, `pbfhogg.SharedNodePins-v1`). They are believed too
and no command verifies what they claim; their meaning is owned by the "Output
header" section of the [CLI reference](../docs/cli/commands.md#output-header).
`digest --form full` checks only the shape of the two prepass payloads a
header declares (one well-formed payload per way blob, one bit per ref) and
refuses a malformed one, since it hashes them.

## The ruling

Every claim in scope is **believed, not verified**, by the commands that act
on it; verification is the job of the explicit verifiers (`check --index`,
`check --ids`). A file whose claims are false is outside the contract: a
command given one may produce wrong output and report success. pbfhogg's
writer derives every sidecar it computes from the payload (`cat` included,
for the blobs it indexes, and `apply-changes --force`, for the unindexed
blobs it would otherwise copy); a raw copy keeps the input's own, and `renumber`
carries each node blob's bbox into the index it rebuilds (below). pbfhogg's
sorted producers produce honestly sorted output. For a file of unknown
provenance, the verifiers are the admission test, within the coverage gaps
this page lists.

Verifying on every read would cost the per-blob decode that the splice and
passthrough fast paths exist to avoid, so verification is opt-in. That cost
holds only for blobs a run skips or copies on the claim; a blob the run
decodes anyway can be compared with its claim for free, and where that
comparison catches a lie that would otherwise publish a wrong answer it is
made (the "Checked in passing" column below). No consuming command adds a
decode pass only to check a claim.

## Kind claims refused at decode

Every decode builds its block through one validation pass that already
visits every element, and that pass records which element kinds the block
holds (a plain node, a dense group with at least one ID, a way, a relation;
empty groups, an empty dense message and changeset groups add nothing). A
phase that scheduled a blob on its claimed kind can therefore refuse the
blob, after decoding it, when it holds an element of another kind: the phase
would drop that element, and the phase that needs it never reads the blob.
The refusal is `ErrorKind::IndexdataMismatch` carrying a
`KindContradiction { claimed, found }`, located at the blob; the CLI's
advice names `check --index` and `cat --reindex`. It costs one compare per
decoded blob and decodes nothing the phase would not.

It is a foreign-kind test, deliberately weaker than `check --index`: an
empty blob under any claim passes it, and it says nothing of count, range or
bbox. It sees only the blobs a phase decodes; a blob skipped or copied on its
claim is not seen.

A phase adopts it where it builds its schedule: the classify schedule
builders and `ScheduledBlob::from_meta` / `at_frame` take
`KindClaims::Refuse` or `KindClaims::Unchecked` (an `Unchecked` is a phase
that believes its claims unchecked), and the pipelined reader takes
`ElementReader::refusing_kind_claims`. The block-building pools and the
pipelined decode step make the check; a raw-bytes walker builds no block and
checks the group fields it reads itself, as `check --locations` does.

In `check --refs`, `check --ids` and (through its own raw scanner)
`check --locations` a contradiction stops the check rather than refuses the
file: the check fails it, exit 1 ([CLI reference](../docs/cli/commands.md#check), `check`). Everywhere
else it is a refusal, exit 2, with nothing published. Adopted by:

- repack, and degrade's decode path and `--drop-ids` selection;
- the sort and `cat --dedupe` sweep, for blobs it decodes in an overlap run
  on a believed claim (an unsorted-header sort indexes blobs from their
  payloads and so has no claim to refuse);
- diff and derive (`diff --format osc`), at the one block decode both use;
- the geocode builder, every pass (Pass 1 through the pipelined reader,
  Pass 1.5 through its raw way scan, Passes 2a and 2b through the pools), so
  every kind lie in its input is refused;
- extract in every strategy and multi-extract, including their raw node and
  way scans and the raw-copy candidate scan;
- tags-filter (two-pass and `-R`), tags-count (so `inspect --tags --type`),
  `inspect --nodes`, getid (include, `removeid`, `--add-referenced`,
  `--id-osm-file`) and getparents;
- `apply-changes`, for every blob a worker decodes (below), including the
  way blobs `--locations-on-ways` searches without a parse;
- `add-locations-to-ways`, both index types, at every decode: the raw way
  and node scans (sparse pass 0 and pass 1, external stages 1 and 2, from
  the kinds the `src/scan` walkers record), both relation member scans, and
  the decode of a node blob for re-encoding (sparse pass 2, external stage
  4). A way inside a blob claiming nodes or relations is refused the same
  way by both, where external used to refuse it with an untyped error and
  sparse wrote it without locations.

`time-filter` builds no per-kind schedule and reads no indexdata, so it has
no kind claim to refuse (its order checks are below).
`tags-filter --distrust-metadata` and `sort --distrust-metadata` believe no
claim and are never refused. A blob a phase skips or copies on its claim without decoding
it is still believed, which is the residual the table below records; in
getid's include and streaming paths, which handle every decoded element by
its real kind, the refusal makes a lie visible rather than changes an
answer.

A blob that carries no `indexdata` or no `tagdata` makes no claim. Readers
also treat sidecar bytes they cannot parse (unrecognised length, version or
kind byte, an inverted range, a repeated field) as absent and decode the
blob; a verifier must not, and `check --index` reports them (ADR-0013,
consequence 3).

## Range, count and order claims refused at decode

The engines that place, count or copy a blob by its claim and decode some
blobs anyway check the claim on the element loop they already run: an
increment and a branchless min and max per element (`blob_meta::IdTally`,
walked by `blob_meta::ClaimWalk`), and where the run depends on order one
key compare with the previous element. At the end of the
blob's complete walk the claim must be exact, `count`, `min_id` and `max_id`
equal to what the walk saw (the `check --index` rule for those fields), or the
run fails with `ErrorKind::IndexdataMismatch` (no `KindContradiction`),
located at the blob, before anything the blob produced leaves the worker.
A decoded blob with a claim and no element is refused too: no index describes
one. Overstated ranges are refused, not only understated ones: an overstated
range hands a neighbour's upsert to the blob as well.

- **`apply-changes`**, every blob a worker decodes: each precise-check
  candidate (a miss walks every element; a hit, a forced rewrite and a
  refresh-only rewrite go through the rewrite loop, which does) and every way
  blob `--locations-on-ways` searches without a parse (a clean search has
  walked every way). An ID below its predecessor in canonical OSM order
  contradicts the header's `Sort.Type_then_ID` and is `ErrorKind::Unsorted`
  (the CLI advises `sort --distrust-metadata`, below), unless an ID the walk has met already lies
  outside the claimed range, which makes it the false claim. Each decoded
  blob's first element travels to the drain, which places the blob by it:
  the inline upserts the blob writes start there, the gap creates before it
  stop there, and the output order guard compares it with what was written
  before. A mixed-sign range cannot name its first element, and the range's
  stand-ins disagreed: blobs `[-1]` and `[-2, 1]` both wrote an upsert of
  -1, and a blob `[-5, 1]` whose first element is -1 passed the guard after
  one ending at -3. Both were wrong answers on honest-range input; the
  first is fixed and the second refused. A blob a worker reads without
  decoding (`--direct-io` passthrough) and the scanner's splice are placed
  by the range alone, as before.
- **`sort` and `cat --dedupe`**, every entry of an overlap run whose claim
  is believed (an indexed blob under a header claiming sorted): the push
  loop tallies the entry, and its claim must be exact before the pops for
  the next entry write anything. Runs and passthroughs are planned on the
  claims, so a claimed minimum above an element let the sweep write it after
  larger IDs, and a claimed maximum below one let a run write past the next
  passthrough's first ID. Order inside an entry is not checked: the sweep's
  heap reorders it. An entry indexed from its payload (an unsorted-header
  sort, a blob without indexdata, a restructured block) makes no claim.
- **`add-locations-to-ways`**, both index types, every blob they decode in
  full: way blobs in sparse pass 2 and external stages 1 and 4 (the
  reframes already count their ways and their extremes, so the check there
  is one compare, and a way-claimed blob holding no way can no longer be
  written back as Way, count 0, IDs 0..0), node blobs in sparse pass 1
  (every node blob) and in external stage 2 and stage 4 (those its ID
  buckets reach, and those it re-encodes), relation blobs in either
  relation member scan. A passed-through blob's statistics and header come
  from its claim, and external places node blobs in its ID buckets by their
  ranges, so an understated range lost present locations there. External
  also checks order on every blob it decodes, inside the blob and against
  the blob before it by its decoded first element (a mixed-sign range
  cannot name it): a descent is `Unsorted`. Sparse does not depend on order
  and does not check it.
- **`time-filter`** reads no indexdata, but it believes the header's
  `Sort.Type_then_ID`, which its history path groups versions by: an
  element whose (kind, ID) sorts below the one before is `Unsorted`, one
  compare per element on both paths (the snapshot path checks every input
  element before its timestamp gate, and block boundaries in its ordered
  drain). Within one object a history input's versions must strictly
  ascend, and a non-history input must hold each object once; either broken
  is `InvalidData`, since `sort` does not reorder versions and keeps
  duplicates.

What stays believed: every blob none of these decodes, which the table
below records (the splice and passthrough fast paths, the sorted fast path's
non-overlapping blobs, and their copied headers).

### The remedy for `Unsorted`

Every `Unsorted` refusal's advice is `sort --distrust-metadata`. Plain `sort`
on a file whose header claims `Sort.Type_then_ID` believes the claim and each
blob's indexdata, and copies a blob that overlaps no neighbour unchanged, so
for a blob out of order inside, or a false range hiding a cross-blob
disorder, it reproduces the file the refusal named. `--distrust-metadata`
scans every blob's payload as for a header without the claim (order, kind,
range, count, bbox), and its tag keys too: an internally unsorted blob is
rewritten, a blob whose indexdata or tagdata (compared as a key set) its
payload contradicts is reframed under the scanned ones, keeping every other
`BlobHeader` field, and an honest blob is copied byte for byte. It reads no
indexdata, so it needs none and accepts a partially indexed file. On a
header without the claim it costs what plain `sort` costs.

## Claims: who believes, what checks, what a lie costs

| Claim | Believed by, and what a lie costs | Checked in passing by | Verified by |
|-------|-----------------------------------|-----------------------|-------------|
| kind | Every per-kind schedule: a blob labelled with the wrong kind is invisible to the phase that needs its elements; the phase that does read it refuses it where the decode-time refusal is adopted ("Kind claims refused at decode") and drops elements of the unclaimed kind silently where it is not. That covers `repack`, `degrade`'s decode path, `tags-filter`, `tags-count`, `getparents`, `inspect tags` / `--nodes` / `--show`, the sorted `extract` schedules, `add-locations-to-ways` (every scan) and the geocode builder. `cat --type` copies a matching blob raw, so a mislabelled blob leaks off-kind elements or vanishes. `cat --clean` keeps every decoded element of a selected kind whatever its blob claims, so with every kind selected the kind claim decides nothing; with `--type` a blob labelled with an unselected kind is skipped whole. `apply-changes` with `--locations-on-ways` extracts coordinates only from blobs claimed as nodes, and searches for surviving ways with stale inline coordinates only in blobs claimed as ways, so a way blob labelled with another kind can be passed through with its stale coordinates. `check --locations` reads a blob claiming ways only in its way passes; every other blob it decodes once and routes on what it holds. `check --ids` / `--refs` read each blob in the phase of its claimed kind and judge type order from the claims; a blob claiming relations is read by `--refs` only under `--check-relations`, and `--ids -t` reads only the selected kinds' blobs, so a lie in a blob no phase reads passes them. | `renumber` (its wire rewrite refuses an element of another kind in a blob scheduled for one kind); `check --ids`, `check --refs` and `check --locations` (a blob claiming ways) stop and fail the file on a decoded element of another kind; every other adopting command refuses it ("Kind claims refused at decode") | `check --index` |
| `min_id` / `max_id` | `apply-changes` routes a blob whose range misses the diff to the splice fast path and copies it unchanged. `getid` skips a blob whose range holds no requested ID; `getid --invert` copies it raw. `diff` cuts its work units from ranges, decides from them which blob pairs overlap, and skips decoding a pair whose claimed range and count match when its compressed bytes are identical (`-c`, `-q`, `--format osc`). `sort` (declared-sorted fast path) and `cat --dedupe` analyse overlap from ranges and copy non-overlapping blobs raw. `add-locations-to-ways --index-type external` partitions node blobs by range, so a node blob no ID bucket with references reaches is never decoded (`check --locations` partitions them by the range it decoded, not the claimed one). `inspect --show` skips on range. | `apply-changes` (every blob a worker decodes) and the `sort` / `cat --dedupe` sweep (every believed overlap-run entry) refuse a range that is not exact ("Range, count and order claims refused at decode"); `diff` refuses ranges that break canonical order, and a block holding elements past its claimed `max_id` where the merge meets them (a work-unit boundary inside the claimed range hides what lies past it); `add-locations-to-ways` (both index types) refuses a range that is not exact on every blob it decodes, and `--index-type external` refuses reversed, negative-node and out-of-order ranges, including an overstated `max_id` overlapping the next blob; the geocode builder's Pass 2a refuses any decoded node above the largest claimed node `max_id` (a global bound, not each blob's range) | `check --index` |
| `count` | `apply-changes` credits a spliced or passed-through blob's `count` to its `base_*` statistics, folded into the observed counts. `sort`, `getid --invert` and `cat --dedupe` report a passed-through blob's `count` separately, as claimed (`passthrough_claimed`). `inspect` reports counts from indexdata unless `--distrust-metadata`. `renumber` assigns ID blocks from it. `add-locations-to-ways` reports a passed-through blob's count in its statistics. `diff -c` and `diff -q` count a byte-identical pair they skip by the claim. | `renumber` (refuses a blob whose decoded count disagrees); `apply-changes`, `add-locations-to-ways` and the `sort` / `cat --dedupe` sweep, on the blobs they decode, as for the range, so a passed-through blob `apply-changes` decoded is credited a verified count | `check --index` |
| bbox | `extract` and the public `BlobFilter::with_node_bbox` skip a blob claimed as nodes whose bbox misses the region, without decoding it; the bbox is consulted only when the claimed kind is node, so a mislabelled blob skipped this way loses everything in it, on the unsorted `extract --simple` path too. `extract` also nominates a node blob whose bbox lies inside the region for a raw copy. | `extract` decodes a nominated blob and copies it raw only when its payload is DenseNodes alone and every node lies in the region, counting the nodes it decoded. That is selection and structural verification, not verification of the copied header. | `check --index` (containment) |
| tagdata | `tags-filter` and `tags-count` skip a blob whose key set lacks every required key: a key set omitting a key its elements carry drops matches. `add-locations-to-ways --index-type external`, removing untagged nodes, drops a whole node blob whose key set is empty and whose claimed range holds no retained relation member, so a false empty key set deletes tagged nodes and a false range hides members. | - | `check --index` (missing and surplus keys, and tagdata on a blob no index can describe) |
| `Sort.Type_then_ID` | Every command that requires sorted input reads the flag alone. `sort` takes its header-only fast path exactly when the header claims sorted and the blob has readable indexdata, and copies non-overlapping blobs raw, so an internally unsorted blob under the claim passes through under the sorted stamp ([CORRECTNESS.md](../CORRECTNESS.md), "`sort`: intra-blob disorder"). `diff` trusts order inside a blob and inside a range spanning negative and positive IDs. | `renumber` and `digest` check order while reading; `sort` and `cat --dedupe` (one shared pass 1) check intra-blob order on every blob outside the header-only fast path; `apply-changes` refuses disorder inside every blob a worker decodes, and a decoded blob whose first element sorts before what it has written (its output order guard checks claimed ranges for the rest); `add-locations-to-ways --index-type external` refuses disorder inside every blob it decodes and across way and relation blobs by their decoded first elements (it merge-walks node tuples against ID-sorted records, where disorder lost present locations); `time-filter` refuses an element below its predecessor on both paths | `check --ids`, partly (below) |

The worst of these, stated plainly:

- `apply-changes` splicing a blob whose range is understated: the blob is
  copied unchanged past a modify or delete aimed at one of its elements,
  and a create whose ID falls in the blob's real span can be written out of
  order under the sorted stamp. The splice is decided on the claim alone;
  a blob whose claimed range meets the diff is decoded and its claim
  refused when false.
- `add-locations-to-ways` copying an undecoded blob on its claim: a relation
  blob is never decoded under `--keep-untagged-nodes` without
  `--inject-prepass`, so a way labelled relation goes out without locations
  under the enrichment declaration; and `--index-type external` never decodes
  a node blob whose claimed range no referenced ID bucket reaches, so a
  range moved wholly off its nodes turns them into missing locations (zeros
  under `--ignore-missing-nodes`). Both backends share the first; sparse,
  which decodes every node blob, refuses the second.
- The geocode builder's node phase (Pass 2a) never seeing a node in a blob
  labelled way or relation, so its coordinates are missing from the index.
- `cat --type` leaking or losing a mislabelled blob's elements.

Believed and unchecked, with no wrong answer worth a refusal:

- `BlobIndex::deserialize` bounds neither `count` nor the ID range by
  kind: a way index with negative IDs, or an absurd count, parses. Nothing
  sizes an allocation from it unclamped, and negative IDs are invalid input
  (ADR-0002). An index whose bytes do not parse at all counts as absent
  (above).
- `BlobFilter` with a node bbox is index-backed (`uses_index`), but the
  classify-phase filter check (`blob_filter_check`) applies only the kind
  and tag-key rules. Only extract builds a bbox filter, and it applies the
  bbox itself, so no command skips or keeps a blob wrongly because of it.
- `inspect --show` exits early on a sorted header once a blob's `min_id`
  passes the target. For a non-negative target that is sound; a negative
  target never exits early.
- `degrade --corrupt-indexdata` and `--corrupt-indexdata-kind` refuse a
  blob whose indexdata does not parse, since there is nothing to corrupt.

## Raw copies carry claims forward

A blob copied raw keeps its `BlobHeader`, claims included: the `apply-changes`
splice, `sort` and `cat --dedupe` passthrough, `cat --type`, `getid --invert`,
the `extract` raw copy (verified for region membership, not for its header),
and plain `cat`, which passes an already indexed blob through as-is and
indexes only blobs that carry no indexdata. A false claim in the input
therefore reaches the output, where the next pbfhogg run believes it again;
plain `cat` cannot repair it. `cat --reindex` can: it replaces every data
blob's indexdata and tagdata with ones scanned from its payload, copies
every data payload and every other blob-header field byte for byte and the
file's `HeaderBlock` unchanged (its frame is written afresh), and refuses a
block no single index describes. A rebuilt index can carry one too: `renumber`
rewrites every node blob but takes the new index's bbox from the input's
claim, so a false bbox survives into a fully renumbered output and a later
`extract` can skip nodes on it.

A false `Sort.Type_then_ID` claim is carried the same way. A command whose
output header takes `Order::AsInput` (`OutputFacts::carry`: sparse
`add-locations-to-ways`, `extract`, `tags-filter`, `getid`, `getparents`,
`repack`, plain `cat`) does not depend on order, so it neither checks nor
repairs it, and a single sorted-claiming input whose elements are out of
order yields an output header that repeats the claim. The next command that
depends on order is the one that checks (`apply-changes`, external
`add-locations-to-ways`, `time-filter`, `renumber`, `sort --distrust-metadata`
as the remedy). `block_type()` is not a kind test: it returns `Mixed` for a
changeset or unknown group as well as for mixed kinds, and no caller treats it
as evidence of a kind.

## Allocation from claims

Anything sized from a claim must survive the claim being wrong (ADR-0013,
consequence 2). `check --refs` and `check --ids --full` pre-allocate their
ID bitsets to the claimed `max_id` bounded by the input file's size,
floored at the live-OSM ID range (`id_alloc_bound` in
`src/commands/check/mod.rs`), and insert any decoded positive ID above the
allocation after the parallel phase. A claim therefore cannot cause an
unbounded allocation, and an ID beyond the allocation is still tracked
correctly; a wrong claim can change the run's memory and time, not its
answer. A negative ID is reported as a property of the input.
`repack` bounds its `BlockBuilder` pre-allocation the same way. `renumber`
pre-allocates its node set to the smaller of the claimed `max_id` and a
file-size bound and inserts any ID above it afterwards, so an overstated or
understated claim costs neither memory nor a refusal. The geocode builder caps
its node-set pre-allocation by the same file-size bound, but an understated
claimed node `max_id` is a refusal at Pass 2a (above). The ID-bitset callers
take the bound from `IdSet::file_bounded_max_id`.

## Verification coverage

- **`check --index`** decompresses every OSMData blob and compares kind,
  `count`, `min_id` and `max_id` exactly and the bbox by containment;
  a tag key set missing a key the blob carries and one listing a key it
  lacks are both reported, as distinct violations. It also reports unreadable sidecars, tagdata on a blob no index
  can describe, and a file indexed on some blobs and not others. It never
  routes on what it verifies. It does not check order: the intra-blob
  sortedness its scanner computes is discarded. The field-by-field contract
  is in the [CLI reference](../docs/cli/commands.md#index-verification).
- **`check --ids`** checks ID order within each blob and across blobs, per
  kind, and type order across blobs on indexed input. It tests the order
  requirement, not the header's claim: an honestly unsorted file fails it
  too. Its per-kind schedules come from the indexdata kind; a scheduled blob
  holding another kind stops it and fails the file, but a lie in a blob no
  selected phase reads (under `-t`) is not seen, which `check --index`
  covers. Combine the two selectors to run both; flag order implies no
  sequencing.

How `check --index` is built, so its verdict means what it says:

- It re-derives the sidecars with `try_scan_block_ids_checked` / `scan_block_tags`, the
  scanners the writer's passthrough path uses. On a re-framed blob it
  therefore cannot catch a bug inside those scanners, only a sidecar that has
  diverged from its payload; against `BlockBuilder`, which derives its index
  incrementally and never rescans what it emitted, it is independent. The
  scanners parse every element through the decoder's `wire.rs` parsers and
  never return a partial index.
- It reads a field's presence separately from its parsed value, because a
  reader's collapse of an unparseable field to "absent" would pass the most
  obviously broken input there is. Absence itself is not reported: a blob
  without a sidecar claims nothing, and a v1 (26-byte) index has no bbox.
- The bbox is containment, not equality: the scan rounds outward, so a
  wider stored box is sound. What a box claims to contain is the exact
  coordinate, `offset + granularity * raw` nanodegrees, computed without
  intermediate overflow; outward rounding also covers the truncating
  decimicrodegree accessors `extract` selects with. A node coordinate
  outside the `i32` decimicrodegree range has no scanned box and lies
  outside every stored one, so any stored box over it is reported. The
  merge engine behind `sort` and `cat --dedupe` keeps indexdata by the same
  rule. For the key set, a missing key is the unsound
  case (a skipped blob holding matches); a listed key the blob lacks only
  costs a decode, and is reported as a separate violation.
- `degrade --corrupt-indexdata`, `--corrupt-indexdata-kind` and
  `--corrupt-tagdata` write well-formed false sidecars through the honest
  encoders, so tests built on them exercise a consumer's belief rather than
  the reader's rejection of malformed bytes.

Gaps no verifier covers:

- Type order on unindexed input: both `check --ids` arms skip it, since an
  unindexed blob's kind is unknown from headers alone.
- The header feature declarations (out of scope above).
- Decode-time arithmetic on metadata (timestamp deltas and the like) in a
  blob copied raw.

## Missing and partial indexdata

Whether a file is indexed is not a claim the file makes; commands infer it.

- **The gate reads the first data blob only** (`has_indexdata`). An
  all-blob preflight was a measured regression across header-walking
  commands ([performance-history.md](performance-history.md), "TAINTED
  window"), so the probe stays first-blob-only. A header-only file counts as
  indexed: no blob lacks indexdata.
- **A partially indexed file lies about nothing, and every command refuses
  it** (ruled 2026-10-26). Indexdata on some data blobs and not others, in
  either order, is refused with the typed missing-indexdata error
  (`ErrorKind::MissingIndexdata` with `partial`), located at the first data
  blob without readable indexdata, and the CLI points at `pbfhogg cat`,
  which indexes every blob. `--force` and `--allow-unindexed` do not lift
  it. This is the command input contract, not a judgement of whether a path
  could cope: many paths decode a blob that lacks indexdata and would give
  the wholly indexed file's output (`time-filter`, `export`, `tags-filter
  --invert-match` and `--distrust-metadata` consult no indexdata at all),
  and they refuse all the same, so that a command never accepts the file on
  one path, option or input size and refuses it on another. The earlier
  rule, that a refusal was owed only where a path could not do without the
  missing indexdata, is retired.
- **The refusal is judged in the one frame walk every reader shares**
  (`read::frame::Frames::refuse_partial_index`). Every command-input read is
  armed by default: `Frames::of_input`, hence every `HeaderWalker` and every
  classify schedule walk, and `BlobReader::open_in`, hence every
  `ElementReader::open_in` and the pipelined reader on every iteration
  method. A frame is judged once complete, so a payload truncated on the
  frame that shows the mixture is the truncation error, and the
  observation precedes any blob filter (a blob a kind, tag or bbox filter
  skips still counts). An armed walk parses indexdata to observe it even
  where its caller does not keep it. Indexdata present but unreadable
  counts as absent. A move of the walk (`reposition`) restarts the
  observation; each input of a multi-input command is judged alone. The
  public library constructors (`BlobReader::new`, `new_named`,
  `from_path`, `open`, `ElementReader::new`, `from_path`, and the like) are
  not armed: pbfhogg as a library reads any file, and the CLI's
  `bench-read` and `bench-write` read through them on purpose.
- **Exempt, each at its call site with its reason**: `cat` in every mode,
  the remedy (plain `cat` indexes every blob that lacks indexdata, `--type`
  decodes such a blob and filters its actual elements, `--dedupe`'s first
  pass scans its IDs and tag keys, `--clean` schedules and decodes every
  blob whatever its indexdata, `--reindex` rescans every blob); `check --index`, the verifier, which
  reports the file as a violation (exit 1); and `inspect`'s descriptions
  (the default index-only scan, which falls back to a full decode at the
  first unindexed blob, `--extended` and `--show`). `inspect --nodes` and
  `inspect tags` are not descriptions of the file but analyses of its
  elements, and refuse. Probes that look at the front of a file only are
  not reads of it and are not armed: the first-blob gate, and the
  blob-count estimate `getid` and `getparents` pick their arm with, whose
  chosen arm then reads, and judges, the whole file.
- **Which refusal comes first depends on the order.** With the first data
  blob indexed, the first-blob gate passes and the partial refusal stops
  the run at the first unindexed blob. With it unindexed, a command with a
  first-blob gate refuses the file as unindexed (the CLI advises `cat` and
  `--force`); under `--force` the run goes on and the partial refusal stops
  it at the first indexed blob, located at the first data blob. `renumber`,
  `add-locations-to-ways --index-type external` and `diff`'s indexed
  block-pair paths refuse any unindexed blob with their own reason, which
  comes first when the first data blob is unindexed; behind an indexed one
  the partial refusal comes first. Each way, the run fails and the advice
  is `pbfhogg cat`. `cli/tests/cli_input_contract.rs` pins both orders.
- **`cat`'s own refusals are not about indexing** but about way
  enrichments the header already declared and a re-encode would break:
  `--type` refuses an unindexed blob that yields ways, and plain `cat` a
  way in an unindexed block no single index describes under a declared
  `WayMembers-v1`, or one carrying pins under a declared
  `SharedNodePins-v1`. `--dedupe` declares its output from what it writes,
  so it refuses neither. Its first-blob probes decide only whether
  `--type` or `--dedupe` needs `--force` and, under a declared way
  enrichment, which way declaration the output header makes.
- **What a command does with a wholly unindexed file** - refuse and offer
  `--force`, refuse with no escape, fall back, or verify - is per command.
  The user-facing statement is the "Proceeding without indexdata" section
  of the [CLI reference](../docs/cli/commands.md#proceeding-without-indexdata);
  the outcome of every command
  line on an unindexed file, and on a partially indexed one in either
  order, is pinned by the tables in `cli/tests/cli_input_contract.rs`. A
  change to any outcome moves a row there. `renumber` has no `--force` and
  `add-locations-to-ways --index-type external` refuses it at parse time:
  both are deliberate, since neither has an unindexed fallback.
- **Plain `cat`, `cat --dedupe`, `sort` and `apply-changes --force` write
  no partially indexed file**, which every other pbfhogg command would
  refuse. All index every blob they write from an unindexed input: an
  untouched blob keeps its compressed payload under a header carrying
  indexdata scanned from it (and, from `cat` in every mode and `sort`, its
  scanned tag keys), a blob no single index describes is re-encoded into
  single-kind blocks, and one holding no element is dropped. `apply-changes`
  refuses a blob whose elements span more than one kind, which it cannot
  place in type order; plain `cat` splits it in input order, and the merge
  engine behind `cat --dedupe` and `sort` merges each kind's share of it
  into type order.
- **`sort` does not copy indexdata it has found false.** Under a header
  that does not claim `Sort.Type_then_ID` it scans every blob, indexed ones
  included, to check their order. A blob whose indexdata disagrees with the
  scan by the `check --index` rule (kind, count or ID range unequal, or a
  bbox not containing the scanned one, or any bbox over a node coordinate
  outside the decimicrodegree range) is copied under indexdata and tag
  keys scanned from its payload, with a `Note:`. Under the sorted claim an
  indexed blob is not scanned, so its indexdata is believed and copied, as
  everywhere else on this page. Tag keys are the exception under the
  unsorted header too: its scan reads them only for a blob with no
  indexdata, so a blob whose indexdata holds is copied under its carried
  tagdata unverified (a per-element cost the scan does not otherwise pay).
  `--distrust-metadata` compares the key set and reframes a blob it
  contradicts.

Current behaviour recorded as such, not as a contract to preserve:

- `check --refs`, `check --ids` and `check --locations` refuse a partially
  indexed file,
  although `check` answers "is this file right?" and `check --index`
  reports the same file as a violation (exit 1). A false kind claim they
  meet at decode is not refused but fails the file (exit 1), and other
  selected checks still run. Combined with `--index`,
  the other check's refusal ends the run: `check --ids --index` stops
  before the index report, and `check --refs --index` computes it and
  then exits on the refusal without printing it.
- The walks see headers only, so they cannot tell a blob no index can
  describe (empty, or mixed kinds), which carries no indexdata legitimately,
  from an unindexed one; such a blob among indexed ones is refused.
  `check --index` exempts it. Since `cat` (and `sort`, and `cat --dedupe`)
  re-encode such blocks into indexable ones, only a hand-made file has one.
- `inspect --indexed` reports the first-blob probe, so it says yes for a
  partially indexed file whose first data blob is indexed, and no for one
  whose first data blob is not.
- Only `tags-filter`, `inspect` and `sort` have `--distrust-metadata`; every
  other command believes the claims with no way to decline them.
