# Hunt: other-checks (opus)

Scope: geocode (+ admin, interp), extract, multi-extract, check-ids, check-refs,
getid-removeid (+ verify_getid_refs), getparents, diff, derive-changes
(`diff --format osc`), tags-filter, tags-filter --input-kind osc, inspect-tags,
plus verify_extract_simple, verify_polygon, verify_fileinfo, extract_config.

Read-only reading of the code. No timings were taken; costs below are counted
from the code (process launches, full-size OPL decodes, bytes materialized) and
the walls the brief reports. Where a number is an estimate it says so.

## The one-paragraph answer

The wall in this scope is not where the brief's two observed walls point. The
biggest in-scope cost is **repeated full-size osmium OPL decodes**, issued per
*question* rather than per *file*: every `compare_outputs` decodes each side
twice (once for an order check, once for the element diff), `osmium_counts`
decodes a third time, and multi-file checks re-decode the same file for every
pair it appears in. getid-removeid issues **9 uncached decodes of
denmark-sized files** (two near-whole-input `--invert` outputs and their
osmium removeid references), derive-changes **10** (13 on its omission path),
extract **16 per region** over 6 distinct files. Each decode is `osmium cat -f
opl` written to disk in full (several GB per denmark-sized file, about 59M
objects) and read back, so getid-removeid alone writes and rereads on the order
of tens of GB per run. Second: everything inside a check is strictly serial
(one child at a time through `Lifecycle::run`), so independent pbfhogg runs,
osmium runs and verifier-side models never overlap. Third, for geocode
specifically: two full index builds, four osmium runs and a single-threaded
model/query phase run back to back, and the verifier-side reference work
(model, query draw, expectations) is a pure function of cached inputs that is
recomputed from scratch every run.

The right move is a coherent rewrite of how these checks ask questions of
osmium's decoding: **one decode per distinct file per run, streamed, with order,
counts and every pairwise comparison answered from that one pass**, plus
content-addressed caching of decodes and merge verdicts (sibling scopes own
the machinery; my checks are its main consumers), plus a launch DAG inside each
check. These are high-conviction. The geocode-specific items are partly
rewrites (concurrent pipeline, cached reference model) and partly local
(parallel exact query draw).

---

## 1. High-conviction rewrite: one decode per file, every question from it

### The bottleneck

Every element comparison in these checks goes through `opl_compare`:
`decode` runs `osmium cat -f opl,...` into a file (marked `.uncached()`), and
`merge` reads two such files back. The checks call it once per *question*:

- `verify_input_order::compare_outputs` (used by getid, getid --invert,
  getid --add-referenced, getparents, tags-filter): `check_sorted(theirs)`
  (decode 1), `check_sorted(ours)` (decode 2), `same_elements(ours, theirs)`
  (decodes 3 and 4). Each file is decoded twice. The order checks only need
  the key stream, which the strict merge in `same_elements` reads anyway.
- `counts::osmium_counts` is `opl_compare::order` again: a third full decode
  of a file already decoded twice (getid: `osmium_counts(&pbfhogg_invert)`
  right after `compare_outputs` on the same file).
- extract (`run_region`), per strategy: `check_sorted_counted(compared)`,
  `check_sorted_counted(osmium_out)`, then `scan_element_diff` decoding both
  again. Then `verify_extract_simple::adjudicate` decodes pbfhogg simple and
  osmium complete_ways a third time (`simple_vs_complete`) and runs osmium
  removeid plus another two-sided diff (`present_members`).
- derive-changes: four `same_elements` over the set {new, rt_pbfhogg,
  rt_cross, rt_osmium} plus `check_sorted(new)` and
  `check_sorted(rt_pbfhogg)`: `new` is decoded 4 times, `rt_pbfhogg` 3 times.
- multi-extract: per region `check_sorted_counted(multi)` then
  `same_elements(multi, seq)`; the sequential strip outputs are decoded again
  for the mixed pass.

Decode counts as the code stands (sorted input, passing run):

