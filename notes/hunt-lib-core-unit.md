Read-only reconnaissance. I made no changes and ran no builds, tests or benchmarks. Costs below come from explicit waits and loop counts in the source, rather than measured run times. Findings are unranked.

1. **The output unit tests share one mutable invocation.** [output.rs:3722](/home/folk/Programs/pbfhogg/crates/lib/src/output.rs:3722) stores the publication token in a process-wide `OnceLock`. Tests share its publication log, execution lock, counters and lifecycle. Independent filesystem scenarios therefore serialize publication and retain each other's publication records. The comment's promise that the token is never cancelled depends on every future caller remembering it; sealing it would also affect subsequent tests.

   Give each scenario an owned fixture containing its invocation and host. Pass that fixture to staging and publication helpers. Mechanically enforce this with a ban on static `Invocation` values in test code, including values nested inside `OnceLock`.

2. **Test hosts are deliberately leaked to manufacture static lifetimes.** [host.rs:1949](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:1949) implements `entered_for_tests` and `entered_leaked` with `Box::leak`. Every call retains the host and its invocation for the lifetime of the test binary, including instrumentation and fault-plan state. Repeated fixture construction accumulates allocations that ordinary ownership would release.

   Keep the host in the test fixture and borrow its entered view, or use a closure helper that scopes both. Breaking these helper APIs is the appropriate fix. A lint against `Box::leak` in test machinery can prevent recurrence.

3. **The supposedly fixed test host still selects the global Rayon pool.** [host.rs:1932](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:1932) fixes resource probes but sets `executor: Executor::Ambient`. Writer unit tests also explicitly select `Executor::Ambient`. The pool remains process-wide, sized and initialized from the runner's surroundings, and shared with other tests.

   This makes the statement in [testing.md:207](/home/folk/Programs/pbfhogg/reference/testing.md:207) that library tests see the same machine everywhere too broad today. Supply a fixed-width, owned executor to ordinary tests. Keep explicit ambient-executor coverage separate. Enforce the distinction through fixture types and a source rule allowing `Executor::Ambient` only in named tests of that behavior.

4. **Public reader constructors bring machine probes into tests that do not request them.** The default worker selection in [reader.rs:555](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:555) and [pipeline.rs:763](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:763) falls back to `MachineProbe` when resources were not attached. Several inline tests construct a public reader and start parallel iteration without fixing its decoder count.

   Their pool size and thread startup cost consequently depend on the host, despite the fixed-host convention. Give public readers a resource-injection builder, and make unit fixtures use it or explicitly set `decode_threads`. A structural fixture API plus a lint on unconfigured parallel test readers can enforce this. Public defaults can retain their convenient machine-based behavior.

5. **Several inline tests still consume the doc-example PBF.** [blob.rs:1247](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob.rs:1247), [blob.rs:1299](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob.rs:1299), [blob.rs:1375](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob.rs:1375), [reader.rs:1424](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1424) and [reader.rs:1941](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1941) read `tests/test.osm.pbf`.

   These are meaningful behavior tests, but their fixtures depend on Cargo's working directory and a separately maintained binary file. The truncation test even discovers the fixture's layout through a production `BlobReader` before testing other readers. The documented exception for this file is the doc examples; its inline use extends beyond that exception.

   Generate a small fixture in each scenario's owned scratch directory. For framing tests, retain independent byte-level framing helpers. Ban the fixture path in executable unit-test code while permitting doc examples.

6. **Timeout helpers leave unfinished work running in the shared test process.** [ordered_pool.rs:1434](/home/folk/Programs/pbfhogg/crates/lib/src/ordered_pool.rs:1434) discards its spawned thread's handle immediately. On timeout, its closure and any pool it owns continue running. The helpers in [pipeline.rs:922](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:922) and [reader.rs:1723](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1723) likewise detach their threads when timeout handling panics.

   A regression can therefore fail one test while leaving blocked workers, CPU activity and fixture handles behind for subsequent tests. An arbitrary deadlocked Rust thread cannot be safely killed or guaranteed joined in-process. Put irrecoverable hang probes in a dedicated child test process, with the parent killing and reaping it on timeout. Keep recoverable rendezvous tests in-process with release-and-join guards. Enforce this through one shared hang-test harness and a ban on discarded test thread handles.

7. **Two internal watchdogs cannot deliver their promised diagnostics under standard checks.** The pipeline and reader helpers above wait 30 seconds, while `brokkr check` kills an individual test after 20 seconds. Their comments promise a local, descriptive deadlock failure, but the standard check's watchdog wins first.

   Use a shared timeout policy below the outer watchdog, or let a process-owning hang harness be the sole timeout authority. Enforce the relationship in configuration validation. Shortening these waits alone does not fix the detached-work problem.

