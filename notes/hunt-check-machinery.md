The main gaps are in what the checks can see and what a green result claims. The compiled-test accounting is stronger than the source-discovery checks; several source rules certify spellings rather than behavior; and the documented pre-merge workflow does not invoke the complete gate.

I inspected the configuration, scripts, manifests, test roots, relevant decisions, and brokkr's authoritative man topics. I ran the source and document checks plus in-memory counterexamples. I did not edit files or run the full build or test suite. Findings below are unranked.

1. **A new test file can remain invisible to every check, including the complete gate.**

   All three packages disable automatic test discovery. A file omitted from `all.rs` and the manifest is never compiled, so brokkr cannot enumerate or account for its tests. [testing.md](/home/folk/Programs/pbfhogg/reference/testing.md:96) acknowledges this, but there is no corresponding source-reachability check.

   My traversal found no currently disconnected test-bearing files: 60 library files, 38 CLI files, and one harness file were reachable. That establishes today's state, not continuing enforcement.

   Keep the consolidated binaries, but add a build-wired audit that requires every Rust file under each test directory to belong to a declared target's module graph or an explicit helper exemption. Check those exemptions for staleness. This is mechanically enforceable and avoids restoring the old linking and startup cost.

2. **The documented pre-merge workflow does not run the complete gate.**

   [testing.md](/home/folk/Programs/pbfhogg/reference/testing.md:172) calls `--profile full` the before-merge tier, and its development contract says to run tiers 2-3. But `full` selects only the `all` sweep. It does not run the other feature shapes, doctests, complete execution accounting, or gate-only member checks. The profile that does those is `gate`.

   Thus someone following the durable runbook can merge without running the checks specifically intended to catch coverage loss and package-resolution defects. Legacy profiles can still exit successfully with `check passed`; they do not carry a partial-certification exit status.

   Make the merge command `brokkr check --gate`, and enforce that exact certification in CI or the merge prerequisite. Keep focused profiles as development tools with explicitly partial claims.

3. **The serial profile does not structurally guarantee serial execution.**

   [checks.toml](/home/folk/Programs/pbfhogg/brokkr.d/checks.toml:174) sets `test_threads = 1` but references `all`, which has `parallel = {}`. Brokkr documents the latter as concurrent execution of prebuilt binaries with a test budget. One libtest thread per binary is not process-wide exclusion across binaries.

   The configuration comment already says to create a parallel-free twin if a `serial::` test appears. That is an instruction waiting for somebody to remember it, while the existing profile advertises the guarantee now.

   Remove the placeholder profile or give it the correct execution shape immediately. Enforce that a profile claiming serial execution references no parallel sweep. Tests that change process state should still receive their own target where appropriate.

4. **Most textlint rules can silently lose their entire corpus.**

   Brokkr explicitly allows a `paths` glob matching nothing to pass. Some rules use `must_scan`, but many do not, including rules concerning environment access, host probes, process mutation, stdout, and scratch allocation.

   [textlint_coverage.py](/home/folk/Programs/pbfhogg/scripts/textlint_coverage.py:72) detects dead selectors, but neither ordinary checks nor the gate invoke it. Its recorded manifest under `notes/` is a migration artifact, not an enforced invariant.

   Enforce nonempty selectors in brokkr itself, including stale exclusions and sentinel membership. A pinned list of every admitted file is unnecessary maintenance for ordinary growth; selector validity and explicit ownership boundaries are the durable checks. The repository helper also duplicates brokkr's selector semantics, creating another implementation that can drift.

5. **The required counter-hook check accepts strings and unreachable code as evidence of an exit-path call.**

   [check_source_invariants.py](/home/folk/Programs/pbfhogg/scripts/check_source_invariants.py:237) strips only `//` suffixes, removes whitespace, and searches the whole CLI source for two strings.

   I reproduced a false pass using a string literal containing:

   ```rust
   pbfhogg::write::emit_writer_counters(&host.invocation)
   ```

   A block comment, unused function, or unreachable branch can similarly satisfy presence without proving exit-path execution. This contradicts the script's introductory claim that comments and literals cannot decide the outcome.

   Move final counter emission into an invocation-finalization operation that owns the lifecycle, and test successful, refused, and cancelled exits through that operation. An AST check can enforce call placement, but substring presence cannot enforce this behavior.

