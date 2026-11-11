I found the following issues in this scope. This was a source inspection only: I changed nothing and ran no builds, tests, benchmarks or verification. Costs below are derived from the code, not measured wall times.

1. **The fixed test host still uses the ambient Rayon pool.** [Host::for_tests](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:1932) supplies four logical workers and an 8 GiB budget, but selects `Executor::Ambient`. That executor uses the current pool or initializes Rayon's process-wide global pool. Command and geocode tests therefore share scheduling state whose width and initialization depend on their surroundings.

   This weakens the claim in `reference/testing.md` that these tests see the same machine everywhere. Resource planning is fixed; execution is not. On another host this changes concurrency and contention without announcing a different test configuration.

   Give each test an owned fixture containing its host and an explicit executor. Keep ambient-executor coverage in tests that deliberately exercise that behavior. A source rule can forbid `Executor::Ambient` in the ordinary test-host constructor, with explicit exceptions for those tests.

2. **Two modules share one live invocation across all their tests.** The `run_host` helpers in [getparents](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getparents/mod.rs:605) and [tags_filter](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:1409) store an `Entered` host in a static `OnceLock`.

   The comments say this is necessary so processing and publication share an invocation. That requires sharing within a test, not across tests. Counters, diagnostic suppression, cancellation and lifecycle state consequently belong to the whole module. Their safety currently rests on the promise that the host is never cancelled or armed. The static entry also retains a live call indefinitely.

   Pass a per-test fixture through the helper chain. Ban static `Host`, `Entered` and `Invocation` fixtures mechanically, including statics inside functions. The existing fault-state rules do not catch these statics.

3. **The ordinary entered-host helper leaks every host it creates.** [entered_for_tests and entered_leaked](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:1946) use `Box::leak`. Their many callers in this scope permanently retain the host and its referenced resources until the test binary exits. This includes custom hosts carrying instrumentation sinks.

   This does not necessarily leak worker threads today, but it makes ownership misleading and makes adding an owned executor to the helper dangerous. It also prevents tests from observing final host destruction.

   Use an owned fixture and borrow its host when entering calls. Remove the general-purpose leaking helper; a source rule can then prohibit `Box::leak` in test infrastructure outside narrowly documented lifetime tests.

4. **The apply-changes failure-order tests do not establish their advertised completion order.** [encode_failing](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/stream_output.rs:421) sets a flag before returning the later block's error. The earlier block waits for that flag, sleeps 20 ms, then fails. The comment correctly admits this is a bias rather than proof, but the test descriptions say the later failure happens first.

   These tests can pass without exercising the intended reversed completion. They also depend on Rayon's splitting implementation: a 16-thread pool is chosen so dependent blocks become separate jobs. A splitter change can turn correct production behavior into a ten-second test failure. Four tests construct this 16-thread pool; the shared encoder introduces three explicit 20 ms sleeps across its current callers.

   Test ordering at the boundary where completed results are collected, using a controllable completion source. Verify that the later result was delivered before releasing the earlier one. Keep a smaller real-executor integration test separately. Types or a test completion harness can enforce the ordering; sleeping cannot.

5. **The router notification test pays real time for a negative scheduling observation.** [every_router_notification_waits_for_the_final_check_gate](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/coord_payloads.rs:1108) waits 50 ms for each of four event variants while holding the notification mutex. That imposes at least 200 ms per execution, before scheduling overhead.

   Seeing the predicate change does not prove the publisher has reached the notification lock: it can be descheduled between those operations. The subsequent condition-variable wait also accepts any non-timeout wake, including a spurious wake, as notification evidence.

   Add an explicit observation point immediately before acquiring the gate, and test the synchronization protocol with controlled interleavings or a concurrency model. Retain a real-thread smoke test with a watchdog for hangs, rather than elapsed time as the evidence. A model test can enforce the protocol; a lint can prevent reintroducing timing-based negative assertions.

6. **The merge executor test is expensive but still accepts completely serial execution.** [encode_jobs_stay_on_the_executor_within_their_lanes](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/stream.rs:920) sleeps 2 ms in every encode job. Its assertion accepts any peak from one through `lanes`, so the two-lane case passes if the implementation serializes every job.

   Its fixture creates 262,144 XML nodes from the current 512 KiB batch constant, then processes that input under three pool configurations. The sleep cost is two milliseconds per batch per configuration. The test checks pool membership and an upper bound, but the expensive overlap encouragement proves no concurrency.

   Feed small synthetic batches into the coordinator and use a handshake to establish two simultaneous jobs in the multi-lane case. Check sequence order explicitly if the comment's “every batch is answered in order” claim belongs here. Enforce this through the test harness, with the batch size supplied to the parser mechanism.

