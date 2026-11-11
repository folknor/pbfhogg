The main problems are false-positive refusal tests, verdicts that trust the harness's own incomplete reports, host-dependent descriptor scenarios, and validation that allocates memory proportional to planet-sized outputs.

This is a source review. I made no edits and ran no builds, tests, benchmarks or verification commands. Costs below are derived from the code, not measured timings. Findings are unranked.

1. **The bad-request executable test can pass when request validation breaks.**

   [bad_requests_are_refused_quietly](/home/folk/Programs/pbfhogg/crates/harness/tests/exe.rs:510) gives every malformed request a nonexistent marker FIFO. If duplicate-key rejection, numeric validation or binding validation stops working, execution can advance to `MarkerFifo::open`, fail there, and still produce every result this test expects: exit 2, empty output, a nonempty refusal reason and untouched roles.

   The parser unit tests provide some independent coverage, but this executable test does not establish that the executable rejected the intended defect.

   Give each malformed request an otherwise working launch, including a live FIFO. Check the refusal category or relevant reason, and include a valid control using the same launch setup. Mutation tests that disable individual validation branches would mechanically establish that these cases fail for their intended reasons.

2. **The executable scenario helper accepts missing coverage as success.**

   [scenario_passes_on](/home/folk/Programs/pbfhogg/crates/harness/tests/exe.rs:320) collects whatever assertion documents happen to exist and checks that every collected result says `pass`. It does not compare the call files or assertion IDs against the scenario declaration. Empty assertion arrays satisfy `.all(...)`.

   Several tests add useful checks afterward, but none makes this helper a complete protocol validator. For example, dropping a scenario assertion can leave its executable test green while removing the capability proof it was meant to exercise.

   Brokkr's artefact consumer does validate declared calls and results. That protection is outside these repository checks. The local helper should compare exact call and assertion inventories, preferably through a shared protocol validator. Enforce with negative fixtures containing missing, duplicate, misplaced and undeclared results.

3. **The runner itself can return success without completing its declaration.**

   [run_launch](/home/folk/Programs/pbfhogg/crates/harness/src/runner.rs:109) computes success from the results the scenario returns and those passed to `finish_call`. Neither [finish_call](/home/folk/Programs/pbfhogg/crates/harness/src/runner.rs:567) nor finalization establishes that every declared call and assertion ran exactly once.

   This compounds finding 2: a scenario implementation can omit work, report fewer passing results, and get exit 0 locally. Brokkr catches the incomplete documents later, but the producer's exit status is misleading.

   Make execution consume declared call and assertion handles, with finalization refusing unused or repeated handles. A runtime inventory check would also work. Tests should deliberately omit and duplicate calls and results.

4. **Plans, execution and CLI counterparts describe the workload independently.**

   Each scenario separately implements `decl()`, `plan()` and `run()`. Common settings are duplicated too: [WRITE and write_json](/home/folk/Programs/pbfhogg/crates/harness/src/scenarios/mod.rs:91), header overrides and their JSON, clean attributes and their JSON, output stance, and host policy.

   The describe test checks planned call names, not that the reported options are the options executed. The context is also assembled from `CallSpec`; it does not independently observe all behavior inside the call body. A changed implementation can therefore execute a different workload under an unchanged plan identity.

   Replace this descriptive duplication with typed workload specifications that generate the plan and execute the call. Generate counterpart arguments from the same specification where semantics permit. Custom scenarios can retain explicit observers. Types can enforce the consolidated representation; tests should check the remaining translation boundaries. Arbitrary handwritten behavior cannot be proven equivalent to prose mechanically.

5. **The requirement-table check reads a formatting convention, not brokkr's configuration.**

   [required](/home/folk/Programs/pbfhogg/crates/harness/src/describe.rs:57) scans the root `brokkr.toml` for an exact table header and extracts lines beginning with a double quote. It does not parse TOML or compose included configuration.

   The current spelling works. A valid change to single-quoted or bare keys, or moving some requirements into an included file, can make entries disappear from this check. The nonempty assertion prevents a completely empty scan, but not partial omission.

   Parse the effective configuration through a defined interface, or put the capability requirements in one structured file consumed by both sides. Enforce with equivalent-format and included-file fixtures. The current claim that this test keeps declarations aligned with brokkr's requirement table is narrower than it reads.

6. **The descriptor scenarios depend on the inherited process, despite the host-independent testing claim.**

   [Nofile::Soft](/home/folk/Programs/pbfhogg/crates/harness/src/process.rs:93) refuses when the inherited hard limit is below 1024. Executable tests expect those scenarios to pass, so a valid restricted host fails the suite.

   More subtly, [nofile-altw-external](/home/folk/Programs/pbfhogg/crates/harness/src/scenarios/nofile.rs:355) expects exactly three workers from the descriptors remaining after ambient opens and global-pool initialization. Its own documentation acknowledges failure when the launch already holds too many descriptors. Most launches also use machine parallelism and the ambient Rayon pool, whose size varies by host.

   This makes the “No test depends on the host” claim in [reference/testing.md](/home/folk/Programs/pbfhogg/reference/testing.md:207) false for this scope.

   Split the proof: inject exact descriptor allowances to test worker selection deterministically, and keep a small child-process contract test for real `getrlimit`, descriptor counting and admission. Detect unsupported hard limits explicitly. Enforce deterministic scenarios with fixed resources and pool sizes, plus tests for restricted launch conditions.

