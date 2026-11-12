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

## VER-143 - Waits outside the timed-waits spelling rule are unreviewed

The hardcoded-wait chain removed every production timed wait and every test
sleep and spin primitive the `timed_waits` audit can spell. Outside its reach
and never inventoried: loops that re-check an `Instant` deadline, and loops
that spin on `try_recv` or an atomic without calling a banned primitive.
Each such loop is asked the same question as the rest: does the other side
notify, and if not, why not. Carried over from the chain's design notes, not
re-audited.

## VER-014 - Empty textlint globs and stale exclusions are only audit-enforced

Every rule in `brokkr.d/lints.toml` now has a `must_scan` sentinel, and the old coverage
script and its migration manifest are retired. brokkr still lets an empty `paths` glob or
a stale exclusion pass an ordinary check; only its audit flags them. Brokkr side: bz
brokkr#157.

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

## VER-107 - The musl sweeps assume an x86_64 host

Reported by check-machinery.

Both executable musl sweeps in `brokkr.d/checks.toml` hard-code `x86_64-unknown-linux-musl`,
now with comments and a host restriction stated in `reference/testing.md`. On aarch64 they
would be cross-builds with foreign executables. Residue is brokkr-side check planning:
resolve a runnable target from host capability, or refuse an unexecutable target before
building. Filed as bz brokkr#170.

## VER-112 - Orphaned expectation keys are not rejected

The six integrity and diff benches take `--require-expectation yes|no` through the shared
`selected` helper in `benches/lib/expect.lua`. Still open: configuration validation rejecting
orphaned digest-pair and OSC expectation keys (bz brokkr#161 is closed without a
close message; whether it landed orphan-key validation is unverified), and an
independent oracle for the correct answer.

## VER-113 - dedupe-pair does not prove its copy path

`diff-unindexed.lua` now probes both fronts before measuring and fails on indexed input.
`dedupe-pair.lua` still cannot require passthrough or coalesced copying: that needs brokkr's
Lua contract to expose child counters (`cat_dedupe_blobs_passthrough`,
`cat_dedupe_copy_range_coalesced`). Proving complete indexdata absence needs an
indexed-blob count from `inspect`; `inspect --get indexed=false` includes partially
indexed inputs.

## VER-122 - Verify's equality oracle is narrower than "identical" and shares the subject's code

Reported by bench-verify.

`VerifyHarness::diff_pbfs` in brokkr uses pbfhogg diff and `verify_sort` prints "identical", while
`crates/lib/src/content.rs` excludes metadata, inline way locations, headers and encoding. A shared decoder
defect can affect both sides.

Fix: an equality policy per check and an independent decoder for promised fields. Full digest strengthens
same-project checks but is no independent oracle. Enforcement: negative controls altering each field class.
Brokkr side filed as bz brokkr#156.

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
