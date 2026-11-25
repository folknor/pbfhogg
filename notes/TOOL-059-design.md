# TOOL-059 design: run fixtures for the remaining verify checks

Settled with codex (session 01a2131d-4fba-7772-b6a2-ea5588bdef23).

## Settled rulings

- One fixture module per check (`crates/verify/tests/<check>_fixture.rs`),
  each with its own root directory name, `main` dispatch arm (with
  `role != "driver"`), entry in the worker's `fixtures` table (bump the
  array length) and `#[test]` in `process.rs`. No shared generic fake: the
  checks' oracle contracts do not overlap; shared primitives belong in
  `fake_pbf.rs`.
- Fakes answer by their own independent computation from the operand's
  bytes, never through the verifier's model.
- Each fault fails exactly one named verdict, or is an error case asserting
  `Failed`, the error needle and no named failure. A clean case must prove
  every arm ran (an invocation log in the root, as the inspect-tags fixture
  keeps, or the absence of any coverage-gap or "not exercised" claim):
  an empty `failed` list alone does not.
- Faults that change a reference answer must stay cache-safe: derive the
  answer from operand bytes, or run the fixture fresh, and say which in
  reference/verify.md's cache paragraph.
- A fixture role whose fault matrix nears the 15 s hang bound is split into
  several bounded worker roles within the same link.

## Links

1. inspect-tags - landed. Small.
2. getparents - landed. One worker role sufficed (eight cases, about a
   second). Decisions made in the link: the fake fileinfo is local to the
   fixture and computed from the operand's OPL records (counts, ranges,
   order, repeats), leaving `fake_pbf::fileinfo` for link 3 to generalise;
   the clean case proves every arm by the check's returned claim being
   exactly none on sorted input and exactly `CLAIM_UNSORTED` on unsorted
   input, captured inside the judged closure, since `Judged` carries no
   claim; the error case is a pbfhogg exit only, because a negative-ID or
   negative-ref refusal is reference-side and would need its own input to
   stay cache-safe, which the plan did not ask for. Original plan: Clean graph with real witnesses for node-to-way,
   node-to-relation, way-to-relation, relation-to-relation and add-self,
   asserting no coverage gap. Fake answers pbfhogg and osmium getparents
   (both modes, osmium `-s`), fileinfo, `cat -t relation -f opl`, and OPL
   decodes of both tools' outputs under the comparison's options. Sorted
   header for the clean case; one unsorted case adds osmium `sort` of the
   input and of pbfhogg's outputs.
3. check-ids - large. `fake_pbf::fileinfo` hardcodes an ordered, unique
   file; generate counts and order and repeat facts from the operand
   records instead, so the inversion and repeat cases are expressible. The
   fake `check --ids --json` must read the verifier-appended empty claim
   from the bytes, never from the case filename. Faults targeted by case
   and by plain or `--full` mode, isolating exit and report agreement,
   verdict, counts and indexed.
4. merge-changes - large. The fake pbfhogg merges the real OSC inputs and
   the check's own generated fixtures, writes `.osc.gz` and plain `.osc`
   with equal decompressed content, and refuses a regression with exit 2,
   the exact needle and no output. It must follow input position where
   osmium's timestamp precedence disagrees, independently of the
   verifier's model. Fake osmium answers `cat -f opl` over OSC and
   `merge-changes -s`.
5. field-policies - large. `first_ids` reads blob payload IDs, so the
   input uses `write_blobs`. The fake OPL-to-PBF writer must strip inline
   way coordinates from the undeclared copy's records as well as dropping
   the header feature. Faults are reference-side (the check runs no
   pbfhogg) and end in `Error::Tool`; the clean case asserts no "not
   exercised" claim.
6. Green: `brokkr check --gate`, close TOOL-059, delete this note.
