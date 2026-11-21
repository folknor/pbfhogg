# Hunt: heavy checks (degrade, add-locations-to-ways, apply-changes, sort, repack, cat, cat --dedupe, renumber, merge-changes, field-policies)

Read-only reading of `crates/verify/src/checks/{degrade,add_locations,apply_changes,apply_model,merge_changes,field_policies,sort,renumber,cat_dedupe,dedupe_plan,cat,repack}.rs` and the shared code they lean on (`verify_reencode.rs`, `opl_compare.rs`, `harness.rs`, `verify_input_order.rs`, `reference.rs`, `reference_cache.rs`, `process.rs`, `pbf_frames.rs`). Nothing was built or run. Every time estimate below is a hypothesis from the code's structure, not a measurement.

## Thesis

The heavy checks barely pay for pbfhogg. They pay for **osmium OPL decodings**, and they pay for most of them several times over.

Every element judgment goes through `opl_compare::walk` (or `compare_with_opl_each`). That code runs `osmium cat -f opl... -o <file>` on side A to completion, then on side B to completion, then merges the two multi-GB text files on one thread through an 8 KiB `BufReader`, then deletes both files. On denmark (about 52M nodes and 6.6M ways, going by the `check --locations` report the apply-changes tests quote) a FULL decoding is roughly 7-8 GB of OPL. So one pairwise comparison means about 15 GB written, 15 GB read back, two osmium runs and a serial merge, and none of those steps overlaps another.

The API is pairwise and decodes inside itself (`compare(harness, a, b, ...)`), so:

- every relation a check wants re-decodes both of its files, even when one of them was decoded seconds earlier in the same check;
- every "is it in strict order" verdict (`check_sorted`, `opl_compare::order`) is a separate full decode of a file the strict merge right after it will decode again, and that merge already enforces strict order;
- nothing can run concurrently, because `Lifecycle::run` is synchronous and `settle` reaps every child of the process (`waitpid(-1)`). The verifier cannot stream osmium into the merge, decode two sides at once, or overlap a pbfhogg run with a decode.

Counting the decodes each check commissions on the suite's inputs (indexed, sorted, standard denmark; the altw base for the `-l` variants):

| check | full-size osmium decodes today | distinct (file, policy) | after the local fixes below | notes |
|---|---|---|---|---|
| degrade | 14 (input x5, drop-a x3, drop-b x2, 4 others x1) | 7 | 6 | `drop-a-again` can be settled by a byte comparison |
| degrade -l | 16 | 8 | 7 | +2 for `observed_coordinates` |
| repack | 8 (input x4) | 5 | 5 | |
| add-locations-to-ways (all) | 10 (osmium output x4, each engine x3) | 6 | 3 | |
| sort | 3 (pbfhogg output x2) | 3 | 2 | one of the three can never fail, see L1 |
| cat | 12 partial, about 4 full-file equivalents | 6 partial | 6 partial, about 2 equivalents | |
| cat --dedupe | 13 full (input x7) + 3 operand order scans | 11 | 10, input once | |
| apply-changes | 3, plus 2 materialized OPL rewrites (`osmium.opl`, `expected.opl`) | 3 | 2, no rewrites | |
| apply-changes -l | 3, plus the same rewrites | 3 | 2 | |
| renumber | 2 | 2 | 2 | already minimal; only streaming helps |

That is about 77 full-size decodes per `all` in these checks. The local fixes bring it to about 39, and run-level sharing of the input's decodings takes off a few more. Streaming (opportunity A) then roughly halves what each remaining decode costs, because it removes the disk round trip and overlaps the two sides with the merge. The observed walls fit this model. Degrade does about 7 compares at sequential decode-decode-merge cost, and that comes to 4-9 minutes; sort does 1.5 compares and takes 2.6 minutes. I expect the heavy checks to fall by 3x to 5x overall: about 2x from the local fixes alone, the rest from the rewrite.

**Measure this before arguing about it.** The PASS line's timing already sums `osmium cat` time per check (`Harness::timing`). The merge is not a step, though, so it hides in the residual. Add an `opl merge` step label in `opl_compare::walk` and `compare_with_opl_each`, and one denmark `all` will show the split between decode and merge.

---

## A. High conviction, full rewrite: decode each distinct file once, stream it, fan it out to concurrent relation consumers