| check | full-size (input-sized) decodes | small decodes | distinct files |
|---|---|---|---|
| getid-removeid | 9 (invert: 4 + counts 1; invert without ways: 4) | 13 | 4 full + 6 small |
| diff --format osc | 10 (13 when osmium's roundtrip needs the omission model) | 0 | 4 |
| diff | 2 | 0 | 2 |
| extract, per region | 0 (region-sized) | 16 | 6 (+1 removeid output) |
| multi-extract | 0 | 3 per region per pass | 1 per output |
| getparents | 0 | 32 (8 modes x 4) | 16 |
| tags-filter | 0 | 12 | 6 |

### Why the structure causes it

`opl_compare`'s API is pairwise and verdict-only: `order()` answers one file,
`compare()` answers one pair and errors on disorder rather than reporting it.
So a check that wants "order verdict for ours, order precondition for theirs,
counts, element diff" has to call three or four entry points, each of which
launches its own osmium and writes its own multi-GB text file. Nothing
remembers that a file was already decoded under the same (or a projectable)
policy earlier in the same check, let alone the same run.

### The redesign

A decoded-stream abstraction owned by the run, not the call site:

1. **`Decoded` handle per (file content, policy)**, produced once. Key streams,
   per-kind counts and strict-order verdict are by-products of producing it
   (they are policy-independent: `order()` reads only kind and ID, so it can
   ride any policy's decode, including the NO_LOCATIONS or FULL decode the
   diff needs; `Policy::CONTENT` decodes used only for order become
   unnecessary).
2. **N-way merge**: one pass over k decoded streams, emitting every requested
   pairwise result (Merged counts, the per-pair listing file, the `each` and
   `paired` callbacks) and every side's order verdict and counts. derive-changes
   becomes one 4-way merge over 4 decodes (10 -> 4). extract becomes one 6-way
   merge per region (16 -> 6), and `present_members` becomes a sorted
   intersection of the requested member IDs with osmium simple's key stream,
   read in that same pass (no osmium removeid, no extra two-sided diff).
   simple_vs_complete is a pair already in the 6-way set.
3. **Order as verdict, not precondition**: the merge records disorder for a
   side and keeps scanning that side's keys to the end (so the counts message
   stays what `check_sorted_counted` prints today), then raises exactly the
   error `compare` raises today. The observable sequence stays the same: the
   order failure is recorded, then the comparison error is raised.
4. `compare_outputs` rewritten on top: 2 decodes instead of 4; `osmium_counts`
   on a file already in hand is free. getid-removeid goes 9 -> 4 full-size
   decodes; getparents 32 -> 16; tags-filter 12 -> 6.
5. **Streamed, never materialized**: `osmium cat -f opl -o -` piped straight
   into the merger, both (or all k) sides decoded concurrently by separate
   osmium processes. This removes the GB-scale write-then-reread entirely and
   turns a pair's wall from sum(decode A, decode B, merge) into roughly
   max(decode A, decode B). Listings are still written, since they hold only
   differing records.

### Why it pays

getid-removeid and derive-changes are each doing on the order of 9-10
input-sized decodes; the brief's "single checks take 2 to 9 minutes" fits
these two better than it fits geocode. Halving the decode count, then making
each remaining decode a pipe instead of a multi-GB file, then overlapping the
sides, compounds: a plausible 4-8x on those two checks. It also removes tens
of GB of scratch writes per run, which on this project's near-full drives is
wall time in its own right (trim debt is a recorded effect on this hardware).

### Risks

- Listing files are failure diagnostics, so a k-way merge must write each
  pair's listing byte-identical to today's (same `-`/`+` convention, same
  first/second assignment per pair). That is a correctness test target, not a
  design problem.
- Streaming loses the "OPL pair stays for diagnosis on error" property of
  `walk` (it keeps the decodings when the merge errors). Replace it with:
  on error, re-run the decode to a file for the error report. That costs time
  only on the failure path.
- k concurrent osmium decoders on denmark-sized files: memory is fine
  (osmium cat streams), CPU contention is real; cap k concurrency.

---

## 2. High-conviction (sibling-owned machinery, my checks are the payload): content-addressed decodes and verdicts

### The bottleneck

`opl_compare::decode` is explicitly `.uncached()`. Every reference artifact
osmium produced (extract, removeid, getid, getparents, tags-filter,
apply-changes roundtrips) is cached by content and stable from run to run,
yet its OPL decoding is recomputed every run. pbfhogg's outputs are also
byte-stable from run to run whenever the commit under test does not touch
that command's encoding (the writing program is `pbfhogg/<version>`, no hash;
encoders are deterministic), which in a dev loop is most commands on most
commits.

### The redesign

- **Merge-verdict cache**: a merge result is a pure function of (content digest
  of each side, policy, strictness, osmium identity, verifier binary digest).
  Cache `Merged` + order verdicts + counts + the (small, on a pass) listing
  under that key. On a hit, neither side is decoded. For a commit that touches
  only geocode, every comparison in getid, derive, diff, extract, multi-extract,
  tags-filter and getparents becomes a digest plus a lookup.
