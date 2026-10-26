# apply-changes --locations-on-ways does not update untouched base ways

Anger: 10 - apply-changes -l loses its splice fast path.

Decided: fix it, as its own project. Until then the CLI help, both CLI
references and DEVIATIONS.md document the current scope (OSC-touched ways
only, rerun altw for a full refresh); those docs change when this lands.

Reaffirmed 2026-10-26, after weighing "it is documented, so leave it" and
"retire `-l`":

- The documented limit is self-defeating. `reference/pipeline.md` keeps
  `-l` in the library and CLI "for users who do not re-enrich", and the
  documented workaround is to re-enrich. On a default altw base that
  workaround does not even work (section 1). For the users `-l` is kept
  for, the output is wrong after the first node move.
- The output contradicts its own header: it declares `LocationsOnWays`, and
  a consumer cannot read DEVIATIONS.md.
- `-l` is public, advertised surface. README lists it in the CLI summary
  and the planet-scale table, sets the 1.0 goal that every CLI command is
  planet-scale safe, claims every CLI command is cross-validated against
  osmium, and names osmium the reference implementation whose use cases the
  CLI covers. Retiring it would be a breaking change, not a cleanup.
- The author's production pipeline not using `-l` is no argument either
  way: `reference/pipeline.md` marks that section as operational context,
  not part of the public API.

Before touching code, read `reference/performance-history.md` and
`reference/performance.md`. The current code is likely not an accident.

## The defect

Only ways the OSC touches get fresh coordinates.
`NodeLocationIndex::build_from_diff` collects refs of OSC ways only;
`rewrite_block_parallel` writes a surviving base way through
`write_base_way_local_with_locations`, which copies the old coordinate
bytes; way blobs with no diff overlap take the splice fast path. If the OSC
moves node N and does not touch way W, W goes out with N's old position,
and a deleted N leaves W with N's last coordinate. `loc_missing` cannot see
it.

The case is routine OSM editing, not an edge case. A way stores node IDs
only; its version changes only when its tags or node list change. Dragging
a vertex (realigning a road, squaring a building, smoothing a coastline,
fixing a junction shared by several ways) produces a node modify and leaves
every way using it out of the diff. osmium-tool's own fixture
(`test/apply-changes/output-data-low.osm`) tests exactly this. How often it
hits way blobs in a real diff is unmeasured (next steps, item 1).

Not a regression: it has been present since `-l` landed in `e7bbfa28`
(2026-03-06), whose message says surviving base ways forward raw lat/lon
bytes and claims this removes the need to rerun altw. Every later
rework inherited that choice.

Why it went unnoticed: the `-l` tests encode the assumption.
`merge_locations_on_ways_osc_node_coords` moves a node but also modifies
the only way using it; `merge_locations_on_ways_basic` asserts an untouched
way keeps its coordinates, correct there only because no node moves.
`loc_missing` looks only at OSC ways, no self-consistency check of inline
coordinates exists, the real-data `-l` runs recorded speed and counters,
not coordinates (and silently carried millions of `(0, 0)` refs), a stale
coordinate is usually metres off and looks plausible, and the author's
pipeline left `-l` on 2026-07-10. Whether `brokkr verify` exercises `-l`
against osmium is unchecked.

Interplay with the barrier (2026-10-26): the barrier hygiene work kept one
pipeline run for both phases and did not split out the node phase. The
scanner's node boundary rides the drain channel and the drain publishes the
coordinate map from one readiness check. This project's need, coordinates
from the merged node population for every way that must be refreshed, can
keep that barrier; nothing there commits it to separate runs.

## Research findings (2026-10-26)

### 1. A default altw output is not a reliable base for -l

Whether a run on a default altw base fails depends on the diff: one OSC way
referencing one dropped, unchanged node fails it; a diff with no surviving
OSC ways, or whose way refs all resolve, passes (and a node-only diff
passes while leaving untouched ways stale, which is this defect). Daily
diffs in practice modify many ways, so in practice it fails.

The documented workaround does not work on such a base either. DEVIATIONS.md
and the CLI help say "rerun add-locations-to-ways for a full refresh", but
altw resolves way coordinates only from node records (the way arm of its
block processing in `src/commands/altw/mod.rs` looks each ref up in the node
index and writes `(0, 0)` and counts it missing otherwise). It never reads
a way's existing inline coordinates, so rerunning it on an output whose
untagged nodes were dropped cannot reconstruct them. That advice needs a
completeness qualification.

`loc_missing` counts distinct node IDs referenced by OSC ways, not missing
or stale locations in the output. Zero does not establish coordinate
correctness: a base made with `--keep-untagged-nodes` is still exposed to
the untouched-way defect.

Which nodes altw keeps: every index type (sparse, external, and auto,
which only picks one of them) keeps a node when `--keep-untagged-nodes` is
set, when it has any tag, or when any relation has it as a node member. The
backends share one relation scan (`relation_member_ids` in
`src/commands/altw/mod.rs`), so they agree. External additionally drops
whole node blobs whose tagdata says untagged and whose ID range holds no
relation member; a blob without tagdata counts as tagged. Two exceptions
to "relation members are kept": member IDs below zero are skipped, so
untagged negative-ID member nodes are dropped (DEVIATIONS.md says members
are kept unconditionally), and the rustdoc on `add_locations_to_ways` omits
the relation-member exception.

The europe altw benchmark file (`europe-20260301-seq4714-altw`) holds 149M
nodes against 453M ways; the input had 3.7B nodes. An OSC that modifies a
way referencing an untagged node it does not touch needs a node that is not
in the base: its coordinate exists only inline in the old base ways.

