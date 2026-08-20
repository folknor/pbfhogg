# ADR-0013: Blob sidecar metadata is trusted by default and verified on demand

Date: 2026-08-20
Status: Accepted

## Decision

`BlobHeader.indexdata` (element kind, ID range, element count, spatial bbox)
and `BlobHeader.tagdata` (the blob's tag key set) stay **trusted by default**
on every read path. Verification is a separate, opt-in command rather than a
precondition of using the fields:

- `pbfhogg check --index` decompresses every blob, re-derives the metadata from
  the payload with `scan_block_ids` / `scan_block_tags`, and reports each
  disagreement. It is a selector on `check`, not part of the default `--ids` +
  `--refs` pair. (Those are the writer's *passthrough*-path scanners, so the
  check is independent of `BlockBuilder`, which derives its index incrementally,
  but shares code with the re-framing path - it catches divergence between
  metadata and payload, not a bug inside the scanner itself.)
- `--distrust-metadata` (on `tags-filter` and `inspect`) declines the hints for
  a single run, for callers who want a metadata-independent answer without
  first running a separate verification pass.
- `degrade --corrupt-indexdata`, `--corrupt-indexdata-kind` and
  `--corrupt-tagdata` generate the adversarial corpus both are tested against.

Three consequences follow, and are binding on new code:

1. **`check --index` gets no `--distrust-metadata` flag.** A verifier that
   consulted the thing it is verifying in order to decide what to look at would
   be worthless. It installs no `BlobFilter`, never calls `require_indexdata`,
   and decompresses every OSMData blob unconditionally - including blobs whose
   stored index claims a kind nobody asked about. Distrust is its only mode, so
   a flag for it would be a no-op that implied a choice existed.

2. **Anything sized from unverified metadata must survive that metadata being
   wrong.** `check`'s `id_alloc_bound` bounds the ID bitset by the input file's
   own length rather than by the claimed `max_id`; `verify_ids` and
   `check_refs` report an out-of-range ID as an input property rather than
   panicking; `repack` bounds its `BlockBuilder` pre-allocation the same way.
   A new consumer that allocates from `indexdata` inherits this obligation.

3. **A verifier must not inherit a reader's safe collapse.** Every read path in
   this crate turns metadata it cannot parse into "no metadata, decode the
   blob", which is the correct and conservative reading *for a reader*. A
   verifier that consumes the same parsed value cannot distinguish a file that
   claims nothing from a file that claims something unreadable, and so passes
   the most obviously broken input there is - which is exactly what
   `check --index` did on malformed `indexdata` in its first release, and on a
   tag index over a mixed-kind blob. Verification code must therefore read the
   *presence* of a field separately from its parsed value, and must treat
   "present but unusable" as a finding rather than as absence.

The trust boundary itself is documented in `CORRECTNESS.md`, alongside the
sortedness boundary it is distinct from.

## Why

The fields exist so a command can classify or reject a blob without
decompressing it, and that is where essentially all of this crate's performance
on large files comes from: `apply-changes` passes ~92 % of planet blobs through
as raw bytes on the strength of their declared ID ranges. Validating them on
every read would delete exactly the work they exist to avoid - the check costs
the full decode the hint was purchased to skip, so a always-verify design has
the same cost as having no metadata at all.

At the same time, the fields are claims a file makes about itself, and several
consumers act on them destructively: `cat --type` copies a blob through raw on
the declared kind (a mislabelled blob leaks off-kind elements or vanishes),
`renumber` assigns ID blocks from the declared count, and `tags-filter` /
`tags-count` skip whole blobs on the tag index - which turns an index omitting a
key its elements carry into a *wrong answer*, not a slow one.

Trusted-by-default plus verify-on-demand keeps the performance and makes the
claim checkable, which is the combination neither half provides alone. Before
`check --index` existed, a file with a lying hint was only detectable by running
two commands and comparing their output.

## Alternatives considered

- **Validate metadata on every read.** Rejected: it costs the full decode the
  metadata exists to skip, which is the entire optimization. A file is checked
  when someone asks whether it is trustworthy, not on every read of a file
  already known to be.

- **Make `--distrust-metadata` a global flag on every command.** Rejected for
  now, and partially self-defeating: the flag is only meaningful where a command
  makes a *decision* from the metadata, and for a verifier it is meaningless by
  construction (point 1 above). The two commands that have it are the two where
  the metadata changes the answer rather than the speed. Extending it further is
  a per-command judgement, not a blanket one - and every new indexdata-consulting
  path must be checked against the flag deliberately, which is how round 2's
  version of it shipped true on one path and false on the reference-expansion
  path.

- **Sign or checksum the sidecar fields.** Rejected: it does not solve the
  problem it appears to. A checksum proves the bytes were not corrupted in
  transit; it says nothing about whether the values describe the payload, which
  is what every consumer above actually depends on. A writer that computes the
  index wrongly would sign the wrong index. Only re-deriving from the payload
  answers the real question, and that is what `check --index` does.

- **Stop writing the fields on inputs we did not produce** (e.g. have `cat`
  decline to index third-party blobs). Rejected: indexdata is valid for any
  blob, the values are re-derived from the payload at write time regardless of
  provenance, and refusing would punish every downstream consumer to protect
  against a file that lies about itself - which the verifier now catches
  directly.
