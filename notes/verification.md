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

## VER-135 - sort's LocationsOnWays drop is undecided without the sort claim

Kept coordinates are compared (cat and `getid --invert` references are written with
`locations_on_ways=true`, and a reference or sorted copy asked for it must declare
it), `getid --invert` is judged exactly from the requested way IDs, and sort is exact
where the input's per-blob claims decide it (`checks::sort::sort_way_locations`).
Residue: on input without `Sort.Type_then_ID`, or with an unindexed blob, a negative
or an empty way claim, sort stays keep-or-warned-drop, since an internally unsorted
blob is only visible by decoding; and the required-drop branch (overlapping way
ranges on a `LocationsOnWays` input) has no real-data witness, as no configured
dataset is both. A decode of each way blob's ID order would close the first.

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

## VER-113 - dedupe-pair does not prove its copy path

`diff-unindexed.lua` now probes both fronts before measuring and fails on indexed input.
`dedupe-pair.lua` still cannot require passthrough or coalesced copying: that needs brokkr's
Lua contract to expose child counters (`cat_dedupe_blobs_passthrough`,
`cat_dedupe_copy_range_coalesced`). Proving complete indexdata absence needs an
indexed-blob count from `inspect`; `inspect --get indexed=false` includes partially
indexed inputs.

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

`pbfhogg-verify geocode` judges all four answer classes, but on the configured datasets some
admin paths are never exercised and one domain is refused:

- On germany no answer carrying a country code is judged (denmark judges over a thousand),
  which points at an unmodelled level-2 relation whose box covers the queries (a ring
  junction, an unlocated member node or a hole near an outer); which relation and why is not
  yet established. Modelling junction groups (a ring decomposition that does not depend
  on search order, or one proved unique) would close it.
- No judged query has two containing areas at one level, so the area-ranking rule is judged
  only as "the one containing area". Witnesses aimed at same-level overlaps would exercise it.
- Interpolation witnesses reach only about 27 judged fine-radius queries on germany against a
  quota of 150; witnesses anchored on range midpoints would raise it.
- The model refuses data near the antimeridian; unwrapping longitude would let an
  antimeridian dataset (the Fiji entry in notes/todo.md) be judged.