The resolution path, confirmed end to end with no fallback to inline
coordinates: `build_from_diff` counts only OSC-way refs as needed, takes
coordinates from OSC nodes, and leaves refs to OSC-deleted nodes out of the
base lookup while still counting them; every node blob goes to a worker,
which keeps base node records in the needed set; the drain's
`barrier_publish_loc_map` merges both; `rewrite.rs` computes
`loc_missing = total_referenced - merged map size` and fails unless
`ignore_missing_nodes`; `add_osc_way` writes `(0, 0)` for a ref not in the
map. An OSC-modified way replaces its base version outright, and the old
version's inline coordinates, decoded in the same block, are never
consulted.

The recorded runs. Europe altw seq 4714 + OSC 4715 (consecutive
sequences), commit `16e3694`, plantasjen, UUID `42b24498`: `loc_needed`
3,576,070, `loc_from_diff` 731,618, `loc_from_base` 37,585 (731,618 +
37,585 = `barrier_loc_map_size` 769,203 exactly), `loc_missing` 2,806,867.
Base lookups found 1.3% of needed nodes. (Comparing that with the about 4%
of nodes altw kept proves little: nodes referenced by changed ways are not
a random sample.) Planet altw (UUID `2161ea62`, same commit): needed
6,803,426, from diff 2,468,812, from base 97,392, missing 4,237,222, map
2,566,204, also on a consecutive pair: the altw base is seq 4912 and
`planet-20260329-seq4913.osc.gz` holds 2026-02-23 edits (the filename date
is the download date, not the data date).

At `16e3694` there was no missing-node guard (`loc_missing` was a stat
only), OSC-deleted nodes were still looked up in the base, and node
coordinate extraction errors were silently swallowed
(`extract_node_tuples(...).is_ok()`). Dropped untagged nodes are almost
certainly the dominant cause; swallowed extraction errors, refs to
OSC-deleted untagged nodes and refs outside the extract boundary cannot be
ruled out from counters alone. Cheap settlement: count how many missing
refs exist as nodes in the europe indexed file but not in the altw file,
separating "absent from altw" from "present but extraction failed", after
checking that the indexed and altw files represent the same source state.
No results row records how the altw files were made; 456,943 node blobs,
about one per input blob, suggests the sparse backend, but that is a
hypothesis, not provenance.

If the missing IDs are truly absent, both runs would fail on the current
code unless `--ignore-missing-nodes` is passed, which writes `(0, 0)` for
every such ref. Extraction and validation have changed since, so this is
not demonstrated by the historical counters alone; the europe and planet
`-l` cells are at least not established as reproducible successful runs
under current defaults. Docs quoting `-l` numbers (README "Planet scale",
`reference/performance.md` "Descriptor-first streaming pipeline",
`docs/guide/performance.md`) rest on an implementation that accepted
millions of unresolved IDs.

Smells on this path:

- The guard fires after the whole pipeline has written its output, though
  the count is known at the node-to-way barrier. Output is staged and
  discarded, but a planet run burns its full wall before refusing.
- The error says "{count} way node references could not be resolved". In
  apply-changes the count is unique node IDs; in altw it is per reference.
- Its hint ("supply a complete input") would not lead a user to
  `--keep-untagged-nodes`.
- Under `--ignore-missing-nodes` on a default base, OSC nodes are still
  written, so the output's node set is neither tagged-only nor complete
  (osmium does the same).

Documentation and header: osmium's altw man page gives
`--keep-untagged-nodes` as the condition under which its output can be
updated by `apply-changes --locations-on-ways` (a sufficient condition, not
a formal "only if"); pbfhogg documents it nowhere (CLI help,
`docs/cli/commands.md`, `reference/cli-reference.md`,
`docs/guide/usage.md`, DEVIATIONS.md, CHANGELOG.md, README.md).
`docs/guide/usage.md` "Applying a daily diff" pitches `-l` as avoiding an
altw rerun and omits both the LocationsOnWays preparation and the
node-retention requirement. The altw output header (`Ways::Located` in
`src/commands/output_header.rs`) records no node-population fact. It does
carry the input's replication position, but that follows the documented
contract: `Replication` in `output_header.rs` defines those fields as
provenance, subsets included, not a promise that later diffs apply. A
completeness fact would be a new contract, and `--keep-untagged-nodes`
alone could not certify it, since its input may already lack nodes.

What a refusal or warning could key on:

1. A new header fact, declared only under `--keep-untagged-nodes`. A
   format contract; files written today lack it, so its absence could only
   warn.
2. The barrier-time missing count. Exact and format-free, but cannot tell
   a default altw base from a genuinely incomplete one (an extract
   boundary). Could fail early with a message naming
   `--keep-untagged-nodes`.
3. Falling back to a modified way's own old inline coordinates for the refs
   it kept. Covers many modifies on a default base; refs a way adds to
   untouched untagged nodes would still be missing.

Test coverage: unresolved refs, the explicit fallback, duplicate base
records and deleted-node refs are covered. What is missing is the composed
workflow: default altw output followed by `apply-changes -l`.
`bootstrap_low_base` in `cli/tests/cli_apply_changes_invariants.rs` always
passes `--keep-untagged-nodes --force`; `cli_apply_changes.rs` builds its
bases with its own fixture writer.

### 2. What osmium actually does

The claim in DEVIATIONS.md that osmium "recomputes way-node locations from
the merged node set" is not accurate. Read from
`research/osmium-tool/src/command_apply_changes.cpp` and libosmium
(`index/detail/vector_map.hpp`, `io/detail/xml_input_format.hpp`,
`io/detail/pbf_output_format.hpp`); osmium was not run. Vendored versions:
osmium-tool `4b613d0` (1.19.1), libosmium `6d473abd` (2.23.1). The
inaccurate DEVIATIONS.md text has known origins rather than verified
parity: `2aa46992` introduced "merged node set", `767a6ac6` the `(0, 0)`
claim.

The index:

- A `SparseMemArray`: a vector of `(id, Location)` pairs. `set` appends, so
  duplicates are never removed. `get_noexcept` is a `lower_bound` on the
  id.