**Bottleneck.** Decoding is hidden inside the comparison, and the comparison is a blocking, file-materializing, pairwise call. That one design choice causes all three costs: repeated decodes, the disk round trip, and zero overlap.

**Redesign.** Each heavy check becomes plan-then-execute:

1. **Producers.** The check runs every pbfhogg and osmium producer first, and reorders its cases to do so. Check order and grouping are not part of the contract. degrade runs its 6 degrades, repack its 4 repacks, cat --dedupe its 6 merges, add-locations both engines (already does), and so on.
2. **Decodings.** The check declares the decodings it needs as `(file, policy)`, memoized by the file's content digest (xxh3 already exists in `run::digest`). Byte-identical files collapse automatically. This catches the obvious duplicates and any coincidental ones, such as `repack (unindexed --force)` and `repack (default cap)`, or `cat --dedupe selected+rest` and `rest+selected`, if pbfhogg writes identical bytes there.
3. **Relations.** The check declares relations over decodings: `sequence(a, b, listing)` (today's `compare_sequence`), `strict(a, b, listing, each, paired)` (today's `compare` and `compare_paired`), `against_expected(a, lazy expected stream)` (apply-changes), `order(a)`, and `counts(a)`.
4. **Executor.** It starts one `osmium cat -f <policy> -o -` per distinct decoding with stdout as a pipe (enlarged with `F_SETPIPE_SZ`). A reader thread cuts the stream into line-aligned `Arc<[u8]>` chunks and broadcasts them over bounded channels to every relation that reads that decoding. Each relation runs its merge state machine on its own thread and writes its listing exactly as today. The order and counts verdicts are just more consumers, or fields every merge already tracks (`Stream` already counts and already checks rank).

The wait graph has no cycle: a producer waits on its consumers, a consumer waits on its producers, and each pbfhogg output has exactly one producer. So there is no deadlock. Memory stays bounded by the chunk budget because the sequence and strict walks only ever look at the current record of each side. Even the degrade drop case, where the reference runs up to 10,000 records ahead of the output, is just channel lag.

**What becomes cheap:**

- degrade: the reference (the input) is decoded once and broadcast to the five reference relations; a and b feed `a-vs-again` and `a-vs-b`. That is 7 decodes instead of 14, all running concurrently.
- add-locations: 3 decodes. The osmium output is broadcast to sparse-vs-osmium and external-vs-osmium; both engine outputs also feed the parity relation; order comes from the same streams.
- cat --dedupe: the input plus 6 outputs in one pass. Every relation there is strict, so this could even be one 7-way key merge.
- apply-changes: osmium's decoding streams through the model's expectation iterator straight into the merge with pbfhogg's decoding. `osmium.opl` and `expected.opl` (two more multi-GB writes) disappear.

**Why it pays.** It removes the largest structural costs at once: about half the decodes, about 15 GB of disk traffic per comparison, and the strict serialization of decode A, then decode B, then merge. Per comparison the wall becomes about `max(decode A, decode B)` instead of `decode A + decode B + merge`.

**Risks.**

- **Process lifecycle.** This is the hard part. `Lifecycle::run` assumes one live child, and `settle` reaps every child of the process. Groups need signal-aware waiting on several pidfds, a stderr capture per child, and settling per group.
- **Message order.** The detail is emitted through `&mut Harness`, so consumers have to return their messages for emission in a fixed order after the group finishes.
- **CPU oversubscription.** Several multi-threaded osmium processes at once on a small host could slow each other down. Cap the concurrent decodes by core count.
- **Diagnostic parity.** Listings and failure strings must come out the same. Keep today's walk code as the consumer bodies, unchanged, and replace only their input source.

**What I need from siblings.**

- *Engine:* a merge that reads from a chunk channel, not a path; relation kinds that match today's four entry points; and an order failure reported as a named verdict (see L1) instead of only an `Err`.
- *Orchestration:* concurrent launch groups in `process.rs`, and a run-scoped decoding memo, so the input's decodings can be shared across checks (C).

## B. High conviction, local: per-check redundancies removable with today's engine

These need at most one small engine helper (a sequence-walk variant of `compare_with_opl_each` that takes one side pre-decoded). They are worth doing first because they roughly halve the decode count with little risk.

