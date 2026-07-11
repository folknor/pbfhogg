# ADR-0006: Blob-count threshold dispatch for selective scans

Date: 2026-07-11
Status: Accepted

## Decision

`getparents` and `getid` include mode choose between the pread-only
`HeaderWalker` and the pipelined reader from a bounded estimate of the
number of OSMData frames. The policy threshold is 150,000 blobs:
smaller inputs use the walker and larger inputs use the pipelined reader.

The estimate probes at most 1,000 leading frame headers. A probe that
reaches EOF reports an exact count. Otherwise it projects the OSMData
count from the sampled OSMData frame mean. Each dispatched command emits
the estimate; walker paths also emit the count observed while walking.

`removeid` remains walker-only because its raw-frame passthrough would be
lost by decode and re-encode. Inputs forced without indexdata also remain
walker-only because the filtered pipelined regime has not been measured.
The indexdata gate probes the first data blob only, so a file with an
indexed head and an unindexed tail can still dispatch to the pipelined
arm; its blob filter passes unindexed blobs through to a full decode,
which keeps that choice correct, merely unpriced.

## Alternatives considered

- Streaming read-and-discard after a header walk: rejected because it reads
  bodies twice outside cache and the prior measurements did not price it.
- A body-carrying sequential iterator for removeid: deferred. It could fuse
  raw passthrough with sequential I/O, but is independent work.
- Applying this rule to every HeaderWalker consumer: rejected pending
  per-command measurements. `sort` pass 1 has a third, seek-skip mechanism.

## Consequences

The choice is internal: no CLI flag or environment variable exposes it.
Tests may force an arm through command-private entry points. Future changes
to the threshold require measurements at both low and high blob densities.
