# Repack Degrade

### repack and degrade encode each blob on its worker (bygg, 2026-11-03, against `1f93e674`)

**Attribution** (uncommitted diagnostic on `1f93e674`, sidecar session
`4ca7a7a9`, 163.7 s, peak 1,734 MB): the repack merge thread emitted
`mallinfo2` beside the bytes held in worker results not yet consumed (frame
capacities, and a heap estimate of the owned elements with each allocation
rounded to glibc's chunk size), every 4 relation, 128 way and 256 node
results.

- Planet relation blobs hold about 31k relations each (452 blobs, 14.1 M
  relations), so at the default cap of 8000 each worker result was about
  3.4 framed full blocks plus about 3,700 relations shipped as owned
  `OwnedRelation`s. The merge thread re-encoded every one of those
  remainders, and because the ordering guard flushed its builder at every
  blob, framed each on its own, synchronously. The phase ran 3.9 s at 8.3
  to 8.8 cores against about 29 in the node and way phases.
- At the relation phase's start the live heap (`uordblks`) was 1.8 MB, with
  348 MB free in the arenas. 0.3 s later 77 results were in flight, holding
  707 MB of owned remainders and 134 MB of frames; live heap 1.42 GB, arena
  1.71 GB. Then 50 to 60 results, 250 to 500 MB of remainders and about
  1.0 GB live to the phase's end. So the relation-phase peak was live data
  queued behind the merge thread, not retention.
- The way phase is worker-bound (no send wait), about 0.6 GB live in a
  1.14 GB arena. The node phase's decode buffers are mmapped chunks
  (`hblkhd` 435 to 489 MB, arena about 216 MB); they are freed when its pools
  end, which raises glibc's mmap threshold, and the way phase's buffers then
  come from the arenas. Not changed here directly, but see the way-phase
  column below.

**The change**: a worker encodes and frames every element of a repack blob
holding at least the cap, the remainder as its own block, and ships only
frames; a blob below the cap still ships owned elements, which coalesce in
the merge thread's builder. degrade outside an unsort mode, whose boundary
flush already made every remainder its own block, encodes every blob on its
worker. The merge thread's blocks go to the pipelined writer, which frames
them on its executor in sequence order, so the synchronous batch framing
(and its `FRAME_BATCH` tunable) is gone. degrade's output is unchanged;
repack's differs only where a blob of at least the cap is followed by blobs
below it, whose elements no longer join its remainder (planet: no coalesce
in any phase, 1,453,673 blobs either way). degrade's worker outside an
unsort mode no longer walks its blob once to count it before encoding, and
repack's takes the count from the block's validated element count when the
block holds no other kind (every indexed blob), walking only an unindexed
one.

Planet (`planet-20260223-with-indexdata`), `--bench 1`, HEAD cells stored
from brokkr's worktree, change cells unstored `--force` cells of the tree on
`1f93e674` (sidecar sessions named); run order repack HEAD, change, degrade
HEAD, change, repack HEAD, change, degrade HEAD, change, with no trim
between (two short denmark cells ran before the second degrade HEAD cell):

| arm | walls (s) | peak anon (MB) | relation phase | way phase peak (MB) |
|---|---|---|---|---|
| repack `1f93e674` (`85aad3a2`, `bbb4d0b6`) | 163.2, 164.6 | 1,718, 1,712 | 3.9-4.1 s, 8.3-8.4 cores | 956, 957 |
| repack, change (`77386e44`, `736f2416`) | 161.3, 160.2 | 777, 781 | 1.4 s, 27.8 cores | 777, 781 |
| degrade `--strip-locations` `1f93e674` (`d89acb69`, `1ebee79b`) | 164.4, 164.0 | 1,728, 1,707 | 3.9 s, 8.4-8.7 cores | 962, 958 |
| degrade `--strip-locations`, change (`4a458795`, `db35770b`) | 152.3, 152.7 | 791, 789 | 1.4 s, 29.3 cores | 791, 789 |

degrade's node phase ran 92.1 and 92.7 s against 99.5 s in both HEAD cells,
the counting walk it no longer makes over 32,835 blobs of about 318k nodes;
its relation phase saves the other 2.5 s. The two repack change cells above
still made that walk; a third, after repack took the validated count
(`776bcb32`, run last, after degrade's second change cell), ran 148.3 s at
800 MB, node phase 88.7 s against 99.3 to 100.1 s in every other repack
cell, 1,453,673 blobs.

Output on denmark (`denmark-20260220-seq4704-with-indexdata`), the change's
binary against `1f93e674`'s: byte-identical for degrade `--strip-locations`,
for repack at the default cap and for repack of the 64k-per-blob snapshot at
the default cap. Repack at `--elements-per-blob 3000` differs where the
rule changed: 22,184 blocks against 22,183 (one more way block, where the
last way blob's elements no longer join the remainder before them), the
same 59,152,282 elements by `pbfhogg diff`, strict type-then-ID order.

The way phase peaks lower too, although its work is the same in both
builds (its blobs hold exact multiples of the cap, so neither has a
remainder there). In the HEAD cells the merge thread framed the node
phase's one remainder (its last blob's) through the executor, which started
its 32 threads before the way phase (65 threads at peak there, against 33
in the change cells, which start none at planet). Those idle threads held
the node workers' freed arena memory; see the next section.

