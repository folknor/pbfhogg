I would replace the general sort engine and rebuild renumber's map construction and lookup path. Those are the strongest architectural opportunities. Changing heap implementations or increasing worker counts would leave the main bottlenecks intact.

One significant finding: **the current sort path has no external spill mechanism for dense overlap.** It streams output, but its live input cursors can still grow with the input.

This assessment comes from source inspection only. I made no edits and ran no benchmarks. Payoff claims below are hypotheses grounded in the work the code performs.

1. **Replace overlap-component sorting with an adaptive, bounded, parallel external sorter. High conviction; a full rewrite.**

   Sort delegates to `blob_merge`. The planner groups intersecting blob ID ranges into connected components, then assigns each component to one serial sweep. That sweep reads and decompresses blobs, maintains cursors, merges elements, encodes blocks and compresses output on its own worker. Parallelism exists between components, not within one component. See [component planning](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:997), [worker dispatch](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1428) and [the sweep](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:662).

   For broadly scrambled input, most node blobs can belong to one component. The expensive node path then becomes a single-worker operation regardless of available cores. Worse, if each blob contains IDs distributed across much of the node range, the sweep must open many blobs before it can emit much. Its memory relief only converts mostly consumed cursors into owned tails. It neither spills nor bounds a dense live overlap. The same-ID group is also accumulated whole. See [cursor demotion](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:950).

   The stronger design has three behaviors within one sorting engine:

   - Already ordered, trustworthy blob extents remain compressed extents.
   - Small disorder is repaired in bounded memory.
   - Large disorder becomes bounded sorted runs, followed by parallel merges over disjoint key ranges.

   Run formation should decode into compact arenas and columns, sort small key-and-offset records, and spill sequential run segments when the byte budget fills. Merge fan-in must be bounded. Large merges should be divided by actual key ranges so one connected node component can occupy the machine. A skewed partition must split further or use bounded merging; equal IDs must not defeat the memory bound.

   Use the existing OSM ordering and explicit version/provenance tie keys where the current rewritten path orders equal IDs. Sort must retain every occurrence, independently of the merge engine's deduplication behavior.

   **Fuse scanning with run formation.** Currently, a file without a sorted header claim is decompressed during indexing to establish its real ranges and intra-blob order. That representation is discarded. Overlapping blobs are subsequently read and decompressed again. See [scan results](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:677) and [sweep reopening](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:735).

   On the general-sort path, that first payload scan should produce the sortable representation itself. Header claims can inform which I/O strategy to choose, but actual decoded keys must determine run contents and partition placement.

   This is plausibly the largest payoff in scope: it removes single-core execution of the largest component, avoids a redundant decompression pass on the general path, and makes genuinely unsorted planet input fit a defined memory budget.

   The risks are scratch amplification, skew, and choosing an intermediate representation that expands PBF data excessively. I would use block dictionaries, packed variable payloads and columnar dense-node data, not serialized `OwnedNode`/`OwnedWay` objects. Repeatedly writing ordinary PBF intermediates would reintroduce much of the compression and dictionary work the rewrite should remove.

2. **Preserve compressed blobs inside an overlap component. High conviction, especially for partially sorted inputs; part of that sorter rewrite.**

   The planner currently treats range overlap as a reason to rewrite every blob in the component. That is much stronger than necessary.

   Consider one internally ordered blob containing two widely separated IDs, surrounded by otherwise ordered, narrow-range blobs. Its interval connects a large section of the file into one component. The sweep then decodes and re-encodes that entire section even though most blobs contain no element that actually needs interleaving with the outlier.

   The merge frontier should support **whole-blob transfer**. Once a candidate blob's complete key range precedes every active competing head and every unopened competing range, its compressed frame can be emitted directly. Only blobs containing actual insertion points need decoding and rebuilding. Equality requires the same version and duplicate-boundary care as the element merge.

   This changes the unit of work from “rewrite the component” to “rewrite the places where streams actually interleave.” For a nearly sorted planet, that can remove decode, encode and compression for almost all of a very large component.

   It requires changing the seam with blob merge. A sweep currently emits only framed byte chunks; it should also emit source extents that the output side can coalesce and copy. The existing [copy-range coalescer](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1596) already supplies the output primitive.

   For the blobs that really need rebuilding, sort also does unnecessary semantic round trips. [Output encoding](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:459) decodes unchanged way references into a vector, reconstructs relation members, resolves strings and interns them again. Existing [raw builder methods](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1651) demonstrate that unchanged reference/member byte arrays can survive without this work.

   Those methods cannot simply be substituted: their string-table mapping assumes one seeded source table. A merge needs a source-dictionary-to-output-dictionary translation, cached per source table and output block. References can remain packed bytes; tag, role and user indices need translation. Dense nodes need a columnar permutation.

   The main risk is replacing avoided CPU work with dictionary duplication and many tiny output blocks. The design should pack output across sources while translating dictionary indices, rather than flush whenever source ownership changes.

   Already sorted, indexed input already receives coalesced raw copying. I would preserve that behavior; a new external sorter should add no payload work to it.

