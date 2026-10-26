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
and no command verifies them; their meaning is owned by the "Output header"
section of [cli-reference.md](cli-reference.md#output-header).

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
passthrough fast paths exist to avoid, so verification is opt-in. The checks
that already happen as part of work a command does anyway stay (the
"Checked in passing" column below); no consuming command adds a decode pass
only to check a claim.

A blob that carries no `indexdata` or no `tagdata` makes no claim. Readers
also treat sidecar bytes they cannot parse (unrecognised length, version or
kind byte, an inverted range, a repeated field) as absent and decode the
blob; a verifier must not, and `check --index` reports them (ADR-0013,
consequence 3).

## Claims: who believes, what checks, what a lie costs

| Claim | Believed by, and what a lie costs | Checked in passing by | Verified by |
|-------|-----------------------------------|-----------------------|-------------|
| kind | Every per-kind schedule: a blob labelled with the wrong kind is invisible to the phase that needs its elements, and the phase that does read it drops elements of the unclaimed kind silently. That covers `repack`, `degrade`'s decode path, `tags-filter`, `tags-count`, `getparents`, `inspect tags` / `--nodes` / `--show`, `check --ids` / `--refs`, the sorted `extract` schedules, `add-locations-to-ways` (node scan) and the geocode builder. `cat --type` copies a matching blob raw, so a mislabelled blob leaks off-kind elements or vanishes. `cat --clean` keeps every decoded element of a selected kind whatever its blob claims, so with every kind selected the kind claim decides nothing; with `--type` a blob labelled with an unselected kind is skipped whole. `apply-changes` with `--locations-on-ways` extracts coordinates only from blobs claimed as nodes. | `renumber` (its wire rewrite refuses an element of another kind in a blob scheduled for one kind) | `check --index` |
| `min_id` / `max_id` | `apply-changes` routes a blob whose range misses the diff to the splice fast path and copies it unchanged. `getid` skips a blob whose range holds no requested ID; `getid --invert` copies it raw. `diff` plans shards from ranges, decides from them which blob pairs overlap, and skips decoding a pair whose claimed range and count match when its compressed bytes are identical (`-c`, `--format osc`). `sort` (declared-sorted fast path) and `cat --dedupe` analyse overlap from ranges and copy non-overlapping blobs raw. `add-locations-to-ways --index-type external` partitions node blobs by range. `inspect --show` skips on range. | `diff` refuses ranges that break canonical order, and a block holding elements past its claimed `max_id` where the merge meets them (a shard boundary hides what lies past it); `add-locations-to-ways --index-type external` refuses reversed, negative-node and out-of-order ranges, including an overstated `max_id` overlapping the next blob; the geocode builder's Pass 2a refuses any decoded node above the largest claimed node `max_id` (a global bound, not each blob's range) | `check --index` |
| `count` | `apply-changes` credits a spliced or passed-through blob's `count` to its `base_*` statistics, folded into the observed counts. `sort`, `getid --invert` and `cat --dedupe` report a passed-through blob's `count` separately, as claimed (`passthrough_claimed`). `inspect` reports counts from indexdata unless `--distrust-metadata`. `renumber` assigns ID blocks from it. | `renumber` (refuses a blob whose decoded count disagrees) | `check --index` |
| bbox | `extract` and the public `BlobFilter::with_node_bbox` skip a blob claimed as nodes whose bbox misses the region, without decoding it; the bbox is consulted only when the claimed kind is node, so a mislabelled blob skipped this way loses everything in it, on the unsorted `extract --simple` path too. `extract` also nominates a node blob whose bbox lies inside the region for a raw copy. | `extract` decodes a nominated blob and copies it raw only when its payload is DenseNodes alone and every node lies in the region, counting the nodes it decoded. That is selection and structural verification, not verification of the copied header. | `check --index` (containment) |
| tagdata | `tags-filter` and `tags-count` skip a blob whose key set lacks every required key: a key set omitting a key its elements carry drops matches. `add-locations-to-ways --index-type external`, removing untagged nodes, drops a whole node blob whose key set is empty and whose claimed range holds no retained relation member, so a false empty key set deletes tagged nodes and a false range hides members. | - | `check --index` (missing and surplus keys, and tagdata on a blob no index can describe) |
| `Sort.Type_then_ID` | Every command that requires sorted input reads the flag alone. `sort` takes its header-only fast path exactly when the header claims sorted and the blob has readable indexdata, and copies non-overlapping blobs raw, so an internally unsorted blob under the claim passes through under the sorted stamp ([CORRECTNESS.md](../CORRECTNESS.md), "`sort`: intra-blob disorder"). `add-locations-to-ways --index-type external` merge-walks each node blob's decoded tuples against ID-sorted records, so a node blob whose elements are out of order (honest range, payload `[2, 1]`) turns present nodes into missing locations. `diff` trusts order inside a blob and inside a range spanning negative and positive IDs. | `renumber` and `digest` check order while reading; `sort` and `cat --dedupe` (one shared pass 1) check intra-blob order on every blob outside the header-only fast path | `check --ids`, partly (below) |

