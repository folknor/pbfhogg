# pbfhogg-verify

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
  and its whole check takes about 9 s.
- pbfhogg groups in getparents, check-ids, extract, tags-filter,
  inspect-tags and check-refs: each run takes a second or two at most.