**B1. sort: drop `check_sorted` after `same_elements`.** `same_elements` uses the strict `compare`, and it returns `Err` the moment the pbfhogg decoding steps down. So the `check_sorted("pbfhogg sort", &ours)` that follows only ever runs on a file the merge already proved strictly ordered. It is a full CONTENT decode that cannot fail. That is 1 of sort's 3 decodes (see L1 for the diagnostic side).

**B2. `compare_outputs` (cat, add-locations-to-ways).** It decodes `theirs` for order, `ours` for order, then both again for the strict merge. Both order decodes are redundant with the strict merge: both sides are held to strict order there, and an `Err` names the OPL file. To keep the order verdict's own wording, fold it into the merge (engine). In add-locations, `compare_outputs` runs once per engine against the same `osmium_out`, so the osmium output is decoded FULL twice and CONTENT twice. Decode it once and compare both engines against that decoding (`compare_with_opl`-style).

**B3. add-locations engine parity by transitivity.** If sparse equals osmium and external equals osmium, record for record, both under `Policy::FULL`, then sparse equals external, record for record. `same_elements(a, b, FULL)` can only pass then. So:

- when both osmium comparisons passed under FULL, record the osmium-decoded parity as PASS without decoding anything;
- otherwise run it as today, because a shared wrong answer must still show as parity.

This is exact, not a weakening. The guard that matters is `Policy::between` having returned FULL for both comparisons. Keep `digest --form full` parity: it covers fields osmium does not represent, and it is pbfhogg-side and cheap. Together B2 and B3 take add-locations from 10 decodes to 3.

**B4. degrade and repack: decode the reference once per check.** `reencode::compare` decodes the reference again for every case: 5 times in degrade, 4 in repack, and 6 in degrade -l counting `observed_coordinates`. Keep one reference decoding for the check's lifetime and compare each output against it. That needs the sequence-walk counterpart of `compare_with_opl_each`, because degrade's unsort outputs are not strictly ordered.

**B5. degrade `--drop-ids` pairs.**

- *`drop-a` vs `drop-a-again`:* if the two files are byte-identical, the element verdict is "identical" with no decode. A determinism change in pbfhogg shows up as a byte difference first, and then the OPL comparison runs as today. Today that pair costs two full decodes.
- *`drop-a` vs `drop-b`:* when both have passed `judge_drop` (exactly n first-only, nothing else, every group minus-only), each output is the reference minus a set of records. The reference holds distinct records, because the case only runs on strictly ordered input. So a equals b exactly when the two listings `drop-a.diff.opl` and `drop-b.diff.opl` are byte-equal. That is exact. If either drop failed, fall back to the merge; the check is failing anyway.

That saves 4 of degrade's 14 decodes on top of B4.

**B6. cat --dedupe.** The input is decoded under NO_LOCATIONS once per merge, six times, plus once more under CONTENT by `opl_compare::order` for the domain check.

- Decode it once, take the order domain check from that decoding's keys (the keys of a NO_LOCATIONS and a CONTENT decoding are the same), and compare each output against it (`compare_with_opl` already exists, strict, and the orientation fits `same_elements(out, pbf)`). That is 7 input decodes down to 1.
- The three derived operands each get a CONTENT order decode. They are cached osmium artifacts, so their order and counts are pure functions of the artifact's digest. Cache those facts next to the artifact (C3), and warm runs skip them.

**B7. apply-changes.**

- `check_sorted(pbfhogg_out)` is redundant with the strict `compare_with_opl_each` that follows. Fold it into the merge, and apply-changes goes from 3 decodes to 2.
- `write_expected` materializes osmium's decoding (`osmium.opl`) and then a second multi-GB file (`expected.opl`) before the merge reads it again. It is a pure streaming merge of osmium's records with the model's sorted expectations, so it can be an iterator the merge pulls from. That needs only an engine entry point taking `impl Iterator<Item = line>` for the second side, and it removes about 12 GB of writes.
- `print_fileinfo("osmium", &osmium_out)` is informational, decides nothing, and lands in the detail that a pass throws away. On a cold cache it is a full osmium read. Drop it, or run it only on failure.

**B8. The verifier's own blob decode (`pbf_frames::index_truth`).** It is single-threaded, and `read_block` opens and seeks the file once per blob, which is about 7,700 opens on denmark. It runs on the input in `reencode::read_input` (twice per run: degrade and repack each call it) and on every output in `judge_index` (5 per degrade, 4 per repack). Each pass inflates the whole file, about 1.5-2 GB raw, with `flate2`'s default backend, and walks every varint.