6. **The lock-opening guard checks incidental tokens rather than the actual open operation.**

   The same script [extracts `probe_lock` with string splits](/home/folk/Programs/pbfhogg/scripts/check_source_invariants.py:250), requires `.write(true)` somewhere in the extracted text, and prohibits only `.create(`.

   A string or unrelated builder can provide `.write(true)`. `.create_new(true)` is not prohibited. Delegating creation to a helper also escapes the rule. Function extraction depends on formatting rather than Rust syntax.

   The current guard therefore does not establish ADR-0018's claim that scanning never creates a lock file. Put lock probing behind an operation whose interface exposes no creation option. Test that missing `LOCK` remains missing and that probing preserves directory contents. An AST check can additionally restrict the implementation's allowed open flags.

7. **The ban on global fault state misses enclosing feature gates.**

   [gated_statics](/home/folk/Programs/pbfhogg/scripts/check_source_invariants.py:183) reads only a static item's preceding attribute block. I confirmed it returns no finding for:

   ```rust
   #[cfg(feature = "test-hooks")]
   mod hooks {
       static X: u8 = 0;
   }
   ```

   The separate textlint catches this only inside the specifically named `test_hooks.rs`. Macro-generated state is another admitted gap.

   This is a demonstrated enforcement hole, not a claim that such state currently exists. Enforce the expanded, resolved production item graph, or prohibit mutable statics outside a small audited ownership list regardless of feature spelling. Add enclosing-module and macro cases to the checker tests.

8. **The source checker repeatedly reparses the tree on every ordinary check.**

   It scans library sources for counter casts, scans CLI sources for the same, then scans library sources again for gated statics. Each scan reconstructs comment and literal masking. These scans run even on brokkr's markdown-only shortcut.

   At commit `c95f9998` on `bygg`, one execution took about **2.12 seconds**. That is a single observation, not a comparative performance result, but it establishes a recurring cost worth accounting for.

   Consolidate these checks around one parsed source corpus, preferably the same syntax-aware machinery needed to fix findings 5-7. Enforce one source-analysis entry point rather than adding another independent whole-tree script for each invariant. This is an architectural consolidation, not a reason to tune the character loop.

9. **Rustdoc link validation is not enabled.**

   There is no `[rustdoc]` table in the composed repository configuration. Brokkr documents that the rustdoc phase is inert without it. Clippy, test compilation, and doctest execution do not replace that pass.

   `check_doc_links.py` handles repository paths and Markdown links; it does not resolve Rust intra-doc links such as ``[`Type::method`]``. Consequently, a refactor can leave those links broken while all configured checks pass.

   Enable the rustdoc phase and require it in the complete gate. Brokkr already supports checking without rendering where the installed rustdoc permits it. This is directly enforceable; I did not run rustdoc to establish whether any links are broken today.

10. **The Markdown checker has a reproduced fence-parsing defect.**

    Both [anchor extraction](/home/folk/Programs/pbfhogg/scripts/check_doc_links.py:365) and [paragraph extraction](/home/folk/Programs/pbfhogg/scripts/check_doc_links.py:520) toggle a Boolean whenever a line begins with three backticks or tildes. They do not remember the opening delimiter's character or length.

    In an in-memory example with a four-backtick fence containing a three-backtick line, the checker examined a link inside the code block and omitted the real link after its closing fence. This produces both false failures and false passes.

    Use Markdown tokens from a parser with compatible fence semantics, then apply repository-specific rules to those tokens. Wire regression cases for delimiter lengths, delimiter types, and embedded examples into the checks.

