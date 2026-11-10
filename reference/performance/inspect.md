# Inspect

### inspect full-decode scan on the decode pipeline (bygg, 2026-10-30)

`inspect --extended` (and every other full-decode inspect mode) used to
decode, validate and fold every element on the calling thread. It now runs
on the ordered read pipeline: each data blob is summarized on a decode
worker into a per-blob scan state, folded in file order on the consumer.
Planet (`planet-20260223-with-indexdata`), `--bench 1`:

| cell | commit | wall | peak anon | avg cores |
|---|---|---:|---:|---:|
| baseline `9b49e8fb` | `627883d7` | 13m22s | 26 MB | 1.0 |
| parallel scan (unstored `--force` cell) | landing commit, tree at `13ec0927` + change | **41.4 s** | 571 MB | 29.3 |

The run is worker bound: the dispatcher spends 96 % of wall waiting for
an admission permit (`pipeline_decode_admit`), the decoded-channel send
wait is 0.5 %. The memory is the 30 workers' transient decode storage
(decompress buffers, offset tables, allocator retention), not retained
summaries: the permit window holds summaries, which are small (tens of KB at
most, under `--locations`).

### inspect --extended metadata census by value (bygg, 2026-10-30)

The census judges each user name by what it resolves to. Resolving the name
per element (a string-table load for every object) cost about 5 % on planet
`--extended`; precomputing, once per block, which string indices are empty
removes the per-element load. Matched interleaved `--bench 1` cells,
baselines stored at `9a2c0275` via `--commit HEAD`, variants unstored
`--force` cells on that tree plus the change:

| cell | wall |
|---|---:|
| baseline `ae2927ba` | 41.76 s |
| per-element resolve | 43.82 s |
| baseline `dd1d937c` | 42.07 s |
| per-element resolve | 43.97 s |
| per-block empty-index table | 42.31 s |
| baseline `ee5cc078` | 42.36 s |
| per-block empty-index table | 42.59 s |

The per-block table is within noise (+0.6 % against the adjacent baseline in
both pairs); phase split identical (29.3 avg cores, same disk read). It
landed; the per-element resolve did not.

## Inspect --extended: validator-projection scan (bygg, 2026-10-31)

`4dbc2d47` scans each blob inside the block validator instead of
building a `PrimitiveBlock` and walking its elements. Planet,
interleaved `--bench 1`:

| UUID | Commit | Wall | Peak anon |
|---|---|---:|---:|
| `970ff659` | `4dbc2d47` | 27.1 s | |
| `3970b8a1` | `587bfc12` | 39.5 s | |
| `c1a2a0e6` | `4dbc2d47` | 26.4 s | 303 MB |
| `102cf3b2` | `587bfc12` | 39.7 s | 379 MB |

-33 % wall. Every decoding inspect mode runs the same scan.

## inspect --nodes reads through a projection (bygg, 2026-11-02, against `5ec5eefc`)

