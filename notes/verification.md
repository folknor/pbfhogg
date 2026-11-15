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

## VER-135 - A dropped LocationsOnWays claim silently turns off coordinate comparison

`opl_compare::Policy::between` compares inline way coordinates only when both sides
declare `LocationsOnWays`. In the `sort` check (and any check that builds its policy
the same way from pbfhogg's output and a reference), a pbfhogg output that loses the
claim on a LocationsOnWays input therefore switches the comparison to
`NO_LOCATIONS` and passes without ever looking at coordinates. Nothing in the sort
check asserts that the output keeps the input's claim. The policy should come from
the input's (or the reference's) declaration, with a missing claim on ours being a
failure. Raised by the spar partner while settling the germany sort reference
strategy; not yet checked against every check that calls `between`.

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

## VER-131 - The test-module audit assumes target_arch is x86_64

Reported by the link implementer that moved the musl test sweeps to `{host_arch}`.

`cfg_value` in `crates/source-policy/src/rules/test_modules.rs` evaluates `target_arch` as
always `x86_64`. The executable musl sweeps now follow the host architecture, so on an
aarch64 host the test-module audit would judge arch-gated test modules against the wrong
architecture. No effect on x86_64 hosts.

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

## VER-132 - No real-data oracle for cat --dedupe

The dedupe-self and dedupe-pair benches used to diff their output against the input with
pbfhogg's own diff (`--check yes`); a bench no longer judges, and pbfhogg-verify has no dedupe
check. `osmium merge` is not an oracle: it orders its queue by object comparison including
timestamp, so for a shared type, ID and version with differing content its survivor need not be
pbfhogg's earliest-input copy, and it rejects repeated ID and version inside one input. A sound
check needs a narrow domain: inputs with unique ID and version keys, the self-merge
`dedupe(A, A)` compared with A as osmium OPL *without* inline locations (dedupe drops enrichment
in overlap blobs, docs/cli/commands.md), and a subset pair built by the verifier (B derived from
A independently, then the contracted projection of `dedupe(A, B)` against A) to cover
passthrough stretches and overlap transitions. Unsorted input must be held to dedupe's refusal,
and unindexed input needs its forced fallback. Precedence on conflicting copies is covered only
by crates/cli/tests/cli_cat.rs fixtures.

## VER-133 - Geocode answers have no real-data verification

The geocode-query bench used to record per-class answer counts and an FNV digest of every
answer, so two revisions could be seen to answer differently; that was answer evidence inside a
measured child and is gone. Nothing outside the library tests (crates/lib/tests/geocode_*.rs,
synthetic fixtures) checks reverse-geocode answers, and pbfhogg-verify has no geocode check.
The vendored traccar-geocoder is the natural reference.

## VER-134 - osmium diff's checksum cannot see a shifted tag boundary

Reported by the pbfhogg#61 implementer and its codex spar.

`verify_reencode::compare` (repack and degrade) and `checks/diff.rs` still judge with
`osmium diff`, whose `update_object_crc` (research/libosmium crc.hpp) feeds each key and
value without length or separator: tags `a=bc` and `ab=c` collide deterministically, so a
string-table boundary bug passes as common. reference/verify.md now names the collision. The
OPL merge comparator has no such blind spot, but these callers need more than its strict
identity merge: repack and order-preserving degrade need sequence equality (a disordered
kind stays disordered), unsort needs the reference with the documented swap applied and then
every record compared, and diff needs its expected differences and common counts from the
full decoded OLD and NEW streams, keeping its metadata exclusion and verbose-listing checks.


## VER-123 - Verify uses the build output directory as its workspace

Reported by bench-verify.

`VerifyHarness` places outputs under `<target>/verify` and `subdir` recursively replaces each check's fixed
directory. Cargo owns `target`, so these PBFs and reports vanish with cleanup and overwrite earlier evidence.

Fix: a brokkr-owned per-run directory under configured scratch and a separate retained-evidence area.
Enforcement: directory capability types.
brokkr declined this (bz brokkr#154, wontfix); brokkr verify is to be removed and
reimplemented in pbfhogg, which must meet the requirement tracked under VER-127.

## VER-124 - Optional context tools add cost without a verdict

Reported by bench-verify.

`verify all` provisions osmosis, and `run_context_tools` in brokkr runs osmosis and osmconvert for context,
whose failures do not change the verdict.

Fix: an explicit diagnostic option. Enforcement: a declarative step graph rejecting unused outputs.
Brokkr side filed as bz brokkr#155.


## VER-127 - Verify workspace requirement on the pbfhogg verify reimplementation

The verify workspace (see VER-123; brokkr side brokkr#154, closed wontfix).
brokkr verify is to be removed and reimplemented in pbfhogg, so this is a
requirement on that implementation: its outputs go to an owned per-run scratch
directory, with any retained evidence kept separately, never under `target/`.
The rest of the test-filesystem work is done; its settled design is stated
in `reference/testing.md` "Test scratch" and in the leaf crate
`crates/test-scratch`.
