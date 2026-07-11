# ADR-0006: Blob-count threshold dispatch for selective scans

Date: 2026-07-11
Status: Accepted

## Decision

`getparents` and `getid` include mode choose between the pread-only
`HeaderWalker` and a full-file scan from a bounded estimate of the
number of OSMData frames. The policy threshold is 150,000 blobs:
smaller inputs use the walker and larger inputs use the full scan.

The full-scan mechanism is per-command, matching what the reference
baselines actually measured. `getparents` decodes a large byte fraction
(every way and relation blob), so its scan is the pipelined reader with
classify parallelized across rayon batch workers. `getid` include
decodes almost nothing under its ID-range prescreen, so its scan is a
sequential `read_raw_frame` streaming loop; routing millions of small
frames through the pipeline's per-blob machinery measured 62 % slower
than plain sequential reads on the 8k-packed planet.

The estimate probes at most 1,000 leading frame headers. A probe that
reaches EOF reports an exact count. Otherwise it projects the OSMData
count from the sampled OSMData frame mean. Each dispatched command emits
the estimate; both arms of getid and the walker arm of getparents also
emit the exact count observed during the scan.

`removeid` remains walker-only: fusing its raw-frame passthrough into
the streaming scan is possible but unmeasured. Inputs forced without
indexdata also remain walker-only because the filtered full-scan regime
has not been measured. The indexdata gate probes the first data blob
only, so a file with an indexed head and an unindexed tail can still
dispatch to the full scan; both scan mechanisms decode unindexed blobs
conservatively, which keeps that choice correct, merely unpriced.

## Alternatives considered

- Streaming read-and-discard after a header walk: rejected because it reads
  bodies twice outside cache and the prior measurements did not price it.
- The pipelined reader as getid's scan arm: implemented first, refuted by
  the landing gates (53.9 s vs the 33.2 s sequential baseline at planet-8k;
  consumer-thread classify was a second refuted shape at 142.8 s for
  getparents). Per-blob pipeline overhead is the scaling hazard on
  high-blob-count encodings.
- A body-carrying sequential iterator for removeid: deferred. The getid
  streaming arm now is one; extending it to invert-mode passthrough is
  unmeasured, independent work.
- Applying this rule to every HeaderWalker consumer: rejected pending
  per-command measurements. `sort` pass 1 has a third, seek-skip mechanism.

## Consequences

The choice is internal: no CLI flag or environment variable exposes it.
Tests may force an arm through command-private entry points. Future changes
to the threshold require measurements at both low and high blob densities.
