The biggest opportunity is a rewrite of the sparse execution path around **one node ingestion phase that both builds the coordinate store and produces node output**. With sufficient memory, that rewrite should also eliminate the way-reference collection pass entirely. When memory is insufficient, the answer is a bounded join, rather than letting random coordinate lookups spill into uncontrolled paging.

These are code-derived hypotheses, not measured speedups. I read the implementation and neighboring APIs without reading project Markdown, history, or benchmark records. No files were changed.

Let `N` be input nodes, `R` way-reference occurrences, and `U` distinct nonnegative referenced IDs.

1. **High conviction: combine coordinate collection and node output into one decode.**

   Sparse currently decodes every node blob during index construction through `extract_node_tuples`, then decodes it again during output whenever untagged nodes are filtered. The second decode constructs `PrimitiveBlock`, iterates elements, converts metadata, and rebuilds retained nodes through `BlockBuilder`. Relation dependencies are collected only after the coordinate index is complete. See [sparse.rs:312](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:312), [passthrough.rs:309](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/passthrough.rs:309), and [mod.rs:507](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:507).

   The stronger design is:

   - Collect relation-member dependencies before node ingestion, overlapping that work with reference collection when needed.
   - Decode each node blob once.
   - Feed its coordinates into the join structure while selecting its retained output nodes.
   - Compress and submit that node output immediately.
   - Once coordinates are ready, process ways, then copy relations.

   On sorted input, nodes precede ways, so node output can occupy its final output position while coordinate collection runs. There is no need to finish the index before beginning output.

   The node selector should retain the source StringTable and raw tag indices. Plain retained nodes can largely be copied as wire messages. Filtered dense groups require rebuilding their delta columns, but not decoding strings and reinterning tags. The existing [raw node builder](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1417) provides a useful intermediate implementation; a dedicated dense-column filter is the stronger endpoint.

   This removes a complete node decompression pass on the default filtering path, avoids repeated coordinate decoding, and overlaps output compression and I/O with index construction. That is a much larger opportunity than accelerating individual lookups.

   Risks are concentrated in dense metadata column presence, delta reconstruction after deletions, tag delimiters, and preserving plain-node fields. Arbitrary input order needs a separate assembly strategy: retain results by source ordinal or spool completed output fragments. The sorted fast path should not inherit that complexity.

2. **High conviction in the fitting-memory regime: eliminate reference collection by indexing actual nodes.**

   Before processing nodes, sparse currently decompresses ways, decodes their references, constructs the referenced-ID set, and freezes its rank index. Ways are subsequently decompressed and their references decoded again during output. The first traversal exists to save coordinate storage. See [mod.rs:482](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:482).

   When an all-node coordinate store fits comfortably, that trade is backwards.

   Build an immutable coordinate dictionary directly from node ingestion. For sorted input, use independently built ID-range segments containing presence information and compact coordinate arrays. Workers can populate segments without a global insertion owner; a small directory locates the segment for each lookup. Use resident owned storage in this regime, without requiring a preallocated scratch file.

   The resulting execution is relation dependencies if needed, node ingestion/output, way lookup/reframe/output, relation copy. Ways are decoded once. There is no global way-reference union or referenced-set freeze.

   Its payoff grows with `R`: it removes an entire way decompression pass and `O(R)` reference accumulation work. Its cost is storing coordinates for unused nodes, approximately `8(N-U)` additional coordinate bytes when all referenced IDs exist and node IDs are unique.

   This should be a real execution regime, not a universal replacement. Files dominated by standalone nodes can strongly favor reference-selected storage. `inject_prepass` also still needs shared-reference information, so its best execution may differ from the ordinary command.

   One semantic trap: current sparse checks the geographic domain only for referenced nodes. An eager all-node store must not start rejecting an otherwise unused out-of-domain coordinate. Store the raw coordinate pair and judge it when consumed, or otherwise preserve that distinction.

