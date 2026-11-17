I would rewrite this engine around **bounded, bucket-local coordinate tables and a fused output assembler**. The current design externalizes the necessary permutations, but also sorts billions of occurrences, repeats routing information per reference, and materializes a third scratch stream solely to connect two concurrent stages.

The highest-conviction opportunities are removing that final materialization and replacing the occurrence sort with an in-memory lookup confined to one node partition. Smaller changes to buffers or worker caps leave those costs intact.

This assessment comes from source inspection only. I ran no benchmarks and made no changes.

**The current disk traffic and the practical floor**

The implementation already avoids spilling the complete node-coordinate stream. Stage 2 reads coordinates directly from the original node blobs. Its large scratch streams are reference requests, resolved occurrences, and encoded payloads.

Let:

- \(R\) be all way-reference positions.
- \(F\) be positions whose nodes are found.
- \(I\) be the encoded request bytes.
- \(P\) be the ordinary, file-backed coordinate payload bytes.

| Stream | Written | Read | Reason |
|---|---:|---:|---|
| ID-bucket requests | \(I\) | \(I\) | Move requests from way order to node-ID partitions |
| Resolved slot records | \(12F\) | \(12F\) | Move results back toward way order |
| Encoded coordinate payloads | \(P\) | \(P\) | Connect stage 3 to stage 4 |

Requests normally occupy eight bytes each, with sixteen-byte escapes. Resolved records occupy twelve bytes each. Thus normal successful input incurs approximately **\(40R + 2P\) bytes of logical scratch traffic**, excluding sidecars and the comparatively small RAM-backed straddler path. These are format-derived quantities, not measured device traffic. Page-cache hits can absorb some reads, while writeback still costs storage bandwidth. See [request encoding](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/mod.rs:488), [resolved records](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/mod.rs:393), and [payload writes](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage3.rs:472).

For arbitrary references, modest RAM, and bulk sequential scratch access, **two external permutations are a sound practical floor**:

1. Write requests by node partition, then read them for resolution.
2. Write replies by output partition, then read them for final assembly.

That is two scratch write/read cycles. The third cycle is unnecessary.

This is not a universal lower bound: a sufficiently resident coordinate index, random disk access, or repeated source scans changes the tradeoff. For this engine's intended operating point, however, eliminating the third cycle is straightforward architectural progress.

The input PBF has a separate ledger: ways are read and decompressed in stages 1 and 4; relevant node blobs are decoded in stage 2, with boundary blobs potentially decoded repeatedly; retained nodes can be read again during assembly. Reading ways twice is reasonable when their original bytes cannot remain resident until resolution. Spilling another copy of those bytes merely substitutes scratch I/O for the second input read.

1. **Replace the occurrence sort with a partition-local coordinate table.**

   Stage 2 reads each request shard, expands its packed words into twelve-byte `IdRecord` values, and comparison-sorts the whole bucket by node ID. It then decodes node blobs into `NodeTuple` vectors and merge-walks the two collections. The sort exists to make a streaming coordinate source usable without a lookup table. See [bucket preparation](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage2.rs:139) and [node decoding and resolution](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage2.rs:490).

   A coordinate table for **one partition** is a very different proposition from a planet-wide coordinate index. For illustration, an ID maximum of fourteen billion split into 256 ranges gives roughly 54.7 million IDs per range: about 438 MB for eight-byte coordinate slots. A small set of active tables fits comfortably within the stated host class.

   The stronger design is to decode a partition's node blobs into a bounded RAM table, then stream its requests through that table in their original way order. No occurrence sort is needed. No whole-bucket `Vec<IdRecord>` is needed. Node extraction can populate the table through its existing callback rather than materializing a `NodeTuple` vector.

   Dense tables suit ordinary bounded ID ranges. Sparse ranges need compact membership/rank indexing or another bounded representation. The existing [ranked ID API](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:1057) supplies useful primitives, although its current shape should not constrain the redesign.

   This also fixes a structural memory weakness: the current equal-width ID buckets do **not** bound request population. A heavily referenced node can put an arbitrarily large fraction of all references in one bucket, which `prepare_bucket` reserves and sorts at once. A table plus streamed request chunks bounds memory independently of that occurrence count.

   Preserving request order unlocks better scratch formats. Stage 1 already knows the source blob and reference positions. Group requests into fragments belonging to one source blob and node partition:

   - Store blob identity and count once per fragment.
   - Encode local node IDs as a column.
   - Encode monotone destination positions as deltas or runs.
   - Carry closure flags separately when injection requires them.

   Replies can retain that fragment order and encode destination positions compactly instead of repeating a four-byte slot number beside every eight-byte coordinate. Coordinate columns can also use independently framed delta or bit-packed blocks. The important opportunity is preserving locality across the whole join, rather than compressing twelve-byte records after destroying that locality.

   **Why this is plausibly high-payoff:** it removes comparison sorting over every occurrence, the packed-to-struct expansion, and substantial memory traffic. It creates the ordering needed to reduce scratch bytes on both sides of the join.

   **Risks:** large tables trade sorted access for random DRAM gathers, so the wall-time benefit remains a hypothesis. Partition sizing must follow an explicit memory budget. Domain errors must still apply to referenced coordinates without accidentally rejecting additional unreferenced nodes. Under `--inject-prepass`, shared-node decisions require counting all non-closure occurrences before emitting final pins; that needs a counting pass over retained request blocks, or an additional request read when they cannot be retained.

   This is a coherent join rewrite. Merely substituting a radix sort would capture a smaller part of the opportunity.