11. **The document checker is also an unverified implementation of renderer behavior.**

    It separately implements GitHub slugging, VitePress slugging, inline rendering, Markdown link recognition, and part of TypeScript sidebar parsing. Unlike the source checker, it has no accompanying self-test corpus.

    Its stated anchor guarantee is broader than the parser: heading extraction recognizes ATX headings, not Setext headings. A valid Setext-heading link can therefore be rejected. Renderer or plugin changes can invalidate its handwritten slug rules without changing this script.

    Test the checker against committed examples and the site's actual renderer output. Use parsed Markdown and renderer-produced identifiers where possible. GitHub parity needs an explicit, tested compatibility contract; it cannot be established by comments describing the algorithm.

12. **Migration inventory comparison discards identities that the complete gate preserves.**

    [test_inventory.py](/home/folk/Programs/pbfhogg/scripts/test_inventory.py:63) writes package, target kind, and target name, then ignores that entire field during comparison. It also ignores build-shape identity and skips inventories that are not dictionaries.

    Its `Counter` does detect reduced multiplicity, so losing one of two same-named tests is not automatically hidden. However, removing an execution from one package or target and adding a same-named execution elsewhere preserves the count and passes without an explicit move declaration. A changed feature shape can likewise retain every name while testing a different configuration.

    Compare complete identities by default. Require explicit mappings for intended package, target, name, and shape moves, and reject stale mappings. Represent unavailable inventories explicitly and refuse an equivalence claim when the necessary inventory is missing. Keep this as a migration tool unless historical comparison is deliberately part of the gate.

13. **The normal test matrix never exercises release semantics or compiles the profiling implementations.**

    `[test] debug = true` applies to the configured sweeps, and none pins release. The deliberate exclusion of `hotpath` avoids stdout contamination, but it also leaves numerous profiling macro expansions, conditional builder code, and the profiling allocator configurations outside the checks. `check_hotpath_cpu.py` resolves metadata; it does not compile those paths.

    These are two uncovered dimensions, not a request to multiply the whole matrix. Add release contract tests for behavior affected by build mode, and compile/lint-only shapes for `hotpath` and `hotpath-alloc`. Brokkr supports separate lint sweeps, so profiling builds need not run stdout-sensitive integration tests. Enforce the required shapes in the gate.

14. **The published minimum Rust version is a claim no check tests.**

    [Cargo.toml](/home/folk/Programs/pbfhogg/Cargo.toml:18) declares `rust-version = "1.99"` and explicitly says only nightly is installed and no MSRV check is planned.

    A successful nightly build provides no evidence for that declared minimum, including the compatibility of resolved dependencies. This is an acknowledged unchecked claim, not evidence that 1.99 currently fails.

    Add a library and installable-CLI compatibility build under the declared version. Derive its toolchain selection from the manifest so the version is not duplicated. That build need not use brokkr's nightly libtest runner. Otherwise, remove the unsupported compatibility claim.

15. **The default musl sweep assumes an x86_64 execution host.**

    [checks.toml](/home/folk/Programs/pbfhogg/brokkr.d/checks.toml:105) always targets `x86_64-unknown-linux-musl` and explains that its binaries run natively on the glibc host.

    On an aarch64 Linux host, this becomes a cross-build with foreign executables, not the advertised native compatibility check. Missing target support fails compilation; absent execution support prevents running the tests. This matters because other test machinery explicitly accommodates aarch64.

    Separate compile-only target coverage from executable coverage. Resolve a runnable musl target from an explicit host capability, or clearly declare and check the runner's x86_64 requirement before starting builds. Enforce the distinction in the check plan rather than relying on a hostname convention.

16. **A shared unit-test binary temporarily loses its panic diagnostics.**

    [The pipeline-death test](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1913) replaces the process panic hook while creating and consuming a deliberately crashed pipeline. Concurrent unit tests use that replacement too. Restoration is manual, so an unexpected unwind before restoration leaves the replacement installed.

    The documentation acknowledges the interference; acknowledgement does not isolate it. Prefer removing the hook mutation and accepting captured panic output, or inject the failure without a process-global hook. If hook behavior itself must be tested, use a dedicated process. A lint banning hook replacement in shared test targets can enforce the result.