3. **High conviction where reference selection remains worthwhile: remove the single owner of every reference.**

   Reference collection constructs a fresh per-blob `Vec<i64>`, after already decoding references into reusable per-way scratch. Those vectors cross a result channel, and one consumer calls `referenced.set` for every occurrence. Under prepass injection, that same consumer performs shared-reference detection. See [mod.rs:974](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:974).

   Parallel decompression cannot remove this serial `O(R)` tail. Increasing decoder workers eventually just feeds the union thread faster.

   Replace the consumer with **ownership partitioned by node-ID chunk**:

   - Decoder workers partition bounded reference batches among chunk owners.
   - Each owner builds only its disjoint subset of the bitmap.
   - Repeated references can be combined into word masks before publication.
   - Completed chunks move into the frozen directory without a global per-reference merge.

   The code already exposes [chunk ownership and move-based merging](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:875). This does not require one complete planet-sized bitmap per decoder worker.

   For injected shared-node pins, owners retain both “seen” and “seen again” facts. Batch compaction must preserve multiplicity: deduplicating a batch without recording duplicates would lose shared-node information. The existing exclusion of a closed way's repeated terminal reference must also survive.

   The plausible payoff is removing an inherently serial pass over billions of references and reducing cross-thread reference materialization. Risks are partition imbalance, batch traffic, and memory-bandwidth saturation. Direct shared atomic insertion is another implementation option, but chunk ownership offers a stronger design: it avoids replacing one serial bottleneck with cache-line contention.

4. **High conviction: replace the binary routing rule with a resource-aware choice of join.**

   The current router compares an estimated `8N` store with 80 percent of available memory after subtracting a fixed 3 GiB allowance. If memory is unknown, or available memory is at or below that allowance, it selects sparse. See [routing](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:698) and [budget calculation](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:759).

   Three structural problems follow:

   - The actual store is `8U`, not `8N`. Mostly standalone nodes can send a small real working set to external.
   - `8N` is not even an upper bound when references are dangling: sparse allocates slots for referenced IDs absent from the node input.
   - Reducing available memory below the allowance switches back to the random-access engine. The decision is nonmonotonic precisely where resource pressure is greatest.

   Once the coordinate mapping exceeds residency, each lookup in [SparseArrayIndex::get](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:153) can become a dependent page fault. More workers overlap some faults but do not fix the access pattern.

   My preferred regimes are:

   | Regime | Preferred execution |
   |---|---|
   | All-node store fits comfortably | Build actual-node dictionary; skip reference collection |
   | Only reference-selected store fits | Sharded reference collection; compact resident store |
   | Coordinate working set substantially exceeds memory | Partitioned reference/node join with sequential scratch I/O |
   | Near the boundary or strongly skewed reuse | Resident partitions plus a bounded spill join |

   The existing external engine already supplies the essential bounded-memory algorithm: partition references by node ID, join coordinates, then restore way-slot order. Its stage 3 also streams readiness into assembly. A redesign should build on those properties.

   For the middle regime, I would use a full hybrid partitioned join: keep selected ID partitions resident and spill the rest as `(node ID, way position)` records. Resident hits resolve directly; spilled partitions use the external join and publish completed coordinate payloads into the same ordered assembly. This avoids both paging a global mapping and forcing every reference through external scratch.

   That hybrid is a substantial, medium-confidence rewrite. Its risks are duplicate handling, skew, scratch lifetime, backpressure, and keeping resident and spilled results synchronized. Choosing resident versus bounded execution correctly is higher confidence than the hybrid's incremental advantage.

   Sparse and external should converge in lifecycle and output assembly where useful, while retaining different physical join algorithms. Forcing resident lookups through external's global slot materialization would sacrifice their advantage. Build the fast paths first; generalize afterward.