`inspect --nodes` reads each node blob through a projection on the block
validator (`NodeCoords`, on the accumulate pool's projection twin)
instead of building a `PrimitiveBlock` and walking every node: a dense
group's validated `lat` and `lon` arrays are decoded once the blob is
accepted, where the element walk stepped `id`, `lat`, `lon` and
`keys_vals` per node. Planet `planet-20260223-with-indexdata`, default
`-j`, interleaved `--bench 1`, both arms built in the main tree (HEAD
cells stored, change cells unstored `--force`), arm order alternated:

| Pair | HEAD | Change |
|---|---:|---:|
| 1 (change first) | 26.03 s (`2dcddb68`) | 22.46 s |
| 2 (HEAD first) | 25.98 s (`c4ad8414`) | 22.36 s |

-14 % wall. The scan phase (after the 6.6-6.8 s schedule walk both arms
share) went from 19.25 and 19.23 s at 25.3 cores to 15.60 and 15.71 s
at 21.7 cores, reading about 3.8 GB/s; peak anon 308 to 296 MB.

The same session measured the way passes that read only IDs and refs,
moved onto `read::project_ids::WaySpans` and kept as block walks, since
none moves its command past the noise floor (same arm protocol):

| Command | HEAD | Projection probe | Phase, HEAD to probe |
|---|---:|---:|---|
| tags-filter `w/highway=primary` | 75.72, 75.77 s (`2612d0fa`, `d7b43157`) | 73.81, 74.22 s | way-dependency walk after its schedule loop 10.27, 10.24 to 8.68, 8.74 s |
| extract `--smart`, Europe bbox | 132.05, 131.81 s (`abc90310`, `2b604dda`) | 131.94, 130.99 s | `PASS1_WAY_CLASSIFY` 16.78, 16.63 to 16.88, 16.85 s; `SMART_PASS2_CLASSIFY` 10.08, 10.06 to 8.64, 8.70 s |

A pass that tests a few way IDs against a set gains about 1.5 s, the
way-message re-parse a projection removes (2.3 % of tags-filter, under
1 % of smart). A pass that scans every way's refs against a node set
(smart and complete pass 1, multi-extract's way classify by the same
shape) gains nothing measurable: the ref lookups are its cost.

## inspect tags counts into a sharded table (bygg, 2026-11-02, against `a77eb163`)

Before the change, `inspect tags` built a `PrimitiveBlock` per blob, walked
every element resolving each tag's strings, and sent a per-blob
`HashMap<String, HashMap<String, u64>>` to the calling thread, which merged
it into the global map. At planet that single merging thread was the whole
wall: in the stored HEAD cell `4e2830c4` (default `-j`, 30 workers)
`classify_unordered_result_send_wait_ns` summed 1,856 s over an 88 s scan,
the workers parked about 70 % of the time, while the consumer's
`result_recv_wait_ns` was 0.3 s; an 18 s single-core tail followed
(reserving the result, walking and freeing about 10^8 strings). The element
walk itself was off the critical path.

The change reads each blob through a projection on the block validator,
counts (kind, key index, value index) per blob, resolves and hashes each
distinct pair once, and has every worker merge its blob into one table of
256 shards, a mutex each, shard by pair hash. A shard stores strings in
fixed 256 KiB chunks with keys interned per shard. Planet
`planet-20260223-with-indexdata`, default `-j`, interleaved `--bench 1`,
both arms built in the main tree (both unstored `--force`; the HEAD arm
by checking out the changed paths), arm order alternated:

| Pair | HEAD | Change |
|---|---:|---:|
| 1 (change first) | 98.91 s, peak anon 17.59 GB | 36.13 s, 10.82 GB |
| 2 (HEAD first) | 98.70 s, 17.59 GB | 36.25 s, 10.83 GB |

-63 % wall, -6.8 GB peak anon. After the 6.1 s schedule walk both arms
share, HEAD's scan and tail (no marker between them) took 92.67 and
92.46 s at 8.8 cores; the change's scan took 29.41 and 29.56 s at 23.4
cores, now CPU-bound, and its drain (`TAGSCOUNT_DRAIN`, which keeps no
rows under brokkr's `--min-count 999999999`) 0.67 and 0.59 s. The stored HEAD cell `4e2830c4`
earlier the same day ran 113.6 s because the host swapped (up to 4 GB,
1.3 M major faults) at its 16-17 GB peak; the change does not reach that
peak.

Table counters at the end of the scan: 187.4 M distinct pairs, 1.64 M
interned keys (107,879 distinct keys over 256 shards), 3.69 GB of strings
in 3.73 GB of chunks, 6.75 GB of hash tables. The first version stored the
key's bytes with every pair in one doubling vector per shard: 14.0 GB peak,
of which 1.86 GB was repeated key bytes and 2.7 GB the vectors' unfilled
reserve, at the same wall.

The figures above are a bare run counting every key=value pair, which is
what a bare `inspect tags` did until the change below; that workload is now
`inspect tags '*=*'`.

## inspect tags counts keys as osmium does (bygg, 2026-11-03, against `719ccf92`)

A bare `inspect tags` and an expression without `=` now count keys, one row
per key, as `osmium tags-count` does; an expression with `=` counts pairs.
brokkr's `inspect --tags` cells pass no expression, so the same brokkr
argv measures pair counting on cells up to `719ccf92` and key counting after
it. Planet `planet-20260223-with-indexdata`, default `-j`, both arms built in
the main tree.

Bare run (brokkr `inspect --tags --bench 1`, `--min-count 999999999`):

| Arm | Wall | Peak anon | Distinct rows |
|---|---:|---:|---:|
| HEAD, pairs (stored `97bd775a`) | 41.35 s | 10.26 GB | 187.4 M pairs |
| Change, keys (`--force`, unstored) | 29.97 s | 0.40 GB | 107,879 keys |

The key run tallies each blob per (kind, key index) instead of per (kind,
key index, value index) and holds 3.3 MB of table: its scan took 23.0 s at
21.7 cores against HEAD's 33.6 s at 22.3. The HEAD cell's scan read part of
the planet from disk (326 K major faults), so its wall is above the 36 s the
same code measured on 2026-11-02; the pair figures below are the matched
comparison.

Pair counting (HEAD bare against the change's `'*=*'`, the same 187.4 M
pairs; `brokkr run --release`, wall from `brokkr history`, order change,
HEAD, HEAD, change): HEAD 36.5 s and 36.5 s, change 36.9 s and 37.7 s. The
2 % median difference is inside the noise band; the change adds one
expression test per distinct pair per blob (`*=*`) and a wider pending
entry.

A bare run with the default `--min-count 1` now builds about 108 K rows at
planet instead of 187 M, so the row build and serial sort that were never
measured for the pair default are no longer a bare run's concern; `'*=*'`
with the default thresholds still builds every pair as a row.