- Filled from the raw change buffers, not the deduplicated object list:
  every version of every node in every change file, deletes included,
  whatever location each carries.
- During the base node pass, a base node referenced by any OSC way version
  is appended when `get_noexcept` returns an undefined location.
- Sorted with `std::sort` on the pair only at the base's node-to-way
  transition. Among duplicates of one id, a lookup then returns the
  smallest `Location`, which compares longitude first.

Writing: `update_nodes_if_way` runs on every way written, untouched base
ways included. A ref is overwritten only when the looked-up location
`is_defined()`; otherwise the inline value stays. Upstream's own test
(`test/apply-changes/output-data-low.osm`) shows an untouched way picking
up a moved node's new position. Moved nodes are the real parity gap.

Deleted nodes: the XML parser marks a node in `<delete>` invisible and sets
its location when `lat`/`lon` were parsed.

- Without them the index entry is undefined. If no OSC way references the
  node, the old inline coordinate stays, matching pbfhogg today.
- But if any OSC way version references it, the base node pass sees the
  undefined entry and appends the base node's coordinate; after the sort a
  defined coordinate sorts before the undefined sentinel and wins. Untouched
  ways, and OSC ways, then get the base node's coordinate although the node
  is deleted from the output. This needs no unsorted-index defect; one
  delete entry suffices. "Keeps the old coordinate" is therefore not a
  general deletion rule.
- With `lat`/`lon`, untouched ways get the delete's coordinate, subject to
  the duplicate-entry rules below.

Coordinate-bearing deletes were seen in `planet-20260411-seq4914.osc.gz`
(node 883548) and Geofabrik's Malta 4706 (node 5030415117). That supports
"these files carry them", not a universal replication contract, and the
coordinates have not been checked against a base's last position.

Unresolved OSC-way refs: such a ref keeps whatever the `<nd>` carried,
normally an undefined location. The PBF writer delta-encodes the raw x/y,
so it goes out as `(2147483647, 2147483647)`, which libosmium reads back
as undefined. DEVIATIONS.md's "osmium-compatible `(0, 0)` fallback" is
wrong; osmium's altw with `--ignore-missing-nodes` writes the same
sentinel. What this strengthens is the case against pbfhogg's particular
fallback, which conflates a missing coordinate with a valid point; it does
not by itself show that refusing output beats preserving explicit absence.

osmium defects that bear on any cross-validation test:

1. Lookups on an unsorted index. During the base node pass `lower_bound`
   runs on a vector that has not been sorted, which breaks its
   precondition. An OSC whose node IDs arrive sorted is not safe either:
   appending a base node breaks the order before the next lookup (change
   IDs `[100]`, then base nodes 1 and 100, gives `[100, 1]` before 100 is
   looked up). One possible outcome is a missed OSC entry and a duplicate
   base coordinate, the westmost winning after the sort, so a moved node
   referenced by an OSC way can keep its old coordinate for that way and
   for untouched ways alike. With the precondition broken this is a
   possible failure mode, not a complete prediction.
2. Several versions of one node (several change files, or several versions
   in one diff): after a successful sort the lookup returns the westmost
   location, not the highest version's, so coordinate selection and
   emitted node version can disagree. A move then delete can give the
   intermediate position (with a coordinate-free delete, the defined
   earlier location sorts first; with a coordinate-bearing one, the smaller
   wins), but that is one outcome among several.
3. No node-to-way transition (an empty base, or nodes followed directly by
   relations): `sort` is never called. The index is then not guaranteed
   sorted; lookup failure is input-dependent.
4. Raw history, not final state: the index is built from every raw way and
   node version, so a way version that references a node and is later
   superseded can still trigger base supplementation for that node, even
   though that way version is never emitted. Cross-validation has to
   account for raw records in the change window, not just the final diff
   state.

Smaller points:

- A delete with only `lat` counts as defined (`is_defined` is
  `x != undefined || y != undefined`).
- osmium builds a fresh header: `LocationsOnWays` plus generator, no
  sort flag, bbox or automatically preserved replication fields (users can
  set them through `--output-header`). It never checks the input for
  `LocationsOnWays`.
- The index keys on `positive_id`, so nodes -5 and 5 collide.
- `-l` is refused with `-H`, `--redact` or history files.
- A node absent from both base and OSC keeps its inline value on untouched
  ways and gets the `<nd>` value (normally the sentinel) on OSC ways.

### 3. Cost structure of the current pipeline

Node phase:

- Under `-l` every blob whose indexdata claims Node is a Candidate on both
  routes (`low_node_must_decompress` keeps it off the splice and off the
  owned Passthrough), so it is decompressed for coordinate extraction.
  The kind is a claim: a mislabelled node blob bypasses extraction.
- It is also fully parsed and run through `block_overlaps_diff`, including
  the large majority whose range the scanner already proved disjoint
  (lateral finding A). An earlier estimate of about 421K of 457K europe
  node blobs being range-disjoint came from legacy prefill counters, which
  measure overlap with needed way-ref node IDs, not with changed node IDs,
  so that figure is unsupported.
- The affected node set is the OSC nodes with a coordinate (creates and
  modifies). Narrowing to nodes that actually moved needs no base node
  record: each base way carries its refs' old coordinates inline, so the
  comparison happens per way (section 4), and dropped untagged nodes
  compare like any other. That moves the decision into the way scan; it
  does not by itself show the scan is the cheapest detection strategy. How
  much base-record narrowing would save (for example by filtering tag-only
  edits of retained nodes) is unquantified. The scan must consult all OSC
  node upserts, including nodes no OSC way references, not the existing
  seeded map.