5. **Medium-value structural change: carry each rewrite through compression before handing it off.**

   A sparse worker currently produces an uncompressed `OwnedBlock`, moves it through an ordered pool, and hands it to a consumer. The consumer submits it to `PbfWriter`, which reserves another slot and dispatches another compression/framing job. The writer then orders completed frames again. See [passthrough.rs:276](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/passthrough.rs:276) and [writer.rs:1122](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1122).

   Make a blob job own read, decompress, lookup, reframe, compression and framing. Hand completed frames to the output stage by ordinal. Ideally, ordering happens once, after expensive work.

   The existing [frame_blob_pipelined](/home/folk/Programs/pbfhogg/crates/lib/src/write/framing.rs:124) already supports worker-side compression. This would also let workers reuse their reframe buffers, currently relinquished through `mem::take`.

   The benefit is fewer ownership transfers, less queued uncompressed data, and no ordering barrier before compression. Compression is already parallel today, so this is a credible medium-value improvement, not a prediction of a large compression speedup. Load balance can worsen when one worker owns both a slow lookup and expensive compression; admission should be bounded by bytes as well as jobs.

   Passthrough has a separate concrete missed API: [Pass2Consumer::passthrough](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/passthrough.rs:551) allocates and reads every unchanged frame on the consumer thread. [write_raw_copy](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1577) already queues range copies and is supported by the parallel writer. Use it for supported backends, coalescing adjacent unchanged frames. Preserve an aligned/userspace route for backends that cannot accept range copies.

6. **Medium-value hot-path work: specialize sorted collection and frozen lookup.**

   These are worth doing, but rank below removing passes.

   During node collection, every decoded node is first materialized as a 16-byte `NodeTuple`, then revisited for membership and rank calculation. The existing [extract_node_coords](/home/folk/Programs/pbfhogg/crates/lib/src/scan/node.rs:240) callback API can feed the store directly. It needs a fallible sink or deferred error capture.

   More significantly, on verified sorted ranges, merge the node stream with the referenced-ID iterator. [RankedIdSet::iter_from](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:1118) and `rank(start)` already provide the starting ID and slot. Advance a cursor instead of recomputing rank for every node. Missing referenced nodes must still consume their rank positions.

   For way lookups, [rank_within](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:324) scans up to seven whole bitmap words and popcounts the partial target word. `rank_if_set` separately tests membership before calculating rank. A dedicated frozen coordinate index could store per-word prefixes and combine membership with the target-word popcount. That trades a modest increase in index memory for fewer operations per reference.

   Finally, counters call `iter().count()` over populated IDs, including [the main referenced set](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:494). Use the existing word-popcount `len()` or the frozen total. Diagnostics should not express a per-ID traversal when cardinality is already cheaply available.

The neighboring scopes could unlock these changes with three concrete capabilities:

- Indexdata scanning should supply reusable descriptors, claims and type schedules from one discovery walk. Consolidating header walks is a consequence of the new execution, not the main performance thesis.
- External should accept already collected reference partitions and publish completed way-coordinate results incrementally, without requiring another independent frontend.
- Output I/O should accept both worker-framed blobs and source-range copies through one bounded ordered submission path.

I also found correctness issues or inconsistencies worth flagging:

- **Public external entry point bypasses missing-node enforcement.** [external_join](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/mod.rs:831) publishes after `external_join_in` without calling the missing-location policy used by the main command. It has no `ignore_missing_nodes` argument. Direct library callers can therefore publish placeholder coordinates without the main entry point's explicit opt-in.
- **Conflicting duplicate node IDs have nondeterministic sparse resolution.** Parallel relaxed atomic stores make the writes race-free, but whichever store runs last determines the coordinate. The implementation does not define an input-order winner.
- **Filtered retained nodes can lose coordinate precision.** [process_block](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/mod.rs:1287) converts retained nodes to decimicrodegrees and rebuilds them under default framing. A legal source coordinate with finer precision can change despite the requirement that retained node content remain unchanged.
- **Signed-ID support is inconsistent.** `osm_id.rs` defines canonical signed ordering, while sparse reference collection and coordinate population discard negative IDs. External's diagnostic even suggests sparse can join negative nodes. A rewrite must explicitly preserve or correct this behavior rather than assuming the signed ordering helpers imply signed coordinate lookup support.

My implementation priority would be the fused node ingestion/output rewrite, including the all-node resident regime, followed by sharded reference construction and corrected routing. Those remove work or prevent a fundamentally unsuitable access pattern. Rank tuning and handoff reduction are worthwhile after those architectural costs are addressed.
