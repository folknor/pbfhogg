The biggest opportunities are **sharing payload work across checks**, **replacing location-check rereads with a partitioned join**, and **computing results while validating the wire data**. I would rewrite those paths aggressively.

The existing code already avoids several obvious traps: inspect summarizes on decode workers, ID/reference checks use projections, and index verification returns compact per-blob verdicts. The remaining opportunities are beyond simply adding parallel iteration.

These conclusions come from source inspection. I made no edits and ran no measurements.

**1. Fuse selected checks into one execution. Highest conviction, broadest payoff.**

The CLI runs locations, IDs, index verification and references as independent library calls. A bare `check` already runs IDs and references separately. Each constructs its own execution and repeats payload work. See [run_check](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:3738).

For an indexed file, reference checking's node, way and relation phases mostly read disjoint portions of the file. Those phases are not themselves three whole-file decodes. The duplication is between checks: ID verification decodes node/way blobs that reference verification subsequently reads, decompresses and validates again. Adding `--index` introduces another independent payload scan.

I would replace this composition with a dedicated audit execution that:

- Reads and decompresses each relevant blob once.
- Computes ID order evidence, membership, reference findings and index observations from the same parsed fields.
- Shares observed-ID membership between reference checking and full duplicate checking.
- Retains separate results and policies for each selected check.

For arbitrary input order, resolve references through deferred misses rather than requiring separate command-wide executions. A successful membership lookup is final because membership only grows. A miss becomes a compact unresolved request, resolved when the target set is complete.

This also improves the unindexed reference path. Today, `unindexed_phase` discards an entire way/relation judgement when any applicable reference misses, then schedules the blob for rereading. One unresolved reference can therefore cause all its already successful references to be decoded and checked again. Keep the unresolved references, their multiplicities and necessary source positions instead. See [unindexed_phase](/home/folk/Programs/pbfhogg/crates/lib/src/commands/check/refs.rs:664).

**Why it could pay:** the default command can eliminate a repeated read/decompress/validate over its dominant node and way data. Combined audit selections offer still more reuse. This removes work rather than making the same work slightly cheaper.

**Risks:** policy differences are substantial. Index checking has a narrower payload acceptance test than block construction; inspect accepts partial index coverage; some checks stop on contradictory kind claims; type filters restrict which payloads ID checking examines. Sharing physical work must not accidentally expand a mode's refusal scope. Error precedence and capped-detail ordering also need explicit preservation. Unresolved requests need bounded storage, including spilling for badly ordered files.

This should be a concrete implementation for these checks, without first designing a general command execution framework.

**2. Replace location checking's repeated PBF scans with an ID-partitioned join. Highest scaling payoff.**

Location checking first discovers node keys, optionally scans ways to add reference keys, then processes coordinate-store partitions. For every partition it reads applicable node blobs and **every way blob**. See [check_locations_inner](/home/folk/Programs/pbfhogg/crates/lib/src/commands/check/locations/mod.rs:766) and [run_partition](/home/folk/Programs/pbfhogg/crates/lib/src/commands/check/locations/mod.rs:1019).

If there are \(P\) partitions, the way-reference and inline-coordinate streams are decompressed and decoded approximately \(P\) times, plus the agreement-mode key pass. Each iteration decodes references belonging to other partitions merely to discard them.

That is the wrong external-memory algorithm. The memory limit multiplies work over the original compressed input.

I would implement a partitioned join:

1. Decode and validate the input once.
2. Route node records and way-reference observations into buckets by target node ID.
3. Reduce each bucket independently: combine all node records for an ID, then evaluate its inline references.
4. Reduce counters and retain violation details using their existing report-order keys.

Use a resident join when its state fits memory; spill partitioned records when it does not. Partition ownership can also remove shared atomic updates from coordinate-state reduction.

The current slot logic already supplies the required algebra: node records reduce into coordinate, conflicting or unusable states, and absent-node inline coordinates reduce into agreement/disagreement. Preserve that logic while changing where it runs. See [Store](/home/folk/Programs/pbfhogg/crates/lib/src/commands/check/locations/store.rs:177).

**Why it could pay:** source decompression and reference decoding become independent of the number of memory partitions. The separate key-discovery and node-reread stages can disappear too.

**Risks:** expanded scratch records can be much larger than compressed PBF. For a file needing only one partition, writing every reference to scratch could lose badly. Skewed IDs need bounded bucket handling, and every node record must remain represented because unreferenced conflicting nodes are also reported. Preserve the exact coordinate-domain arithmetic, duplicate-node state precedence, absent-node agreement and violation ordering.