3. **Build renumber maps by owned ID ranges and bitmap words. High conviction; rewrite map construction rather than retain the shared mutable-set topology.**

   Renumber already has the right fundamental mapping: membership plus rank. It does not need a hash map containing billions of explicit old/new pairs.

   Its construction path nevertheless ignores the strongest property it verifies: IDs strictly increase.

   Every node executes an atomic bitmap `fetch_or`. Consecutive IDs repeatedly update the same word. Ways are distributed dynamically across workers, each worker builds a general `IdSet`, and the resulting sets are merged serially. Workers progressing through nearby IDs can populate the same large chunks in several shards. Rank construction then scans the bitmaps serially. See [node insertion](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:470), [atomic setting](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:716), [way merging](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/mod.rs:583) and [rank construction](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:981).

   I would construct a renumber-specific immutable map:

   Each worker receives a contiguous schedule region and accumulates complete bitmap words locally. It combines all IDs belonging to a word before storing the word. Workers return owned pages, local rank summaries and actual first/last IDs. Joining regions moves page storage, combines boundary fragments, verifies strict order, and prefix-sums page totals. Local rank construction runs alongside page formation.

   This replaces billions of atomic read-modify-write operations with sequential local writes. It also removes broad bitmap replication and union work from the way path. The existing [`IdSet::chunk_of` and consuming merge](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:880) expose the ownership opportunity, but their current chunk granularity and dynamic task distribution do not realize it.

   Range ownership must follow observed IDs. Unverified indexdata must not authorize concurrent non-atomic writes into supposedly disjoint memory. Local pages and a checked join make malformed order an error without creating a race.

   This should also eliminate dependence on preallocating a node-ID-wide bitmap from a bounded maximum claim. Sparse high-ID inputs can use sparse pages; dense planet ranges can use bitmap pages.

   The payoff scales with every node, not merely with relation count or exceptional inputs. The main uncertainty is how much wall time atomic construction currently contributes beside decompression and compression. The removed work is certain; the end-to-end gain needs measurement.

4. **Make reference lookup one cache-line operation, and resolve references in batches. Strong hypothesis; a substantive map and rewrite redesign.**

   Current dense lookup obtains membership from a bitmap, reads rank information from separately allocated prefix arrays, and popcounts up to seven preceding words. `rank_if_set` already combines membership and rank directory lookup, so merely combining those APIs again would accomplish nothing. See [rank calculation](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:324) and [combined lookup](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:1115).

   The way rewriter resolves each reference immediately after decoding it, then encodes its replacement before proceeding. That exposes the map access through a largely scalar dependency chain. See [reference rewriting](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:665).

   A throughput-oriented dense map could store an interleaved pair per bitmap word:

   `membership_bits, rank_before_word`

   Membership and rank would come from the same cache line, followed by one masked popcount. The upper directory remains small. Empty pages and sparse pages need compact alternative representations; completely populated pages can use arithmetic mapping.

   The dense representation roughly doubles bitmap storage compared with the present bitmap plus coarse rank prefixes. That is a deliberate trade for fewer unrelated memory accesses and less query computation, not a proposal for an ID-wide array of 64-bit replacement IDs.

   The rewrite loop should then operate on bounded reference batches: decode absolute old IDs, issue independent map accesses, compute replacement IDs, and delta-encode the results. Across a batch, outstanding lookups can overlap. A small per-worker cache for recently accessed map lines or repeated references may help locality without another global table.

   This is the map shape I would pursue to make rewriting approach memory-bandwidth throughput: immutable pages, colocated rank and membership, and multiple independent queries in flight. Changing the rank representation alone cannot guarantee that outcome; the scalar loop also needs changing.

   Risks include increased cache/TLB footprint, extra batch scratch traffic, and workloads whose reference locality already makes the existing prefixes cheap. Orphan detection must remain exact. Missing references still require the current per-kind shift and overflow behavior; a rank-only approximation is incorrect.