- **Decode cache as a fallback** for one-side hits (a pbfhogg output changed,
  its osmium reference did not): cache a compact per-object stream, not OPL
  text, for example `(kind, id, xxh3-128 of the OPL line)` (24 bytes/object,
  versus about 100 bytes/object of OPL with metadata). A verdict needs only
  line equality; the listing of differing records is produced by re-decoding,
  which happens only when something differs. diff.rs is the exception: it
  needs the full content of created/modified/deleted objects on the passing
  path, but those are OSC-sized and fetched from the listing as today.

### Why it pays

The dev loop is the stated problem. With verdict caching, the steady-state
cost of the element-comparison checks becomes "run pbfhogg, digest its
outputs". That is the largest wall reduction available to this scope, and it
needs no change to any claim: same bytes in, same osmium decoding, same merge.

### Risks

Key completeness (verifier digest, osmium identity including libs, policy,
strictness, argv-level choices such as `Policy::between`'s outcome); never
cache an Environment or Interrupted outcome; disk budget (per-object hash
streams for a denmark-sized file are around 1.4 GB, so cache verdicts first,
hash streams second). This is orchestration and engine work; I state what I
need in section 7.

---

## 3. geocode: concurrent pipeline, cached reference side, exact parallel query draw

### What the check does today (strictly in this order)

1. `build-geocode-index` at tolerance 20 (full denmark build).
2. `build-geocode-index` at tolerance 0 (another full build; only its admin
   answers are ever judged).
3. osmium `tags-filter` of the full input (nw/addr:housenumber, w/highway,
   w/addr:interpolation), then `add-locations-to-ways -f opl`.
4. osmium `tags-filter` of the full input (boundary relations), then
   `add-locations-to-ways --keep-untagged-nodes -f opl`.
5. `build_model`: single-threaded `BufReader::lines` over the first OPL,
   allocating a `String` per line, a decoded `String` pair per tag, `Vec`s per
   way (and a clone of `nodes` per interpolation way).
6. `admin_model`: same over the second OPL, every node record into a SipHash
   `HashMap<i64, Option<Pt>>`.
7. `queries`: 1000 uniform + up to 40 000 witness attempts + 600 admin
   witnesses, every attempt computing all three class expectations in full.
8. Two `geocode-query` runs, then judging.

Steps 1, 2, 3 and 4 are mutually independent; 5 depends only on 3, 6 only on
4; 7 needs 5 and 6; 8 needs 1, 2 and 7. Today the wall is the sum.

### Strong hypotheses about where the time goes

- The two builds are probably the largest warm-cache item (two full builds of
  a 450 MB input, each a 4-pass pipeline with scratch buckets and a smoke test).
- The 2.5 to 4.4 min spread is most likely the osmium reference runs being
  cached in some runs and not in others (four osmium runs, two of them full
  input reads with referenced-object passes, retained under a 20 GiB LRU
  budget that every other check competes for). The verifier-side work is
  deterministic for a dataset, so it cannot explain run-to-run spread.
- The witness loop is the largest verifier-side item and can be tens of
  seconds. The binding quota is likely interpolation within the fine radius:
  anchors are drawn class first (1/3 interp), reach is uniform in radius over
  0..1300 m, so about 5.8% of interp-anchored attempts land within 75 m, which
  needs on the order of 8 000 attempts to reach 150; if it never fills, all
  40 000 run. Each attempt evaluates about 30 grid cells of 0.01 degree for
  all three classes; address anchors sit where addresses are dense (the draw is
  density weighted), so a typical attempt scans tens of thousands of address
  candidates, and `expect_street` does a SipHash `HashSet` insert and a
  `String` clone of the street name for **every** candidate segment, not only
  the tied ones. Estimate: a few ms per attempt, single-threaded.
- `AdminModel::expect` is a linear scan of every polygon per query, with a
  full O(n) even-odd test against every polygon whose box holds the query
  (the level-2 country polygon has many vertices), and it is computed twice
  per query (once per judged build) although only the band differs.

### The redesign (a coherent rewrite of the check's driver)

1. **Launch DAG**: start both builds and both osmium chains at once; build
   both models while the builds run; draw queries as soon as the models
   exist; run both `geocode-query`s together. Wall becomes about
   max(build A, build B under contention, osmium chain + parse + draw).
   Needs a concurrent launch API from orchestration (section 7).
2. **Cache the reference side as a product**: the model, the query set and
   every expectation are a pure function of (digest of the two OPL artifacts,
   verifier binary digest; the tunables are compiled in). Persist
   `queries` + `Expect` per class + `AdminExpect` per band under that key. On
   a warm run the check is: two builds, write points, two queries, judge.
3. **Exact parallel witness draw**: the draw sequence does not depend on
   acceptance. Every witness attempt consumes exactly four splitmix64 draws
   (class, pick, reach, bearing) whether kept or not, and the uniform phase
   consumes exactly two per query. splitmix64's state is a counter
   (`state = seed + k * 0x9E3779B97F4A7C15`), so attempt k's point is O(1)
   to compute directly. Evaluate attempts in parallel in batches, then apply
   the sequential tally-and-accept in attempt order, stopping at the same
   attempt the serial loop stops at. The admin witnesses then start from the
   draw state `seed + (2*1000 + 4*attempts_used) * golden`, which is exact.
   The query set is bit-identical to today's.
4. **Cheaper acceptance**: acceptance needs only each class's category, which
   is a function of the nearest distance alone. Compute the nearest (no
   allocation, no name clone, no `HashSet`: segments can be deduplicated by
   a per-query generation stamp array) and build the full tied `Expect` only
   for attempts that are kept. Optionally search cells nearest-first with a
   lower-bound prune and a finer grid; this keeps the model independent of
   pbfhogg (plain lat/lon grid, no S2) but needs a conservative float margin
   on the cell lower bound so pruning never drops a tied candidate.
5. **Parallel model parse**: mmap the OPL, split at newlines into chunks,
   parse records in parallel, apply `model_record` in line order (the order
   of `addresses` and `segments` matters: witness picks index into them and
   tie lists follow them), so the model is identical.
6. **Admin**: bucket polygons by grid cell of their box; compute containment
   once per query and apply both bands to the same containment result; count
   crossings through the existing edge grid rather than over the whole ring.

### Medium option with a contract question: the tolerance-0 build's input

The tolerance-0 index is used only for admin answers. Building it from the
boundary-only decoding (osmium's `geocode-admin-filtered.osm.pbf`, which the
check already produces) would make that build tiny. But it is not free of
risk: that file has no indexdata, so the build would take the
`--allow-unindexed` path, a different code path from the one the check
exercises today, and any defect where non-boundary content perturbs
tolerance-0 admin answers (node-store chunking, scratch bucketing on the full
input) would only be caught outside the 20 m band by the other build. I would
not do this without the maintainer's ruling; concurrency gets most of the
win without touching what is built.

### Lateral, cold cache only

The two osmium `tags-filter` runs both read the whole input. One union
filter (`nw/addr:housenumber w/highway w/addr:interpolation
r/boundary=administrative,postal_code`) with `--keep-untagged-nodes` would feed
both models from one decoding: the address/street model ignores every record
that is not an addressed node, street, building or interpolation way, and the
admin model needs exactly the boundary closure the union still contains. The
cost is a larger OPL for the address model (untagged highway nodes become
records). It matters only on a reference-cache miss.

---

## 4. Launch DAG inside every check (needs a concurrent harness)

### The bottleneck

`Harness::pbfhogg`, `Harness::reference` and the probes all go through
`Lifecycle::run`, which spawns one child, waits, and settles the tree (as
subreaper it kills anything adopted). One child at a time, by construction.
Every check in this scope has independent sub-runs that wait on each other
for no reason:

- check-ids (40 s observed): three `degrade` runs on the input are mutually
  independent; then 8 `check --ids` runs over the four input-sized cases and
  10 over the fixtures are all read-only and independent. The 40 s is mostly
  these pbfhogg runs back to back (the osmium fileinfo runs on the derived
  files hit the content cache whenever pbfhogg's degrade output is
  byte-stable).
- diff: four `pbfhogg diff` runs; the two unindexed ones need only the two
  strips, which need only `new`.
- derive-changes: after `new`, the pbfhogg derive, osmium derive and both
  strips are independent; after the derives, three roundtrips are
  independent; the unindexed derive needs only the strips.
- extract: three pbfhogg extracts per region, independent, each reading the
  whole input; plus every region independent of every other.
- multi-extract: 5 strip + P polygon sequential extracts, all independent of
  each other and of the single-pass run.
- getparents: 8 pbfhogg getparents runs over the input, independent.
- tags-filter: 3 expressions; tags-filter --input-kind osc: 4 cases x 2 runs;
  inspect-tags: 2 runs; check-refs: 2 runs. All independent.

### The redesign

Each check builds a small plan (launches with dependencies, then judgements
that read their outputs) and hands it to an executor with a concurrency
budget, recording verdicts in the same order as today so failure lists and
detail read the same. Checks stay sequential relative to each other unless
orchestration decides otherwise.

### Why it pays, and the limits

pbfhogg commands are internally parallel but have serial phases and I/O waits;
running 2-4 at once on page-cached input usually beats running them serially.
I expect around 2x on check-ids, extract, multi-extract and getparents, more
where the sub-runs are I/O-bound or partly serial. The step timings in the
result line (`pbfhogg X 2.0s (3 runs)`) become summed CPU-ish time rather than
wall; that line's timing text is not a claim, but say so.

Risks: memory (two geocode builds, or several extracts, at germany scale),
contention noise, and above all `settle` semantics: as subreaper the verifier
adopts orphans, and with several children alive it can no longer attribute
an adopted orphan to a launch. That needs per-launch process groups or
cgroups (section 7).

---

## 5. Cross-check fixture sharing (within one run)

The same pbfhogg command on the same input is run by several checks:

- `apply-changes INPUT OSC -o new` (buffered): diff, derive-changes, and
  apply-changes itself when `--direct-io` is off. Three full applies per run.
- `degrade --strip-indexdata INPUT`: check-ids, repack, diff, derive-changes.
  Four strips of the input per run.
- `degrade --strip-indexdata new`: diff and derive-changes.
- `extract --simple --polygon P INPUT` (buffered): extract's simple polygon
  pass and multi-extract's sequential polygon reference run the identical
  argv when `--direct-io` is off.
- `degrade --unsort`: check-ids passes `--elements-per-blob 8000 --force`, the
  degrade check passes neither; on an indexed input with the default cap equal
  to the 8000 `DEFAULT_CAP`, aligning the argv would make the two outputs one
  artifact.

The redesign is a run-level memo of pbfhogg launches keyed by (pbfhogg digest,
canonical argv with inputs by content digest, direct-io), returning the
earlier output (hard link or reflink) and its recorded stderr and exit. Each
check still judges its own claims on the artifact. Fixture-producing runs
(the buffered `new`, the strips) are exactly where this is safe: the checks
that use them do not judge the fixture's own production (diff and derive say
so explicitly). Two cautions: a check that judges the launch's stderr (the
`judge_way_locations` warning) must get the recorded stderr; and the memo
must not alias a run whose output a check mutates (`append_empty_claim`
mutates only the fixture copy in check-ids, which is not shared).