Sidecars: none answers "does this way blob reference node N". BlobHeader
fields are type, indexdata, datasize, tagdata and waymembers
(`src/read/blob_wire.rs`). Indexdata is kind, ID range and count; tagdata
the tag-key set; `WayMembers-v1` a per-way relation-membership bitmap;
`SharedNodePins-v1` (Way field 20, ADR-0007) a positional bitmap of shared
ref positions with no IDs, dropped by rewritten ways and never declared on
apply-changes output. The near-miss is the indexdata v2/v3 bbox slot,
written only for node blobs (way blobs write `BBOX_ABSENT`). A way-blob bbox
over inline coordinates could soundly rule out N when N's old position lies
outside it, but ID-ordered way blobs are spatially scattered, so the boxes
would be wide, and it is a format-semantics change.

How a way blob flows under `-l` today:

- Scanner: range-disjoint indexed way blob becomes `CopyRange` (splice) or
  `Passthrough` (owned), held in `pending_post_barrier` until the barrier;
  range-overlapping becomes a Candidate.
- Worker: pread, decompress, parse, precise check. No overlap and no
  in-range upsert is a false positive (`take_copy_range`, or
  `handle_owned_passthrough`, which re-preads the whole frame). Otherwise
  `rewrite_block_parallel` with `LocMode::Required` copies surviving base
  ways' raw lat/lon bytes, writes OSC ways through `add_osc_way`, frames
  with `frame_blob_pipelined`, and sends `Rewritten`.
- Drain: order guard, gap and trailing creates (OSC ways only, through
  `state.loc_map`), type-transition flush; only `Rewritten` advances the
  upsert cursor.

What a refresh route changes, by stage:

- Scanner: `pending_post_barrier` stores already-built
  `ScannerEmit::Drain(CopyRange)` values and the scanner never sees the
  published map. With the inline rule the probe set is known at setup, so
  routing can stay at scan time: every way blob a Candidate under `-l`
  with a non-empty probe set.
- Worker: a way branch with a wire-level scan after decompress; no hit, no
  overlap and no upserts sends `CopyRange` on splice, and on owned should
  ship the frame from one pread rather than reading it twice. The precise
  check is skippable when the range already proved no overlap.
- Rewrite: the base-way branch needs a third arm: decode the lat/lon
  deltas, substitute, re-delta-encode, keep keys, vals, refs and info raw.
  No `BlockBuilder` method takes that mix today.
- Drain: gap and trailing creates are unaffected; a refresh-only
  `Rewritten` has empty `inline_upserts`, so its cursor advance is a
  correct no-op.
- Stats: a refresh-only rewrite would land in `blobs_forced_rewrite`, whose
  doc says "pure creates". It needs its own counters: blobs, ways and refs
  refreshed, refs to deleted nodes. `loc_missing` keeps its OSC-way
  meaning.
- `LocMode`: `Required(&LocMap)` becomes `Required { map, affected }`, and
  the base-way branch chooses raw copy or refresh. `Pending` stays an
  error.
- Header: the `build_header_bytes` comment "every way keeps or gets inline
  coordinates" becomes true in substance.

Reading the recorded runs. Counter meanings at `16e3694`:

| Counter | Meaning at `16e3694` |
|---|---|
| `merge_loc_node_blobs_scanned` | Equal to `blobs_processed`: every worker-handled blob. On the owned route false positives count twice (still true). Node-only since `e7f5ee9f` (2026-10-22). |
| `merge_loc_needed` | Seeded plus `needed_set`. Referenced deleted nodes are included: with no `get_node` result they went into `needed_set` (verified at `16e3694`). Today `total_referenced`, same population. |
| `merge_loc_from_base` | `coord_pairs_extracted`: tuple hits, not unique keys. |
| `merge_loc_missing` | `needed_set` minus extracted; includes referenced deleted nodes not found in the base. A deleted node still present in the base was wrongly satisfied with its pre-deletion coordinate; today that is refused. |
| `merge_bytes_rewritten` | Compressed framed bytes; not comparable with legacy runs, which reported uncompressed. |
| `blobs_false_positive` | Included blobs whose only in-range upserts were pure creates; forced rewrites since `388eb4b3` (2026-08-20). Those outputs probably emitted creates after larger IDs under a sorted header. |

Node-blob totals from legacy prefill counters: europe 95,071 + 361,872 =
456,943; planet altw 42,119 + 9,315 = 51,434.

| | Europe `42b24498` | Planet `2161ea62` |
|---|---|---|
| Blobs | 514,664 | 197,585 |
| Way + relation blobs | 57,721 | 146,151 |
| Spliced by scanner (all way/relation) | 29,868 | 84,566 |
| Way/relation to workers | 27,853 | 61,585 |
| Rewritten / false positive, all kinds | 40,958 / 443,838 | 77,453 / 35,566 |

Byte arithmetic (estimate) says nearly all way/relation candidates were
rewritten. Planet altw under `-l` rewrote 39% of blobs and 44% of input
bytes, so splice matters at planet for `-l`. The europe altw per-kind sizes
from `inspect` are GiB: 3.5 node, 31.6 way, 0.49 relation, file 35.53 GiB.

Where worker time went (summed counters). These are `Instant`-based
elapsed times inside instrumented operations, descheduling included, not
thread CPU; they omit the initial pread, coordinate-slot sync and sends.
The "busy" figures are instrumented elapsed time over workers times
pipeline wall, and mix node and way phases, so they do not establish CPU
utilization or headroom.

| | Europe, 22 workers, 51.4 s pipeline | Planet, 16 workers, 100.4 s pipeline |
|---|---|---|
| decompress | 77.8 s | 157.4 s |
| precise | 140.5 s | 64.5 s |
| rewrite | 86.4 s | 242.9 s |
| frame (incl. compress) | 82.5 s | 187.9 s |
| parse | 10.3 s | 18.2 s |
| coord extract | 4.8 s | 10.6 s |
| Total | 402 s (36% busy) | 682 s (42% busy) |

