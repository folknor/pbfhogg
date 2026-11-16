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

## VER-135 - Kept LocationsOnWays coordinates are not compared, and permitted drops are not shown necessary

Each output's declaration is now held to its command's header rule
(`Harness::judge_way_locations`), so a lost declaration no longer passes silently.
Residue: (1) where pbfhogg keeps the declaration (cat, `getid --invert`, sort) the
osmium reference of every check but sort is written without `locations_on_ways=true`,
so `Policy::between` is `NO_LOCATIONS` and the kept coordinates are never compared;
(2) sort and `getid --invert` may drop the declaration with the warning, and the
verifier does not show the drop was required (no way blob rewritten, no way removed
would make keeping it decidable for `--invert` from the requested IDs).

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

## VER-122 - Verify's field policies have no real-data negative controls

Reported by bench-verify.

Every equality judgment once made by `pbfhogg diff` is now the verifier's merge of osmium's
OPL decodings (`crates/verify/src/opl_compare.rs`) under a declared field policy, so the
independent decoder and the per-check policy exist (reference/verify.md "How outputs are
judged"). Residue: nothing proves each policy reads the field classes it claims on real data.
The generated tool protocols in `crates/verify/tests` fault whole records, not one field
class at a time; negative controls that alter metadata, inline locations, tags and members
separately on a real derived copy, each required to fail its check, are still missing.

## VER-132 - The cat --dedupe verify does not witness passthrough or overlap transitions

`pbfhogg-verify cat-dedupe` merges the input with osmium `tags-filter -R` selections of it and
judges each answer against the input, but osmium packs a sparse selection into blobs whose ID
ranges can span many of the input's, and the planner extends overlap components by running
maximum, so whether any blob passed through raw, or any run started and ended between passthrough
stretches, is not established. A derived operand built from bounded ID windows chosen against the
input's own framing (read by the verifier's frame reader), with the expected plan predicted from
both framings, would witness them.

## VER-133 - Geocode interpolation and admin answers have no real-data verification

`pbfhogg-verify geocode` judges address and street answers of `pbfhogg geocode-query` on
denmark and germany against a model built from osmium OPL; interpolation and admin answers
are not judged. traccar-geocoder is not an oracle for either: it reads nine cells only, merges
house and street under a 75 m cutoff, maps admin levels to per-country semantics on f32
coordinates with an eroded interior, and needs a C++ build, which the osmium-only verifier
rule excludes. What remains:

- Interpolation: a model of the range rules in docs/cli/commands.md (street or place choice
  from the end addresses, interior splits, step and parity, refusals), where a range the
  model cannot support makes that query's interpolation verdict inconclusive rather than
  dropping it from the competition.
- Admin: an independent ring assembly with hole ownership, one polygon per outer ring ranked
  by outer-ring area stored as f32 (polygon-index tie order is not public, so equal-area
  levels need an allowed-set or exclusion), first at `--admin-tolerance 0`, then at the
  default outside the tolerance band.
- The model refuses data near the antimeridian; unwrapping longitude would let an
  antimeridian dataset (the Fiji entry in notes/todo.md) be judged.

## VER-123 - Verify uses the build output directory as its workspace

Reported by bench-verify.

`VerifyHarness` places outputs under `<target>/verify` and `subdir` recursively replaces each check's fixed
directory. Cargo owns `target`, so these PBFs and reports vanish with cleanup and overwrite earlier evidence.

Fix: a brokkr-owned per-run directory under configured scratch and a separate retained-evidence area.
Enforcement: directory capability types.
brokkr declined this (bz brokkr#154, wontfix); brokkr verify is to be removed and
reimplemented in pbfhogg, which must meet the requirement tracked under VER-127.

## VER-137 - The verifier's OSC scanner does not decode entity references in `id`

Found working the apply-changes final-delete rule.

`extract_id` in `crates/verify/src/osc.rs` parses the raw attribute text, so a legal `id="&#49;"`
is refused by the strict scanner (and skipped by the lenient one) instead of read as 1. Every
strict caller fails the check rather than passing, so no false pass is known; it is a false fail
on exotic but legal input. The same file's module header still describes the retired brokkr
`verify_merge` ("request 4"), stale since the verifier moved into pbfhogg.

Fix: decode `id` as `parse_meta` decodes the other attributes, and rewrite the header.


## VER-127 - Verify workspace requirement on the pbfhogg verify reimplementation

The verify workspace (see VER-123; brokkr side brokkr#154, closed wontfix).
brokkr verify is to be removed and reimplemented in pbfhogg, so this is a
requirement on that implementation: its outputs go to an owned per-run scratch
directory, with any retained evidence kept separately, never under `target/`.
The rest of the test-filesystem work is done; its settled design is stated
in `reference/testing.md` "Test scratch" and in the leaf crate
`crates/test-scratch`.
