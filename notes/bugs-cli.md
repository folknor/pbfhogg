# CLI binary defects

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

Defects from the 2026-11-17 defect hunt in `crates/cli/src/`. Every finding
was made by one codex hunter reading source at `63d4ec1d`. None was built,
run or reproduced, and the bracketed priority is the hunter's own label.
Entries are in no order of importance.

## CLI-002 - Stderr I/O failures panic instead of failing the command

Reported by: cli-binary [P2].

`crates/cli/src/output.rs` panics on every stderr error except `BrokenPipe`.
With stderr redirected to `/dev/full`, an ordinary diagnostic or the final
error report unwinds the process, which normally exits 101 and bypasses the
promised exit-2 classification. The source comment endorses this behaviour
explicitly. The hunter says it contradicts the public CLI contract that I/O
errors are failures with status 2.

## CLI-006 - A header --get key accepts --distrust-metadata beside a decode flag

Reported by: spar partner while resolving the inspect --indexed/--get
combinations.

`run_inspect` (`lib.rs`) refuses `--distrust-metadata` unless the resolved
mode is `Full`. `inspect F --get header.bbox --distrust-metadata` is refused,
but adding `--extended` or `--locations` routes the header key through the
full scan, and the same flag, still ineffective for a header key, is then
accepted. `docs/cli/commands.md` (`--distrust-metadata` row) promises the
refusal for every `file.*` or `header.*` key. Either refuse by key kind
rather than by mode, or narrow the doc to the header-only route.

## CLI-005 - --help and --version ignore non-pipe write failures

Reported by: cli-binary.

`clap_exit` (`lib.rs`) discards the result of `err.print()`, so `--help`
redirected to `/dev/full` returns success. This contradicts the promise in
`output.rs` that stdout failures other than `EPIPE` fail the command.
