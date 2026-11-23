# TOOL-032 design: refusal tests that accept any error

Settled under spar with codex (one session, three exchanges, converged on
mechanism). The first link landed; the rest is a chain.

## Settled rulings

1. **A caught panic is its own kind.** `ErrorKind::Panicked { who, payload }`
   is what `panic_error` builds; `Internal` means a broken invariant only.
   `Error::is_panic` walks the whole chain (context, `io::Error` carriers at
   any depth, every cause, foreign errors between), unlike `is_cancelled`:
   a failure a panic caused is a defect however it is wrapped. It classifies
   the returned error only; panics that lost a first-failure ranking
   (`FirstFailure`, apply-changes `FailureSlot`, altw stage priorities) are
   out of its reach, and the rankings are unchanged.
2. **CLI exit 101** for a chain holding `Panicked`, ahead of every other
   status including cancellation. Test-hook builds' own `exit(101)` on a
   malformed hook spec stay; 101 is documented as the shipped-build panic
   status.
3. **Harness outcome kind `panicked`** for any error whose chain holds one;
   never planned.
4. **The assertion bar is the cause the test promises**: an expected kind or
   a closed set of kinds, plus detail where the kind is broad. Merely
   excluding `Panicked`/`Internal` is acceptable only for a genuinely
   cause-agnostic contract, stated at the call site. A generic helper that
   accepts every other kind is not the bar.
5. **Scope includes `cfg(test)` code under `crates/*/src`**, not only the
   three integration trees: reader and writer unit tests run real worker
   threads. A direct parser whose panic would escape into libtest has no
   false-pass mechanism, so those sites take a reasoned exemption, not a
   blanket src exclusion.
6. **Enforcement is a source-policy rule, not a textlint.** A line regex
   loses to formatting (a newline between `expect_err` and `(`) and a
   brokkr `except` sees raw text, so marker text in a string exempts code.
7. **`Err(_) => panic!(..)` is legal**: it makes an unexpected outcome fail.
8. **Rejected**: a runtime panic hook per test binary (tests share a
   process; panics are not attributable, fault tests panic on purpose).

## Landed (link 1)

`ErrorKind::Panicked`, `Error::is_panic`, `Error::panicked` (doc-hidden,
for exit and outcome tests), the three hand-spelled panic sites converted
(`PipelinedBlocks::next` only on a panicked join, the `AbortOnDrop` guard
only when `std::thread::panicking()`, a parallel-writer unit test join),
CLI exit 101, harness `panicked`, docs (`docs/cli/commands.md`,
`reference/library-protocol.md`, `reference/pipeline.md`,
`reference/testing.md`) and CHANGELOG.

## Link plan

**Link 2 - the CLI refusal bar (medium).** `CliInvoker::assert_failure`
becomes `assert_refused`: exit exactly 2. Callers that meant a usage
refusal (`cli_degrade` `degrade_requires_at_least_one_flag`, the degrade
option-conflict helper near line 959, any other clap refusal) move to an
exact 64 assertion. Sweep direct `!out.status.success()` sites (about 26,
for example `cli_apply_changes` out-of-range coordinate near line 1955,
`cli_cat` near 1004) to an exact status plus a positive diagnostic where the
test promises a specific refusal. Printing stderr as the failure message
checks nothing.

**Link 3 - the source-policy rule (large).** New rule in
`crates/source-policy/src/rules/` over the three integration test trees and
the test-only regions of `crates/*/src` (reuse the `library_text`
test-region detection; scan assertion macro arguments explicitly, since it
treats macro input as opaque; cover test-only files reached by `mod`
declarations). Flags the forms that accept or discard an arbitrary error:
`.is_err()`, `!x.is_ok()`, `.err().is_some()`, `.is_err_and(`,
`matches!(_, Err(_..))` and `Err(_..)` arms whose body does not fail, and
`unwrap_err`/`expect_err` whose value is discarded (bare statement, `let _`,
`_`-prefixed binding, `drop`). Exemption: a `// any-error-deliberate:
<reason>` comment token bound to the expression's span, not the line; test
two candidates on one line. In-memory fixtures for each form, the
exemption and the legal `Err(_) => panic!` shape. Mind the 20 s watchdog: a
structural rule parses its files with syn itself (the `Corpus` doc in
`crates/source-policy/src/lib.rs` says why and names the parse-once shape).

**Link 4 - the sweep (large, may split by tree).** Every site link 3
flags: name the kind (or closed set) the test promises, or take the
marker with the reason (catch_unwind of a deliberate helper panic, join
guards, filesystem probes, test-local `Result<(), String>`, `QueryError`
parsers). Known bare refusals: `osc_parser_parity` 314, `corrupt_input`
663, `geocode_index` 3758, `publication` 192, `fault_pipelined_writer`
170/179/182, `read_paths` 556/588, `edge_cases` 531, `host_contract` 517,
`common/process` 692, and the writer unit tests' repeated
`flush().is_err()`. Lands together with link 3 so the tree is green.

**Link 5 - green agent.** Gate, close TOOL-032 in
`notes/bugs-tooling.md`, delete this file, commit.
