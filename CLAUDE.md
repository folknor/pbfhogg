@AGENTS.md

## More rules

### Communication rules

- Never use the `AskUserQuestion` tool - the harness runs in don't-ask mode and it will be denied. When you need a decision from the user, just ask in chat with the options laid out in prose.

### Memory rules

Do not use your Memory functionality. Durable context belongs in CLAUDE.md or the relevant docs.

### Bash rules

- Never use `sed`, `find`, `awk`, `head`, `tail`, or complex bash commands.
- Never `find /`.
- One Bash() invocation === one command.
- Never chain commands with `&&`.
- Never chain commands with `;`.
- Never chain/pipe commands with `|`. Exception: piping into `review` is allowed.
- Never capture stdout into env vars (`UUID=$(...)`).
- Keep `git commit -m` messages free of zsh metacharacters - braces `{}`, brackets `[]`, parens `()`, angle brackets `<>`, `#`. They trip the permission matcher and block the commit. Spell lists out (`syntax, vm, data and runner`, not `{syntax,vm,data,runner}`), write `5.1 per bar` not `5.1/bar`, name attributes in prose not `#[attr]`.

### git commit rules
- Always run `brokkr fmt` before a commit.
- Never commit markdown changes and/or results.db alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along brokkrs 'results.db' and markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Hard-wrap the message body at ~72 columns, matching the existing history; the
  subject stays one concise line. The wall-of-text we keep producing comes from
  `git commit -m "<whole paragraph>"`: a single `-m` is recorded as ONE unwrapped
  line. Embed real line breaks so every body line wraps at ~72 (one `-m` per
  paragraph is fine only when each paragraph already carries its own newlines).
  Newlines are not metacharacters, so this composes with the no-metacharacters-in
  `-m` rule in the Bash rules above - wrap with literal newlines while still
  avoiding braces, brackets, parens, angle brackets and the hash sign.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless the user explicitly asks. Stop after the commit.
- Standing instruction: when a large slice of agent work has landed and been
  verified, commit it before launching the next agent, so each slice gets its
  own commit instead of mixing into the next agent's edits.
- Remember to update CHANGELOG.md for relevant commits (but not general small performance improvements.)
- Never offer to commit or tell the user "per your rules I've left things uncommitted". Don't mention git commits, ever. The user will instruct you when to commit.

#### What gets added to CHANGELOG.md

Audience: library + CLI users deciding whether to upgrade. Not a commit digest.

**Add:** breaking changes (removed flags, widened bounds, format bumps); new capabilities; behavior changes at the same surface (silent truncation → hard error, new warnings); user-visible bug fixes; perf changes large enough to matter (headline numbers, not 5% sub-phase deltas).

**Skip:** internal refactors, module splits, helper extractions; sidecar instrumentation (markers, counters, `hotpath`) - serves brokkr, not users; F-numbered fix rollups; sub-phase timings that don't move the headline; test additions, code-quality cleanups, dead-code removal; doc-file edits (CORRECTNESS.md, DEVIATIONS.md, notes/*.md); private internals.

Test: would a user change what they do after reading this entry? If no, it belongs in `git log`.

The user can allow things that contravene these rules, for example allowing commits that are pure markdown updates. Do not ask them for this, they will tell you when.

### Subagents

- Always get permission from the user before launching subagents.
- Do NOT use git worktree isolation for parallel agents. Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership - zero overlap.

Agent coordination rules:
- Each agent gets exclusive ownership of specific files. No two agents touch the same file.
- Agents must read their target file FIRST. Do not replace existing code with placeholders or stub it out.
- Agents must NOT run `brokkr` or `cargo`. The orchestrator validates between agents.

Audit protocol:
- Do not trust agent claims of completion. Verify existence + wiring + behavior.
- Use the 3-pass audit structure: domain-specific verification, then cross-cutting reconciliation (is the new code actually wired into its callers?), then editorial normalization.
- Any discrepancies doc should contain only current gaps, not historical records. Remove resolved items entirely.

Subagent prompt rules:
- Scope the investigation, not the report. Caps like "under 1500 chars" or "max 15 findings" throw away signal you asked them to surface.
- Invite lateral findings up front. If they notice a bug, optimization, smell, or anything surprising while doing the scoped work, they should flag it, even when it's outside the immediate task.
- Name the question, not the method. Don't prescribe tools ("use `git diff`", "use `Read`"), don't prescribe steps ("read in full, not just hunks"), don't enumerate files when the scope already implies them. Prescribing the method wastes tokens and signals distrust.
- Don't restate rules the agent already inherits. Subagents load the same CLAUDE.md / AGENTS.md as the main session, so the bash rules, no-cargo, no-worktrees, gremlins, etc. are already in scope. Re-listing them is noise.
- Do pass anything learned in *this* conversation that the agent can't see: the user's framing, prior decisions, what's already been ruled out, the specific claim being audited.

### Codex agents

Never tell a codex agent to read CLAUDE.md (it is Claude-specific and contradicts their job), and never tell them to read AGENTS.md (codex loads it automatically). Put any rule they need directly in the prompt.
