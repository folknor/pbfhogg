I found one substantial false-coverage defect in the truncation sweep, several guards that can silently lose coverage, and recurring costs caused by subprocess polling, duplicated source checks and ambient host policy. These findings are unranked.

This was a read-only audit. I did not run builds, tests or benchmarks, so costs below are derived from the code rather than measured wall times.

1. **The truncation sweep can pass without reading the truncated input.**

   [cli_truncation_sweep.rs:29](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_truncation_sweep.rs:29) allocates one output path for the entire sweep. [run_and_assert:133](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_truncation_sweep.rs:133) neither removes it nor requests overwrite. Once a tolerated truncation publishes an output, subsequent output-producing cases can refuse that existing destination. For hard truncations, the assertion accepts any unsuccessful exit, so those refusals count as successful corruption coverage.

   The same assertion accepts usage errors and most abnormal terminations. Checking stderr for `panicked at` does not establish that the decoder returned the expected error.

   Give every cell a fresh output destination. Assert the runtime-failure exit code and a framing error identifying the truncated input or location. Add a valid-input control for each command configuration. Enforce this through a shared corruption-test helper whose result requires an expected error category, rather than a generic failure assertion.

2. **The truncation sweep's claimed command coverage is not enforced.**

   Its opening comment says it drives “every command”, but the body invokes six hardcoded commands. Nothing compares that selection with the command tree or records which read paths each representative covers.

   This is a false claim today. Either make it an explicitly representative read-path matrix, with named coverage obligations, or enumerate commands and require every one to have a case or an explicit exemption. A build-wired inventory comparison can enforce the latter.

3. **A large subprocess matrix is packaged as one watchdog unit.**

   That same test runs six children per selected offset. With `B` blobs, its construction selects at most `8B + 12` offsets before deduplication, giving up to `6 * (8B + 12)` invocations. Every child has an eight-second timeout, while the entire test has brokkr's twenty-second watchdog.

   Consequently, a few slow cells can cause the outer watchdog to kill the test before its own timeout explains the failure. The matrix also repeats across the CLI check sweeps.

   Split the matrix into independently named command cases or bounded shards. Keep comprehensive byte-boundary behavior at the library reader level, then use CLI cases to prove routing and error propagation. Enforce the intended cell inventory and maximum shard size mechanically.

4. **`CliInvoker::run` adds a polling delay to every ordinary subprocess.**

   [invoker.rs:62](/home/folk/Programs/pbfhogg/crates/cli/tests/invoker.rs:62) sets a twenty-millisecond polling interval. A child that completes just after an unsuccessful `try_wait` is not observed until the sleep ends. This applies to help, version, usage refusals and every matrix cell.

   The observation delay is up to twenty milliseconds per poll, before scheduler delay. Hundreds of quick subprocesses turn that into seconds per test; repeated sweeps pay it again.

   Use blocking child completion with an independent timeout mechanism. An invoker test using a controlled child can enforce completion, timeout and output-draining behavior without encoding a tight host-speed assertion.

5. **The two invocation paths have different cleanup guarantees.**

   [invoker.rs:347](/home/folk/Programs/pbfhogg/crates/cli/tests/invoker.rs:347) implements `run` separately from `spawn` and `RunningCli::wait`. The former holds a raw `Child`; the latter starts with a cleanup owner but removes the child from that owner before waiting.

   A `try_wait` error therefore panics without reliably killing and reaping the child in either path. `RunningCli::drop` kills and reaps, but does not join its capture threads. Capture read errors and capture-thread panics are also converted into shortened or empty output.

   Make one owner retain the child and capture threads through all exit paths, and implement `run` through it. Cleanup should reap and join before fixture directories disappear. Return capture failures explicitly. Fault-injected invoker tests can enforce these properties.

6. **The invoker inherits ambient environment and working directory.**

   [CliInvoker::new:125](/home/folk/Programs/pbfhogg/crates/cli/tests/invoker.rs:125) creates an ordinary `Command`, preserving the parent's environment, stdin and working directory unless a test overrides them.

   Inherited `PBFHOGG_TEST_*` values can arm unrelated children, redirect hook reports or park commands. `BROKKR_MARKER_FIFO` can make unrelated test children write to an external instrumentation destination. Rayon settings can change their execution policy. The harness does not establish that these effects belong to the test.

   Give children an explicit environment policy: remove instrumentation and hook variables by default, establish execution settings, and permit deliberate overrides. Default stdin to null and make working-directory ownership explicit. Test the command builder's environment policy and forbid raw CLI construction outside the invoker.