7. **Two location-store tests create 3,000 native threads per execution for probabilistic race coverage.** [The concurrent join tests](/home/folk/Programs/pbfhogg/crates/lib/src/commands/check/locations/store.rs:350) perform 200 repetitions per case, spawning three fresh threads each time: 1,800 threads for node joins and 1,200 for agreement.

   These are useful stress tests, but repetition does not establish which atomic interleavings were exercised. They run as ordinary tier-1 tests in every applicable sweep, multiplying thread creation and contention with unrelated tests.

   Model the small atomic state machine systematically, keep a bounded real-thread smoke test, and put extended stress coverage in an explicit stress lane. The model adapter and tier placement can be checked mechanically. Repetition alone cannot enforce interleaving coverage.

8. **Merge correctness fixtures scale with production batch tuning.** [The merge fixtures](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/mod.rs:919) derive their input sizes directly from `MERGE_CHANGES_BATCH_BYTES`. A throughput tuning change therefore changes test allocation, XML parsing, compression and disk work.

   For example, `rendered_simplify_writes_the_owned_bytes` performs 12 configurations, running both implementations in each configuration: 24 complete simplify operations. Its four inputs contain 34,952 generated changes at today's constant. The forced-spill test likewise needs large files to reach a small number of state transitions.

   Give the private batching mechanism a configuration passed by production and tests. Exercise boundaries and spill transitions with small batches, plus one focused test connecting the production configuration. This follows the testing document's allowance for narrow mechanism parameters. The configuration type provides structural enforcement; do not introduce scattered test overrides.

9. **A complete retired merge implementation remains compiled as a test oracle.** [merge_changes/mod.rs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/mod.rs:43) retains a test-only owned change representation, parser sink, stream construction and simplified writer. [owned_simplify](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/mod.rs:1227) runs that machinery beside production.

   This is meaningful differential coverage, not a tautology. Its cost is maintaining two implementations and repeatedly parsing and writing the same large inputs. Shared parser and version-floor components also mean agreement is not independent evidence for every semantic property.

   Replace this broad legacy implementation with a small semantic reference model over explicit changes, and independently check rendered output against that model. Preserve exact byte comparisons only where byte identity is the contract. Confining reference models to a dedicated test module can be enforced structurally; their independence still needs review.

10. **The create-run fixture guard checks a copy of the fixture size.** [the_batch_boundary_fixture_spans_more_than_one_batch](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/stream_output.rs:354) checks a local `BOUNDARY_FIXTURE_FULL_BLOCKS = 65`. The integration fixture separately declares [BOUNDARY_RUN_FULL_BLOCKS](/home/folk/Programs/pbfhogg/crates/lib/tests/apply_changes_create_runs.rs:185).

    Both match today. Changing the real fixture without changing the local copy leaves the guard green while its claim stops holding. The instruction to “grow ... this constant together” is an unenforced two-writer invariant.

    Have the integration test observe the batch transition it claims to cross, or share a test-support definition that actually constructs the fixture. A runtime transition assertion is stronger because it also catches changed dispatch behavior.

11. **The geocode defaults test does not validate the defaults it claims to validate.** [rejects_invalid_levels_and_radii](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/mod.rs:1016) starts with `BuildConfig::default()`, but overwrites street level, coarse level and both radii before asserting “the defaults must validate.”

    Those assigned values equal the defaults today. A future invalid default in one of those fields can nevertheless pass this assertion because the test replaces it first. The later comment also says the default fine radius fails at a finer level, while the tested radius has been changed to 1,000 metres.

    Add a direct assertion on an untouched default configuration. Build each invalid case independently and check the intended refusal, so another invalid field cannot satisfy a broad `is_err()`. These are mechanically enforceable behavioral tests; prose alone is not.