---

## 6. Medium-value local changes

- **getid**: `osmium_counts(&pbfhogg_invert)` is a fifth full decode of a file
  `compare_outputs` just decoded twice; take the counts from that pass. This
  falls out of section 1 but is worth doing on its own first, since it is
  one full decode removed with no API change other than returning counts.
- **derive-changes**: `print_fileinfo("roundtrip-osmium")` is informational
  only (an osmium fileinfo -e full read on a cold cache); the counts are
  available from the comparison pass.
- **extract**: drop the osmium removeid round trip in `present_members`; the
  presence of a requested member in osmium simple's node and way sets is a
  sorted intersection with the key stream of a decode the check already
  makes. Negative IDs are refused earlier, so removeid's absolute-value
  matching never mattered.
- **multi-extract**: fuse `check_sorted_counted(multi)` into the
  `same_elements` pass (3 -> 2 decodes per region per pass), and decode each
  sequential output once across both passes. If a multi output is
  byte-identical to its sequential reference (both written by pbfhogg's simple
  extract encoder; the all-bbox single pass may copy node blobs raw, so not
  guaranteed), the element comparison is exactly "identical" without decoding
  the second file; one decode still certifies order.
- **check-refs**: osmium's `check-refs -r` report already contains the
  ways-only count (its first line); the separate ways-only osmium run is
  redundant. Cached, so this only matters on a cold cache.