There is also a contract issue to resolve explicitly: the report exposes `partitions` and `blob_decodes`. Keeping those fields is straightforward; preserving their old numerical values while making them accurately describe a different algorithm is impossible. They should not become fictional accounting.

**3. Fuse validation with useful decoding. High-conviction architectural work across this scope.**

The projection machinery is a good starting point, but it often projects *positions of arrays that were just scanned*, then decodes those arrays afterward.

Examples:

- ID validation counts dense IDs, then `IdScratch::each` delta-decodes them.
- Way-reference validation scans the reference array, then reference checking decodes it.
- Dense tags are validated, then inspect scans them again to count tagged nodes.
- Extended inspect scans metadata columns after their length validation.
- Location checking validates the block, then runs its separate raw scanners over it.

The key seams are [validate_groups](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1427), [GroupProjection](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1319), [IdScratch::each](/home/folk/Programs/pbfhogg/crates/lib/src/read/project_ids.rs:267) and [InspectProjection](/home/folk/Programs/pbfhogg/crates/lib/src/commands/inspect/project.rs:184).

I would change the decoder/validator boundary so requested values are decoded and consumed during their validation traversal. Examples include:

- Checked ID decoding that also computes order summaries and membership updates.
- Dense-tag validation that also determines tagged-node counts and requested key usage.
- Way-array decoding that checks lengths while supplying references and coordinates.
- Metadata-column decoding that checks cardinality while computing requested coverage or digest fields.

Results remain provisional until the blob passes its applicable checks. Failure discards local statistics or aborts the execution before a report escapes. There is no need to materialize an element graph merely to enforce this.

The current group validator also separately traverses group framing, nodes, ways, relations and dense-message discovery. A single framing traversal can gather the message spans needed to preserve validation and element order.

**Why it could pay:** it removes repeated packed-array interpretation and message parsing across billions of elements, while also enabling the fused checks above.

**Risks:** do not replace cheap validation of an unused column with full value reconstruction. `packed_varint_count` already processes ordinary data a word at a time. The appropriate request describes both useful values and required validation; they are different things. Preserve malformed-varint handling, narrowing, wrapping arithmetic, repeated-field rules and refusal precedence.

The block-decode neighbour should provide these requested checked traversals. The index-scanning neighbour should provide raw claims and observations without imposing a stronger validity policy than the requesting mode currently has.

**4. Rewrite digest production around canonical wire projections. High conviction on wasted work; medium conviction on the resulting wall-time ceiling.**

Digest still requests a built `PrimitiveBlock`, classifies it, iterates `Element`s and writes canonical records into a `Vec<u8>`. The ordered consumer then hashes those records. See [summarize_block](/home/folk/Programs/pbfhogg/crates/lib/src/commands/digest.rs:502) and [DigestState](/home/folk/Programs/pbfhogg/crates/lib/src/commands/digest.rs:596).

This repeats work unnecessarily:

- Plain element messages were parsed during validation and are parsed again during iteration.
- Full-form plain metadata is accessed by `check_user` and again by record production.
- Dense iteration finds tag-run boundaries before checked tag iteration reads their pairs again.
- Content forms reconstruct dense metadata columns whose values are discarded, although user-name references still require validation.

I would produce canonical bytes directly from checked wire projections. Refactor `content.rs` so its canonical field-writing rules remain authoritative without requiring an `Element` as their input. Preserve exactly the current record framing, stored tag/member order, coordinate normalization and metadata claims.

This remains an aggressive producer rewrite even if its first implementation retains bounded canonical buffers.

**Why it could pay:** even an untagged content node produces 19 canonical bytes. Billions of nodes expand into a very large byte stream, and the current producer performs substantial interpretation before producing it.

There is also a particularly worthwhile **local change**: content and content-locations digest hash the node prefix twice, once into `all` and once into `nodes`. Those states are identical throughout the node prefix. Hash it once, obtain the node digest through the non-consuming `digest128()`, then move that state forward as the all-elements hasher when ways begin. No digest change or hash-state cloning is required.

The larger ownership problem remains: every canonical byte crosses to one hashing consumer. A true rewrite of that seam would assign canonical stream offsets from compact length summaries, then have workers compute exact XXH3 stripe contributions and boundary fragments. The ordered fold would perform the required state transitions without rereading the entire canonical stream.

That is technically plausible from the installed XXH3 implementation, but its throughput payoff is **medium conviction**. It requires algorithm-specific handling of offsets, scrambling, short inputs and final stripes. Combining ordinary per-blob digests would change the answers and is invalid. I would prioritize direct canonical production and node-prefix reuse before making this custom hash engine the main undertaking.

**5. Make full ID verification's fallback proportional to ambiguity. Medium-priority algorithmic redesign.**

