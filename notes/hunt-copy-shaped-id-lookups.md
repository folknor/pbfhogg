I would rewrite the copy routing, repack's grow path, getid's dependency traversal, and history time-filter. Their current structures impose costs that faster parsing or compression cannot remove.

The strongest opportunities are eliminating payload reads entirely, making output partitions the unit of parallel encoding, and selecting records before constructing elements. These are conclusions from the current source, not measured speedup claims. I read no project markdown or git history, changed no files, and ran no benchmarks.

1. **Plain cat and type-filtered cat should copy file ranges, not transport blobs through userspace.**

   [cat_passthrough](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:399) calls `Frames::next_raw()`, allocates a complete frame, reads its compressed payload, and hands that allocation to the writer. Indexed blobs need none of those payload bytes inspected.

   [cat_type_passthrough](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:842) is more wasteful: it also calls `next_raw()` before checking the kind. Consequently, even rejected node blobs have already entered userspace.

   The stronger implementation is a header walk producing three actions: omit a frame, copy an original range, or transform a blob. Adjacent copied frames from one input should become one range operation. Plain indexed cat should ordinarily copy almost the entire data section through the kernel. Type filtering should read no rejected payloads and copy selected payloads without loading them.

   This capability already exists. [PbfWriter::write_raw_copy](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1545) supports queued range copies, and [blob_merge's CopyRun](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1596) coalesces contiguous frames. This is an architectural replacement of the read-and-forward path using an existing backend primitive.

   Header-only [degrade](/home/folk/Programs/pbfhogg/crates/lib/src/commands/degrade/mod.rs:655) should follow the same principle. It currently reads every payload, then `reframe_raw` allocates another frame and copies the payload again. Its output should instead consist of a rewritten prefix/header followed by the original payload range.

   The output-I/O neighbour should provide one ordered submission containing inline bytes and referenced file ranges. That avoids forcing callers to assemble contiguous frames. For degrade, unlike plain cat, changed headers prevent coalescing whole frames into enormous ranges. Millions of alternating small header writes and payload copies could erase the benefit unless the sink handles that composition efficiently.

   Risks are concentrated in framing, omitted unknown blob types, input lifetimes, truncation checks, and backend support. Existing direct-I/O and stream fallbacks remain necessary. Kernel copying also does not guarantee that storage avoids reading and writing the data; its unconditional benefit is avoiding the userspace payload path.

2. **Repack's grow path needs a full ownership and work-partition rewrite.**

   The decisive branch is in [worker_split_blob](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:666). A blob with at least `cap` elements is encoded and compressed on its worker. A smaller blob becomes `Vec<OwnedNode>`, `Vec<OwnedWay>`, or `Vec<OwnedRelation>`.

   [run_kind_phase](/home/folk/Programs/pbfhogg/crates/lib/src/commands/repack/mod.rs:445) then feeds every owned element through one central `BlockBuilder`. Parallel decompression surrounds a serial encoder. Tags, user names, references, and members are materialized to cross that handoff, then consumed and freed centrally. Increasing decode workers cannot fix this.

   I would replace that branch with output-partition jobs:

   - Decode each input blob once into a retained byte buffer with compact element boundaries and authoritative counts.
   - Plan output blocks as ordered slices of those buffers, allowing one output block to span several input blobs.
   - Give each output partition to a worker that performs encoding and compression completely.
   - Send completed frames to the ordered writer.

   The planner handles counts and slice descriptors, not individual elements. Buffers remain alive until their last output slice finishes. Admission should be bounded by retained bytes as well as task count. There should be no compulsory conversion to owned strings and vectors between decoding and encoding.

   This can preserve the present packing policy, including flushing a grow run before a shrink blob and keeping shrink remainders separate. Tighter packing is a separate choice where block structure is not promised.

   Degrade's unsort modes have the same structural problem. [worker_decode_kind](/home/folk/Programs/pbfhogg/crates/lib/src/commands/degrade/mod.rs:1345) sends every selected element owned because one adjacent pair per kind must swap. That tiny perturbation serializes the whole kind. The swap belongs in the partition description: change two positions, reconcile the affected boundary, and encode the remaining partitions independently.

   This is high conviction for grow-heavy repack and unsort workloads. Repack shrink and ordinary degrade re-encoding already encode and compress on workers; they do not need the same concurrency fix.

   The principal risks are ordering across partitions, retaining a large source blob for a small residual slice, and accidentally changing metadata or location handling when replacing owned conversions.

3. **Clean, split, and filter operations should transform encoded fields instead of reconstructing whole elements.**

   The current [cat process_block](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:969) resolves tags to strings, resolves metadata users, expands way references into a vector, expands relation members, and interns strings again. Repack's shrink path performs similar work even though most fields are unchanged.

   The repository already demonstrates a stronger seam. [BlockBuilder](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1360) can preserve source string indices and write raw way and relation arrays. [apply-changes](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/element_writes.rs:252) uses those methods. Renumber also has direct field-splicing code.

   For these commands, I would go beyond merely calling those helpers:

   - Cleaning should rewrite the selected `Info` fields or `DenseInfo` columns while carrying unchanged IDs, geometry, tags, references, and members as encoded slices.
   - Splitting ways and relations should copy their unchanged arrays without expanding reference or member vectors.
   - Splitting dense nodes should find column slice boundaries and repair the first delta of each sliced delta column, rather than decode and rebuild every value.
   - Growing blocks should reconcile source string tables once per source table. References and geometry remain independent of that remapping; only fields containing string indices need remapping.
   - Whole compatible groups can retain their independent delta origins instead of being flattened into one element stream.

   This is a coherent replacement for the transform representation, not a request for a general abstraction first. I would implement the concrete clean and repack paths, then share whatever proves useful.

   The payoff is broad: it removes repeated string hashing, reference expansion, member allocation, numeric decoding, and re-encoding of unchanged arrays. Compression still has to run on changed payloads, so compression-heavy workloads impose a ceiling on the wall-time gain.

   Correctness is the hard part. Existing commands normalize coordinates and timestamps in particular ways, distinguish dense and plain metadata presence, and deliberately drop some enrichment fields. A raw copy is legal only where it reproduces those promised results. Different granularities, partial metadata, unusual protobuf forms, and string-table slot zero require explicit handling. Existing raw helpers also copy and index the source string table; they are useful building blocks, not the final fast representation.

4. **Getid with references should traverse candidate blobs, not scan entire kinds repeatedly.**

   Basic getid already checks kind and ID range before reading payloads. Its weakness at large query sizes is that both [walker](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getid/mod.rs:711) and [streaming](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getid/mod.rs:941) include paths perform candidate decompression and processing serially.

   The larger opportunity is [getid_with_refs](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getid/mod.rs:1110):

   - Relation closure initially scans all relation blobs and builds relation-to-relation edges for unrequested relations.
   - A second relation scan may collect newly reached relations' node and way members.
   - `collect_way_nodes` scans every way blob, even for one requested way.
   - The output pass filters blobs by kind alone, then decompresses every blob of each selected kind.

   A request for one way and its nodes can therefore decompress the entire way section and then the entire node section. This is unnecessary given the available ID ranges.

   I would retain a searchable blob directory from one header walk, including kind, ID interval, offsets, and provenance. Dependency discovery would operate on a frontier of requested relation IDs, visit all intersecting relation blobs, collect typed members, then visit candidate way blobs and collect node references. Final output would schedule only blobs intersecting the completed ID sets, in file order.

   Sparse queries should stay sparse throughout. Broad queries should naturally become coalesced scans when their candidate ranges cover most of a section. The current blob-count-only arm selection does not describe that distinction.

   The indexdata neighbour needs to return all candidate intervals, including overlapping blobs and versions crossing boundaries. A reusable directory could also remove the repeated header walk for repeated queries, but that benefit depends on a reliable binding to the exact input file. It does not make the first query free.

   There is another useful rewrite at the decoding seam. `SelectProbe` and getparents' `ParentProbe` currently return a Boolean. On a hit, the command builds a block and walks its elements again. Projections should instead record selected message spans or dense positions during validation, then emit those selections directly. [GroupProjection](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1319) and the existing [reference-span projections](/home/folk/Programs/pbfhogg/crates/lib/src/read/project_ids.rs:60) already provide much of the machinery.

   Minimal application decoding should be:

   - **Getid:** own IDs for candidate elements; complete content only for selected records. Dependency discovery additionally reads selected ways' refs and selected relations' typed members.
   - **Getparents:** way refs and relation member IDs/types until each object's parent status is decided; complete content only for emitted objects.
   - **Dense node selection:** IDs first, then the coordinate and metadata delta prefixes needed to recover selected positions, with tag boundaries scanned as necessary.

   These are application-decoding minima. The current [validator](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1427) still checks unrelated arrays and string-table UTF-8. Avoiding element construction must not silently discard those refusal conditions.

   Getparents also has an immediate pruning opportunity: with `--add-self`, node blobs should be selected by node ID range. Way blobs needed only to emit explicitly requested ways should likewise use their own ID ranges when no queried nodes require a parent scan.

   **The limit for getparents is different:** current indexdata describes an element's own IDs, not what it references. Without additional reference metadata, all relevant way/relation references must be inspected. A major further improvement requires a reverse index from typed referenced IDs to candidate blobs or parent elements. That could transform repeated small queries, but its build and maintenance cost makes it conditional on reuse. Own-ID min/max ranges cannot substitute for it.

5. **History time-filter should be a parallel grouped reduction with boundary reconciliation.**

   [time_filter_history](/home/folk/Programs/pbfhogg/crates/lib/src/commands/time_filter/mod.rs:207) parallelizes decoding, then runs grouping, selection, ownership conversion, and output building on one consumer thread.

   Worse, every version before the cutoff is cloned into `OwnedElement`, replacing the previous candidate. An object with many eligible versions repeatedly copies tags, members, and references even though only one version can survive.

   Cross-blob groups do not justify serializing every group. I would partition the input by blobs and have workers:

   - Validate local key and version ordering.
   - Reduce complete interior ID groups to their selected versions.
   - Retain compact summaries for the first and last groups.
   - Encode interior survivors on workers.

   An ordered boundary reconciler joins equal IDs across partitions. A group spanning many blobs needs an associative summary: first/last ordering information, counts, and the last eligible candidate. It does not need every historical element owned. An output sequence can place a reconciled boundary winner before the next partition's interior frames.

   Selection should primarily read identity, version, timestamp, and visibility. Construct full output content for winners rather than every eligible candidate. Where the current path checks user or role indices on superseded candidates, preserve that validation without copying their strings and arrays.

   This is a high-conviction full rewrite. It removes a serial floor and work proportional to versions rather than survivors.

   Snapshot time-filter already performs worker-local filtering, encoding, and compression. Its stronger improvement is field projection plus encoded selection. Entirely accepted blobs can avoid rebuilding only when their original representation already satisfies the command's output semantics.

   Risks include groups spanning arbitrary numbers of blobs, strictly ascending version checks, missing timestamps, deletion accounting, and malformed superseded versions. Boundary reconciliation must validate the input sequence, not merely the survivors.

6. **Dedupe should recognize repeated compressed sources and stop treating an overlap component as an indivisible serial job.**

   [The merge planner](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:997) sends overlapping ranges into a sweep. For an input merged with itself, corresponding blobs overlap everywhere, so the command reads and decodes both copies and re-encodes the survivors.

   Repeated pinned sources should be recognized before that work. Decode an aliased frame once, retain its input multiplicity and precedence, and evaluate duplicate identity once. Compatible output frames can then be copied rather than recompressed.

   There is an important trap: simply deleting the repeated input argument is incorrect. The code explicitly allows duplicates inside an isolated blob to survive, while overlap sweeps remove equal `(kind, ID, version)` records. Merging a file with itself can therefore change duplicates within that file. The fast path needs to establish the surviving sequence, not assume that one original frame is already the answer. Indexdata alone contains no uniqueness proof.

   For different files, compressed equality can prove equality after comparison; a fingerprint alone cannot. Whether reading both compressed bodies is worthwhile depends on how much decoding and compression it avoids.

   Separately, [each overlap sweep](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:638) performs reading, decoding, merging, encoding, and compression on one worker. Independent runs already execute concurrently, so ordinary self-copy runs are not globally serial. A very large connected overlap component, however, becomes one serial task.

   The stronger general design partitions that component by complete ID groups, distributes decoded slices to those output partitions, and merges/encodes partitions independently. It must avoid decompressing a wide source blob separately for every partition. The same retained-buffer ownership model proposed for repack fits here.

   Repeated-source recognition is especially compelling for the stated self-dedupe workload. Partitioning giant components has high potential when such components occur; it is less compelling for inputs already producing many independent runs.

The intended userspace byte budget should be explicit. Let `H` be framing/header bytes, `P` compressed data payload bytes, and fractions be weighted by compressed bytes rather than blob count. These are logical payload budgets; read windows, pages, and backend fallbacks add physical read amplification.

| Command/path | Input payload entering userspace after redesign | Necessary semantic work |
|---|---:|---|
| Indexed plain cat | Approximately 0% | Headers and range planning |
| Indexed cat with type filter | Approximately 0%, including selected payloads | Header kind selection |
| Unindexed cat | 100% of unindexed payloads | Decompress once; scan index/tag fields |
| Cat clean | Selected-kind payload fraction | Validate and transform affected fields |
| Cat reindex | 100% | Decompress and scan index/tag fields; no recompression |
| Cat repair | 100% | Decompress and validate; recompress repaired payloads only |
| Repack | Normally 100% | Repartition; preserve unchanged encoded fields |
| Header-only degrade | Approximately 0% | Rewrite headers |
| Degrade re-encoding | Normally 100% | Requested transforms and block shaping |
| Time-filter, either input form | 100% without additional trustworthy summaries | Inspect ordering and selection fields; fully construct survivors |
| Getid | Candidate-blob fraction | IDs, then selected content |
| Getid with references | Union of dependency and output candidate blobs | Typed dependency traversal and selected content |
| Getparents without reverse index | Relevant way/relation byte fraction, plus ID-selected self blobs | Reference membership tests |
| Dedupe | Overlap/rewrite fraction; read aliased frames once | Duplicate selection and genuine merging |

I would keep three smaller changes distinct from those rewrites:

- **Parallelize plain cat's unindexed scan.** It currently decompresses and scans serially. Reindex and repair already use the existing frame pool. Reusing that mechanism has substantial potential on unindexed input without redesigning the representation.
- **Use raw-field builder methods in current per-blob rebuilds.** This can remove tag re-interning and reference expansion before the larger encoded-field rewrite. Sparse selections need care: copying an entire source string table for one result may increase output size substantially.
- **Make degrade's drop-selection pass an ID projection.** It currently builds blocks and iterates full elements merely to hash IDs, then decodes again for output. Existing ID projections can remove irrelevant element work. Large `N` also warrants bulk selection rather than funneling per-blob candidates through one global heap.

Two findings should affect correctness work and interpretation of results:

- **Likely bug: degrade does not reliably select N unique IDs.** [select_drop_sets](/home/folk/Programs/pbfhogg/crates/lib/src/commands/degrade/mod.rs:1102) counts occurrences and allows repeated identical `DropKey`s into its heaps, then inserts them into hash sets. Two versions of one ID can consume two selection slots but become one selected ID. It can accept N greater than the distinct-ID count and still report N dropped.
- **Cat's decode statistic hides decompression work.** Plain unindexed cat decompresses ordinary blobs to generate indexdata, but reports them as passthrough; `blobs_decoded` counts the unclassifiable blobs it rebuilds. That counter cannot establish how much decompression an optimization removed.

My strongest priorities are the range-copy paths, eliminating repack's central element encoder, and keeping getid dependency traversal range-selected end to end. History time-filter then deserves an aggressive rewrite of its own. Faster parsers and more workers around the existing serial consumers would leave their main throughput limits intact.
