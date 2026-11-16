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

The per-run charge `TEST_HEAVY_RUN_FDS` is now witnessed in `process_nofile.rs` for the
largest buffered-output external join a heavy grant admits (fleet rendezvous, kernel
limit leaving exactly the charge free). Residue, still asserted: the charge for Auto
routing, fault paths, the direct-I/O and io_uring writers and a scenario's own
fixtures; the light reserve `TEST_LIGHT_FD_RESERVE` (a count in the shared binary is
process-wide and light use scales with runner threads, so it cannot be checked
without failing on large hosts); a ticket's units versus its simultaneous runs; own
`host::FdPool` tests holding a ticket by convention. Every current heavy holder runs
`write_base_pbf` (one way blob, so one 256-file fleet), well under the witnessed shape.

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

## VER-018 - The gate's backend requirement checks refusals, not execution

io_uring cells assert a registered pool and serialize on a locked-memory lease;
io-uring is not in the gate's `PBFHOGG_TEST_REQUIRE`, because the writer registers
its full fixed pool or refuses and a standard host's memlock limit refuses it, so
on such a host the io_uring cells skip and only the opt-in ignored
`full_registered_pool_instrument` proves the pool, where run by hand on a host
whose limit was raised. Residue: O_DIRECT cells (about forty, library fault_*_input, host_contract and
roundtrip cells, unit tests through `test_support`, seven CLI cells) still have no
positive witness, so one that stops reaching O_DIRECT without consulting a predicate
passes; a candidate is `writer_direct_write_calls` for writers, while direct reads
have no counter. The io_uring witness in `roundtrip_uring_tiny_output` is the
dispatch count and exists only in hook builds, so the `release` sweep runs that cell
unwitnessed. The lease cannot cover other processes of the user or a deferred kernel
unpin. Also open: the `promise-guard` and `direct-io` requirements were not checked
against the AGENTS.md host rule (a scratch filesystem that refuses O_DIRECT, an
architecture or kernel without seccomp).

## VER-090 - The requirement-table check is convention-bound

`required` in `crates/harness/src/describe.rs` now refuses anything outside its convention
(exactly one root table, exact line shape, no duplicate ids, and any pbfhogg or library key
or nested include in an included file), and the authoritative composed check is
`brokkr library --all`. It still does not parse composed TOML; residue only if a
composed-configuration parse is ever wanted, which needs a brokkr configuration export
brokkr does not offer.

## VER-113 - dedupe-pair does not prove its copy path

`diff-unindexed.lua` now probes both fronts before measuring and fails on indexed input.
`dedupe-pair.lua` still cannot require passthrough or coalesced copying: that needs brokkr's
Lua contract to expose child counters (`cat_dedupe_blobs_passthrough`,
`cat_dedupe_copy_range_coalesced`). Proving complete indexdata absence needs an
indexed-blob count from `inspect`; `inspect --get indexed=false` includes partially
indexed inputs.

## VER-122 - No check is shown to pass the field policy its claim needs

Reported by bench-verify.

`pbfhogg-verify field-policies` shows on real data that each `opl_compare::Policy` reads
exactly the field classes it claims. What it does not show is which policy each check hands
the comparison, nor that the check's verdict follows from it: a check passing CONTENT where
its claim needs metadata would stay green. The generated fixtures in `crates/verify/tests`
cannot see this either, because their fake osmium ignores the `-f` format string everywhere
except `encoding_fixture.rs` (which asserts `locations_on_ways=true` only); no fixture
asserts `add_metadata=false` or its absence. A fake that decodes by the requested options,
with per-check faults altering one field class, would close it.

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
