The biggest opportunities are to remove whole walks, repeated decompression, and per-element work owned by one consumer. I would rewrite those paths before tuning the header parser or dispatch channels.

These conclusions come from source inspection only. I made no edits or measurements and did not read project markdown or git history. The ranking reflects architectural conviction, not measured speedups.

1. **High conviction: replace command-specific schedule construction with one incremental input catalog that also feeds execution.**

   Today, schedule construction is usually a completed phase before execution begins. [`schedule_walk`](/home/folk/Programs/pbfhogg/crates/lib/src/scan/classify.rs:590) walks the entire file, builds a vector, and only then returns the input descriptor to workers. `WillneedPrefetch` subsequently walks that vector again to construct its range list.

   The larger problem is repeated discovery. Sparse ALTW can independently walk headers for its automatic size estimate, way-reference collection, node-coordinate collection, relation-member collection, and output schedule. Each pass rediscovers framing and throws away information another pass needs. The relevant calls are visible in [ALTW orchestration](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:428), [reference collection](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:943), [coordinate collection](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:289), and [output scheduling](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/passthrough.rs:119).

   There are already partial precedents: tags-filter retains a catalog, external ALTW retains metadata, and geocode builds several schedules together. But these retain different subsets and reproduce the planning logic.

   I would make the command own one catalog with:

   - Stable blob ordinal, frame offset, header length, and payload length.
   - The complete readable index claim: kind, range, count, and bbox.
   - Optional tag metadata in compact storage, retained only when useful.
   - Observed facts from payload processing, kept distinct from header claims.
   - Views containing blob IDs or ranges, rather than copied schedule records.
   - Explicit completion and failure state for the discovery walk.

   Crucially, this should be **an executing catalog**, not another prerequisite materialization. Publish bounded batches of discovered entries to workers while appending them to the catalog. Later phases derive their plans from those entries without touching headers again.

   The I/O neighbour should provide a framed stream whose consumer can choose, after inspecting a header, to skip the payload, obtain its compressed bytes, or dispatch a positional body read. For workloads that consume nearly every body, build the catalog during that consumption instead of probing headers first and rereading the same file. [`time_filter_snapshot`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/time_filter/mod.rs:384), for example, currently constructs a complete schedule despite scheduling every data blob.

   **Why the payoff is credible:** this removes entire dependent I/O traversals and overlaps the remaining discovery with useful work. It benefits commands that cannot eliminate their later payload passes.

   **Limits and risks:** the next frame offset depends on the current header's datasize. Parallel header parsing alone cannot break that first-walk dependency. Global sizing decisions may still need catalog completion. Partial-index refusals must still abort the operation even when workers have already started. Keep `InputFront`'s lazy precondition ordering, and preserve separate file descriptions where I/O advice needs isolation. Sharing metadata does not require sharing one descriptor.

2. **High conviction: fuse classification with output production where selection has no backward dependency. Extract simple deserves a substantial rewrite.**

   The sorted simple-extract path currently does:

   - Read and decompress node blobs, materialize tuples, select IDs.
   - Read the node blobs again to produce output.
   - Read and decompress way blobs, materialize references, select IDs.
   - Read the way blobs again to produce output.
   - Classify relations, then read them again to produce output.

   This is concrete in the [node and way phases](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/simple.rs:410) and [relation phases](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/simple.rs:593). The later write schedule is not restricted to blobs with matches.

   Simple extraction has a useful dependency shape: node selection determines way selection; node and way selection determine relation selection. Writing selected nodes does not depend on finishing way classification. Writing selected ways does not depend on relation classification.

   I would replace each classify/write pair with one worker operation:

   - Read and decompress once.
   - Compute selection and the required validation facts.
   - Produce the selected output while the decoded bytes remain available.
   - Return framed output plus compact membership updates.
   - Commit output in order and finish the membership set before starting the dependent kind.

   Verified raw-copy candidates fit this design too: verify while the body is present, then emit the retained frame without a later body read. Rebuilt blobs should go directly through worker-local encoding and compression.

   **Why the payoff is credible:** it removes a second body read and decompression for the blobs currently visited by both phases. It also removes tuple/result allocations and repeated element decoding. This attacks the expensive work named in the task, not just planning overhead.

   **The important correctness complication is duplicate IDs.** Current selection is ID based: one matching occurrence can cause other occurrences of that ID to be emitted. A naive “test this element, immediately write this element” rewrite changes that behavior. Valid sorted inputs permit resolving duplicate runs at ordered boundaries, including runs crossing blobs. Other accepted input shapes need deferred occurrence handling. Preserve this behavior deliberately.

   The same principle should guide other commands, but it does not make every command single-pass. Tags-filter's relation dependencies can discover node inclusions after the node section has passed. Those are real backward dependencies. Extract simple's separate classify/write phases are a much stronger initial rewrite target.

