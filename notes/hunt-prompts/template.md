pbfhogg is a Rust library and CLI for reading and writing OpenStreetMap PBF files. `pbfhogg-verify` (`crates/verify/`) is its real-data cross-validation tool: it runs pbfhogg commands on a real dataset (denmark, about 450 MB of PBF), runs reference tools on the same input (osmium-tool mostly), and judges each pbfhogg output against the reference or against a verifier-owned model. `verify all` runs every check in sequence. On denmark it used to take about 3 minutes and now takes 12 to 20; single checks take 2 to 9 minutes. The verifier has become the bottleneck of the development loop.

The observable contract is: the set of claims each check judges (what is compared and how strictly), its PASS/FAIL verdict and failure diagnostics, and the CLI surface (subcommands, flags, the one-line-per-check stdout, exit codes). A check must not get weaker - every divergence it catches today it must still catch. Everything else is not sacred: how outputs are decoded, whether OPL is materialized, check order and grouping, what is shared between checks, how reference results are produced and cached, what runs in parallel.

Find the biggest remaining performance opportunities for this scope specifically. Read the code deeply, along with the internal APIs it could be taking advantage of and is not (including the pbfhogg library's own readers in `crates/lib/`). Prioritize what removes structural bottlenecks, stage seams, intermediate materialization, and serialized ownership or handoffs. For each major opportunity, explain what the bottleneck is, why the current structure causes it, what the stronger end-to-end redesign would look like, what makes it plausibly high-payoff, and what risks it carries. Distinguish clearly between high-conviction architectural rewrites and medium-value local changes. If the right move is a big rewrite, say so directly.

Read code, not documents or history: do not read the project's markdown documents, its git history or records of earlier attempts. The code is your input.

- This is NOT primarily about preserving existing stage boundaries.
- This is NOT primarily about preserving existing internal APIs.
- Prefer structural and architectural opportunities over micro-optimizations.
- Assume pre-1.0: breaking internal API is acceptable.
- Do not assume public or internal API stability matters pre-1.0.
- Assume we are willing to rewrite internals aggressively if the payoff is real.
- We do care about correctness and maintainability, but not about preserving old abstractions just because they already exist.
- Do not preserve structure just because it exists today.
- Do not preserve abstractions just because they are shared.
- Do not optimize for minimal or least-invasive change by default.
- Do not default to conservative or minimal suggestions.
- If you think the right move is a big rewrite, say so directly.
- Do not assume engineering time or resource constraints. We have unlimited resources.
- This is NOT primarily a generic cleanup or API exercise. A generalization that becomes obvious on the way is a follow-up, framed as "afterwards, if this works", never a constraint on the redesign.
- Focus on opportunities that could materially improve real wall time.
- Be explicit about which ideas are full coherent rewrites and which are local changes.
- Do not center the analysis on environment variables, benchmark knobs or temporary routing switches, and do not propose gated experiment scaffolding. An experiment is the full change, benchmarked, then kept or reverted.
- Correctness means the observable contract above, and only that. If the contract still holds, a change is legal no matter what internal shape it breaks.
- Be opinionated. Prefer strong hypotheses over exhaustive lists.

This is a read-only reading: do not edit, build, test or run anything. The scope below is where you start and what you are answerable for; following a question outside it is expected. If you notice a bug, a correctness gap in a check, a smell, or anything surprising, flag it even when it sits outside the task.
