# ADR-0013: Blob sidecar metadata is trusted by default and verified on demand

Date: 2026-08-20
Amended: 2026-10-27 - consequence 4 covers the range and count of a blob
`apply-changes` or the `sort` / `cat --dedupe` sweep decodes, not only the
kind.
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

Four consequences follow, and are binding on new code (the fourth added on
2026-10-27):

1. **`check --index` gets no `--distrust-metadata` flag.** A verifier that
   consulted the thing it is verifying in order to decide what to look at would
   be worthless. It installs no `BlobFilter`, never calls `require_indexdata`,
   and decompresses every OSMData blob unconditionally - including blobs whose
   stored index claims a kind nobody asked about. Distrust is its only mode, so
   a flag for it would be a no-op that implied a choice existed.

2. **Anything sized from unverified metadata must survive that metadata being
   wrong.** `check`'s `id_alloc_bound` sizes the ID bitset from the claimed
   `max_id` bounded by the input file's own length and floored at the
   live-OSM ID range, so a claim can neither request an unbounded allocation
   nor shrink it below that range; `verify_ids --full` and `check_refs`
   insert decoded positive IDs above the allocation after the parallel
   pass, and report only a negative ID as an input property; `repack`
   bounds its `BlockBuilder` pre-allocation by the file size.
   A new consumer that allocates from `indexdata` inherits this obligation.
   `renumber` bounds its initial node bitmap by the file size and inserts
   decoded IDs beyond that allocation after the parallel pass, preserving
   legitimately sparse IDs without trusting the sizing claim. The geocode-index
   builder caps its node-set pre-allocation by the same file-size bound; it
   also treats the largest claimed node `max_id` as a functional bound (refs
   above it are dropped as dangling), so it checks every decoded node against
   it and refuses the file when one exceeds it, rather than tracking it.

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

4. **A run that decodes a blob anyway checks the claim it relied on.** The
   cost argument below is about blobs a run skips or copies on their
   claims. A blob a phase scheduled on its claimed kind and then decodes can
   be compared with that claim for free: every decode already visits every
   element, and the block records the kinds it holds. Where the claim chose
   the blob's phase, a decoded element of another kind is a refusal
   (`ErrorKind::IndexdataMismatch` with a `KindContradiction`), because the
   alternative is the phase silently dropping it and the phase that needed
   it never seeing the blob. In `check`, which answers "is this file right?",
   the same finding stops the check and fails the file (exit 1) instead of
   refusing it. This adds no decode and no per-element pass; phases adopt it
   one by one (`KindClaims::Refuse`), and the ones that have not yet still
   believe their claims unchecked. The same holds for the range and count of
   a blob a run places by its range and decodes anyway (`apply-changes` and
   the `sort` / `cat --dedupe` sweep): the element loop the run already makes
   tallies the IDs, and a claim that is not exact for them is the same
   refusal without a `KindContradiction`
   ([indexdata-trust.md](../reference/indexdata-trust.md), "Range, count and
   order claims refused at decode"). Neither is a substitute for
   `check --index`: a blob no phase decodes is still believed.

The trust model this decision sets - which claims are believed, what checks
them in passing, what verifies them and with which gaps, and what a false one
costs per command - is stated in `reference/indexdata-trust.md`, which also
covers the header's `Sort.Type_then_ID` claim (believed the same way, and
verified by `check --ids` rather than `check --index`).

## Why

The fields exist so a command can classify or reject a blob without
decompressing it, and that is where essentially all of this crate's performance
on large files comes from: `apply-changes` passes ~92 % of planet blobs through
as raw bytes on the strength of their declared ID ranges. Validating them on
every read would delete exactly the work they exist to avoid - for a blob
skipped or copied on its claim, the check costs the full decode the hint was
purchased to skip, so an always-verify design has the same cost as having no
metadata at all. That argument does not reach a blob the run decodes anyway:
there the comparison costs nothing, which is why consequence 4 makes it.

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

- **Validate metadata on every read.** Rejected: for the blobs a run skips or
  copies, it costs the full decode the metadata exists to skip, which is the
  entire optimization. A file is checked when someone asks whether it is
  trustworthy, not on every read of a file already known to be. Blobs a run
  decodes anyway are the exception consequence 4 takes.

- **Make `--distrust-metadata` a global flag on every command.** Rejected for
  now, and partially self-defeating: the flag is only meaningful where a command
  makes a *decision* from the metadata, and for a verifier it is meaningless by
  construction (point 1 above). The metadata changes the answer, not just the
  speed, in many commands (the consumers above among them); the two that have
  the flag are the two where it was asked for. Extending it further is
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
  single-kind, non-empty blob (`cat` re-encodes a block no single index can
  describe into ones that can be indexed), the values `cat` writes are derived
  from the payload regardless of provenance (a blob that already carries
  indexdata is passed through as it is, so plain `cat` does not repair a
  false one; `cat --reindex` does), and refusing would punish every downstream consumer to protect
  against a file that lies about itself - which the verifier now catches
  directly.