3. **High conviction: remove the single owner of billions of membership insertions.**

   Sparse ALTW's reference collection is a clear structural bottleneck. [`scan_way_refs`](/home/folk/Programs/pbfhogg/crates/lib/src/scan/way.rs:26) first decodes references into reusable per-way scratch. The caller then copies them into a per-blob `Vec<i64>`, sends that vector through the result channel, and a single consumer calls `referenced.set()` for every reference. See [the complete collection loop](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:968).

   Tags-filter uses similar per-blob vectors followed by serial membership insertion.

   This structure bounds memory, but it places an O(total references) loop on one core and transfers expanded IDs between cores before doing random bitmap writes. Adding decode workers cannot scale that part.

   I would rewrite collection around **bounded, chunk-partitioned membership updates**:

   - Workers decode references directly into small batches grouped by destination ID chunk.
   - Each chunk has one reducer owner, so independent chunks update concurrently.
   - Dense updates travel as word masks where profitable; sparse updates remain compact offsets.
   - Reducers publish completed membership state before the next dependent phase.
   - Memory is bounded by queued bytes and active chunks, not a planet-wide set per worker.

   This is a coherent replacement for the collection algorithm, not a change of channel implementation. The existing [`IdSet` atomic setters](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:637) and the [geocode reference collector](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass1_5.rs:23) also show that workers need not always send individual references to a serial owner. Where a bounded dense allocation is appropriate, direct checked atomic insertion is a smaller alternative.

   **Why the payoff is credible:** it removes the serial reference loop and an intermediate expanded representation. Reference-heavy commands can expose this bottleneck even when decompression is parallel.

   **Risks:** chunk skew, batching overhead, and cache coherence can erase gains on sparse workloads. Do not allocate a full ID-space bitmap per worker. Header maxima are not safe allocation guarantees, and dangling way references can exceed the maximum actual node ID. Overflow handling must remain complete. ALTW's shared-reference detection also needs its current multiplicity and closed-ring treatment preserved.

   I would prioritize this over replacing `Mutex<Receiver<usize>>`: the mutex is acquired per blob; the consumer insertion loop runs per reference.

4. **High conviction about wasted work, medium conviction about whole-command payoff: replace post-validation projections and tuple scans with fused field execution.**

   The code already has useful projection APIs, so “use projections” alone is insufficient.

   [`BlockBuffer::project`](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1150) avoids constructing a `PrimitiveBlock`, but its hooks run after validation has scanned the relevant arrays. `KindIds` and `WaySpans` retain spans that consumers subsequently decode. [`validate_groups`](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1427) also traverses group framing and then revisits the group through separate element iterators.

   Meanwhile, the raw scanners avoid much of that overhead but impose other materializations:

   - Node scanning commonly builds all `(id, lat, lon)` tuples before selection, tallying, or storage.
   - Way scanning always builds a complete reference vector before calling its consumer.
   - The callbacks cannot return an error, which discourages putting fallible tallying and storage directly into the scan.

   I would ask the decode neighbour for a field execution path that combines required checking with useful computation:

   - Fallible node sinks receiving decoded coordinates directly.
   - Reference folds that can perform membership tests, accumulation, or bucket emission without a mandatory `Vec<i64>`.
   - ID/order/count/bbox summaries accumulated during the same decode.
   - Borrowed spans for fields that will actually be reused.
   - Results kept provisional until all checks required by that command succeed.

   Two concrete uses are immediate: sparse ALTW could combine coordinate decoding, `ClaimWalk`, rank lookup, and storage without its [tuple replay](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:312); simple-extract way classification could compute its “any referenced node matches” predicate during reference decoding.

   Selection may finish early, but required validation must continue. A way matching its first reference does not make a malformed trailing reference legal.

   The metadata scanner has a related overreach. [`Walk::run`](/home/folk/Programs/pbfhogg/crates/lib/src/blob_meta/scan.rs:148) does nearly the same work for ID-only and tag-only requests. Tag-only scanning still reconstructs IDs, coordinate extremes, and sortedness. ID-only scanning still materializes string-table slices and traverses tags. Separate required validation from requested summaries: for example, array cardinality checking can use the existing [`packed_varint_count`](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:800) without reconstructing values that nobody consumes.

   **Why the payoff is credible:** these loops run billions of times, and several materializations exist only to cross an internal API boundary.

   **Risks:** this is decoder work with a large correctness surface: wrapping deltas, unpacked fields, repeated fields, late scalar values, mixed groups, coordinate scaling, and ordering. The raw scanners and full validator currently accept different subsets of malformed input. Unifying their implementation must not accidentally change command refusals. Also, removing a cheap validation byte scan is less valuable than eliminating a decompression pass; that is why this ranks below the preceding rewrites.