7. **CLI children use the real machine even when their fixture preparation uses a fixed host.**

   [host.rs:20](/home/folk/Programs/pbfhogg/crates/cli/src/host.rs:20) constructs `MachineProbe` resources and an ambient executor. Meanwhile, [common::test_host:1711](/home/folk/Programs/pbfhogg/crates/lib/tests/common/mod.rs:1711) supplies fixed resources only to in-process fixture operations.

   Thus synthetic CLI tests can select different resource-dependent paths on different hosts. Concurrent test children can each size pools against the machine rather than the test runner's aggregate budget. This is a source of variable cost and watchdog failures.

   Separate process setup from command execution. Hand command execution a host and resolved execution policy; use a fixed policy for broad behavioral tests and retain subprocess tests for actual process integration. Signatures can enforce the boundary. Explicit child configuration can cover cases that must remain subprocess tests.

8. **`test_host()` mutates the shared process's descriptor limit during fixture creation.**

   Its `Once` raises `RLIMIT_NOFILE` on whichever test first requests a host. Every sibling test and later child inherits the result.

   The documentation acknowledges the shared state, but the timing still depends on test order. `NoProcessActions` in the returned host does not describe this setup side effect.

   Remove the mutation from the fixture factory. If the test executable needs a larger limit, make that a declared runner setup operation. Tests of descriptor admission should continue to receive their explicit child limit. A source rule can prohibit process mutations in fixture factories.

9. **Cancellation and replacement hook setup fails open.**

   In [signals.rs:185](/home/folk/Programs/pbfhogg/crates/cli/src/signals.rs:185), malformed cancellation-pause specifications silently return. Ready-file, acknowledgement-file and count-file write failures are discarded. Input replacement similarly returns on incomplete or invalid configuration, and its report writer discards write errors.

   Publication and merge-spill hooks already fail explicitly for comparable setup errors. The cancellation and replacement paths instead turn configuration mistakes into ordinary execution, missing observations or delayed readiness failures.

   Parse all hook configuration into validated types before dispatch. Reject incomplete combinations, invalid ordinals and failed control-file writes. Give the invoker typed hook methods so tests do not repeatedly construct an unvalidated string protocol. Negative configuration tests can enforce refusal.

10. **“Product builds never enable test hooks” is a convention.**

    [Cargo.toml:82](/home/folk/Programs/pbfhogg/crates/cli/Cargo.toml:82) exposes a normal feature that enables environment-controlled pauses, faults, file writes and input replacement in the production executable. Comments and `reference/testing.md` say product builds never enable it, but this scope contains no structural distinction between a test driver and a distributable CLI with that feature enabled.

    Put these controls in a dedicated test executable, or enforce their absence in the release packaging pipeline by inspecting the resolved feature graph. The latter checks release artifacts; it does not make arbitrary hook-enabled builds impossible.

11. **Permission tests silently lose coverage under root and assume UID predicts access.**

    [cli_scratch.rs:136](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_scratch.rs:136), [cli_scratch.rs:248](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_scratch.rs:248) and [cli_scratch.rs:681](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_scratch.rs:681) return without announcing the omitted case. [cli_output_contract.rs:1105](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_output_contract.rs:1105) similarly removes an unwritable-directory case.

    These omissions violate the documented skip convention today. UID is also only a proxy for access behavior: capabilities and filesystem policy can change what permission bits accomplish.

    Probe whether the fixture actually denies the operation, announce omitted coverage, and use existing publication fault injection for portable error-path contracts. Keep separate permission integration tests for genuine filesystem behavior. A shared refusal-fixture helper can enforce detection and reporting.

12. **Permission restoration is not consistently unwind-safe.**

    In [cli_scratch.rs:258](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_scratch.rs:258), a directory becomes non-writable before a loop containing assertions; restoration occurs afterward. An assertion failure bypasses restoration and can prevent `TempDir` cleanup from removing its contents.

    Backup-removal testing has the same general ownership problem: permissions and namespace changes are managed through later procedural cleanup.

    Use a permission-restoration guard that remains alive inside the scratch owner. Restore permissions during unwinding before recursive cleanup. A helper test that deliberately unwinds can enforce this.

13. **The timestamp test does not prove the timestamp was updated to the current time.**

    [cli_derive_changes.rs:859](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_derive_changes.rs:859) accepts any timestamp beginning with `20`. A hardcoded date from 2000 would satisfy its “recent ISO date” assertion.

    Production obtains the value directly from `SystemTime::now` in [lib.rs:4565](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:4565). There is no injection point at the command-execution boundary.

    Resolve time at the process boundary and hand the transformation an explicit timestamp. Test exact per-element values with a fixed instant, including which elements are updated. Keep a small process-level test for clock wiring if needed. The function signature and exact-value tests can enforce this.

