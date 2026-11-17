I found three defects.

1. **Merge timings can come from an input filename.**  
   [workload.lua:20](/home/folk/Programs/pbfhogg/benches/lib/workload.lua:20) takes the first `elapsed_ms=(%d+)` substring anywhere in stderr. Before the instrument prints its measurement, the library prints [the OSC pathname](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/rewrite.rs:337). A configured file named `elapsed_ms=7.osc` therefore makes `record.cells[].elapsed_ms` equal 7, regardless of the actual timing.

   This breaks merge-workload's promise to retain the child's time through publication start. The Lua consumer is wrong; the instrument's timing block is correct. Parse a unique, complete measurement record, ideally on a channel separate from diagnostics. Brokkr's independently measured process wall remains unaffected.

2. **Geocode-query silently changes the requested sampling box.**  
   Its argument [uses the extraction bbox parser](/home/folk/Programs/pbfhogg/crates/bench/src/main.rs:59), which deliberately [rounds every edge to decimicrodegrees](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/mod.rs:63). For example, the valid box `0.00000001,0.00000001,0.00000002,0.00000002` becomes four zeroes. Every generated query then targets `(0, 0)`, outside the supplied box, while the Lua record retains the original bbox text.

   This breaks the instrument's explicit promise to draw points uniformly from the supplied box. The extraction parser is correct for extraction; reusing it here is wrong. Query sampling needs validation without coordinate quantization.

3. **Diff-text accepts inputs that bypass its advertised engine.**  
   The [registry](/home/folk/Programs/pbfhogg/brokkr.d/benches.toml:425) and script describe an indexed-input benchmark through block-pair work units. Both variant selectors nevertheless accept raw inputs, and [the script](/home/folk/Programs/pbfhogg/benches/diff/diff-text.lua:46) performs no indexdata probe. Selecting raw variants can complete successfully through [the sequential fallback](/home/folk/Programs/pbfhogg/crates/lib/src/commands/diff/mod.rs:304).

   The resulting measurement does not exercise the documented workload. The library dispatch is correct; the benchmark should establish its indexed-input precondition, as diff-unindexed establishes its opposite.

Two additional concerns are worth flagging:

- **Query profiling has no query-level instrumentation.** The script advertises `--hotpath` and `--alloc`, but neither the query instrument nor `Reader::query` and its search helpers contains profiling scopes. The guard supplies a process-wrapper measurement, without attribution inside the query work.
- **The query optimizer barrier consumes only administrative-result counts.** [Both loops](/home/folk/Programs/pbfhogg/crates/bench/src/geocode.rs:151) discard address, street and interpolation results, then black-box the accumulated `admin.len()`. With fat LTO enabled, this leaves those computations unprotected against elimination. I have not established actual elimination, so this is a benchmark-validity concern, not a confirmed timing defect.

Read-only review completed. No files changed, builds performed or tests run.