Scheduling cost the estimates below ignore: once a way or relation
descriptor enters `pending_post_barrier`, the scanner stops polling the
readiness channel and releases the buffer only at EOF, so publishing the
map does not promptly start way work. Sidecar markers: europe
`42b24498` map published at 8.496 s, scanner at its EOF barrier wait at
12.507 s; planet `2161ea62` 9.639 s and 20.166 s. That is about 4.0 s and
10.5 s with the map ready and way work held back, plus descriptor memory
no channel bound covers (the scanner comment already flags the unbounded
buffer).

Cost estimates, hypotheses only. The counters aggregate node, way and
relation work and the passthrough counts mix ways and relations, so they
give no way-only marginal cost; applying aggregate averages to extra way
blobs assumes comparable sizes, element and ref counts and rewrite work.
Both cells ran `zstd:1`, so they say nothing about a `zlib:6` refresh.
"Net wall needs a run" governs all of these:

- Decompress throughput about 270 MB/s of compressed input per core
  (europe: about 21.0 GB in 77.8 s).
- Decompress every way blob: europe +17.2 GB, about +64 s CPU (+16%,
  +2.9 s per worker); planet +about 50 GB, about +186 s (+27%, +11.6 s per
  worker). Ref scan roughly +7 to 18 s CPU at europe (ref count assumed).
  Workers have headroom in both runs, so this may hide behind writer and
  disk.
- Re-encode every way blob through `rewrite_block_parallel`: about 6-7 ms
  per blob. Europe about +250 s CPU (x1.62, 29.6 s busy per worker of 51);
  planet about +790 s (x2.16, about 92 s busy of 100), where workers
  become the bottleneck. Plus 17 GB (europe) or 50 GB (planet) moving from
  `copy_file_range` to userspace writes; net wall needs a run.

### 4. Design space (undecided)

#### Detection rule: compare inline coordinates

For a surviving, previously consistent base way with complete inline
coordinates, it needs a refresh exactly when some ref N is an OSC node with
a coordinate and the way's inline coordinate for N differs from it. This is
incremental maintenance, not validation: it cannot find stale coordinates
for nodes this OSC does not carry, and it needs an explicit policy for a
way with no coordinate arrays. That limitation belongs in the rule's
stated contract.

- The probe set is the OSC node map (`diff.get_node`), known at setup,
  before the pipeline starts. Detection does not depend on the node-to-way
  barrier; the barrier stays only for refs of OSC ways (`needed_set`), as
  today.
- The probe must stay separate from the published location map. That map
  (`build_from_diff`) seeds only nodes OSC ways reference, so it lacks a
  moved node referenced only by untouched ways, which is the defect's case.
  Broadening it instead is unsafe: `rewrite.rs` computes `loc_missing` as
  unique OSC-way refs minus the map's key count, so unrelated moved-node
  entries would hide unresolved refs.
- Nodes absent from the base need nothing special: their old position is
  the inline coordinate.
- Tag-only node modifies compare equal, so blobs touched only by those can
  still splice.
- Versus osmium (overwrite wherever its index has a location): this rule
  leaves delete coordinates out, while osmium indexes them and can
  supplement deleted nodes from the base through raw intermediate way
  versions (section 2, deleted nodes and defect 4). So it differs on more
  than an inconsistent base, besides osmium's defects 1 to 3.
- It does not heal files already written by the buggy `-l`: their stale
  coordinates stay until the node moves again, or altw reruns. Worth a
  CHANGELOG line.
- Hazards: coordinate framing (altw's `reframe.rs` refuses non-default
  granularity and offset; a comparator must scale or refuse the same way,
  and see lateral finding H: the rewrite path already gets this wrong);
  one map lookup per ref, roughly 4B refs at europe (453M ways at about 9
  refs), estimated 2-4 s wall at 22 workers, where a small Bloom filter in
  front of the map may be cheaper. `merge_streaming_coord_extract_ns` is a
  weak anchor: node extraction validates coordinates before filtering,
  while detection decodes refs and coordinate deltas and probes.
- Validation: `PrimitiveBlock` parsing already rejects unequal
  refs/lat/lon counts when coordinate arrays are present
  (`validate_way_geometry_arrays` in `src/read/block.rs`; absent arrays are
  accepted). A wire scanner that skips the parse must keep that rejection
  or justify changing it, decode fallibly and check its arithmetic: zipping
  iterators can stop early silently, and the current coordinate iterator
  uses wrapping arithmetic.

#### Full way pass (the detection vehicle)

- Scanner: under `-l` with a non-empty probe set, route way blobs as
  `Candidate` instead of `CopyRange`. Routing stays at scan time because
  the probe set is known at setup. An empty probe set costs nothing, but
  that means no OSC node creates or modifies at all, not "no node moved":
  tag-only modifies still produce probes.
- Workers: in `handle_candidate`, a wire-level scan before
  `PrimitiveBlock::from_vec_with_scratch` (today the parse runs
  unconditionally before `block_overlaps_diff`). Skipping the parse needs
  no coordinate match and no ordinary merge work: a blob can still hold an
  OSC-modified or deleted way or an in-range create, and the worker checks
  both overlap and `inline_upserts` for exactly that reason.
  `crate::scan::way::scan_way_refs` and `WireWay` already read refs
  without building a block; lat/lon would need adding.
- Drain and barrier: unchanged; the drain already coalesces `CopyRange`
  items workers send back. Relation blobs keep the splice.
- The same bug lives inside already-rewritten blobs:
  `write_base_way_local_with_locations` copies stale bytes there too.
  Those blobs are decoded already, so refreshing them is close to free,
  and any fix must cover both paths.
- Reading twice: on ext4 `copy_file_range` is an in-kernel read and write
  (about 800 MB/s at planet per performance-history), so a blob scanned and
  then spliced is read twice unless the page cache serves the second read.
  The alternative is one full-frame read and `OwnedBytes`. Whether
  detection hides under the planet write floor needs measuring.