7. **Descriptor exhaustion does nearly a thousand real opens to test an allowance decision.**

   [fill_descriptors](/home/folk/Programs/pbfhogg/crates/harness/src/scenarios/nofile.rs:301) opens the private directory repeatedly until only eight descriptors remain. With few descriptors initially open, this means roughly one thousand opens and closes per execution. Tier1 runs the harness under three sweeps.

   The helper also silently opens nothing if the process already has fewer than eight descriptors free. The fixture then no longer establishes its advertised starting condition. Counting `/proc/self/fd` assumes procfs availability and stable descriptor activity.

   Inject the allowance for the command-level refusal test. Test the real counting mechanism separately in a child with controlled descriptor ownership, and assert the established free count. A fixture type carrying the verified allowance can enforce the precondition structurally.

8. **Repeatability validation uses two output-sized allocations.**

   [outputs_equal](/home/folk/Programs/pbfhogg/crates/harness/src/scenarios/repeat_cat.rs:106) reads both complete outputs into memory before comparing them. For two outputs of size `S`, validation adds approximately `2S` heap memory and reads `2S` bytes after the transformations.

   At planet scale this can OOM a successful streaming operation. It also inflates process-level measurements with validation memory and I/O outside the call intervals.

   Compare lengths and then compare bounded chunks. Exact byte equality does not require whole-file residency. Enforce with a bounded comparison helper and a source rule preventing whole-file reads in scale-facing scenario assertions.

9. **Seed replacement reads an entire PBF to distinguish it from a tiny seed.**

   [SeededReplace::run](/home/folk/Programs/pbfhogg/crates/harness/src/scenarios/seeded_replace.rs:129) reads each destination into a `Vec`. For the replacement output, it needs only to establish that the file exists and does not contain the seed.

   This adds one output-sized allocation and full read. Brokkr already has the appropriate shape in [seed::holds](/home/folk/Programs/brokkr/src/pbfhogg/library/seed.rs:125): inspect length, then read at most the seed length plus one.

   Use a bounded seed-state helper in the harness. Enforce its use with a source rule and test wrong types, unreadable paths, exact seeds and larger files. The same bounded approach should cover initial seed validation in `check_bindings`.

10. **Absence checks treat every filesystem error as proof that nothing exists.**

    [absent](/home/folk/Programs/pbfhogg/crates/harness/src/scenarios/mod.rs:208) returns true for any `symlink_metadata` error. [check_bindings](/home/folk/Programs/pbfhogg/crates/harness/src/runner.rs:185) uses the same approach for initially absent written roles.

    Permission failures, filesystem errors and unsearchable parents are therefore treated like `NotFound`. Cancellation and refusal assertions can claim an output is absent when they could not inspect it.

    Return `Result<bool>` and recognize only `NotFound` as absence. Other errors must refuse inspection or fail the assertion. Enforce through one filesystem-state helper and injected-error tests; permission-only tests would themselves depend on the running user's privileges.

11. **FIFO reader threads leak when the test unwinds before `finish`.**

    [Fifo](/home/folk/Programs/pbfhogg/crates/harness/tests/exe.rs:88) has no `Drop` implementation. Its stop flag is set only by `finish`. A panic in launching or waiting drops the `JoinHandle`, detaches the reader, and leaves it polling forever with its descriptor open.

    In the merged `all` binary, later tests inherit the cost: a thread waking about every millisecond, an open descriptor and retained memory. Reader errors are also swallowed into the polling loop rather than reported.

    Give the reader RAII shutdown and joining, including unwind paths, and distinguish expected nonblocking reads from real errors. Apply equivalent ownership to child processes so any early unwind kills and reaps them. Enforce with deliberate-unwind tests that observe fixture termination.

12. **Polling and timeout policy are duplicated and inconsistent.**

    The executable runner polls children every 5 ms with a 15-second deadline. FIFO readers poll every millisecond. The concurrent scenario's gate waits up to [60 seconds](/home/folk/Programs/pbfhogg/crates/harness/src/scenarios/concurrent_cat.rs:63), beyond both the executable helper's 15 seconds and check's 20-second watchdog.

    Normal runs do not sleep through those full deadlines, but each child incurs polling wakeups and up to one polling interval of completion delay. A broken rendezvous gets killed before it can report its own timeout diagnosis.

    Use event-driven readiness for FIFO and child completion, and give a launch one explicit deadline budget shared by its internal waits. Real process and kernel deadlines still need real monotonic time; they should not all be replaced by a fake clock. Enforce deadline relationships and exercise the gate's failure path with a short injected budget.

