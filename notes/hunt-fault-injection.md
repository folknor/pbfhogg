The per-invocation fault plan solves the main isolation problem: an armed fault does not reach an unrelated invocation. The remaining weaknesses are mostly in proving that a scenario ran, making coverage claims match the actual sweep, and keeping the test machinery independent of host state.

This is a source review. I did not edit files or run checks, so costs below are derived from the code rather than measured timings.

1. **The io_uring panic test can pass on an unrelated failure.** [fault_uring_writer.rs:66](/home/folk/Programs/pbfhogg/crates/lib/tests/fault_uring_writer.rs:66) accepts any error and any unwind. Neither outcome proves dispatch 3 reached the injected panic. A framing error, channel failure, or unrelated panic can satisfy the test. It also accepts an unwind despite describing an error returned through `flush`.

   Keep an invocation handle for observation, assert the dispatch count reached the armed dispatch, and require the injected panic payload in the returned error. Reject unwinds if returning an error is the intended contract. This is mechanically enforceable with a shared fault-result assertion helper and a regression test that supplies an unrelated error.

2. **The geocode test explicitly accepts the behavior its contract says to reject.** [fault_geocode_pass3.rs:16](/home/folk/Programs/pbfhogg/crates/lib/tests/fault_geocode_pass3.rs:16) says a worker panic must surface as `Err`, but its result matching accepts an unwind carrying the injected message. Removing panic-to-error handling could therefore leave this test green.

   Separate error propagation from cleanup: require `Ok(Err(e))`, verify its provenance, then check leftovers. If unwinding is the intended API instead, change the claim. The result shape can be enforced directly by the test.

3. **The cancellation sweep silently accepts sampled runs that never cancel.** [cancel.rs:366](/home/folk/Programs/pbfhogg/crates/lib/tests/cancel.rs:366) chooses checkpoint ordinals from a counting run, but [cancel.rs:382](/home/folk/Programs/pbfhogg/crates/lib/tests/cancel.rs:382) ignores successful runs at those ordinals. Only one cancellation per command is required. Changes in scheduling or checkpoint layout can remove much of the intended coverage without failing the test.

   Record whether each requested callback fired. Distinguish an unreachable ordinal from cancellation that fired too late to affect an already decided publication. Require every sampled scenario to have an explicit outcome. A structured scenario result can make unexamined successes impossible.

4. **“Every checkpoint” and “every command” overstate cancellation coverage today.** The sweep samples when the count exceeds 64, despite its test comment claiming every checkpoint. Its command registry at [cancel.rs:249](/home/folk/Programs/pbfhogg/crates/lib/tests/cancel.rs:249) contains ten entries with `commands` enabled and omits numerous command families covered elsewhere in this scope, including getid, getparents, repack, renumber, tags-filter, tags-count and time-filter.

   Rename the current claims immediately. For complete coverage, build a command-scenario registry with explicit supported modes and exclusions, and compare its inventory mechanically against the command inventory. A registry alone still needs an enforced completeness comparison; otherwise it becomes another drifting list.

5. **The census test can pass without observing anything.** [cancel.rs:397](/home/folk/Programs/pbfhogg/crates/lib/tests/cancel.rs:397) initializes the observed maximum to zero and checks that it remains zero. It does not assert that its callback ran. A command losing its checkpoints can satisfy this test vacuously, even though the separate mid-run test might catch the same change.

   Require a positive observation count in this test itself. Also narrow its claim to “at observed checkpoints”: temporary census issuance between checkpoints is invisible. Enforcing the stronger invariant belongs at the issuance operation, using an explicit internal-versus-issued construction path or an observer there.

6. **Installing a callback twice silently leaves the old callback active.** [host.rs:627](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:627), [test_hooks.rs:596](/home/folk/Programs/pbfhogg/crates/lib/src/test_hooks.rs:596), and [test_hooks.rs:630](/home/folk/Programs/pbfhogg/crates/lib/src/test_hooks.rs:630) discard `OnceLock::set` failures. A fixture and its caller can both “configure” a hook while only the first configuration takes effect.

   Return a must-use error, or panic on duplicate installation in this test API. Better still, install callbacks while constructing the scenario, before sharing its invocation. Types and signatures can enforce this.

7. **The pool-joining test pays for a timing assumption on every successful run.** [fault_library_pools.rs:79](/home/folk/Programs/pbfhogg/crates/lib/tests/fault_library_pools.rs:79) waits 200 ms to infer that the calling thread has not returned. Across its three tests, that imposes at least 600 ms of successful-path waiting. A detached implementation whose driver is descheduled for that window can evade the negative assertion.

   Expose the owner's join transition and coordinate it with the parked exit hooks. Use timeouts only as failure bounds. A shared handshake API can enforce this pattern; a paused clock alone cannot establish thread ordering.

8. **Hook synchronization is spread across writable atomics and polling loops.** [test_hooks.rs:794](/home/folk/Programs/pbfhogg/crates/lib/src/test_hooks.rs:794) polls every millisecond with a fixed ten-second bound. Tests duplicate polling with their own bounds. Several scenarios depend on both production seams and test code updating related flags correctly.

   Replace these groups with scenario objects owning their state transitions, notifications, release guards and observations. Keep a real deadline as a deadlock backstop, but make ordinary progress event-driven. This removes recurring polling and makes invalid transition combinations unrepresentable. The fixed hook timeout also belongs in the repository's centralized tunables under its current convention.