- Hit rate: a hypothesis, not evidence. 0.5-1M moved nodes a day over
  50-140K way blobs, read as a Poisson spread, would leave only a few
  percent of blobs without a hit, so the way-blob splice would mostly
  disappear and recompression would dominate (plausibly doubling `-l` wall
  at zlib:6 at planet). But dividing changed nodes by blobs does not
  establish that distribution: what matters is how many changed nodes this
  base's ways reference, how many blob memberships each has, how they
  cluster, and how many coordinates actually differ. Measurement 1 decides
  it.

#### Patching: wire-level, not re-encode

Re-encoding through `rewrite_block_parallel` and `BlockBuilder` for a
coordinate-only change is wasteful. Patching Way fields 9/10 (lat/lon) in
place preserves IDs, refs, tags and metadata; recompression is still
needed. altw's `reframe.rs` is a model, not a byte-preservation contract:
it also strips field 20, can normalize the refs encoding, reconstructs
enclosing messages and does not keep every unknown block-level field. A
refresh patcher should define its own preservation requirements.

Indexdata and tagdata stay valid for an otherwise unchanged way blob.
Shared-node pins need care if a refresh changes whether a previously
missing location resolves, and apply-changes output deliberately stops
declaring the private prepass extensions, so keeping their bytes does not
restore their authority (ADR-0007). A compressed blob cannot be patched in
part; overlay or patch blobs would be a format change with duplicate IDs,
ruled out.

#### Sidecars and reverse indexes: defer

Deferred for complexity and unmeasured benefit, not disproven:

- Per-blob Bloom filters, queried with every changed node against every
  blob: with about 1M probes, expected false positives per blob are 1M
  times the rate, so getting below 0.1 needs a rate near 1e-7 (about 34
  bits per element), more than an exact set. That rules out this query
  strategy, not every approximate summary.
- Exact per-blob sets: about 60K distinct refs per blob at about 2 bytes
  (both unmeasured), roughly 120 KB per blob and 7 GB at europe. That
  exceeds the 64 KiB BlobHeader limit (`src/read/blob_wire.rs`; libosmium's
  `pbf.hpp` has the same), so it cannot sit in the header beside
  `WayMembers`; exact sets stored elsewhere are not ruled out.
- Coarse summaries do not filter: ref-ID min/max (ways reference nodes of
  every age), node-ID bucket bitmaps (daily edits touch every bucket),
  spatial cells (sound only when queried by old position), way-blob bboxes
  (a new indexdata version, and near world-sized since way blobs are in ID
  order).
- A global node-to-way-blob index: about 36 GB at planet (about 9B entries
  at 4 bytes, assuming one association per node and no encoding overhead,
  neither established). Blob ordinals shift whenever apply-changes splits a
  rewritten blob; ordinal remapping or stable identifiers could avoid a
  per-run rebuild, at their own cost.
- Any new private field needs an ADR in the ADR-0007 mould, including
  "authoritative only where the header declares it".

#### Default-on, not opt-in

An opt-in fix leaves the default output declaring LocationsOnWays over
stale coordinates: silent bad data. If cost is the concern, the honest
fallback is an opt-out flag, or refusing `-l` with refresh disabled.

#### Deletes

For a consistent, correctly matched replication chain deletes are moot:
the OSM API refuses to delete a node a way still uses, so a way that kept a
deleted node is modified in the same diff. Nothing enforces that
assumption, though: the command accepts arbitrary extracts, mismatched
bases and synthetic OSC. pbfhogg's OSC parser drops `lat`/`lon` on deletes
(`delete_node` takes only id and metadata).

Suggestion: put deleted IDs in the probe as count-only, so an untouched way
referencing a deleted node raises a counter or warning flagging a base/OSC
mismatch and never triggers a rewrite. A defensible policy, but not osmium
parity (osmium overwrites with delete coordinates and base supplements,
section 2). The plan should state which goal each behaviour serves:
maintain a consistent snapshot, diagnose inconsistent input, or match
osmium's observable behaviour.

#### Additions

- Same-way fallback for `loc_missing`: when an OSC-modified way keeps a ref
  from its old version and the node is not in the OSC, the old version's
  inline coordinate is correct if the base itself is (a base already
  damaged by the buggy `-l` may hold a stale one, which the fallback would
  carry forward). It is not free: the barrier publishes an immutable map
  before way workers run, and the old version's coordinates only appear
  later, while its blob is processed, so they cannot join that map under
  the current protocol. A local per-way fallback needs its own resolution
  logic and revised accounting: global `loc_missing` would still count
  those IDs, and one ID can be resolved for a modified way through its old
  version yet stay unresolved for a newly created way. How much of the
  2.8M `loc_missing` in `42b24498` it would resolve is unmeasured
  (measurement 5), and lowering `loc_missing` that way does not prove the
  output correct. Refs newly added to dropped untagged nodes stay
  unresolved (osmium writes its undefined sentinel there). Better treated
  as separate resolution work than part of the refresh fix.
- `check --locations`: verify each way's inline coordinates against node
  coordinates where the node is present, and count the rest. An oracle
  independent of osmium that would detect this bug class and stale files
  already in the wild. Belongs on `check` per AGENTS.md.

#### Tests

Self-built inputs. Native correctness tests run without osmium; only the
cross-validation tests need it and skip without it. A LocationsOnWays base
with at least two way blobs, and an untouched way W, in a blob outside the
OSC's ID ranges, referencing node N:

- Move N: W is refreshed.
- Change only N's tags: W's blob still splices. Assert on that blob's
  frame, not global `blobs_passthrough`, which other blobs also feed.
- Base built without N (as a default altw output is), OSC moves N: W is
  refreshed, against explicit expected coordinates in the fixture
  (`check --locations` cannot validate omitted nodes).
- Delete N with W untouched: the old coordinate stays and the mismatch
  counter fires.
- A surviving way inside an already-rewritten blob; way edits and node
  moves in one diff; in-range creates; both output routes; non-default
  coordinate framing; malformed coordinate arrays; several node versions;
  repeated refs (delta reconstruction).