- **geocode**: `expect_street` and `expect_interp` clone a `String` (or an
  `InterpAnswer`) per candidate and insert into a SipHash `HashSet` per
  candidate; collect indices and distances, clone only the tied ones. This is
  the cheap first step of item 3.4.

---

## 7. What I need from the sibling scopes

From **engine** (`opl_compare` and the decode path):

1. A decoded-stream handle with key stream, counts and order verdict as
   by-products, and an N-way merge that writes per-pair listings exactly as
   today's pairwise listings (section 1).
2. Order as a recorded verdict plus a continued key scan, then the same
   error `compare` raises today, so `compare_outputs` and extract can drop
   their separate `check_sorted` decodes.
3. Streaming decode (`-o -` into the merger), sides decoded concurrently.
4. Decodes and merge verdicts cached by content (section 2), and a
   whole-file byte-equality shortcut (two byte-identical files have one
   decoding: decode once, certify order, report identical).

From **orchestration**:

1. A concurrent launch API in the harness (spawn, then join on a set) with
   per-launch process-tree ownership (process group or cgroup per launch) so
   that settling stays exact with several children alive.
2. A run-level memo for pbfhogg fixture launches (section 5), and for the
   geocode reference product (section 3.2), keyed by content.
3. Memoize per run, by the identity stamp the run already keeps, the
   input digests that `key_text` recomputes on every reference call: the
   denmark input is re-hashed in full by every osmium run that reads it
   (geocode x2, tags-filter x3, extract x3 per region, getid and getparents x
   several, inspect-tags x2, check-refs x2, every fileinfo). Also memoize the
   osmium identity (`--version` and `ldd` probes, digests of osmium and every
   shared library), which `cache_key` recomputes per reference call and
   twice on a miss.