9. **The shared fixture host mutates the entire test process.** [common/mod.rs:1711](/home/folk/Programs/pbfhogg/crates/lib/tests/common/mod.rs:1711) raises the descriptor limit once, before returning a host whose process policy is `NoProcessActions`. Every fault test calling it changes the environment for sibling tests. Which test calls it first determines when that mutation happens.

   This can mask descriptor assumptions and contradicts the apparent isolation provided by the returned host. Remove the mutation from the general fixture helper. Tests requiring a particular process limit should own a dedicated test process or receive an injected resource model. A lint banning process-toolbox calls in shared fixture code can hold that boundary.

10. **Fixed resources do not provide a fixed executor.** The same helper uses `Executor::Ambient`, and the io_uring test chooses it explicitly. Rayon execution therefore still depends on the process-global pool and actual host concurrency. Fixed reported resources do not guarantee the concurrency needed by a parked-worker scenario; concurrent tests also contend for that pool.

    Hand concurrency-sensitive scenarios an owned executor with an explicit worker count. Reserve ambient-executor tests for testing ambient behavior. This can be enforced through a dedicated fault-test host constructor and a rule forbidding `Executor::Ambient` in those scenarios.

11. **The input replacement matrix silently loses its procfs arm on some hosts.** Tests such as [fault_time_filter_input.rs:277](/home/folk/Programs/pbfhogg/crates/lib/tests/fault_time_filter_input.rs:277) request forced path reopening and automatic reopening, then adapt expectations to `reopens_through_procfs()`. When procfs reopening is unavailable, both cells exercise path reopening. The lost arm is not announced.

    Report an explicit skip for the unavailable procfs arm and avoid rerunning the identical fallback cell. Separate reopen-strategy behavior behind an injected opener from real platform integration checks. On non-Unix systems, several `else if cfg!(unix)` branches additionally leave intermediate-replacement outcomes unchecked. Those cases need an explicit platform outcome or skip. A scenario runner can require every cell to return a checked outcome.

12. **The chunk tests retain large fixtures despite already having size injection seams.** [fault_diff_parallel.rs:124](/home/folk/Programs/pbfhogg/crates/lib/tests/fault_diff_parallel.rs:124) uses 120,000 nodes to exceed the default chunk size. [fault_diff_parallel.rs:158](/home/folk/Programs/pbfhogg/crates/lib/tests/fault_diff_parallel.rs:158) runs a 60,000-node fixture through 27 combinations, including seven-byte chunks. These run at the root tier in the hooks-enabled sweep.

    Test chunking semantics with small injected thresholds and small fixtures. Keep one separately tiered test for the production default boundary. Likewise, element-cut tests using hundreds of thousands of nodes would benefit from an injected planner policy. Enforce consolidation through typed policy construction; exact runtime savings need measurement.

13. **The derive helpers use process-wide counters for fixture naming unnecessarily.** [fault_derive_parallel.rs:65](/home/folk/Programs/pbfhogg/crates/lib/tests/fault_derive_parallel.rs:65) and its gzip helper each maintain a static run counter. Atomic increments make the current naming safe, but output names depend on unrelated test execution order, and every output remains until the enclosing scratch guard drops.

    Give each invocation a child fixture directory and use fixed names inside it. That makes ownership local and permits immediate cleanup. A text rule can prohibit static fixture-name counters in tests once the shared helper exists.

14. **The publication observer documentation is false about locking.** [test_hooks.rs:626](/home/folk/Programs/pbfhogg/crates/lib/src/test_hooks.rs:626) promises “no lock held.” Publication runs under the invocation's execution lock, as described in [output.rs:2284](/home/folk/Programs/pbfhogg/crates/lib/src/output.rs:2284) and `reference/testing.md`. An observer waiting for another publication under that invocation can create a dependency cycle.

    Correct the contract and state the allowed blocking dependencies. Same-invocation re-entry can be tested mechanically; arbitrary cross-thread callback cycles cannot be fully enforced. Prefer bounded scenario notifications over unrestricted blocking callbacks where possible.

15. **“Product builds never enable test-hooks” is a convention, not an enforced property.** [Cargo.toml:51](/home/folk/Programs/pbfhogg/crates/lib/Cargo.toml:51) exposes an ordinary additive Cargo feature. A release build can enable it, including through feature unification, and then exposes panic, replacement and scheduling controls.

    Off-by-default compilation is enforced; exclusion from product artifacts is not. Add a product-build gate checking the resolved feature graph. If hook-free artifacts are a hard boundary, use a separate internal testing build arrangement. A blanket release-mode prohibition would conflict with release-mode fault testing.

16. **The existing no-global-hook checks are narrower than their architectural promise.** [check_source_invariants.py:18](/home/folk/Programs/pbfhogg/scripts/check_source_invariants.py:18) explicitly misses statics inside gated modules and macro bodies. The companion textlint forbids statics in `test_hooks.rs`, but does not prevent equivalent global state elsewhere under another module name. These are useful tripwires, not enforcement that all fault state is invocation-owned.

    Use a Rust-aware check for static mutable state and synchronization containers across hook-enabled modules, with explicit exceptions. Add presence assertions for the protected source files so moving a file cannot silently empty a rule's scope. An architectural dependency rule restricting fault-state construction to the invocation provides a stronger boundary than spelling checks.

All current scoped fault files have module declarations in `tests/all.rs`; I found no undeclared fault test file. I also found no nested build-tool invocation or scoped write into another tool's cache. `test_support.rs` is correctly test-gated and uses typed direct-I/O refusals rather than treating arbitrary I/O errors as platform skips.