- Run `check --locations`, or an in-test equivalent, on every output where
  node records exist.

#### Measurements that decide it

1. Hit fraction per way blob, by the inline rule and by ID-only membership
   side by side (the gap shows how much tag-only modifies matter). Europe
   4714 + 4715, Denmark, planet. A read-only program.
2. Detection-only A/B: scan everything but always splice, `CopyRange`
   versus `OwnedBytes`, interleaved `--bench 1` cells.
3. Patching cost: wire patch versus `rewrite_block_parallel`, at zlib:6
   and zstd:1, reading `rewrite_ns`, `frame_ns` and the phase split.
4. End to end versus the plain-merge-then-altw pipeline.
5. How much of `loc_missing` the same-way fallback resolves.

Under the missing-node guard the europe and planet `-l` cells first need a
base from `--keep-untagged-nodes`, `--ignore-missing-nodes`, or the
same-way fallback.

Doc drift spotted: `reference/performance.md` "Pipeline end-to-end" still
names `apply-changes --locations-on-ways` as the steady state, contradicting
`reference/pipeline.md`.

### 5. Measured: stale way blobs on a real daily diff (2026-10-26)

`examples/stale_way_blobs.rs` (read-only; run through `brokkr run`) on
europe altw seq 4714 + OSC 4715, a matched pair, at commit `8f8ef92a` plus
uncommitted work, host bygg. Labels per way blob: T = rewritten by today's
merge (predicted by emulating the `streaming.rs` / `DiffRanges` rules, not
checked against a real merge), S = an untouched way holds an inline
coordinate that differs from the node's OSC coordinate, M = an untouched
way references any live OSC node.

| Set | Blobs | Blob % | Framed GiB | Bytes % |
|---|---:|---:|---:|---:|
| All way blobs | 56,692 | 100 | 31.58 | 100 |
| T, rewritten today | 26,926 | 47.50 | 15.553 | 49.24 |
| S, stale untouched way | 10,477 | 18.48 | 6.190 | 19.60 |
| M, ID-only | 12,910 | 22.77 | 7.614 | 24.11 |
| S minus T, extra rewrites for an inline-rule fix | 2,951 | 5.21 | 1.630 | 5.16 |
| M minus T, extra rewrites for an ID-only fix | 3,945 | 6.96 | 2.178 | 6.90 |
| S and T, stale survivors in already-rewritten blobs | 7,526 | 13.28 | 4.560 | 14.44 |

Way-blob splice kept, by framed bytes: today 50.76%, inline-rule fix
45.60%, ID-only fix 43.86%.

What it settles:

- The fix keeps almost all of the splice. The Poisson prediction in
  section 4 (only a few percent of way blobs left clean, recompression
  dominating) is wrong: an inline-rule fix rewrites 5.2% more way blobs.
  The cost moves to detection: finding those 2,951 blobs means decoding
  the roughly 29.8K way blobs (16.0 GiB framed) spliced today.
- The defect is real and daily: per this one europe diff, 23,221 untouched
  ways and 74,194 refs go out stale. 56,928 OSC nodes differ on some
  untouched way; 44,540 of those are referenced by no OSC way at all, the
  defect's pure case.
- Stale survivors inside already-rewritten blobs (S and T, 7,526 blobs)
  outnumber the extra rewrites 2.5 to 1, so covering the rewrite path is
  most of the fix, and it is nearly free there.
- A pre-filter is worth it: 342,862 probe hits in 4,690,095,140 refs
  (0.0073%).
- The inline rule versus ID-only saves about 1,000 blobs (1.7% of way
  bytes); tag-only node edits are a small effect (13,306 probe nodes equal
  on every way referencing them).
- Stale blobs are spread out, not clustered (runs ratio 0.94 for S, 1.00
  for S minus T); stale ways are a small share of each blob (all S blobs
  under 10%, 61% hold exactly one stale way), which favours wire-level
  patching over re-encoding.
- Consistency held: no untouched way references an OSC-deleted node (the
  OSM API rule from section 4), no OSC node is equal on some base ways and
  different on others, no ways without coordinate arrays, no coordinates
  finer than a decimicrodegree (no evidence of non-default framing, though
  that does not rule it out).