5. **Medium-value changes worth making after the structural work.**

   **Answer tag-index predicates without building owned key objects.** Header walking retains tagdata as an owned byte buffer; [`TagIndex::deserialize`](/home/folk/Programs/pbfhogg/crates/lib/src/blob_meta/tag_index.rs:115) then allocates a vector and a separate box for every key before a yes/no query.

   Use a borrowed, validated key view or evaluate compiled exact-key/prefix predicates directly over the encoded sorted list. Catalogs can retain one byte arena and compact offsets where repeated querying warrants it. Header consumers that need no tag metadata should request no retention. `Frames` already has field-retention controls.

   Preserve complete canonical validation before declaring a miss: malformed ordering, duplicates, bad counts, or trailing bytes must fall back conservatively rather than justify a skip. Empty-key compatibility behavior also matters. This removes substantial allocation traffic across many headers, but does not fix cold header-walk latency.

   **Use retained ID ranges to plan sparse dependency work.** Tags-filter's [`CatalogEntry`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:1299) retains kind but discards ranges. Its later dependency and output schedules consequently cannot use [`IdSet::any_in_range`](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:800), although getid already does.

   Retain ranges and intersect them with the final inclusion sets before scheduling bodies. This can eliminate reads and decompression for sparse selections. It will do little when matches occur in nearly every blob. Apply it within the existing trust rules and do not bypass payload checks that the command is required to perform.

   **Replace repeated relation closure scans with frontier processing.** Tags-filter's [closure loop](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:1208) scans every relation blob on every iteration and revisits already-expanded relations. A retained adjacency representation plus a frontier avoids repeated decompression and expansion. Alternatively, the catalog can locate only blobs containing newly included relation IDs. This is structural, but its usual whole-command ceiling is lower because it concerns the relation subset.

   **Stop enumerating set members just to count them.** ALTW calls `referenced.iter().count()` immediately before the sparse builder constructs a ranked set. `IdSet::len()` already counts dense words with popcount, and `RankedIdSet::total_count()` provides the count after ranking. This is a concrete local opportunity outside the central scan code.

Two correctness hazards should remain explicit during these rewrites. [`blob_filter_check`](/home/folk/Programs/pbfhogg/crates/lib/src/scan/classify.rs:441) ignores bbox filtering even though it accepts a `BlobFilter`; current callers compensate, but it is a misleading planning interface. And [`parallel_scan_blobs_raw`](/home/folk/Programs/pbfhogg/crates/lib/src/scan/classify.rs:1569) does not enforce the schedule's kind claim: callers must do so themselves. These are API hazards, not demonstrated wrong results in the callers inspected.

My implementation order would be the incremental catalog and execution path, fused simple extraction, then parallel membership construction. Those remove complete traversals, decompression work, and serialized element processing. Compact metadata and scanner kernels follow once those larger costs are gone.
