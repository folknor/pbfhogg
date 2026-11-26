# pbfhogg-verify

## Shared expected file and the pooled frame reader (bygg, 2026-11-26, `1361533e` against `da1f1a55`)

Degrade and repack share one expected file per run and reuse input
claims, warm drop cases inflate once, and the frame reader uses one open
file with pooled inflation. Denmark, warm cache, the same pbfhogg binary
on both arms (`brokkr run --commit 1361533e` builds only the verifier):

| Check | `1361533e` | `da1f1a55` |
|---|---|---|
| `degrade` | 56.4 s | 40.4 s, 41.9 s |
| `repack` | 32.5 s | 22.2 s, 22.2 s |

About 27 % and 32 % faster. A full denmark `verify all` on `da1f1a55`
ran 6m24s, 25 of 25 PASS; its step walls put osmium `cat` and the OPL
merge as the largest costs of the slowest checks (cold degrade: osmium
cat 18.4 s, merge 16.3 s of 59.5 s).

Wall time of the real-data cross-validation checks (`brokkr run --with
pbfhogg pbfhogg-verify -- CHECK --dataset denmark`), read from each check's
PASS line: the check wall and its step walls. All cells below are denmark on
plantasjen, one run per cell, interleaved, the tree switched only while no
brokkr command was queued.

## Current baselines

### apply-changes: expected stream projected, never written

A = cb4fd54d, B = the change that stopped writing `expected.opl`, decides
the comparison from the output's stored decoding summary, decodes a cold
order check under the comparison's policy and drops the informational
`osmium fileinfo`. Warm summaries (one B warm-up, 12.9 s, not counted),
order B, A, A, B:

- Check wall B 12.8 / A 25.1 / A 24.7 / B 12.8 s, about -49 % in both pairs.
- B: `opl expected projection` 5.8-5.9 s, `osmium cat` 5.8-5.9 s (2 runs,
  the OSC and osmium's output), no merge. A: `osmium cat` 11.5-11.8 s
  (3 runs, pbfhogg's output decoded too), `opl merge` 5.5-5.7 s, the
  untimed `expected.opl` write in the remainder, `osmium fileinfo` cached
  0.4 s.
- Verdict: pays. A changed or cold output still decodes and merges, against
  a second pass over the pinned osmium decoding.
- B under `--fresh-reference` (no summaries, every reference run made, one
  cell, not paired): PASS 42.6 s, `osmium cat` 12.0 s (3 runs: the order
  check's decode of pbfhogg's output under the comparison's policy is the
  one the merge reuses), `opl merge` 12.5 s, `opl expected projection`
  5.9 s, `opl order scan` 2.9 s, `osmium apply-changes` 7.1 s. The merge
  reads the projection through a reader that re-splits its records, which
  likely accounts for the merge being slower than A's file merge; the cold
  path was not A/B-measured.

### diff --format osc: both derives in one launch group

A = cefb81c6, B = cefb81c6 plus the change that strips both inputs as one
group, then runs the indexed and unindexed `diff --format osc` together,
holding the strips' and the unindexed derive's outcome to the parity
verdict, and drops the informational `osmium fileinfo`. Warm summaries
(one B warm-up, 17.6 s, not counted), order B, A, A, B:

- Check wall B 17.5 / A 18.3 / A 18.3 / B 17.3 s, -4.4 and -5.5 %.
- `pbfhogg diff` (2 runs) 15.3-15.5 s against 16.2-16.3 s: the unindexed
  derive is most of it, so the overlap hides only the indexed derive's
  second or two. The earlier estimate of "up to about 8 s" assumed the
  two derives took about equal time; they do not.
- A first shape, grouping the strips with the indexed derive and the
  unindexed derive with pbfhogg's roundtrip apply (under 1 s), measured
  B 18.2 / A 19.4 / A 18.3 s against the same A: within noise, not landed.
- Verdict: pays, modestly.

### degrade: drop and unsort cases decided from predicted streams

A = bdf10df4, B = bdf10df4 plus the change that holds a warm drop or
unsort output's stored summary to a predicted stream (the expected file
with the documented swap, or less the records the output's blobs do not
hold), on plantasjen, denmark, warm summaries, order B, A, B (B measured
uncommitted):

- Check wall B 1m17s / A 1m55s / B 1m13s, about -35 %.
- A: `opl merge` 33.0 s (4 runs), `osmium cat` 29.9 s (5 runs). B: no
  merge, `osmium cat` 6.3 / 5.8 s (the input only). `pbfhogg degrade`
  19.7-22.2 s on both arms is now most of the check.
- `verify repack` on B (its cases take the unchanged prediction): 46.8 s,
  in line with the 43-45 s recorded when its summaries landed.
- Verdict: pays. A cold run still decodes and merges every case.

### renumber: paired walk decided from canonical readings

A = 8cf241db, B = 8cf241db plus the change that stores each output's
canonical reading after a passing walk and decides a later run from two
of them, on plantasjen, denmark, warm plain summaries, B measured
uncommitted:

- Where the time went before: with the attribution closure replaced by
  a no-op, A's `opl merge` was 10.3 s against 8.7 s with it, so the
  record-by-record proof costs nothing measurable; the merge is the pair
  loop over two OPL decodings of about 59 million records each.
- Cold readings (removed before each B cell), order B, A, A, B: check
  wall 27.0 / 22.0 / 22.4 / 27.0 s; `opl merge` 18.5 / 13.6 / 13.8 /
  18.4 s. Building both readings costs about 4.8 s, paid whenever a
  reading is missing: an output's bytes changed, or the reading was
  never stored or was evicted.
- Warm readings: B 1.5 / 1.7 s, against A's 22.0-22.4 s: no decode, no
  merge; `pbfhogg renumber` (1.2-1.3 s) is the check. pbfhogg's denmark
  output is byte-stable across runs, so its keys repeat.
- The stored readings counted 441 node, 32943 way and 706 relation
  orphans, the walk's own figures.
- Verdict: pays.

### add-locations-to-ways: reference order read under the comparison's policy

A = 6ced6dd4, B = 6ced6dd4 plus the change that decodes the reference
for its order verdict under the policy its comparison reads (inline
coordinates when the reference declares them), on bygg, denmark, B
measured uncommitted, one cell per arm:

- Stored summaries removed before each cell, ordinary run: A 9.3 s
  (`osmium cat` 2.4 s, 3 runs; `opl merge` 1.1 s), B 8.4 s (`osmium cat`
  2.2 s, 2 runs; no merge: the reference summary the order scan publishes
  lets each engine comparison decide from one scanned side).
- `--fresh-reference` (summaries off): 20.4 s on both arms. One decode
  fewer, but the sparse decode no longer shares a launch group with the
  reference's, so the wall does not move.
- Warm summaries: 5.0-5.1 s on both, no decode.
- Verdict: pays on the cold-summary path; neutral elsewhere. Engine
  parity by transitivity was not built: with summaries live the parity
  is already decided from the engines' summaries.

### geocode: one launch group for both builds, borrowed model parse

A = 6ced6dd4 with step labels added, B = A plus the build group and the
borrowed-line OPL parse, on bygg, denmark, warm reference cache, B
measured uncommitted:

- A 5.2 s: `geocode model` 2.7 s (parse and finish of a 1.24 GB OPL),
  `pbfhogg build-geocode-index` 1.6 s (2 runs), `geocode-query` 0.29 s,
  `admin model` 0.27 s, `query draw` 0.086 s, `admin judge` 0.086 s
  (both bands).
- B 4.3-4.4 s: `geocode model` 2.4 s (2.1 s parse and 0.33 s
  interpolation resolve, timed apart in one cell), builds 1.0 s; the rest
  unchanged; the same 1727 queries.
- Cold reference (`--fresh-reference`, A): 11.3 s, adding `osmium
  tags-filter` 4.6 s and `osmium add-locations-to-ways` 1.1 s.
- Verdict: pays. The witness loop and admin polygon scan cost under a
  tenth of a second each and were left alone.

## History

### Launch groups (2026-11-21)

Concurrent child launches, built in two steps.

Step 1, barrier-settled launch groups and concurrent OPL decodes in one
comparison (16e70d27). A = 4f484d00, B = 16e70d27:

- `sort`, order BA, AB, BA: B 13.1 / A 18.1, A 18.1 / B 13.0,
  B 13.0 / A 17.9 s, -27 to -28 % in every pair. `osmium cat` (2 runs)
  7.0-7.1 s against 11.7-11.9 s; `opl merge`, `pbfhogg sort` and the cached
  `osmium sort` unchanged.
- `cat-dedupe`, order A, B, A, B: totals 2m55 / 2m50 / 2m50 / 2m44, with
  `osmium cat` (11 runs) 45.1-45.4 s against 40.7-40.8 s. Its one two-miss
  comparison is a small share of its decodes, so this is a mixed workload,
  not evidence either way; the third cell's arm was inferred, not proven.

Step 2, pbfhogg runs as launch groups (cb4fd54d), at most
`tunables::PBFHOGG_GROUP` members. A = e6207da8, B = cb4fd54d uncommitted,
warm summaries, order B, A, A, B:

- `diff`: B 32.9 / A 47.5 / A 47.5 / B 32.8 s, about -31 %. `pbfhogg diff`
  (4 runs) 19.1 s against 33.4-33.5 s.
- `cat --dedupe`: B 41.3 / A 1m21 / A 1m21 / B 41.5 s, about -49 %.
  `pbfhogg cat` (6 runs) 23.5 s against 1m3s.

Refused, no saving to win:

- Grouping cached reference runs: each cached run takes under 0.6 s
  (denmark `verify all` at 0e07dcd6), so overlapping them saves nothing
  measurable.
- A per-launch subreaper shim, so that one launch's result can be consumed
  while an unrelated sibling still runs: only geocode's chains need that,
  and its whole check is short (the denmark cells under Current baselines,
  "geocode: one launch group for both builds", run 4.3-5.2 s warm and
  11.3 s with a cold reference).
- pbfhogg groups in getparents, check-ids, extract, tags-filter,
  inspect-tags and check-refs: each run takes a second or two at most.