- T predicts 26,926 rewritten way blobs; the `16e3694` sidecar arithmetic
  put way and relation candidates at 27,853. The program now checks
  indexdata against blob contents (disagreements go to U) and has a
  `--verify-output` mode that hashes output blob bodies to find which input
  blobs were really rewritten. On the Denmark pair it matched exactly: 302
  way blobs predicted, 302 rewritten, 0 disagreeing (plus 258 non-way
  blobs, totalling apply-changes' 560). Europe and planet T stay predicted.

Denmark (an altw base built from seq 4704 plus OSC 4705): S minus T 2.05%
of way blobs, splice from 60.2% to 58.1% of way bytes.

Planet altw seq 4912 + OSC 4913 (consecutive), same commit and host,
145,699 way blobs (80.26 GiB framed):

| Set | Blobs | Blob % | Framed GiB | Bytes % |
|---|---:|---:|---:|---:|
| T, rewritten today | 61,097 | 41.93 | 35.430 | 44.14 |
| S, stale untouched way | 21,284 | 14.61 | 12.639 | 15.75 |
| S minus T, extra rewrites, inline rule | 6,347 | 4.36 | 3.477 | 4.33 |
| M minus T, extra rewrites, ID-only | 8,550 | 5.87 | 4.707 | 5.86 |
| S and T, stale survivors in rewritten blobs | 14,937 | 10.25 | 9.162 | 11.42 |

Way-blob splice kept by framed bytes: today 55.86%, inline-rule fix
51.52%, ID-only fix 49.99%. 58,701 untouched ways (244,817 refs) go out
stale; 202,564 OSC nodes differ on some untouched way, 176,528 of them
referenced by no OSC way. Probe hit rate 0.0114% of 12.4B refs. No
untouched way references an OSC-deleted node, no inconsistent base, no
sub-decimicro coordinates. Stale blobs spread out (runs ratio 0.92 for S,
0.99 for S minus T). The scan took 57.2 s wall (decode and probe summed
over workers 500 s and 407 s). The planet picture matches europe: the fix
costs about 4% of way-blob splice, and detection is the cost to manage.

A europe base that keeps untagged nodes now exists for benchmarking:
`brokkr.toml` variant `europe.pbf.altw-keep-untagged` (bygg), made by
`add-locations-to-ways --keep-untagged-nodes` from the seq 4714 indexed
file (3,715,575,708 nodes, none dropped, 0 missing). Baseline `-l` with OSC
4715 at `8f8ef92a`, bygg, UUID `78858442`: 45.9 s wall, `loc_missing` 0
(3,576,070 needed, 731,618 from the OSC, 2,844,452 from base node records).
That confirms section 1 on real data: the same diff resolves fully on a
base with its untagged nodes and leaves 2.8M refs unresolved on the
default altw base.

### Lateral findings

- A. `block_overlaps_diff` (`classify.rs`) starts every per-kind cursor at
  0 and walks linearly through the whole diff ID prefix below the blob's
  first ID; its doc's "visits at most the sub-slice spanning the block's
  ID range" is false. Europe `-l` precise time is about 307 us per node
  blob of about 326 nodes. Fixes: seed the cursor with `partition_point`;
  skip only the redundant precise check for range-disjoint node blobs
  under `-l` (narrow); or skip parse as well (loses the strict-parse
  framing check `node_coords.rs` leans on). The 140.5 s europe precise
  total covers all candidates, not just the redundant checks, so how much
  is recoverable is unmeasured; it is not established that it outweighs
  the estimated cost of decompressing every way blob.
- B. Owned-route false positives re-pread the full frame though the body is
  in `read_buf`, double-count `blobs_processed`, and `blobs_index_hit` is
  never bumped on `OwnedBytes`, so it reads 0 under `--direct-io`.
- C. Stale docs: `stats.rs` says `bytes_rewritten` is pre-compression size
  (it is framed compressed bytes); `node_locations.rs` names
  `extract_node_tuples`; `descriptor.rs` mentions a "consumer-build"
  false-positive path.
- D. `reference/pipeline.md`'s "rewrites about 92% of blobs" does not match
  the recorded plain-merge planet runs (`3ad57fc5`, `fda9f7a6`: 86%,
  43,670 of 50,816); `reference/performance.md`'s "~85 MB of changed
  output" for those runs contradicts their `bytes_rewritten` of 174-343 GB.
- E. `reference/performance.md`'s altw section still describes the removed
  dense mmap index.
- F. On an altw base, an OSC modify of a dropped untagged node is written
  as a node, so repeated `-l` runs grow back some of the dropped nodes.
- G. The `4914` to `4920` planet OSCs are named `planet-20260411-*` but are
  the primary's chain (4914 holds 2026-02-24 edits), not snapshot
  20260411's (seq 4954). The naming invites applying them to the wrong
  base.
- H. A coordinate-framing defect already in the rewrite path, verified:
  `write_base_way_local_with_locations` copies a base way's raw lat/lon
  deltas into `BlockBuilder`, which omits granularity and offsets
  (protobuf defaults 100, 0, 0; see the comment where it encodes the
  block). On a base block with non-default framing, a surviving way in a
  rewritten blob changes coordinates with no node moving (raw 1 at
  granularity 1000 becomes raw 1 at granularity 100). altw's `reframe.rs`
  refuses non-default framing for exactly this reason; apply-changes has no
  such guard. The same raw-copy pattern may affect metadata timestamps
  under a non-default `date_granularity` (unverified).
- I. The defect predates the descriptor pipeline: `e7bbfa28`, which
  introduced `merge --locations-on-ways`, already forwarded surviving base
  ways' raw coordinates, while its commit message claimed it removed the
  need to rerun altw. An original semantic limitation, not a regression
  from the splice architecture.
- J. Section 1's negative-ID point overgeneralizes: the shared relation
  scan does skip negative member IDs, but external altw rejects node blobs
  whose indexdata includes negative node IDs
  (`src/commands/altw/external/blob_meta.rs`), so on correctly indexed
  input it refuses rather than dropping them.

## Next steps

1. Measurement 1 is done for Denmark, europe and planet (section 5).
2. Check the README claims the research puts in doubt:
   - The planet-scale row `apply-changes --locations-on-ways` (daily diff),
     2m15s, about 3.3 GB. Trace which run it comes from; if it is the
     `2161ea62` family (4.2M unresolved refs, before the missing-node
     guard), it is not reproducible as a successful run today.
   - "All CLI commands are cross-validated against osmium on Denmark."
     Either `brokkr verify` does not exercise a node moved under an
     untouched way for `-l`, or it would have flagged this defect. Read
     brokkr's verify for apply-changes to find out which. Filed as
     brokkr#43, together with the altw bases the `-l` bench cells need and
     the OSC file naming.
3. Decisions for the maintainer:
   - Should `-l` refuse, or warn, on a base without its untagged nodes?
   - Parity target: osmium's actual semantics (overwrite where the index
     has a location, delete coordinate included) or a full recompute? osmium
     defects 1 to 3 in section 2 are deviations either way.
   - Correct DEVIATIONS.md: osmium's behaviour description, and its
     unresolved-ref sentinel, which is not `(0, 0)`.
4. Enforce with a cross-validation test against `osmium apply-changes -l`:
   a diff that moves a node referenced only by an untouched way, and a
   delete whose `lat`/`lon` differ from the base. For deterministic parity
   it should keep one version per node, avoid moved nodes that an OSC way
   also references (osmium defect 1) unless the new position is the
   westmost, avoid unresolved refs, and have at least one base node and
   one base way.

Reported by: change application.