13. **Executable fixtures create unused inputs and construct JSON through path substitution.**

    [scenario_passes_on](/home/folk/Programs/pbfhogg/crates/harness/tests/exe.rs:320) always writes both indexed and raw PBF fixtures. Most scenarios need no raw input; generated-input and refusal scenarios need neither external fixture. That repeats compression and filesystem work for every scenario under each harness sweep.

    [RunDir::new](/home/folk/Programs/pbfhogg/crates/harness/tests/exe.rs:142) substitutes filesystem paths into JSON text without JSON escaping. A checkout path containing a quote or backslash makes otherwise valid fixtures malformed; non-UTF-8 paths are lossy or rejected elsewhere.

    Build only requested inputs and serialize a structured request with `serde_json`. Keep malformed-request mutations separate from valid fixture construction. Enforce with paths containing JSON-sensitive characters and fixture constructors that require an explicit input recipe.

14. **New scenarios and test files can silently escape the suite.**

    The current `exe.rs` is declared and all twelve registered scenarios have executable tests. I found no currently orphaned test file in this scope.

    Nevertheless, [autotests = false](/home/folk/Programs/pbfhogg/crates/harness/Cargo.toml:13) means a new file without a module or target declaration never compiles. Separately, adding a scenario to `scenarios::all()` does not require adding an executable test. The gate inventories compiled tests, so it cannot discover either missing obligation from that inventory alone.

    Add a source-reachability check for test files and an exact registry-to-executable-coverage check for scenarios. Preserve specialized assertions, but drive basic scenario execution from structured case data whose IDs are checked against the registry.

15. **The diagnostic-closure assertion samples late events; it does not prove quiescence.**

    [late_report](/home/folk/Programs/pbfhogg/crates/harness/src/runner.rs:607) checks counters once before writing `scenario.json`. An event arriving afterward cannot change the recorded result.

    The collector comment correctly says prevention rests on runner ordering. The scenario assertion's wording is broader: “no call diagnostic arrived after its call document.” If the ownership premise regresses and work escapes, a sufficiently late diagnostic can evade that assertion.

    Keep diagnostic delivery within an execution scope whose work is joined before document finalization. Where the API permits independently retained diagnostic handles, complete future silence cannot be proven by a finite counter sample. Enforce owned-task completion structurally and test delayed emission across the closure boundary; describe the residual observation limit explicitly.

16. **Diagnostic collection is unbounded even though individual messages are truncated.**

    [Collector::event](/home/folk/Programs/pbfhogg/crates/harness/src/diag.rs:96) formats the complete message before truncating it and retains every event in a vector. Finalization then materializes JSON values and serialized documents.

    A noisy large-data run can consume substantial memory and write an artefact that brokkr subsequently refuses under its [16 MiB document limit](/home/folk/Programs/brokkr/src/pbfhogg/library/artefact.rs:21). The bound is enforced after the producer has paid the allocation and serialization costs.

    Introduce bounded formatting and an explicit collection budget with an overflow verdict. Silent truncation of event counts would itself mislead. Enforce with oversized-message and many-event tests.

17. **The harness's local checks do not exercise its profiling configurations.**

    [checks.toml](/home/folk/Programs/pbfhogg/brokkr.d/checks.toml:46) tests ordinary harness configurations; it excludes hotpath features for understandable output-contamination reasons. But [run_from_env](/home/folk/Programs/pbfhogg/crates/harness/src/runner.rs:78) activates a profiling guard from `HOTPATH_OUTPUT_PATH`, and brokkr builds separate instrumented feature graphs for profiling.

    The dependency check against hotpath's child-process profiler is useful, but does not establish quiet describe mode, quiet unprofiled execution of an instrumented binary, report placement or report validity. The executable helper always removes profiling variables and never tests an activated profile.

    Add a dedicated profiling lane with small generated inputs and explicit expectations for describe, inactive and active modes. This can mechanically cover the feature graphs without contaminating ordinary CLI tests.

18. **Brokkr always builds a release CLI, including runs with no CLI counterpart.**

    [LibraryHarness::build](/home/folk/Programs/brokkr/src/pbfhogg/binary.rs:751) builds the configured harness, then a release/default-feature CLI whenever this is a real run. Consequently, `page-cache-keep` and the descriptor-refusal scenarios build a CLI they never use. A debug harness run can also trigger a separate release build.

    This is documented and deliberate in brokkr's library-internals reference, rather than an accidental nested build inside the tests. Its cost is one extra build invocation and, when release artifacts are cold, compilation and linking of a separate configuration. I did not measure that cost.

    Resolve build needs from scenario metadata before acquiring executable handles, then build only required artifacts. For matching configurations, build both required workspace binaries together and retain Cargo-reported paths. Enforce build planning with tests for listing, dry runs, scenarios without counterparts and scenarios with counterparts.

The executable tests already use `env!("CARGO_BIN_EXE_pbfhogg-harness")`, which is the correct way to find the binary their standard test build produced. I found no nested Cargo invocation in them, no environment mutation or global-pool initialization in the shared parent test binary, and no harness writes into another tool's cache or build-output directory. Test scratch is repository-owned under `data/test-scratch`; scenario outputs and private staging are placed in caller-supplied run directories.