2. **Fuse slot reconstruction, coordinate encoding, and way assembly.**

   Stage 3 reads a complete slot bucket, scatters its records into an eight-byte-per-slot array, encodes per-blob coordinate payloads, writes them to worker files, and publishes their locations. Stage 4 then reads those payloads back alongside the original way blobs.

   Running these stages concurrently does not remove their intermediate storage. The actual handoff remains `pwrite -> router -> pread`. See [stage 3 reconstruction](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage3.rs:308) and [stage 4 consumption](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage4.rs:643).

   I would make a bounded group of complete way blobs the final assembly work unit. One worker reconstructs that group's coordinates, reads its original way blobs, splices the locations, and submits completed blocks for compression. Coordinate ownership stays with that worker until assembly finishes.

   Group boundaries should fall between blobs and be sized by reference count. That removes the equal-slot-range straddler machinery: no left/right halves, concatenation, separate straddler encoding, or per-blob readiness router. A pathological oversized group still needs an explicit bounded handling path.

   This removes the entire \(2P\) logical scratch round trip. It also prevents stage 3 from running far ahead and turning the filesystem page cache into an implicit payload queue.

   The native payload format should change even if a producer/consumer split remains. Today stage 3 emits interleaved latitude and longitude varints. Stage 4 walks **every reference again**, discovers both varint boundaries, and copies the bytes into separate latitude and longitude buffers. The arithmetic is finished; the representation is wrong for its consumer. See [payload encoding](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/coord_payloads.rs:838) and [de-interleaving](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage4.rs:1174).

   Use a per-way offset table or length-prefixed entries whose contents are already complete appendable PBF fields:

   ```text
   field 9: packed latitude deltas
   field 10: packed longitude deltas
   field 20: optional pin bitmap
   ```

   Stage 4 then appends one byte range per way. It never walks coordinate varints. Zero-reference ways have an empty append range. Reference counts and diagnostic totals can travel in the entry metadata if needed.

   With fusion, those bytes need never become scratch data at all. The assembler can also compute enclosing message lengths and copy original fields plus appended fields into the final block buffer, reducing the current `reframed_way -> group_out -> output` copying.

   **Why this is plausibly high-payoff:** it removes an entire materialization, billions of varint-boundary checks, and several ownership and copying steps.

   **Risks:** admission must be bounded in bytes, including coordinate arrays, decompressed input, assembled blocks, and compression queues. Ordering can otherwise retain later results behind a slow early group. The rewrite must preserve stale-location stripping, pin semantics, coordinate framing checks, and output publication behavior.

   This is my highest-conviction architectural change.

3. **Replace the file fleets and shared appenders with private extent storage.**

   Stage 1 opens 256 buffered shard files per worker. Its CPU parallelism is therefore capped by descriptor allowance. The associated buffers also scale as workers times buckets. This couples decode throughput to file layout. See [descriptor-based worker sizing](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage1.rs:650).

   Stage 2 uses the opposite ownership shape: one shared writer per output bucket. Workers flush local buffers under a bucket mutex and can hold that mutex across an actual file write. Finally, `finish` serially flushes, calls `sync_data`, and evicts every bucket before stage 3 starts. See [slot writes](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage2.rs:710) and [the stage-ending writeback loop](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage2.rs:281).

   I would use one or a few append-only scratch files per producer, with large framed extents and a directory mapping logical partitions to those extents. Producers retain private files; readers use positional reads of committed extents.

   This gives each worker independent file ownership without requiring one descriptor per logical bucket. It removes the shared output-bucket append mutexes and makes the fragment formats above natural. The [scratch API](/home/folk/Programs/pbfhogg/crates/lib/src/scratch.rs:551) currently supplies basic file creation and opening; the algorithm needs ownership of append storage, committed extents, and consumed ranges.

   Completion should be published per output group, using the request manifest to know which node partitions contribute to it. Assembly can begin when those contributors finish, without waiting for unrelated partitions or a global sync-and-evict sweep.

   There is a real limit here: an output group may reference nearly every node partition. Queues alone cannot erase that dependency. I would expect reliable gains from removing forced writeback and shared ownership; early assembly overlap is more dependent on reference distribution.

   Scratch cache management should follow extent lifetime. Consume retained bytes directly where possible, reclaim consumed extents promptly, and manage writeback for actual memory pressure. A successful write makes bytes available to a same-run reader; durable persistence of every intermediate file is not part of the output contract.

   **Why this is plausibly high-payoff:** it removes descriptor-bound CPU utilization, append serialization, and a mandatory storage barrier.

   **Risks:** poorly sized extents can replace file-count problems with fragmented reads and an excessive directory. Extent publication, short transfers, cancellation, and error propagation need precise ownership. A single shared arena for all producers would reintroduce shared-file contention; private producer files are central to this design.

