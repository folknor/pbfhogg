# time-filter

Current baselines first; History after. The snapshot path's older planet
cells (claim checks, exact timestamps) are in
[cross-command](cross-command.md), and its memory cells in
[memory](memory.md).

## History path: workers select and encode, the stitcher joins seams (bygg, 2026-11-25, against `6e6b6b5c`)

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
3 cores, its consumer thread the bound; the variant is read-bound. The file
is mostly one version per object (the two snapshots share most objects
unchanged), so it measures the serial consumer, not version fan-out;
a deeper history file would favour the variant further, since the old path
cloned every eligible version.

## Snapshot path after the shared encoder (bygg, 2026-11-25, against `6e6b6b5c`)

The same change moves the snapshot path's per-element encoding into a
helper both paths share, with no change in what it does.
`time-snapshot --dataset planet`, `--iterations 1`, interleaved:

| Order | Arm | Wall | Run |
|---|---|---|---|
| 5 | baseline | 2m04s | `9e5b3736` |
| 6 | variant | 2m03s | `5b92ebd3` |
| 7 | baseline | 2m04s | `24e5270d` |
| 8 | variant | 2m03s | `7df85917` |

Within noise: no cost.

## History

Nothing earlier: no history-path cell was recorded before the `history2`
snapshot existed (every earlier time-filter cell ran a non-history input,
`timefilter_is_history_path` 0).
