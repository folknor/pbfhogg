I found eight concrete gaps by source inspection. No files were changed, and no builds or tests were run.

1. **Re-encoding can silently remove tags.** [cat/mod.rs:1005](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:1005), [repack/mod.rs:645](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:645), [owned.rs:673](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:673).

   The ordinary cat, repack and degrade rewrite paths consume infallible `tags()` iterators. An out-of-range string index ends that iterator, dropping the faulty tag and every subsequent tag. Block validation does not resolve these indices, so the command can publish shortened content successfully.

   This is already acknowledged in [CORRECTNESS.md:43](/home/folk/Programs/pbfhogg/CORRECTNESS.md:43). That document accurately describes the implementation, but the implementation contradicts the stated contract that input faults produce refusals rather than silent wrong answers. The checked accessors already exist; cat's unclassifiable-block fallback uses them.

2. **`degrade --drop-ids` confuses element occurrences with unique IDs.** [degrade/mod.rs:1102](/home/folk/Programs/pbfhogg/crates/lib/src/commands/degrade/mod.rs:1102).

   Selection admits each occurrence into its bounded heaps, then collapses the winners into ID sets. Rewriting removes every occurrence of each selected ID.

   With two historical versions of node 1, requesting one removal deletes both records. Requesting two unique IDs from an input containing only those two versions passes the size check but selects only one unique ID. Consequently neither the CLI's “exactly N elements” promise nor `Reencode`'s “exactly this many unique element IDs” promise holds generally. The reported dropped count is simply the requested N.

   Selection must operate on the same identity that removal and counting use.

3. **Degrade's corruption modes can publish an invalid PBF frame.** [degrade/mod.rs:804](/home/folk/Programs/pbfhogg/crates/lib/src/commands/degrade/mod.rs:804).

   `reframe_raw` appends replacement hints and checks only whether the resulting header length fits `u32`. It does not enforce the PBF header limit of less than 65,536 bytes.

   A valid header near that limit can grow beyond it when `--corrupt-tagdata` adds a hint, or when index corruption replaces a 26-byte v1 index with a 42-byte v2 index. `write_raw_owned` trusts the supplied framing, so the command can publish successfully a file its own reader refuses.

   Deliberately false hints are the requested degradation; invalid framing is not. This reframe needs the size gate already present in cat's reindex implementation.

4. **The strict timestamp parser accepts malformed signed components.** [commands/mod.rs:915](/home/folk/Programs/pbfhogg/crates/lib/src/commands/mod.rs:915).

   Components are parsed as signed integers, while day and time validation omit negative lower bounds. For example, `2026-01-01T-1:00:00Z` passes and becomes the previous day's 23:00. `2026-01--1T00:00:00Z` also passes. Leading `+` signs are accepted in numeric components.

   This crosses into both header overrides and OSC metadata through [xml_parse.rs:1206](/home/folk/Programs/pbfhogg/crates/lib/src/osc/xml_parse.rs:1206). Malformed timestamps can silently become different valid timestamps. The shared parser is the wrong end of that boundary: fixed-width components need ASCII-digit validation.

5. **Cap-1 `--unsort` does not produce the promised overlap-rewrite workload.** [degrade/mod.rs:891](/home/folk/Programs/pbfhogg/crates/lib/src/commands/degrade/mod.rs:891).

   With unique ascending IDs and cap 1, swapping the first pair produces singleton blobs `[2]`, `[1]`, `[3]`. Their ranges are disjoint. The merge planner sorts entries by their first ID before detecting overlap, so `sort` can restore order through blob permutation without rewriting an overlap run.

   The associated [test helper:535](/home/folk/Programs/pbfhogg/crates/cli/tests/cli_degrade.rs:535) masks this: it checks only `previous.max >= next.min`. That counts descending disjoint ranges as overlapping.

   The command does create disorder, but the advertised benchmark shape and its test are wrong at cap 1.

6. **Repack's exact round-trip documentation overstates preservation.** [commands.md:1366](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1366), [repack/mod.rs:637](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:637).

   The documentation promises that every inline way coordinate round-trips exactly. Both worker and owned paths convert coordinates to decimicrodegrees before rebuilding them. A valid location at granularity 1 with coordinate 1 nanodegree becomes zero.

   Metadata also passes through whole-second timestamp normalization in [owned.rs:544](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:544). Given the project's documented normalization policy, the repack documentation is the wrong side here: “same corpus, different encoding” needs explicit precision qualifications.

7. **Repair statistics omit decoded and rewritten blobs.** [cat/mod.rs:573](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:573).

   `repair_passthrough` validates every data blob, but sets `blobs_decoded` only from blobs rebuilt into single-kind blocks. `Outcome::Rewritten` increments neither decoded nor passthrough counts.

   A file containing one repaired, classifiable blob can therefore report `0 blobs passed through` followed by `Repaired 1 input blobs`, with `cat_blobs_decoded = 0`. That contradicts `CatStats::blobs_decoded`'s documented meaning and makes repair's decode cost invisible in that counter.

8. **Cat's normalization note describes obsolete metadata behavior.** [cat/mod.rs:619](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:619).

   The emitted note says absent-version metadata is discarded and missing fields receive defaults. The actual fallback uses presence-preserving partial metadata conversion and `add_node_partial`. A versionless plain node carrying a timestamp can retain that timestamp and remain plain.

   The same obsolete claims appear in the fallback's comments and CLI documentation. Here the current implementation agrees with the newer deviation, “absent metadata fields stay absent”; the note and older descriptions are wrong.