- Parallelize it over blobs (`std::thread::scope`, chunked by claims).
- Keep one open file and use `read_exact_at`.
- Fuse it with the claims walks that judge the same output right after (`output_blobs`, `LayoutCheck`, `judge_header`).
- Memoize its result per file digest within a run, so the input is not done twice.

This is a medium-value local change, likely a few seconds per file, which adds up to tens of seconds per run. The same applies to `check_index(input)`, which degrade and repack each run.

**B9. field-policies.** The base file is decoded under each of FULL, NO_LOCATIONS and CONTENT inside every control (`judge_policies`). That is 52 decodes of the same small base plus about 100 other osmium launches. The files are tiny (64 IDs per kind), so this costs process spawns, not data. Memoizing the base's three decodings in the check removes about 40 launches. Low value. The full-size cost here is the two `osmium getid` runs over the whole input, which are cached.

**B10. merge-changes.** This works on OSC-sized files and is not heavy in data. Two small items:

- `simplify-j1` could be held to `simplify` with `listing::same_content` before its own listing decode and model comparison. If they are identical, the second model comparison is implied.
- Each fixture's model is built twice (the probe, then inside `judge`).

Process launches dominate here. A launch group (A) would let the per-case pbfhogg runs (default, `--jobs 1`, simplify, simplify `--jobs 1`) run concurrently.

## C. Orchestration-side needs (cross-check redundancy)

**C1. The input is the most-decoded file in the suite, and it is decoded independently in each check.**

- degrade decodes it FULL 5 times (its reference on plain input is the input itself).
- repack decodes it FULL 4 times (same reasoning).
- cat --dedupe decodes it NO_LOCATIONS 6 times and CONTENT once.

That is 16 decodes of one unchanging, pinned file per `all`. With B4 and B6 it is 3, one per check. A run-scoped memo keyed by `(digest, policy)` makes it 2. The suite should also order repack, degrade and cat --dedupe next to each other, so the kept decoding lives only for that window. That is orchestration's to own. The heavy checks need the API "give me the decoding of X under P, shared".

**C2. `reencode::read_input` runs once in degrade and once in repack on the same file.** That covers fileinfo (cached), header, framing, `index_truth`, and `check --index`. Memoize it per digest per run. The `check --index` run is a pbfhogg run, but on the *input*, not an output under test, so sharing it within one run does not weaken anything.

**C3. A cross-run cache for decodings of reference artifacts and of the dataset.** Every osmium reference output (sorted copy, add-locations output, apply-changes output, renumber output, cat `-t` outputs) is already cached as a PBF, but its OPL decoding is marked `uncached` because the text is huge. Its decoding is a pure function of (osmium identity, artifact digest, policy), and so is the pinned dataset's. Two ways to cache it:

- *Compressed text:* zstd or lz4 OPL, about 1 GB per denmark decoding, read back far faster than osmium decodes. This is exact.
- *A record-digest stream:* kind, ID and xxh3-128 of each line, about 32 bytes per record, falling back to a real decode only when a record differs, to write the listing. Strictly speaking this is a hash-equality claim, a theoretical weakening of byte equality at 2^-128 per record. I would choose the compressed text.

With either, a warm run decodes only pbfhogg's outputs, which removes about one decode per comparison in sort, renumber, add-locations, apply-changes and cat. Watch the cache budget (`retain`).

