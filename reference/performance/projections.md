# Projections

## check --refs and getid read through projections (bygg, 2026-11-01)

check `--refs` and `getid --add-referenced` read each blob through a
projection on the block validator (`crates/lib/src/read/project_ids.rs`, and getid's
include probe) instead of building a `PrimitiveBlock` and walking its
elements, as check `--ids` and getparents already did.

What the element walk cost (base, `25172fc3`, planet): check `--refs`
hotpath `ecfaf4c7` spent 291 core-s in `decompress_blob_raw` out of about
830 core-s in its two phases (nodes 17.7 s at 26.1 cores, ways 13.3 s at
27.5); the rest was validation, the offset tables, the element walk and
the set lookups. `getid --add-referenced` (`4c08d663`) ran pass 2 for
32.8 s at 29.0 cores over every node and way blob, walking every element
with its metadata to test one ID against a set; pass 1's pool ran 10.5 s
at 26.4 cores walking every way blob.

Planet `planet-20260223-with-indexdata`, interleaved `--bench 1`, base
`25172fc3` (stored, `--commit`) against the working tree (`--force`, not
stored). Spans are `--durations` START/END pairs.

| Command | Base wall | Projection wall | Main spans, base to projection | Peak anon |
|---|---:|---:|---|---|
| check `--refs` | 38.4, 38.5 s (`752a3925`, `913fd7fe`) | 31.6, 31.8 s | nodes 18.3-18.5 to 13.5-13.6 s, ways 13.2-13.3 to 11.6 s | 2.24 to 2.17 GB |
| `getid --add-referenced` | 49.9, 50.4 s (`232979ea`, `0b6748dd`) | 40.9, 41.0 s | pass 2 32.8-33.0 to 25.0-25.1 s, pass 1 17.1-17.4 to 15.8 s (6.5-6.8 s of it the schedule walk) | 467 to 460 MB |
| getid (include) | 113.7, 114.5 ms (`29f5741e`, `515b464c`) | 119.4, 119.7 ms | `GETID_SCAN` 99-100 to 101 ms; the rest is `PUBLISH` | |

check `--refs` -18 % wall, `getid --add-referenced` -18 %. Both nodes
phases are now read-bound: check `--refs` nodes ran at 20.6 cores
against 25.2 (58 GB in 13.6 s), getid pass 2 at 18.2 cores against 29.1
(67 GB in 25 s). The include walk decodes only the handful of blobs whose
indexdata range holds a requested ID, so its probe changes nothing
measurable; its few-millisecond gap sits in output publication, outside
the changed code. The first include base cell (`31c13dc7`, 6.9 s) read
cold headers after the add-referenced cells and is not a pair.
`getid --invert` was not measured: it builds every block it decodes as
before, the only change being that construction and the kind-claim
refusal are one call instead of two.

## check --ids and getparents read through projections (bygg, 2026-11-01)

Both commands validate each blob through a `GroupProjection` on the block
validator (`BlockBuffer::project`) instead of building a `PrimitiveBlock`
and walking its elements. Block construction runs the same call with an
empty projection, so the refusals are the same code.

What the element walk cost (hotpath, HEAD `96243688`, planet, UUIDs
`037ca3b3` and `ea1f782f`): on check `--ids` the workers spent about
297 core-s in `decompress_blob_raw` and 157 core-s in construction
(validation and string-table UTF-8; the offset tables were 1.9 s of it)
out of about 737 core-s in the scan phases. Most of the rest was the
element walk, which for dense nodes stepped `id`, `lat`, `lon` and
`keys_vals` per node and for ways re-parsed every message the validator
had already parsed, all to read the ID. getparents spent 110 core-s
decompressing and 124 constructing out of about 350, the rest
re-parsing every way and relation to test refs and members.

Planet `planet-20260223-with-indexdata`, interleaved `--bench 1`, base
`96243688` (stored) against the working tree (`--force`, not stored).
Spans are `--durations` START/END pairs.

| Command | Base wall | Projection wall | Main spans, base to projection | Peak anon |
|---|---:|---:|---|---|
| check `--ids` | 34.9, 35.1, 35.1, 35.0 s (`c55ab749`, `f9b9bed4`, `bb0b6535`, `b0532d7b`) | 29.0, 29.0 s | nodes 18.0-18.3 to 13.7 s, ways 10.5-10.7 to 9.2 s | 556 to 542 MB |
| check `--ids --full` | 36.6, 36.7, 36.8, 36.9 s (`8045d024`, `10e7598f`, `932dacbf`, `4c6515ae`) | 29.8, 29.7 s | nodes 19.7-19.9 to 14.2-14.3 s, ways 10.8-10.9 to 9.4 s | 2.35 to 2.36 GB |
| getparents, walker arm | 18.96, 18.93, 18.94 s (`db9ddfb3`, `342927f3`, `3abd7aa8`) | 16.64, 16.70, 16.71 s | `GETPARENTS_DECODE` 13.2 to 11.0 s | 288 to 277 MB |
| getparents, FullScan arm (europe) | 11.78, 11.77 s (`a694cb57`, `35bba63f`) | 10.92, 10.89 s | `GETPARENTS_DECODE` 11.7 to 10.8 s | |

check `--ids` -17 % wall (`--full` -19 %), getparents -12 % on planet
and -7 % on europe's FullScan arm. The 5.7-6.1 s schedule walk ahead of
both is untouched. The check `--ids` nodes phase is now read-bound
(about 20 cores against 25 before, 58 GB in 13.7 s).

A first version copied each blob's IDs into a per-worker vector. Planet
node blobs hold a few hundred thousand nodes, so that vector kept a few
MB per worker for the whole phase: the nodes phase peaked at 637 MB
against 556 MB. Recording where each dense `id` array lies, and decoding
it once after acceptance, removed it at the same wall.

Construction itself moved onto the same path (validate, then append the
offset tables, rather than the reverse). check `--refs`, which still
builds blocks, kept its work phases at the same length: nodes 18.0 and
18.6 s base against 18.5 s, ways 13.5 s on both (`daa8ce1b`,
`7cb64fb5`).

## check --index and cat --reindex on a frame pool (bygg, 2026-11-08, against `74be4015`)

Both were one thread: read a frame, decompress, scan (`check --index`
scanned twice, IDs then tags). Now the frame walk stays sequential on the
calling thread (a stream is still an input) and feeds an ordered worker pool
(`read::frame_pool`, `workers + 2` frames in flight) that decompresses and
scans in one walk; results are folded in file order. Planet
`planet-20260223-reindexed.osm.pbf`:

| Command | Base `74be4015` | Working tree (`--force` / `brokkr run`, not stored) |
|---|---:|---:|
| `check-index --bench 1` | 494.2 s (`3965701e`), 1.0 cores, peak anon 15 MB | 28.9 s, 15.5 cores, peak anon 334 MB, read 3.1 GB/s |
| `cat --reindex` of `planet-20260223-with-indexdata` | 444 s (`brokkr history` 62313, at `3f8ed0ec`, dirty) | about 44 s (`brokkr history` 62538, 1.8 m minus a 64 s build) |

The base was CPU-bound on one core; the pool is near the drive's read rate.
The reindex output is byte-identical (`cmp`) to the sequential run's. One
cell per arm: the gap is 10 to 17 times, far outside any drift band.

