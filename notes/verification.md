# Verification findings

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.
```

Findings are as reported by the hunters and have not been verified. Hunters read
source; costs are derived from the code unless an entry says measured. "Reported
by" records which scopes raised a finding and is a fact about the reports, not a
verdict on the code. Entries are in no order of importance.
## VER-130 - The descriptor gate's charges are asserted, not observed

Residue, still asserted: stage 3 and stage 4 at their most simultaneous descriptors
(they start independently, so a completed witness run need not overlap them; forcing
it would need a rendezvous like stage 1's); the io_uring arm of the witness on any
host whose memlock limit refuses the writer; the light reserve `TEST_LIGHT_FD_RESERVE`
(a count in the shared binary is process-wide and light use scales with runner
threads, so it cannot be checked without failing on large hosts); a ticket's units
versus its simultaneous runs; own `host::FdPool` tests holding a ticket by convention.

## VER-135 - sort's required LocationsOnWays drop has no real-data witness

sort's declaration verdict is exact (`checks::sort::sort_way_locations` predicts the
blob planner, decoding every blob pass 1 would scan), but every drop branch (overlap
component, internally unsorted or restructured way blob on a `LocationsOnWays` input)
is witnessed by unit tests only: no configured dataset both declares
`LocationsOnWays` and makes sort rewrite a way blob. No producer makes one:
re-encoding `degrade` drops the declaration, and `cat A A` keeps it but duplicates
every identity, which the strict element comparison and order check refuse. A
witness needs overlapping way blobs with disjoint identities that keep inline
coordinates and true indexdata.

## VER-018 - Backend witnesses prove setup, not every transfer

Library and CLI cells that run a command with a direct option now assert a
successful `O_DIRECT` open (`direct_io_*_opens`) on their own invocation
(reference/testing.md "Platform-dependent tests"). Residue: the count proves
setup, not that each transfer used the direct descriptor, and a cell with several
direct paths (header walk and payload reader, two inputs) is satisfied by any one;
only the fault plan's open counts attribute opens, and they do not record the
transport. Unit cells gated by `test_support::direct_reads_supported` other than
sort and dedupe (copy_range, frame_conformance_tests, getid, header_walker's
direct-transport leg, dedupe's pass-2 helper) assert no direct count. io_uring
stays outside the gate's requirement, so on a default-memlock host its cells skip
and only the opt-in `full_registered_pool_instrument` proves the pool. Refusal
predicates are the only route to a backend skip by convention, not by any audit.

## VER-122 - Some checks are not shown to pass the field policy their claim needs

Reported by bench-verify.

The generated fixtures' fake osmium now decodes by the requested OPL options
(`fake_pbf::project`), and one-field-class faults witness the policy of extract,
multi-extract, cat, tags-filter, repack, degrade, apply-changes (metadata in both modes,
coordinates left out in locations mode) and the derive-changes roundtrip (metadata ignored).
Not yet witnessed:

- Text diff: it classifies metadata-only differences as common (`equal_listed`) and reads
  versions in its verbose headers, so its witness is a version change on a content-modified
  object with a wrong listed version, plus a metadata-only change that must pass.
- Renumber (`compare_paired` attribution parses `N`/`M` lists), add-locations engine parity
  (the per-engine osmium comparisons and digests can supply a failure, so the parity verdict
  itself must be asserted), and the both-declared coordinate branch of `Policy::between` on
  sort, cat, getid invert and cat-dedupe: the header-only fake PBFs cannot keep
  LocationsOnWays where a check requires indexed data blobs (cat's `first_data_blob_indexed`).
- The witnesses assert a final `Failed`, and multi-arm checks fault every arm at once, so one
  correct arm can mask another arm on the wrong policy; asserting the named comparison's
  diagnostic per arm would close that.

## VER-133 - Geocode admin and antimeridian paths the real-data check does not reach

`pbfhogg-verify geocode` judges all four answer classes; on the configured datasets two
paths stay out of reach:

- A relation whose outer or inner ways do not assemble uniquely (an endpoint shared by three
  or more member ends) leaves its whole member box unjudged at its level. On germany that is
  r16239 and r17511 at level 2 and r52343 at level 4 (southern Bavaria, the run prints them).
  Localising to the branching group's box is sound for containment, but the builder refuses
  the whole relation when its ring-search budget runs out, which only a branching group can
  cause. A conservative placement bound over the group's member count (about 531,804
  placements for seven members, under the 2^20 extra allowance) could certify small groups
  safe and localise them; whether these relations' groups are small enough is not yet known.
- The model refuses data near the antimeridian; unwrapping longitude would let an
  antimeridian dataset (the Fiji entry in notes/todo.md) be judged. No eligible dataset
  crosses it (the verify dataset list is denmark and germany).

Same-level competition is judged only by chance (germany judges one or two queries among
several containing areas); witnesses aimed at overlaps would need an overlap search over edge
crossings and interiors, not only vertices, since two areas can overlap with no vertex of
either inside the other.