8. **The stalled-transform test has an unbounded failure-path leak.** [pipeline.rs:1073](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:1073) spawns an orchestration thread without retaining its handle. Its transform spins on `release`; the test sets that flag only after observing `stalled`. If the initial observation times out and panics, a worker that reaches the transform afterward can spin indefinitely.

   Use an RAII guard that releases the transform and joins its owner on every exit path, following the nearby `ReleaseTeardowns` pattern. A common parked-worker fixture can make cleanup structural. A discarded-handle lint would also catch this instance.

9. **Some “blocked” tests never establish that the operation reached its blocking point.** Examples include [reader.rs:1524](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1524), [reader.rs:1611](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1611), [reader.rs:1647](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1647) and [pipeline.rs:1347](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:1347).

   Sending “ready” before calling `acquire` does not establish that `acquire` ran. Waiting 50 ms without a result can mean the child was descheduled. In the queue-close test, the child can start after closure and pass without testing a wakeup at all. The admission-gate test can also fail valid code when release occurs before the delayed child attempts acquisition.

   Add a per-instance observation at the actual wait transition, then release or close only after that event. These four tests currently spend at least 200 ms collectively per build shape on absence observations. Enforce the corrected behavior with a common handshake helper and require negative concurrency assertions to identify their observed wait transition.

10. **Sleep-based scheduling makes several tests slower without proving their intended ordering.** Concrete cases include:

   - [ordered_pool.rs:1543](/home/folk/Programs/pbfhogg/crates/lib/src/ordered_pool.rs:1543): twenty repetitions with a 5 ms delay, nominally 100 ms plus twenty pool runs. No assertion establishes that the later failure actually finished first.
   - [ordered_pool.rs:1719](/home/folk/Programs/pbfhogg/crates/lib/src/ordered_pool.rs:1719): a 200 ms slow head task.
   - [ordered_pool.rs:2194](/home/folk/Programs/pbfhogg/crates/lib/src/ordered_pool.rs:2194): a 20 ms delay intended to reverse failure completion.
   - [scan/classify.rs:2259](/home/folk/Programs/pbfhogg/crates/lib/src/scan/classify.rs:2259): 24 blobs sleeping 2 ms each, nominally 48 ms, with positive stall-counter assertions that still depend on scheduling.
   - [parallel_gzip.rs:1120](/home/folk/Programs/pbfhogg/crates/lib/src/write/parallel_gzip.rs:1120): a sink sleeping 40 ms per write to distinguish dispatch from completion.
   - [threads.rs:213](/home/folk/Programs/pbfhogg/crates/lib/src/threads.rs:213): three teardown/ownership tests use 50 ms sleeps.

   Replace delays with controlled completion gates and explicit observations of the schedule being tested. An injected clock does not control OS thread scheduling; these need rendezvous. A source rule can require an explicit exemption for test sleeps, while behavioral tests verify the handshakes.

11. **The join tests pay real time to infer non-completion.** [joined_pool.rs:174](/home/folk/Programs/pbfhogg/crates/lib/src/joined_pool.rs:174) and [pipeline.rs:1198](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:1198) each wait 200 ms for an expected timeout. Their release guards are good, but delayed scheduling can let a broken early return escape the observation window.

   Add an observation at the owner's join boundary and coordinate that with the parked destructor. Retain a bounded outer watchdog only for failure containment. This removes 400 ms of deliberate waiting where both tests compile. Enforce the protocol through a shared join-test fixture.

12. **Counter tests wait for values they could supply directly.** [metrics.rs:368](/home/folk/Programs/pbfhogg/crates/lib/src/write/metrics.rs:368) sleeps 2 ms before calling `record_sync_all`, although that method already accepts elapsed nanoseconds. Pass a distinctive fixed value and assert the exact accumulated result.

   [counters.rs:474](/home/folk/Programs/pbfhogg/crates/lib/src/counters.rs:474) couples stopwatch arithmetic to real elapsed time. The blocked-operation tests below it use 10 ms release delays, retrying up to twenty times per operation: normally 30 ms collectively, up to 600 ms of deliberate delay.

   Separate elapsed-time conversion from clock acquisition, and observe actual slow-path entry for blocking tests. Exact-value tests and a ban on sleeps in counter-unit tests can enforce this.

13. **Several platform-dependent tests silently report success without exercising their subject.** Bare early returns occur for root in [scratch.rs:1207](/home/folk/Programs/pbfhogg/crates/lib/src/scratch.rs:1207), [scratch.rs:1532](/home/folk/Programs/pbfhogg/crates/lib/src/scratch.rs:1532), [scratch.rs:1592](/home/folk/Programs/pbfhogg/crates/lib/src/scratch.rs:1592), [output.rs:4000](/home/folk/Programs/pbfhogg/crates/lib/src/output.rs:4000) and [output.rs:4341](/home/folk/Programs/pbfhogg/crates/lib/src/output.rs:4341). [input.rs:760](/home/folk/Programs/pbfhogg/crates/lib/src/read/input.rs:760) returns when procfs reopening is unavailable. [output.rs:4862](/home/folk/Programs/pbfhogg/crates/lib/src/output.rs:4862) returns when `/proc` cannot provide a different-device mount.

   These violate the documented announced-skip convention today. Root is also only a proxy for permission behavior; capabilities and filesystem policy can differ.

   Test permission-error handling through an injected filesystem operation, retaining a smaller real-platform adapter test with an explicit prerequisite result. Mount and procfs coverage should announce unavailable prerequisites. Mechanically require skips through one helper and have the runner report them separately from executed passes.