4. Give the OPL artifacts that geocode depends on a retention priority, or
   make the reference budget aware of re-creation cost: a geocode run that
   loses its four osmium artifacts to LRU pays two full-input tags-filter
   passes again (the likely source of its 2.5 vs 4.4 min spread).

---

## 8. Bugs, smells and surprises noticed on the way

- `opl_compare::decode` is `.uncached()` while every reference artifact it
  decodes is cached and stable; this one flag is the root of most of the
  repeated decode cost across this scope.
- `Run::watch` breaks hard links by copying the whole file
  (`private_input_copy`) the first time a cache-placed artifact becomes an
  input. For getid's two denmark-sized osmium removeid outputs that is two
  full copies per run, on the critical path. A reflink or `copy_file_range`
  would make it near-free; verifying the artifact's digest at use instead of
  isolating the inode would avoid it.
- getid-removeid's two `--invert` comparisons write roughly 8 OPL files of
  several GB each to scratch and delete them; derive-changes writes 10. On the
  drives this project runs on, that scratch churn is itself a recorded
  performance hazard (trim debt), so the streaming change also helps the
  checks that run after these.
- geocode: `AdminModel::expect` recomputes polygon containment for every
  query once per judged build; only the band differs.
- geocode: the witness loop's cost is dominated by attempts that are thrown
  away, and every attempt computes full tied candidate lists for all three
  classes before acceptance looks at only the categories.
- check-ids and degrade build nearly the same `--unsort` fixture with
  different argv; nothing in check-ids depends on the `--force` or the
  explicit cap beyond knowing the cap's value.
- Timing text in the result line sums step durations; once anything runs
  concurrently those sums exceed the check's wall. Harmless, but the line
  should say which it is.
- I found no correctness gap in the claims these checks judge. The closest
  thing is a fragility in geocode: `queries` keeps drawing from one
  splitmix64 stream across the uniform, witness and admin phases, so the
  admin witness set depends on where the witness loop stopped, which in turn
  depends on the model. That is deterministic and correct, but any change
  to the witness loop silently changes the admin query set; a parallel
  rewrite must preserve the attempt count exactly (item 3.3), and a test
  that pins the attempt count for a fixed fixture would guard it.

---

## Ranking

1. Section 1 (one decode per file; fused order, counts and diff; N-way
   merge; streamed). Full rewrite of the comparison layer as used by these
   checks. Biggest payoff on getid-removeid and derive-changes, real on
   extract, getparents, tags-filter and multi-extract.
2. Section 2 (content-addressed decode and verdict caching). Sibling-owned;
   the largest dev-loop win once section 1 exists.
3. Section 3 (geocode pipeline: concurrency, cached reference product, exact
   parallel draw). Rewrite of the check's driver; the model logic is unchanged.
4. Section 4 (launch DAG inside checks). Needs the harness change; check-ids
   is the clearest beneficiary.
5. Section 5 (fixture sharing). Local to a run-level memo; removes 2 applies
   and 3 strips of the input per `verify all`.
6. Section 6 local items, which can land before any of the above.