14. **Several command-table checks still discover capabilities by parsing rendered help.**

    Examples include [cli_flag_contract.rs:717](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_flag_contract.rs:717), [cli_flag_contract.rs:1257](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_flag_contract.rs:1257) and [cli_output_contract.rs:1425](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_output_contract.rs:1425).

    The command list comes from Clap, but capability membership still depends on spellings and formatting such as `-j, --jobs`, a line beginning with `--force`, or a substring containing `--tmp-dir`. A formatting change can silently remove a command from an inferred set. The jobs check only requires at least five matches and checks membership in one direction.

    These checks also start a child per command or row, sometimes repeatedly for the same command.

    Inspect built Clap arguments for capability membership, compare sets in both directions, and reserve rendered-help tests for presentation. A shared command-tree accessor makes this enforceable. The output-contract module's claim that its command list comes from binary help is already stale.

15. **The promise guard's unsupported-architecture fallback does not compile as advertised.**

    [guard.rs:66](/home/folk/Programs/pbfhogg/crates/cli/tests/guard.rs:66) defines `AUDIT_ARCH` only for x86_64 and aarch64. [program:191](/home/folk/Programs/pbfhogg/crates/cli/tests/guard.rs:191) references it unconditionally.

    On another Linux architecture, the runtime `supported()` check cannot provide the documented unguarded fallback because compilation fails first.

    Compile the supported implementation behind an architecture gate and provide a separate unsupported implementation. Cross-target compile checks can enforce that fallback without requiring execution on every architecture.

16. **Guard coverage can disappear from an otherwise green run.**

    [guard.rs:121](/home/folk/Programs/pbfhogg/crates/cli/tests/guard.rs:121) converts every unsuccessful action-availability probe into `false`. `apply` then does nothing. Only the control test reports this, using `skipped:` rather than the common skip mechanism, and passing-test output is normally hidden.

    Unsupported kernels are legitimate, but probe failure and lack of support should not collapse into an invisible Boolean.

    Return a reasoned availability result. Record guarded versus unguarded execution in the test run's coverage accounting. Hosts may still skip; the run should visibly state which guarantee was exercised. This needs runner-visible accounting, not another assertion in an otherwise passing test.

17. **The library text rule stops recognizing stale command advice after a command rename.**

    [names_subcommand:720](/home/folk/Programs/pbfhogg/crates/cli/src/library_text_rule.rs:720) recognizes `pbfhogg <word>` only if `<word>` exists in today's command tree. Remove or rename a command, and old advice naming it stops matching the rule.

    This is precisely the stale text the module says it prevents. Its allowlist-use accounting is sound, but command-name recognition remains keyed to a changing vocabulary.

    Detect invocation-shaped advice independently of current command membership, with narrowly checked exceptions, or structurally separate diagnostic prose from frontend advice. A rename/removal regression fixture can enforce that stale advice remains forbidden.

18. **The library text scanner's exclusions can hide production strings.**

    [library_literals:662](/home/folk/Programs/pbfhogg/crates/cli/src/library_text_rule.rs:662) scans files independently, then globally excludes paths or subtrees named by test-only module declarations.

    That is not equivalent to production reachability. If a source file is referenced by both a test-only module and a production module through `#[path]`, the test declaration can exclude its production literals too. Path handling also approximates Rust module resolution. I did not establish a current production violation hidden this way.

    Use parsed module reachability and exclude a file only when every relevant inclusion is test-only, or apply a simpler conservative lexical policy with explicit exemptions. Test shared-file inclusion and path resolution. Structural source analysis can enforce this; a global path exclusion cannot.

19. **Source-only checks are repeatedly coupled to feature-specific Rust test builds.**

    The library text test reads 203 Rust files totaling approximately 6.53 MB in this checkout. It implements its own lexer, cfg interpretation and module exclusion machinery. It runs in the CLI unit-test target across the `all`, `consumer` and `musl` sweeps, although its source policy is largely identical across those shapes.

    Markdown coverage tests add another custom parser inside the CLI unit-test build.

    Consolidate source-policy analysis into one check stage, using one maintained Rust parser where Rust syntax matters. Export the command schema once for checks requiring Clap data. Keep adversarial parser fixtures there. Wiring this into brokkr enforces execution without repeating the same source scan per target shape.

20. **Generated reference data is maintained as handwritten duplication.**

    [io_reach.rs:175](/home/folk/Programs/pbfhogg/crates/cli/src/io_reach.rs:175) compares a Markdown matrix with text generated by `matrix()`. [lib.rs:5853](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:5853) independently parses the short-flag table back into the vocabulary. The larger documentation checker reconstructs command and option inventories from Markdown.

    These checks catch drift, but maintainers still edit lists the code already knows and maintain parsers for the duplicate representation.

    Generate the command, option, vocabulary and I/O inventory sections while retaining authored explanatory prose. Enforce generation with a check mode. Any writer should compare bytes before replacing a file so regeneration does not create needless mtime changes.