The clean sorted path is already sensible: strict ID order proves uniqueness without a membership set. But any order violation makes full verification reread the entire affected kind with a shared atomic set. One misplaced blob can trigger another planet-scale node scan. See [verify_ids_full_parallel](/home/folk/Programs/pbfhogg/crates/lib/src/commands/check/verify_ids.rs:920).

The proof pass should also retain each blob's observed canonical ID interval and internal strict-order result.

Then:

- Internally strict blobs with disjoint intervals cannot share IDs, even if their file order is wrong.
- Duplicate checking is needed only for internally non-strict blobs and overlapping interval components.
- Only those components need replay or retained ID data.

Use observed canonical-key bounds, not unverified index claims or raw signed comparisons.

**Why it could pay:** nearly sorted damaged files stop paying a full-kind second pass merely to discover that misplaced ranges contain no duplicates.

**Risks:** broadly overlapping intervals still require substantial work. Duplicate multiplicities, negative-ID findings and capped-example ordering must remain correct. This improves suspicious/disordered inputs; it offers little to the already efficient clean sorted case.

**What each mode actually needs**

The useful projections are much narrower than full elements:

| Mode | Useful payload |
|---|---|
| ID checks | Kind/group position, IDs, counts, order boundaries; membership evidence where needed |
| Reference checks | Node IDs; way IDs and refs; relation IDs and typed member IDs |
| Index checks | Actual indexability shape, counts and numeric ID bounds; coordinates for claimed bbox verification; key SIDs/bytes for tag claims |
| Basic decoded inspect | Counts and tagged-node status; optional ID order and way-location lengths |
| Extended inspect | Exact reader-coordinate bounds, timestamps and metadata coverage, including whether referenced user names are nonempty |
| Node statistics | Node coordinates |
| Content digests | Canonical content fields plus required reference validation, including metadata user names |
| Full digest | Content, exact coordinates, metadata claims and declared prepass fields |

Unneeded values can remain undecoded while their required structural validation still runs.

Combined inspect modes should share payload work too: the CLI currently runs the report and node statistics separately. See [run_inspect](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:5172). Preserve the distinction between an index-based description of claims and a decoded measurement, even when another requested result happens to decode the same blobs.

**Smaller changes worth keeping separate from those rewrites**

- **Bound retained ID violations at production.** Workers currently retain complete violation vectors across the schedule before `merge_kind` applies `max_errors`. Carry exact totals separately from bounded examples. This prevents malformed input from turning a small requested report into enormous materialization. See [BlobVerifyResult](/home/folk/Programs/pbfhogg/crates/lib/src/commands/check/verify_ids.rs:885).
- **Batch membership updates by bitset word or give shards exclusive ownership.** Reference checking currently performs an atomic operation per observed ID. Dense nearby IDs repeatedly modify the same words. Word masks can reduce that traffic substantially, although arbitrary input and cross-worker duplicates complicate ownership.
- **Request fewer index observations.** Index verification currently computes intra-blob sortedness it discards, and computes coordinate extents even where no stored bbox needs comparison. Expose observation requests through the index scanner while retaining its existing malformed-input checks.
- **Narrow inspect getters.** A timestamp-only getter currently enables the complete extended census, including coordinate bounds and other metadata work. Select the requested statistic while preserving the scan's refusals.

I would leave inspect's and index checking's compact ordered folds largely alone. Their consumers process per-blob summaries. Digest's byte-consuming fold and ID checking's retained per-element violations are materially different bottlenecks. `descriptors.rs` is admission logic, not a throughput opportunity.

**Two correctness findings surfaced during this reading**

- **Node-statistics histograms depend on worker scheduling.** Each worker carries a partial 128-coordinate window across whichever blobs it receives, then finalizes its own remainder. Thus scheduling changes coordinate adjacency, histogram buckets and potentially the number of analyzed windows. [WorkerAccum](/home/folk/Programs/pbfhogg/crates/lib/src/commands/inspect/node_stats.rs:190) implements this, and the [parity test](/home/folk/Programs/pbfhogg/crates/lib/tests/inspect.rs:618) deliberately excludes histogram equality. Deterministic file-order windows require global node positions and boundary-fragment reconciliation. That would be a correctness change requiring an explicit contract decision.
- **Inspect and ID checking disagree about negative-ID order.** Inspect's [TypeIdRange](/home/folk/Programs/pbfhogg/crates/lib/src/commands/inspect/types.rs:223) uses signed numeric comparisons; ID checking uses canonical OSM order. For example, `0, -1, -2, 1` is canonically ordered but inspect reports non-monotonicity. This also affects decoded `data.objects_ordered`.