**C4. Digest churn.** `key_text` digests every input of every cached reference call, `lookup` re-digests every artifact on a hit, and the identity probe (`--version`, `ldd`, the libraries' digests) runs on every call and again before publishing. That adds up to seconds per run, not minutes. A per-run memo of digests by (path, stamp) is easy. Low priority.

## D. Higher payoff, higher risk: payload-identity splicing

osmium decodes each `PrimitiveBlock` independently: its own string table, granularity and dense deltas. File-level state is limited to the header's features. So a pbfhogg output blob whose serialized `Blob` payload is byte-identical to an input blob decodes to the same OPL lines as that input blob. The check could take those lines from the input's (shared) decoding and have osmium decode only the re-encoded blobs. It would hand osmium a mini-PBF made of the output's header frame plus those blobs, framed as written, and splice the lines in using claim counts, which are already proven by `index_truth`.

Where this would pay:

- `cat --type` on indexed input, where every blob is copied whole (the check's own `WayLocations::Kept` reasoning says so);
- `cat --dedupe` passthrough stretches (`dedupe_plan` already knows exactly which blobs are passthrough and checks their payload bytes);
- possibly `sort` on sorted indexed input.

The risk is real: it stops having osmium read the output's framing. A BlobHeader that libosmium would reject but `pbf_frames` accepts would no longer fail. It would need a verifier framing check at least as strict as libosmium's reader (type string, header length bounds, datasize), held to that by a negative control. Treat it as a second-phase experiment after A. On its own it does not justify the rewrite.

---

## Lateral findings

**L1. sort's order verdict can never fail, and the same pattern is in `compare_outputs` and apply-changes.** In `sort::run`, `check_sorted` runs after a strict `same_elements`, and that comparison errors on any disorder, so the verdict and its "header declares Sort.Type_then_ID but element order disagrees" hint can never appear. In `compare_outputs` and apply-changes, the order verdict is recorded first, but the strict merge after it then errors anyway. So a disordered output always ends as an `Err` whose text names an OPL file in the check directory, not the order verdict. Folding order into the merge (B1, B2, B7) both saves the decode and gives back a deliberate diagnostic.

**L2. Small coverage gap in degrade.** `drop-a-again` is only element-compared with `drop-a`. Its header, indexdata (both the verifier's blob decode and `check --index`), blob sizes and layout are never judged. A nondeterminism in blob headers or indexdata, invisible in OPL, would pass. The B5 byte comparison closes this as a side effect when the files are byte-identical. When they are not, judging `again` with `run.common` would close it fully.

**L3. The brief's suggestion to use pbfhogg's own readers conflicts with the verifier's independence rule.** `crates/verify/Cargo.toml` says "Links no pbfhogg: no verdict may rest on code compiled into the verifier." A pbfhogg reader decoding pbfhogg's output would hide a shared decoder bug, which would weaken every check. The levers are osmium-side restructuring and the verifier's own `pbf_frames` decoder, not `crates/lib`.

**L4. `process::settle` reaps every child of the process via `waitpid(-1)`.** That is the hidden serialization point: no check can have two live children. Any concurrency (A, or overlapping a pbfhogg run with the previous output's decode) has to change this first.

**L5. 8 KiB buffers on multi-GB streams.** `opl_compare::Stream`, `verify_reencode::Listing`, `apply_changes::opl_lines` and `verify_change_listing::zip` all use `BufReader::new`. That is about 2 million `read` syscalls per 16 GB merge. Use 1 MiB.

**L6. `pbf_frames::read_block` opens the file for every blob.** Used by `index_truth`, `blob_ids`, `blob_ids_and_shape` and `dedupe_plan::entries`. Harmless in correctness, wasteful at about 7,700 blobs per file (B8).

**L7. Peak scratch per comparison is two full OPL decodings, about 15 GB on denmark, on the same drive as the pbfhogg outputs.** That drive's page cache is also what makes the repeated input decodes cheap or expensive. This is a source of run-to-run wall variance that streaming removes.

**L8. add-locations runs `check --locations --agreement` on each engine output, and degrade judges `--strip-locations`.** If the two engine outputs are byte-identical, the second `check --locations` and the `digest --form full` parity are implied (and in A the whole external side collapses onto the sparse decoding). The same digest memo catches it for free.

**L9. `field_policies` guards the policies, not their reuse.** If the decoding memo (A, C1) ever derives NO_LOCATIONS or CONTENT records from a FULL decoding by text projection, instead of a second osmium run, field-policies has to gain a control holding projection(FULL) to osmium's own policy decoding on the slice. Until then, decode once per (file, policy) and do no projection. The extra decode of the input under a second policy costs one decode per run and is not worth the new verifier code.

## Order of work I would take

1. **Add the merge step label**, so the PASS line shows the decode/merge split, and measure one denmark `all`.
2. **Apply B1-B7**, check by check. Every item is exact, and together they about halve the decodes.
3. **Make B8 parallel and memoized.**
4. **Build the rewrite (A) and C1**, which need the launch-group and memo work from the sibling scopes.
5. **Then C3**, then D only if what remains of a warm run still points at decoding pbfhogg's own outputs.