4. **Produce retained-node output while stage 2 already owns the decoded nodes.**

   Stage 2 extracts node coordinates. Stage 4 later rereads and decompresses selected node blobs, constructs `PrimitiveBlock`, filters nodes, and re-encodes them. With `--keep-untagged-nodes`, stage 4 instead rereads all node frames for passthrough. See [node assembly](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage4.rs:711).

   Move retained-node production into the node-input work used by the join. For blobs requiring filtering, reuse the decompressed bytes for both coordinate extraction and retained-node assembly. For all-node passthrough, an input task could supply the original frame to the output path while its payload supplies the coordinate scanner.

   Output still needs one owner per source blob, especially at ID-partition boundaries. Node blobs untouched by reference resolution must remain scheduled if they contain nodes the command retains.

   **Why this can matter:** it removes a second read/decompression cycle and lets the node prefix of the output progress during the join.

   **Risks:** the benefit depends on how much node output survives filtering. Ordered node output must not force an unbounded queue behind a slow partition. This is a worthwhile structural follow-up, with more conditional payoff than the first two changes.

**Medium-value changes**

Several narrower improvements are clear, but they should not define the redesign.

- Stage 1 copies each way's decoded references into a second, blob-wide `blob_node_ids` vector, then walks it again to emit records. Emit fragments from the scanner callback and maintain the blob-local slot counter directly. Closure detection already has the complete per-way slice available. See [reference staging](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage1.rs:420).
- Stage 3 holds the entire twelve-byte record bucket alongside the eight-byte scatter array. Chunked reads can scatter directly into the destination array, substantially reducing its live memory without changing the algorithm.
- Passthrough assembly allocates and preads a frame on the single consumer thread. The writer already provides [range-copy submission](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1577). Use it where the selected backend supports it, coalescing only provably adjacent eligible frames. The framing/input neighbour should provide owned frames for decode and forwarding when range copy is unavailable.

A more ambitious but lower-confidence extension is deduplicating requests by **node ID and output group**. The reply would carry a small coordinate dictionary rather than one coordinate per occurrence, and assembly would resolve original way references against that dictionary. Its payoff depends on duplication within output groups; global node reuse alone is insufficient. It also adds lookup work and must preserve per-occurrence missing counts and shared-node pin semantics. I would rank it below the rewrites above.

`coord_pack` itself offers no large fixed-width compression win: arbitrary geographic coordinates already require 63 bits. Useful byte reductions need locality-aware block encoding. One detail is less constrained than the comments suggest, though: presence plus a pin can fit in eight bytes. Encode biased latitude plus one in 31 bits, reserve zero for absence, and use the remaining bit for the pin. That preserves Null Island distinctly. It is useful representation freedom for a new local table, rather than a major standalone optimization.

**Correctness findings**

Two accepted duplicate node IDs with different coordinates can already produce different backend results. The external merge walk resolves references against the first matching tuple. The sparse builder stores both tuples into the same slot, so within one blob the later tuple wins; duplicates across parallel blobs can also be scheduling-dependent. Equal IDs pass the ascending checks. This is a code-level parity defect that the table rewrite needs an explicit policy for. See [external matching](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage2.rs:612) and [sparse insertion](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:330).

Missing-node handling also has an API inconsistency. The command wrapper enforces its missing-location policy after the external engine returns, but the public low-level [`external_join`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/mod.rs:831) publishes directly and exposes no corresponding policy parameter. Direct callers can receive published zero placeholders that the default command rejects.

Moreover, the exact resolved count is available when stage 2 returns. The command currently finishes coordinate encoding, way assembly, compression, and output writing before enforcing the strict policy. Passing that policy into the orchestration would allow failure immediately after the join, preserving successful output semantics while avoiding a potentially enormous unsuccessful final pass.
