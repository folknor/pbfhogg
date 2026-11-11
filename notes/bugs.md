# Bugs from the verification hunt

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

Findings are as reported by the hunters and have not been verified. Costs and
behaviour are derived from source reading unless an entry says otherwise.

## BUG-001 - ref-integrity with relation checking reads the plain expectation

`benches/check-refs/ref-integrity.lua` always selects `expect.check-refs`, also
under `--check-relations yes`. Brokkr's dataset documentation defines a separate
`check-refs-relations` expectation because way references can be clean while
relation references are broken. The result can be false acceptance or false
rejection.

Fix: select the expectation from the actual check configuration, as
`location-integrity` does for agreement mode. Enforcement: a small script
contract test with different plain and relation expectations.

## BUG-002 - geocode-query can report a speedup while queries fail

`run_bench_geocode_query` in `crates/cli/src/lib.rs` ignores query errors during
timed rounds. Warm-up counts errors but still returns success, and
`benches/build-geocode-index/geocode-query.lua` never rejects a nonzero error
count. A revision that starts returning errors can complete less work and appear
faster.

Fix: fail unexpected query errors, including during timed rounds, and declare
expected outcomes separately if error workloads are wanted. Enforcement: an
instrument test using a reader abstraction that returns a deliberate query error.

## BUG-003 - plantasjen north-america entry carries a known false replication sequence

`brokkr.d/hosts/plantasjen.toml` records that the north-america base is actually
sequence 4700, while its entry says 4710 and offers OSC 4710 with intervening
changes missing. The warning protects a reader, but `apply-diff` and
`diff-applied` can bind those entries and run successfully, which transforms an
incomplete update chain rather than the claimed daily update.

Fix: correct or quarantine the entry and represent replication provenance as
data. Enforcement: validate the configured sequence against the PBF header and
require contiguous changes for workloads claiming a replication update. Where
provenance is unavailable, the workload must state that continuity is unverified.

## BUG-004 - The promise guard's unsupported-architecture fallback does not compile

`crates/cli/tests/guard.rs` defines `AUDIT_ARCH` only for x86_64 and aarch64, and
`program` references it unconditionally. On another Linux architecture the
runtime `supported()` check cannot provide the documented unguarded fallback
because compilation fails first.

Fix: compile the supported implementation behind an architecture gate and provide
a separate unsupported implementation. Enforcement: cross-target compile checks,
which do not need execution on every architecture.

## BUG-005 - The signal reporter holds a mutex across a potentially blocking stderr write

`crates/cli/src/signals.rs` holds `FINISHED` while printing the stopping notice,
and `finish` takes the same lock before returning the command outcome. A full,
still-open stderr pipe can therefore block completion behind the notice thread.
The dedicated signal waiter still permits a second signal to terminate the
process, but ordinary completion is coupled to the reporter's blocking write. The
invoker drains stderr and cannot expose this state through its normal API.

Fix: make reporter lifecycle and completion ownership explicit, with a bounded or
nonblocking diagnostic handoff. Enforcement: a subprocess test that holds a full
stderr pipe and controls when it drains, and keeping blocking output outside
completion-state locks.

## BUG-006 - The geocode builder's smoke check can pass after address lookup breaks

The smoke query in `crates/lib/src/geocode_index/builder/mod.rs` queries the first
stored address point but reports `Passed` when it finds either an address or a
street. A broken address index can be masked by a nearby street, and any address
result passes without its identity or distance being checked. The enum accurately
documents this weak condition, so the hunter calls it a weak check rather than a
contradiction. It pays for opening and validating the whole newly written index
while its query component establishes little about address retrieval. A complete
miss is only a warning and still permits publication.

Fix: separate structural validation from a stated semantic check. If the claim is
address retrievability, verify an exact-coordinate candidate through an API that
exposes candidates and handles colocated addresses. Enforcement: a test that
removes address-cell entries while retaining street entries. Whether failure
should block publication is a product policy decision.