21. **Scratch ownership is appropriate, but its implementation and location discovery are duplicated.**

    [test_scratch.rs:12](/home/folk/Programs/pbfhogg/crates/cli/src/test_scratch.rs:12) duplicates the integration helper and another harness implementation. The comment that CLI unit tests “cannot mount” the integration file is not a Rust restriction.

    Root discovery uses the nearest ancestor containing `Cargo.lock`. That assumes an in-place checkout at the compile-time manifest path and that a lockfile identifies the intended workspace. A relocated test executable fails; an intervening lockfile changes the selected scratch root. Numerous tests additionally require the resulting absolute paths to be UTF-8.

    Put scratch allocation in one test-support component with an explicit workspace or runner-owned scratch root. Continue using `data/test-scratch/` for repository-local runs; each fixture owns its `TempDir`. Pass paths as `Path` or `OsStr` instead of converting them to strings. Enforce one allocator and prohibit lossy or panicking path conversion in invocation helpers.

22. **The scratch text rule forbids certain constructors rather than enforcing ownership.**

    [lints.toml:318](/home/folk/Programs/pbfhogg/brokkr.d/lints.toml:318) bans common ambient-temp spellings, but permits `_in` constructors anywhere. It does not prevent `TempDir::new_in` from targeting a build directory, tool cache or another unowned ignored directory, nor does it prevent fixed paths constructed through filesystem calls.

    I did not find a current write into another tool's ignored state in this scope. The enforcement claim nevertheless exceeds what the pattern establishes.

    Restrict allocation entry points to the shared scratch implementation. Have tests derive child paths from an owned scratch object. A wrapper type and constructor restrictions provide stronger ownership enforcement than an expanding spelling blacklist.

23. **Test assembly has no source-to-build completeness check.**

    [all.rs:10](/home/folk/Programs/pbfhogg/crates/cli/tests/all.rs:10) and `reference/testing.md` explicitly acknowledge that undeclared test files vanish because `autotests = false`. I compared the current files with its module declarations: every current top-level CLI test file is declared.

    This is an unenforced invariant, not a current orphan. Coverage accounting cannot discover a file that never compiled.

    Add a pre-build check requiring every test source to be reachable from a declared test target or explicitly classified as support code. Check both directions. The existing inventory utility also discards package identity when comparing names, so preserve multiplicity and use explicit move mappings during migrations.

24. **External oracle tests bypass the bounded subprocess machinery.**

    [cli_sort.rs:550](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_sort.rs:550), [cli_digest.rs:1119](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_digest.rs:1119) and [cli_apply_changes_osmium.rs:912](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_apply_changes_osmium.rs:912) invoke PATH-resolved osmium directly, without their own bounded child owner.

    Sort and digest treat any initial spawn error as “not found”; the apply-changes test correctly distinguishes `NotFound`. A stalled oracle relies on the outer watchdog, which can kill the test process without cleaning up its child. These tests do prove repository behavior when they run; their problem is uncontrolled oracle execution and inconsistent refusal reporting.

    Move the fixture-based comparisons into brokkr's verification machinery as the documented migration intends, or share a bounded external-tool runner with explicit version reporting and kill/reap ownership. Enforce that direct oracle spawning occurs only in that runner.

25. **The signal reporter holds a mutex across a potentially blocking stderr write.**

    [signals.rs:124](/home/folk/Programs/pbfhogg/crates/cli/src/signals.rs:124) holds `FINISHED` while printing the stopping notice. [finish:158](/home/folk/Programs/pbfhogg/crates/cli/src/signals.rs:158) takes that same lock before returning the command outcome.

    A full, still-open stderr pipe can therefore block completion behind the notice thread. The dedicated signal waiter still permits a second signal to terminate the process, but ordinary completion is coupled to that reporter's blocking write. The invoker drains stderr and cannot expose this state through its normal API.

    Make reporter lifecycle and completion ownership explicit, with a bounded or nonblocking diagnostic handoff. Add a subprocess test that deliberately holds a full stderr pipe and controls when it drains. This can be enforced by that test and by keeping blocking output outside completion-state locks.

The existing Cargo binary lookup is correct: [pbfhogg_bin](/home/folk/Programs/pbfhogg/crates/cli/tests/invoker.rs:637) uses compile-time `CARGO_BIN_EXE_pbfhogg`. I found no nested Cargo build in this scope, no currently undeclared CLI test file, and no generated-file writer here that rewrites identical bytes. The process-wide CLI state is currently exercised through separate children; I did not find inline tests calling `run()` concurrently and racing those globals.