17. **`test_host()` secretly changes process state and still uses an ambient executor.**

    [common::test_host](/home/folk/Programs/pbfhogg/crates/lib/tests/common/mod.rs:1711) raises `RLIMIT_NOFILE` once, then returns a host containing fixed resources and `Executor::Ambient`.

    The first caller changes every sibling's descriptor limit and future children's inherited limit. The separate `process_nofile` target correctly avoids that helper, but this knowledge remains a maintenance obligation. Fixed reported parallelism also does not fix the ambient Rayon pool's actual size.

    Separate host-value construction from process initialization. Give ordinary tests an explicitly owned, bounded executor and descriptor policy. Tests of real process limits belong in their dedicated process. Enforce that host constructors do not mutate limits, configure global pools, or read ambient policy. This removes ordering dependence and makes executor cost predictable across hosts.

18. **Scratch placement is duplicated and discovered through an incidental lockfile.**

    The shared integration helper and the CLI and harness unit-test helpers independently search ancestors of compile-time `CARGO_MANIFEST_DIR` for the nearest `Cargo.lock`, then write `data/test-scratch` there.

    A missing workspace lockfile causes a panic. An intervening lockfile changes the selected root. A moved test executable remains tied to the original source path. The copies can diverge, and the existing textlint bans several allocation spellings without proving that all scratch goes through the intended allocator.

    Use one test-support implementation with an explicit test-run scratch parent. In repository runs, the test runner should own a directory under the project's `data/test-scratch/`, with each fixture owning a random child and its cleanup guard. Enforce dependency on that allocator and reject alternative scratch constructors. The current location is appropriately test-owned; I found no need to move it into Cargo or brokkr's private state.

19. **Platform skips can satisfy the complete gate without exercising the promised behavior.**

    The platform tests return successfully on typed setup refusals. Brokkr then sees a passed test, not a distinct unsupported outcome. [testing.md](/home/folk/Programs/pbfhogg/reference/testing.md:15) explicitly warns that a misclassified regression can become a passing skip.

    The promise guard has a related silent degradation: [guard::apply](/home/folk/Programs/pbfhogg/crates/cli/tests/guard.rs:281) simply returns when `supported()` is false, so CLI tests continue without the lease/process protection.

    Record executed, unsupported, and failed outcomes separately. A portable developer run may accept unsupported capabilities, while a designated capability gate should require evidence that each promised backend and guard actually executed. This can be enforced with structured capability accounting without making every developer host support every feature.

20. **A test-only dispatch instrument ships in production.**

    [The hidden `--full-scan-min-blobs` argument](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:1911) and [its public library entry point](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getparents/mod.rs:106) exist specifically to force a dispatch arm on small fixtures. Neither is gated by `test-hooks`; hiding the flag from help does not prevent production use.

    Make the dispatch policy an explicit internal input and expose the override across crates only in test-hook builds. Alternatively, adopt it as a supported production policy and document and validate it accordingly. Feature-gated command-tree assertions and compile-time visibility can enforce either choice.

21. **The durable testing contract contradicts itself and duplicates implementation inventories.**

    [The stable allowlist](/home/folk/Programs/pbfhogg/reference/testing.md:260) says everything outside its list requires CLI conversion. Later text explicitly permits `host` and `process`; `process_nofile` also directly calls an internal external-join entry point. Fault and host-contract tests intentionally exercise additional internal surfaces. The universal statement is false today.

    The document also repeats feature/package selection lists and labels the one-sweep `full` profile a full pre-merge gate. These manually maintained inventories have already diverged from the newer certification machinery.

    State the intended dependency boundaries by test purpose, enforce them through resolved imports or test-support crate dependencies, and generate configuration tables from the composed check configuration. Keep rationale in prose. Assertions such as "every internal contract has a tier-1 test" still require human review; enumeration cannot mechanically prove semantic completeness.

No repository build scripts were present, so I found no generated-file rewrite cost in this scope. The CLI and harness tests already obtain their executables through Cargo's `CARGO_BIN_EXE_*` paths; they do not need a nested build or a binary selected from another profile. The consolidated `all` layout addresses the former per-file linking and focused-run discovery cost. The remaining structural need is to enforce which sources enter those binaries and to make execution claims narrower and more explicit.