5. **Order renumber output after compression, with one ownership path. Medium conviction for wall time; a coherent pipeline change.**

   Renumber workers return uncompressed blocks through an ordered pool. The consumer validates cross-blob order and hands each block to `PbfWriter`, which reserves another slot and dispatches compression onto its executor. The writer orders those results again. See [renumber's consumer](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/pool.rs:91) and [writer dispatch](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1118).

   Thus, a slow early rewrite prevents later completed blocks from even entering compression through this path. Uncompressed blocks also cross ownership boundaries, and workers immediately reserve replacement buffers after handing their previous buffers away.

   I would give each admitted job ownership through read, decompress, map/rewrite, compress and frame. It returns a compressed frame plus validation summaries. The ordered consumer checks summaries and commits frames. One byte budget covers the whole job lifecycle, and rewrite buffers stay with workers for reuse.

   The existing [`frame_blob_pipelined`](/home/folk/Programs/pbfhogg/crates/lib/src/write/framing.rs:122) already permits worker-side framing. Sort's sweep and apply-changes demonstrate this arrangement elsewhere.

   This removes premature ordering, a task dispatch and an uncompressed-buffer handoff. However, compression is already parallel today. Fusion can also reduce scheduling flexibility when reads or compression vary substantially. I rank it below the sorter and mapping rewrites.

   The output neighbour should provide independently completable frame slots with a single final ordering point and explicit byte credits. Concurrent positional writing can sit behind that interface; it need not dictate the transformation architecture.

There are smaller worthwhile changes, but I would keep them separate from those rewrites:

- **Flatten wire-patch assembly.** Dense-node payloads are copied through `other_fields`, `element`, `group` and finally the block output. Ways and relations also pass through element and group buffers. Compute replacement lengths and write retained spans directly into the final reusable buffer. This removes repeated copying of unchanged tags, coordinates and metadata. See [dense assembly](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:512).
- **Decode relation member arrays once.** The relation rewriter first parses both arrays to count them, then parses them again to rewrite. Consume them in lockstep and check that both end together, retaining strict varint validation. See [member processing](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:917).
- **Move relation-ID collection off the critical tail.** R1 currently runs sequentially after way processing. It can collect IDs in parallel while node work proceeds, potentially retaining a bounded relation representation for rewriting later. Relations need lookahead because they can reference later relations, but that does not require today's placement of a second sequential scan. This is smaller than improving the billions-node path.
- **Avoid transporting full element views as sort keys.** Internally unsorted blobs allocate and sort `Element` views. Compact keys plus offsets/ordinals would reduce sort traffic and pinned storage. The existing [projection hooks](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1319) can collect information during validation; the full replacement should extend that approach rather than repeat element parsing.

Several observable issues also surfaced:

- **Sort rewriting drops embedded way locations.** Both the borrowed way output and owned demotion path omit them, despite existing location-preserving APIs. Consequently, whether a way retains its coordinates depends on overlap routing. See [way output](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:486) and [`read_way`](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:705).
- **Sort rewriting can lose finer coordinate and timestamp precision.** Coordinates pass through decimicrodegree accessors; metadata conversion truncates timestamps to whole seconds. Passthrough preserves the original precision. A replacement intermediate format should preserve source precision and metadata presence. See [coordinate conversion at output](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:467) and [metadata conversion](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:532).
- **Renumber rejects plain node encoding.** Its node rewriter requires `DenseNodes` and rejects plain nodes, including valid sorted output that the library's partial-metadata encoding can produce. See [`single_dense`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:400).
- **Renumber's direct-read flag misses the bulk reads.** The initial header read receives the flag, but payload workers use ordinary `pin.open()`, and schedule construction opens the ordinary header walker. The large data phases therefore remain buffered. See [input setup](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/mod.rs:489).
- **Explicit `zlib:6` becomes `zlib:1` in renumber.** This is also disclosed in the CLI source, but it is an observable value-based API trap to account for when replacing the pipeline. See [compression selection](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/mod.rs:456).
