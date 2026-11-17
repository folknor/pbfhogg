Static review of commit `63d4ec1d`. I found seven defects. No files were changed, and no builds or tests were run.

1. **[P1] The public external entry point publishes fabricated coordinates without opting in.**

   [`external_join`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/mod.rs:848) calls the engine and immediately publishes. It never calls the missing-location policy enforced by [`add_locations_to_ways_to`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:451).

   A sorted, indexed input containing a way referencing an absent node therefore returns success and publishes `(0, 0)` coordinates through this public API. Its signature provides no option authorizing placeholders.

   This breaks [CORRECTNESS.md's explicit publication guarantee](/home/folk/Programs/pbfhogg/CORRECTNESS.md:291). The public wrapper is wrong. Backend selection and publication policy should have one owner, so selecting another entry point cannot bypass the policy.

2. **[P1] A scratch-file metadata failure silently relocates resolved vertices to Null Island.**

   The external handoff turns every failed bucket `stat` into zero entries using [`unwrap_or(0)`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/mod.rs:1331). Stage 3 then takes its [empty-bucket branch](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage3.rs:249), skips reading that bucket, and generates zero coordinates.

   Missing-location accounting still uses [stage 2's earlier resolved count](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/mod.rs:1532). Consequently, the ordinary CLI entry point can publish the corrupted geometry with zero missing locations, even without `--ignore-missing-nodes`.

   This breaks the refusal-on-partial-failure contract and the missing-location publication guarantee. The stage 2-to-3 boundary is wrong: stage 2 already maintains authoritative bucket counts, but the handoff substitutes filesystem observations that suppress errors. It should carry and verify an explicit bucket manifest.

3. **[P1] Stage 3 startup failure can deadlock the external pipeline.**

   Stage 3's abort guard is constructed [inside each worker](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage3.rs:161). If creating its first worker fails, [`Scope::spawn` panics](/home/folk/Programs/pbfhogg/crates/lib/src/threads.rs:60) before any guard exists.

   An execution order where stage 4 has already started waiting is sufficient: the stage 3 parent unwinds past [`mark_producer_done`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/mod.rs:1425), and the outer join converts its panic into an error without aborting the router. Scope cleanup then waits for stage 4, whose readiness wait has neither publication, abort nor producer completion to observe.

   This breaks the router's documented terminal-state and panic-safety contract. The stage 3 parent needs failure ownership covering worker creation as well as worker execution. This is a source-derived interleaving, not a reproduced hang.

4. **[P1] Conflicting records for one node ID produce different, sometimes scheduling-dependent geometry.**

   Sparse performs an unconditional [`AtomicU64::store`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:220) for every occurrence. Within one blob the last occurrence wins; across blobs whichever worker stores last wins. External instead uses the [first matching node tuple](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage2.rs:617).

   Thus, two records for node 1 at different coordinates, followed by a way referencing node 1, produce different inline coordinates depending on the engine and potentially worker timing. Exact indexdata does not prevent this: the order checks permit equal IDs.

   There is also a refusal discrepancy. If the first record is geographically valid and the second is outside the domain but fits `i32`, sparse refuses the second insertion. External resolves against the first record and never domain-checks the second.

   This breaks the stated agreement between index strategies, including the [explicit requirement that domain refusals agree](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage2.rs:627). Atomics remove memory unsafety here, but do not establish correct duplicate semantics. Conflicting coordinates need a common refusal policy.

5. **[P2] Legal non-default way coordinate framing is an undocumented refusal.**

   [`check_default_coord_framing`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/reframe.rs:202) rejects any way block declaring a granularity other than 100 or nonzero coordinate offsets. Both indexed engines use it.

   For example, an otherwise ordinary indexed way block declaring `granularity = 1000` fails even when every referenced node exists. Removing indexdata and using the forced decode-all sparse path avoids this restriction.

   This contradicts the [documented sparse input coverage](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1786), and the deliberate protobuf restriction is absent from `DEVIATIONS.md`. The refusal prevents wrong coordinates, but the advertised acceptance boundary is wrong. Supporting the existing claim requires converting or normalizing framing; retaining the refusal requires documenting it.

6. **[P2] Both way reframers remove a required StringTable when its payload is empty.**

   Sparse [writes the table with `encode_bytes_field`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/reframe.rs:75), as does [external](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage4.rs:1077). That helper omits empty payloads.

   An empty but present StringTable, with a tagless way and honest indexdata, passes these paths and is published without the required `PrimitiveBlock.stringtable` field. A subsequent ALTW run then fails its own [missing-StringTable check](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/reframe.rs:180).

   This breaks the PBF output contract. Whether an empty table should be accepted or refused, converting its presence into absence and publishing the result is wrong.

7. **[P2] External statistics omit whole blobs of dropped nodes.**

   When stage 4 identifies an entirely untagged node blob without relation members, it [continues without updating node statistics](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage4.rs:382).

   A file containing only such nodes and ways referencing them can therefore report `0 nodes read, 0 written, 0 dropped`, despite resolving coordinates from those nodes and removing every node record. Sparse reports the dropped nodes.

   This breaks the returned `Stats::nodes_dropped` and printed summary's meaning. Skipping a decode does not mean no nodes were dropped.

Two additional observations:

- The description's project-wide negative-ID rejection claim is already contradicted by [documented ALTW exceptions](/home/folk/Programs/pbfhogg/DEVIATIONS.md:603). Negative way and relation IDs can survive, while negative relation-member node IDs are [silently excluded from retention](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:112). This is an existing acknowledged contract mismatch.
- [`wait_ready`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/coord_payloads.rs:430) starts its `WAIT_*` span before confirming a Condvar park. A publication racing the first check can therefore emit a wait span without blocking. The nanosecond stall counter is correctly gated; the marker span is not.
