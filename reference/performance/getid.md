# Getid

## Signed getid and getparents queries (bygg, 2026-11-18, against `f88235d2`)

getid stopped refusing negative text IDs and stopped dropping negative
`-I` IDs, way refs, relation members and relation edges (the guards went;
every insert is a plain signed `IdSet::insert`). Germany,
`id-select --add-referenced yes`, `--iterations 1` cells in A B B A B A
A B order, baseline `--commit f88235d2`, variant the uncommitted change.
Baseline 2737.6 (first cell, after its build), 2854.7, 2891.3, 2868.7 ms,
median 2861.7; variant 2898.6, 2930.4, 2899.1, 2881.9 ms, median 2898.9:
+1.3 %, noise. The phase split of the second pair puts the difference in
`GETID_PASS2`, whose code the change does not touch; the relation and way
phases, where the guards were, are level. Verdict: neutral. No
`parent-select` cell: getparents' scan, probe and writer did not change,
only the shared text parser, which runs once per query ID.

## getid's full scan skips rejected payloads (bygg, 2026-11-10, `04bacd00` against `93be495d`)

The full-scan arm prescreens each blob's indexdata from its header and
skips a rejected payload instead of copying it. Planet `8k` snapshot,
where dispatch takes the full scan, `--iterations 1` interleaved pairs,
both arms `--commit`: 28.2 s (`afbfa6ad`) against 28.1 s (`3dcce726`),
27.9 s (`01b438b4`) against 26.7 s (`168f1b05`). `GETID_SCAN` reads
92.5 GB on both arms at 0.2 cores: the scan is bound on disk read, and a
skipped payload is still read through the buffered reader, so the change
saves only the copy. Within noise to slightly faster; kept, as it does
less work. The low-density control on the walker arm was not run, since
that arm's code did not change.

## getid --add-referenced follows relation members (bygg, 2026-11-03, against `7bdd2813`)

`getid --add-referenced` gained osmium's relation closure: one split
header walk serves a relation read (`GETID_RELATIONS`: the requested
relations' members and every relation-in-relation edge), a second
relation read only when the closure reaches a relation not requested, and
the way read (`GETID_WAYS`) over the requested and pulled-in ways. The
bench ID set requests three relations, so the variant cell does more work
than the base: its relations pull in 9 more ways and about 400 more nodes
(`getid_dep_way_ids` 12, `getid_dep_node_ids` 472 against 76), and no
relation in a relation is reached, so the second read does not run.

Planet `planet-20260223-with-indexdata`, `--bench 1`, base `7bdd2813`
(stored, `4f9572e9`) against the working tree (`--force`, not stored):

| Cell | Wall | `GETID_RELATIONS` | `GETID_WAYS` (base: pass 1 after the walk) | Pass 2 | Peak anon |
|---|---:|---:|---:|---:|---:|
| base | 40.9 s | | 8.8 s | 25.2 s | 402 MB |
| edges in a `Vec`, dropped after the closure | 41.1, 41.2 s | 0.29, 0.30 s | 8.7, 8.6 s | 25.2, 25.4 s | 572, 591 MB |
| edges in their own anonymous mapping | 41.2 s | 0.31 s | 8.7 s | 25.3 s | 412 MB |
| control: no relation requested | 40.9 s | | 8.8 s | 25.3 s | 391 MB |

The relation read finds about 1.04 million relation-in-relation edges
(`getid_relation_edges`, 16 bytes each). Holding them in a `Vec` and
freeing it after the closure cost 170-190 MB of peak memory, none of it
live: `mallinfo2` after the way read showed a 541 MB arena with 0.2 MB in
use, against 35 MB in the control. Freeing a large mmapped block raises
glibc's dynamic mmap threshold, so the way read's and pass 2's decompress
buffers came from the arenas instead of mmap and stayed resident once
freed; leaking the `Vec` instead brought the arena back to 128 MB and the
peak to 445 MB. The store now lives in its own anonymous mapping
(`EdgeStore`), grown by remapping and unmapped after the closure, so it
never passes through `free`: wall and peak are within noise of the base.

