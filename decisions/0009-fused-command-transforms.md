# ADR-0009: Fuse full-scan command transforms in decode workers

Date: 2026-07-12
Status: Accepted

## Decision

Full-scan command transforms run in the decode workers. `getid` pass 2 with
`--add-referenced`, getparents FullScan, single-pass tags-filter, and the
decode-all add-locations-to-ways fallback transform each decoded block before
it crosses to their ordered writer. The decoded admission budget charges the
decoded input size, not transformed output size: transforms such as
add-locations-to-ways can expand output, so the bound remains the configured
decoded-block count times a bounded input payload. The former 64-block command
materialization, its second rayon dispatch, the command-batch byte override,
and the shared batch helper are deleted. See
`reference/performance-history.md` for the measured basis: the retained
high-blob-count cells improved getid by 7.68%, getparents by 6.51%, and
tags-filter by 6.97%; getid primary's pass-2 peak RSS fell from 1.18 GB to
596 MB.

## Alternatives considered

- Retain an environment gate and both command shapes: the A/B run proved
  byte-identical output, and a gate would leave dead operational surface and
  duplicate maintenance with no remaining experiment.
- Retain command-side 64-block batches with a byte budget: the materialization
  and second dispatch are the removed cost; `PBFHOGG_CMD_BATCH_BYTES` regressed
  getid-8k by 3.49% and has no remaining consumer.
- Move all read users onto one generic fused engine: plain pipelined consumers
  have different needs, so that broader unification remains separate from the
  command-transform policy.
