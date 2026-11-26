# time-filter

## Projected selection with winner positions (bygg, 2026-11-26, `1361533e` against `da1f1a55`)

Selection reads only ID, version, timestamp and visibility from a
projection, and the history encoder gets winner positions directly.
Planet `time-snapshot --snapshot history2 --variant raw`, `--iterations
1` interleaved: `1361533e` 2m41s `f750c3c1`, 2m46s `f6d3e5b7`;
`da1f1a55` 2m30s `d5a86eac`, 2m35s `0112d9c6`. About 6 % faster.

Current baselines first; History after. The snapshot path's older planet
cells (claim checks, exact timestamps) are in
[cross-command](cross-command.md), and its memory cells in
[memory](memory.md).

## History path: the alternative ending forks the builder (bygg, 2026-11-25, against `095422c7`)

A blob's last group, when its candidate is visible, needs two endings: the
open block without and with the candidate. The worker used to build the
second by re-encoding the open block's winners from an empty builder and
then appending the candidate. Input blobs hold about 8000 elements, so the
open block is usually every winner the blob has, and most blobs were
encoded twice (the hotpath cell `d419e758` at `095422c7` shows 2.64 M
`take_owned` calls for 1.47 M input blobs and 1.21 M written blocks). The
worker now clones the builder just before the candidate and appends it to
the clone, so the prefix is encoded once.

Same input and command as the first history-path cells in the History
section; baseline clean `095422c7` via `--commit`,
variant the change uncommitted:

| Order | Arm | Wall | Run |
|---|---|---|---|
| 1 | baseline | 3m07s | `0222ff0d` |
| 2 | variant | 2m39s | `699780a4` |
| 3 | baseline | 3m12s | `1d34555c` |
| 4 | variant | 2m38s | `a8acf7da` |

About 16 % faster, both pairs agreeing; same summary on every arm. The
history phase (`TIMEFILTER_HISTORY` span, pair 2) went from 191.5 s at
26.2 cores to 158.3 s at 27.2 cores with the same 91.9 GB read; peak anon
rose from 460 MB to 585 MB, the clones. The path is CPU-bound, not
read-bound: in `d419e758` the worker's `select_blob` (selection plus
encoding) took about 3170 thread-seconds and blob compression
(`frame_blob_into`) about 2390, against 387 for decompression. The
snapshot path does not use the fork and was not re-measured.

## Snapshot path after the shared encoder (bygg, 2026-11-25, against `6e6b6b5c`)

The history-path rewrite in the History section moved the snapshot path's
per-element encoding into a helper both paths share, with no change in what
it does. `time-snapshot --dataset planet`, `--iterations 1`, interleaved:

| Order | Arm | Wall | Run |
|---|---|---|---|
| 5 | baseline | 2m04s | `9e5b3736` |
| 6 | variant | 2m03s | `5b92ebd3` |
| 7 | baseline | 2m04s | `24e5270d` |
| 8 | variant | 2m03s | `7df85917` |

Within noise: no cost.

## History

### History path: workers select and encode, the stitcher joins seams (bygg, 2026-11-25, against `6e6b6b5c`)

The history path used to decode in parallel and then, on the calling
thread, check order, clone every version at or before the cutoff into an
owned element and re-encode the winners. It now selects and encodes on the
decode workers (`for_each_fused_blob`), and the ordered consumer only joins
objects across blob seams and writes pre-encoded blocks.

Input: bygg's `planet` snapshot `history2`, a two-version history file
(`osmium merge` of the 2026-02-23 and 2026-04-11 planets, `.osh.pbf`,
uncompressed, 96.6 GB, no indexdata). `time-snapshot --dataset planet
--snapshot history2 --variant raw`, `--iterations 1`, interleaved baseline
(clean `6e6b6b5c` via `--commit HEAD`) and variant (the change,
uncommitted):

| Order | Arm | Wall | Run |
|---|---|---|---|
| 1 | baseline | 20m11s | `b5a09d40` |
| 2 | variant | 3m05s | `80f9c208` |
| 3 | baseline | 20m07s | `8a4c7f46` |
| 4 | variant | 3m09s | `1730eace` |

About 6.5 times faster, both pairs agreeing. Both arms print the same
summary (11,769,154,930 versions scanned, 9,306,266,603 written,
2,430,664,867 without a version at the cutoff). The baseline ran on about
3 cores, its consumer thread the bound; the variant ran on about 27 cores
(this document once called it read-bound; the profile above says CPU). The file
is mostly one version per object (the two snapshots share most objects
unchanged), so it measures the serial consumer, not version fan-out;
a deeper history file would favour the variant further, since the old path
cloned every eligible version.

### Before

No history-path cell was recorded before the `history2`
snapshot existed (every earlier time-filter cell ran a non-history input,
`timefilter_is_history_path` 0).