12. **The geocode reader absence test never calls the reader methods.** [defensive_fixed_record_readers_expose_absence_in_their_types](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:2338) assigns four methods to function pointers, then tests `Table::get` on empty tables.

    It proves the signatures remain optional and the table primitive handles absence. It does not prove that any `Reader::read_*` method delegates correctly or returns absence. Replacing a method body with a panic would leave this test green.

    Keep the signature checks if desired, but invoke each reader method on a minimal valid reader with absent and present records. Put generic table behavior beside `Table`. The behavioral tests enforce the missing connection.

13. **Sparse allocation failure coverage stops before the allocation syscall.** [sparse_store_precheck_reports_insufficient_space_before_mapping](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:417) and `failed_preallocation_leaves_no_file_in_scratch` request `u64::MAX`. On ordinary filesystems, the free-space precheck refuses before `posix_fallocate` runs.

    Thus the latter test does not exercise failure of the allocation operation itself, despite its name. The success test also requires the actual scratch filesystem to support `posix_fallocate`; a filesystem returning an unsupported-operation error causes a test failure.

    Separate free-space policy from allocation, inject allocation success and failure for lifecycle tests, and retain a distinctly named platform test for the syscall adapter. This is enforceable through an allocation interface and fault tests. The existing test is useful precheck coverage, but should not stand in for syscall-failure coverage.

14. **The sparse unlink assertion is keyed to the production filename.** [successful_allocation_unlinks_the_store](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:455) checks only that no entry is named `sparse-index`.

    That name matches today. Rename the backing file and accidentally retain it, and the test passes. The actual invariant is that allocation leaves no additional directory entry while the returned descriptor remains usable.

    Compare the complete directory contents before and after allocation, including the scratch bookkeeping entries, and verify descriptor contents. This behavioral test avoids a filename-maintenance rule entirely.

15. **A manual release timing probe is automatically run by the full correctness profile.** [verbose_sequence_budget_timing](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/seq.rs:193) is ignored with the reason “manual release-mode timing.” However, the [full profile](/home/folk/Programs/pbfhogg/brokkr.d/checks.toml:169) includes ignored tests, and `[test] debug = true` selects debug builds.

    It consequently runs 22 timed sequence-diff calls during the full profile under a configuration unlike the one its annotation describes. It does have correctness assertions, so it is not a test that proves nothing; the misleading part is the manual-release designation and its placement in routine correctness work.

    Split the small work-limit assertions from the timing workload. Put timing in a registered brokkr bench. A source check can prohibit timing-probe tests in correctness targets, while bench registration makes the execution mode explicit.

16. **The geocode builder's production smoke check can report success after address lookup breaks.** [The smoke query](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/mod.rs:845) queries the first stored address point but reports `Passed` when it finds either an address or a street. A broken address index can therefore be masked by a nearby street. Any address result also passes without checking its identity or distance.

    The enum accurately documents this weak condition, so this is a weak check rather than a contradiction in the documentation. Nevertheless, it pays for opening and validating the entire newly written index while its query component establishes little about address retrieval. A complete miss is only a warning and still permits publication.

    Separate structural validation from a stated semantic check. If the intended claim is address retrievability, verify an appropriate exact-coordinate candidate through an API that exposes candidates and handles colocated addresses. A test that deliberately removes address-cell entries while retaining street entries can enforce that the smoke check detects the failure. Whether failure should block publication is a product policy decision.

17. **All scoped filesystem tests inherit a compile-time checkout location.** The shared [scratch helper](/home/folk/Programs/pbfhogg/crates/lib/tests/common/scratch.rs:8) searches upward from `env!("CARGO_MANIFEST_DIR")` for `Cargo.lock`, then writes under that checkout's `data/test-scratch`.

    The location has a clear owner and is preferable to borrowing another tool's cache. The dependency is still ambient: moving a prebuilt test binary to another machine, removing its original checkout, or running against a read-only source tree causes failure or writes back into the original checkout if it remains accessible.

    Have the runner provide an owned scratch root, and let standalone test execution use an explicit documented fallback. Continue allocating private guarded directories beneath that root. Centralizing this in the existing helper makes enforcement practical: forbid other scratch-root discovery and allocation spellings.

I did not find a scoped test that invokes Cargo to obtain another project binary, nor a confirmed write into another tool's cache or build directory. The direct environment and child-process restrictions already remove much of that class here. The main remaining ambient dependencies are execution scheduling, real-time synchronization observations, filesystem allocation behavior and the test scratch root.