14. **A regular-file classification test reads the source checkout unnecessarily.** [file_writer.rs:334](/home/folk/Programs/pbfhogg/crates/lib/src/write/file_writer.rs:334) opens the package's `Cargo.toml` using a compile-time absolute path, solely to obtain a regular file's type. A relocated test binary or removed source checkout fails this test despite working production behavior.

   Create a regular file in the test's owned scratch directory, or pass an explicit target classification to the pure decision function. A source rule can forbid executable tests from opening manifest-relative repository files except designated source-contract checks.

15. **The I/O-size source guard is both repeated work and incomplete enforcement.** [io_buffer_contract.rs:6](/home/folk/Programs/pbfhogg/crates/lib/src/write/io_buffer_contract.rs:6) recursively scans the read and write source trees at test runtime. The same scan runs in all five ordinary tier-1 build shapes.

   It catches only compacted spellings equivalent to the text `256*1024` and decimal `262144`. `1 << 18`, `128 * 2048` or a hexadecimal spelling bypass it. Conversely, comments, strings and unrelated occurrences of those digits can fail it. An emptied directory passes without proving any owner was examined.

   Move this into the once-per-check source-invariant phase, assert expected files are present, and use parsed Rust expressions or a structural transfer-buffer abstraction for the actual ownership rule. Runtime behavior tests should verify buffer alignment and boundaries. This is enforceable; the present spelling scan does not establish its broad test name.

16. **The scratch-lock source guard does not prove what its passing result claims.** [check_source_invariants.py:257](/home/folk/Programs/pbfhogg/scripts/check_source_invariants.py:257) checks that the text of `probe_lock` contains `.write(true)` and lacks `.create(`. It would accept `.create_new(true)`, a later `.write(false)`, or a qualifying call on an unrelated builder. Its function extraction also depends on textual brace layout.

   The current [probe_lock implementation](/home/folk/Programs/pbfhogg/crates/lib/src/scratch.rs:1040) does open read-write without creation. The present code is correct; the enforcement can fail open. The unit test named `the_probe_opens_the_lock_for_writing` acknowledges that local filesystems cannot distinguish the relevant modes and delegates that claim to this guard.

   Encapsulate probe-open options in one constructor and inspect the operation through a narrow adapter, or use an AST-based check of that constructor. Add negative fixtures for each forbidden option. That would enforce the property rather than the presence of one spelling.

17. **The cgroup parent-limit test never exercises parent discovery.** [host.rs:1967](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:1967) is named `cgroup_memory_budget_caps_host_and_parent_headroom`, but its “parent” case merely calls `cgroup_memory_available` twice. It cannot detect a broken ancestor walk, mount-root mapping, mountinfo decoding or membership selection.

   Production [machine_memory_available](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:1768) reads kernel files directly, leaving those decisions without a fixture-driven seam. Supply kernel-file reads through a narrow reader abstraction and test complete synthetic hierarchies, including unlimited leaves with limited parents and delegated mounts. Those tests can mechanically enforce the claimed behavior. The current test name overstates its reach.

18. **Scratch identity generation has no deterministic failure or collision seam.** [scratch.rs:200](/home/folk/Programs/pbfhogg/crates/lib/src/scratch.rs:200) directly acquires randomness and the process ID. [random_u128](/home/folk/Programs/pbfhogg/crates/lib/src/scratch.rs:431) directly calls `getrandom`. Tests of creation therefore depend on the real entropy source and cannot deliberately exercise interruption, entropy failure or repeated-name collision through this interface.

   Keep the public constructor convenient, but delegate to an internal constructor accepting identity generation. Scripted generators can verify retries, refusals and cleanup. Restrict direct entropy and PID acquisition to the adapter with a lint. This requires no global mutable test hook.

19. **Output unit tests bypass the production binding protocol.** The shorthands beginning at [output.rs:3637](/home/folk/Programs/pbfhogg/crates/lib/src/output.rs:3637) open writing sessions on unbound outputs and bind only at publication. `OutputDir::writer` even ignores its invocation argument.

   These helpers are correctly gated by `cfg(test)` and are not production-reachable. The problem is the alternate protocol they preserve: filesystem tests can keep passing while failing to exercise enlistment, invocation ownership and capability lifetime as production uses them. They also motivate the shared-token fixture above.

   Rewrite these tests around owned bound-output fixtures and remove the alternate methods. The compiler then enforces the same sequencing in tests and production.

I found no nested build-tool invocation, network dependency, or write into another tool's cache in the inspected inline tests. The findings above do not require preserving the current test helper APIs or fixture structure.
