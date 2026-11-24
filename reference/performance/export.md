# export

`pbfhogg export` to GeoJSON, measured with the `geojson-export` script bench
(output to an iteration-owned file on scratch). All cells below: host bygg
(16c/32t, 30 GB, one NVMe for data, scratch and target), default
`geojsonseq`, every kind, no metadata, `--iterations 1`.

## Current baselines

Workers format, ordered consumer writes (variant measured uncommitted against
`7ddcd4b6`; it landed as the commit that adds this file).

| Dataset (variant) | Baseline `7ddcd4b6` | Variant | Output | Peak RSS |
|-------------------|---------------------|---------|--------|----------|
| denmark (altw-keep-untagged) | 20.7 s (`1e43b678`) | 1.6 s (8 KiB output buffer) | 4.4 GB | - |
| germany (altw-keep-untagged) | 187 s (`38195b60`) | 10.8 s (8 KiB buffer, `b7b8f65d`), 8.7 s (256 KiB buffer, `7fbff77d`) | 30.9 GB | 269 MB |
| planet (altw) | over 21 min, stopped (see below) | 300 s (`826dc283`) | 523 GB | 791 MB |

The planet baseline cell was stopped by hand after 21 min 25 s, when it had
read 37.4 GB of the 88 GB input on one core (`brokkr lock` reading), so
its full wall is not recorded; at that rate it is about 50 minutes, and 21
minutes is a hard lower bound, over four times the variant.

Phase split of the planet variant (`brokkr sidecar 826dc283`): 6.5 cores on
average, 523 GB written at 1.7 GB/s mean against a peak Dirty of 5.6 GB and
a 20 ms mean write wait, so the run is bound by the drive's sustained write
rate, not by formatting; on germany, whose 31 GB output fits the drive's
fast write window, the same build averages 18 cores at 3.5 GB/s. No planet
block reached the worker's chunk cap (`export_oversize_blocks` 0).

The output buffer: the library's staged text output (`OutputWriter`) wrote
through an 8 KiB `BufWriter`, 3.9 million write calls for germany's 31 GB on
the one consumer thread. 256 KiB (`IO_BUF_BYTES`) took germany from 10.8 s
to 8.7 s.

Output bytes are identical to the baseline's (`cmp`, denmark, sequence with
`--metadata`, and a FeatureCollection of `--type way` with an expression).

## History

None before this file: export had no bench until `geojson-export`.