The worst of these, stated plainly:

- `apply-changes` splicing a blob whose range is understated: the blob is
  copied unchanged past a modify or delete aimed at one of its elements,
  and a create whose ID falls in the blob's real span can be written out of
  order under the sorted stamp.
- `add-locations-to-ways --index-type external` dropping present node
  locations when a node blob's elements are out of order under a sorted
  header.
- The geocode builder's node phase (Pass 2a) never seeing a node in a blob
  labelled way or relation, so its coordinates are missing from the index.
- `cat --type` leaking or losing a mislabelled blob's elements.

## Raw copies carry claims forward

A blob copied raw keeps its `BlobHeader`, claims included: the `apply-changes`
splice, `sort` and `cat --dedupe` passthrough, `cat --type`, `getid --invert`,
the `extract` raw copy (verified for region membership, not for its header),
and plain `cat`, which passes an already indexed blob through as-is and
indexes only blobs that carry no indexdata. A false claim in the input
therefore reaches the output, where the next pbfhogg run believes it again;
plain `cat` cannot repair it. A rebuilt index can carry one too: `renumber`
rewrites every node blob but takes the new index's bbox from the input's
claim, so a false bbox survives into a fully renumbered output and a later
`extract` can skip nodes on it.

## Allocation from claims

Anything sized from a claim must survive the claim being wrong (ADR-0013,
consequence 2). `check` bounds its ID bitsets by the input file's size, and
`check --ids` / `--refs` report an out-of-range ID as a property of the input.
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
  is in [cli-reference.md](cli-reference.md#--index-the-metadata-trust-boundary).
- **`check --ids`** checks ID order within each blob and across blobs, per
  kind, and type order across blobs on indexed input. It tests the order
  requirement, not the header's claim: an honestly unsorted file fails it
  too. Its per-kind schedules come from the indexdata kind, so its verdict
  leans on the kinds being right, which `check --index` establishes.
  Combine the two selectors to run both; flag order implies no sequencing.

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
- **A partially indexed file lies about nothing**: it is in-contract input.
  A schedule-building walk over every blob header that opts into
  `HeaderWalker::refusing_partial_index` refuses it with the typed
  missing-indexdata error, in either order (indexed then unindexed, or the
  reverse), located at the first unindexed data blob. Most schedule walks
  opt in; `getid`'s, `getparents`', `tags-filter`'s and `time-filter`'s do
  not (below). `renumber`,
  `add-locations-to-ways --index-type external` and `diff`'s indexed
  block-pair paths refuse any unindexed blob (a wholly unindexed `diff`
  input takes the sequential fallback instead). Commands whose arm the first
  blob alone decides, and walks that do not opt in, run on such a file.
- **Refusing a partially indexed file is owed only where a path cannot do
  without the missing indexdata.** A path that decodes a blob lacking
  indexdata and works from its actual elements produces the same output as
  on the wholly indexed file, so for it the refusal removes a capability and
  protects nothing. Each classify-schedule caller states its policy
  (`scan::classify::PartialIndex`). `getid` accepts on every path by this
  rule: the first-blob probe only decides whether the primary input needs
  `--force` (its first data blob unindexed) and picks the scan arm, and no
  path skips, copies raw or counts a blob on
  indexdata it lacks - both include arms, the `--invert` walker,
  `--add-referenced`'s classify walk and second pass, and the `-I` ID file
  reader (shared with `getparents`) all decode such a blob. The one refusal
  left is `getid --invert`'s, for an unindexed blob holding ways under a
  carried way-enrichment declaration, which re-encoding would break.
  `getparents` accepts on both arms by the same rule, in either order, and
  has no `--force` gate: its walker schedules a blob without indexdata
  whatever the query and its full-scan arm's blob filter passes one through
  to a decode, so both select parents from actual elements, and its output
  is the wholly indexed file's. `cat` accepts in every mode by the same
  rule, in either order. Its first-blob probes decide only whether `--type`
  or `--dedupe` needs `--force` (first data blob unindexed) and, under a
  declared way enrichment, which way declaration the output header makes;
  no blob is skipped, copied raw or counted on indexdata it lacks. `--type`
  decodes an unindexed blob and filters its actual elements; `--dedupe`'s
  first pass scans the IDs and tag keys of every unindexed blob (checking
  their order, as `sort`'s shared pass 1 does), so its overlap plan and way
  declaration rest on observed ranges and a blob it copies is indexed as
  plain `cat` indexes it; `--clean` schedules and decodes
  every blob once whatever its indexdata. The refusals that remain are not
  about indexing but about way enrichments the header already declared and
  a re-encode would break: `--type` refuses an unindexed blob that yields
  ways (as `getid --invert` does), and plain `cat` a way in an unindexed
  block no single index describes under a declared `WayMembers-v1`, or one
  carrying pins under a declared `SharedNodePins-v1`. `--dedupe` declares
  its output from what it writes, so it refuses neither. `tags-filter`
  accepts on
  every path by the same rule, in either order. Its first-blob probe decides
  only whether `--force` is needed (first data blob unindexed; never under
  `--invert-match` or `--distrust-metadata`). `-R`'s pipelined reader, the
  two-pass schedule walks for pass 1 and pass 2, the relation closure and the
  way-dependency scan each decode a blob lacking indexdata, unless its
  tagdata (a separate claim) excludes it, and match or write its elements
  by their actual kind and ID, so the output is the wholly indexed file's.
  `time-filter` reads no indexdata at all, so it accepts on both paths, in
  either order, and has no `--force`: the header's `HistoricalInformation`
  picks the path, the history path's pipelined reader decodes every blob,
  and the snapshot path schedules every blob with no kind filter; both
  select from decoded elements by their actual kind and ID. For the same
  reason it believes no sidecar claim, false ones included (it does believe
  `Sort.Type_then_ID` and `HistoricalInformation`).
  The other refusals on this page are recorded as current behaviour, not
  justified by this rule.
- **What a command does with a wholly unindexed file** - refuse and offer
  `--force`, refuse with no escape, fall back, or verify - is per command.
  The user-facing statement is the `--force` section of
  [cli-reference.md](cli-reference.md#--force); the outcome of every command
  line on an unindexed file, and on one indexed on its first blob only, is
  pinned by the two tables in `cli/tests/cli_input_contract.rs`. A change to
  either outcome moves a row there. `renumber` has no `--force` and
  `add-locations-to-ways --index-type external` refuses it at parse time:
  both are deliberate, since neither has an unindexed fallback.
- **Plain `cat`, `cat --dedupe`, `sort` and `apply-changes --force` write
  no partially indexed file**, which pbfhogg's own schedule walks would
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
  everywhere else on this page.

Current behaviour recorded as such, not as a contract to preserve:

- `check --refs`, `check --ids`, `inspect --nodes` and `inspect tags`
  refuse a partially indexed file through their classify walks, although
  `check` answers "is this file right?" and `check --index` reports the
  same file as a violation (exit 1). `extract --simple` refuses it too,
  although it accepts a wholly unindexed file.
- The walks see headers only, so they cannot tell a blob no index can
  describe (empty, or mixed kinds), which carries no indexdata legitimately,
  from an unindexed one; such a blob among indexed ones is refused.
  `check --index` exempts it. Since `cat` (and `sort`, and `cat --dedupe`)
  re-encode such blocks into indexable ones, only a hand-made file has one.
- The geocode builder refuses a partially indexed file at its Pass 1.5
  schedule walk, after Pass 1 has decoded the whole file once.
- `inspect --indexed` reports the first-blob probe, so it says yes for a
  partially indexed file whose first data blob is indexed, and no for one
  whose first data blob is not.
- Only `tags-filter` and `inspect` have `--distrust-metadata`; every other
  command believes the claims with no way to decline them.
