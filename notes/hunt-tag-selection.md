I would rewrite the PBF filtering engine around dictionary predicates, dependency planning, and selective wire encoding. The current element iterator is convenient, but it makes filtering repeatedly interpret information that already exists in a more useful form.

Counting starts from a stronger position: it already counts string IDs locally, resolves distinct rows afterward, and merges into a sharded table. Its largest remaining opportunity is to reject tags before building those local histograms.

These are code-based hypotheses, not measured speedups. I read the implementation and neighboring Rust APIs, without reading project documents or history, changing files, or running workloads.

1. **High conviction: replace element-based filtering with a predicate integrated into the decoder.**

   [`element_matches`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:129) loops over applicable expressions and then the element's resolved string tags. Both filtering paths first collect those tags into a temporary vector. Consequently, a common key or value gets resolved and compared repeatedly across thousands of occurrences within the same block.

   Dense nodes pay an additional structural cost. [`validate_groups`](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1427) validates their tag stream; [`DenseNodeIter::next`](/home/folk/Programs/pbfhogg/crates/lib/src/read/dense.rs:185) walks it again to find each node's tag boundaries; `DenseTagIter` decodes those pairs again for matching. Classification skips metadata, but still advances node coordinates. Output selection advances metadata and coordinates even for rejected nodes.

   `PrimitiveBlock` itself is a zero-copy view. The opportunity is removing repeated wire interpretation and unnecessary column decoding, rather than eliminating an owned input object graph.

   **The stronger design:** give the filter a block-local predicate over string IDs and let it operate while the decoder traverses tags.

   For each dictionary entry, compute or lazily cache:

   - Which key matchers accept it.
   - Which value matchers accept it.
   - Which expressions apply to nodes, ways, and relations.

   A tag occurrence then combines integer masks. For an ordinary expression, its bit must occur in both the key and value masks. Key-only expressions need only the key mask. Negated values complement the relevant expression bits.

   Expression identity must survive this compilation. Independently combining "some key matched" and "some value matched" would incorrectly cross-match expressions such as `amenity=school` and `shop=bakery`.

   This needs neither a quadratic dictionary-pair table nor a cache entry for every possible pair. Cache each distinct string matcher against the dictionary entries it actually needs. A single exact-key expression can have a particularly small representation: key acceptance by SID, plus value acceptance where applicable.

   The decoder should supply string-table entries, raw tag IDs, element boundaries or ordinals, and independently accessible ID/reference columns. Filtering should produce selection masks and dependency records without constructing `Element` wrappers or borrowed-tag vectors.

   There is already an entry point for this: [`BlockBuffer::project` and `GroupProjection`](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1142). Merely replacing `elements()` with today's projection would remove some reparsing. The larger rewrite would let validation and selection consume tag varints together, keeping results provisional until the complete block passes validation.

   **Why this could matter:** it removes repeated string work on every tagged element and multiple traversals of dense tags. It also lets classification avoid coordinate reconstruction entirely. These savings apply across broad workloads, including cases where blob indexes cannot skip anything.

   **Risks:** preserve element-level OR semantics, empty-expression behavior, type restrictions, and inversion. Keep the existing handling of an out-of-range tag index: it ends that element's visible tag list; for dense nodes the scanner must still find the next node's boundary. Early predicate success must not bypass required validation.

2. **High conviction: replace fixed-point rescanning with dependency-directed execution.**

   The default path is substantially more than two passes. It classifies direct matches, repeatedly scans relation blobs until closure converges, scans way blobs for newly included ways, then decodes the output schedule.

   [`collect_relation_member_closure`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:1192) expands every currently included relation on every iteration. Previously expanded relations contribute their members repeatedly. Workers retain accumulated member vectors until the iteration ends, after which one thread inserts the IDs into sets.

   [`collect_way_node_dependencies`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:1339) scans the entire way schedule, even though it needs only relation-selected ways. Directly matched ways have already decoded their references during classification, and selected ways decode them again during output rebuilding.

   **The stronger design:** execute dependency discovery in dependency order, independently of final output order.

   - Read relevant relations, evaluate their direct predicates, and construct a compact membership store.
   - Close relation membership with a worklist, expanding each reached relation once.
   - Process ways after relation closure is known. Decide direct matches and relation inclusion together, collecting each included way's node references once.
   - Process nodes after all dependencies are known, combining direct matching and dependency inclusion in one visit.
   - Assemble output in the promised order using recorded source ordinals.

   For relations, store adjacency and packed typed member lists, without tags, roles, or full element objects. Any stored spans must reference retained packed storage or scratch, never a decompression buffer that has been returned to its pool.

   The adjacent [`getid::close_relations`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getid/mod.rs:1301) already demonstrates an explicit worklist over stored relation edges. Its `RelationSpans` and `WaySpans` projections are also available. Tag filtering currently takes the repeated-scan approach despite this neighboring implementation. I would use that code as a starting point, then go beyond its bounded relation rereads by retaining the typed member data needed by the worklist.

   For ways, I would avoid constructing a planet-wide way-to-node graph. After relation closure, route requested way IDs to source blobs and batch their reads. Direct predicates and dependency collection should share the same visit.

   This redesign also addresses the serial ID-set consumer. Classification currently sends matched ways as `(id, Vec<refs>)`, and one consumer inserts their references individually. Partition bounded reference batches by ID range and give each partition an owner. That distributes insertion without making every decode worker maintain a copy of the entire referenced-node bitmap.

   **The unavoidable tradeoff:** dependency-first execution discovers ways and relations before it can emit preceding nodes. Preserve compact selected output in bounded buffers or compressed scratch streams, then assemble it afterward. That introduces scratch traffic. For already sorted inputs, assembly can follow kind order; unsorted or mixed inputs require stable source ordinals.

   **Why this could matter:** relation closure changes from repeated decompression plus repeated expansion to one graph construction and one traversal. Broad expressions can avoid decoding nodes once for classification and again for output. Ways can combine matching, membership selection, dependency collection, and encoding.

   **Risks:** compressed spooling could outweigh saved decoding for some workloads. Preserve cycles, missing references, duplicate IDs or versions, source order, and provenance: direct matches, nodes from directly matched ways, and objects from relation closure affect tag removal and statistics differently.

   My conviction is strongest about removing the fixed-point scans. The wall-time payoff from relation closure specifically will concentrate in relation-heavy queries; way-only queries depend more on the first and third opportunities.

3. **High conviction: replace survivor rebuilding with selective wire compaction and worker-side compression.**

   [`filter_block_parallel`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:411) and [`filter_block_pass2`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:593) rebuild every selected element:

   - Tags become strings and are interned again.
   - Way references become absolute IDs in a vector, then become deltas again.
   - Relation members resolve roles and materialize `MemberData`, then get encoded again.
   - Metadata goes through conversion helpers.

   Both paths also flush at every input-block boundary. The referenced path reuses its builder allocation, but still empties it after each input block. Sparse selections therefore produce output blocks sized by the input partition rather than by useful output density.

   The worker returns uncompressed `OwnedBlock`s through ordered delivery. The consumer then submits them to the writer's compression pipeline, introducing another scheduling and ownership boundary.

   **The stronger design:** make selection feed a dedicated survivor encoder.

   For ways and relations, copy packed reference/member arrays when their representation remains valid. Remap string IDs through an array computed once per source/destination dictionary pair. Copy fields whose meaning and framing remain unchanged; rewrite only fields that need remapping or removal.

   For dense nodes, filter columns directly. Skipped deltas must still contribute to the absolute state used for the next survivor. Emit new deltas relative to the preceding output survivor, including metadata columns. This requires a column compactor, not deletion of arbitrary byte ranges.

   Pack survivors across contiguous input ranges into useful output blocks. Parallel workers can own contiguous source ranges, preserving internal order while allowing substantial packing within each range. This avoids funneling all survivors through one central builder.

   Compress completed blocks on those workers and return framed bytes. Existing [`FramedBlocks`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/mod.rs:115) already combines `lend_encoded` with framing. Tag filtering instead uses `Vec<OwnedBlock>`, even though the worker-framing mechanism exists.

   There are also existing [`pre_seed_string_table`, raw-way, and raw-relation APIs](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1345). They prove that semantic element reconstruction is unnecessary. However, copying the complete source dictionary into every sparse output block is not the final design: it retains unused strings and makes sparse output expensive. Dictionary compaction and array remapping belong in the rewrite.

   **Why this could matter:** it removes work proportional to the contents of selected ways and relations, reduces repeated string interning, improves sparse block packing, and removes the uncompressed worker-to-consumer-to-compressor handoff.

   **Risks:** preserve coordinate and timestamp framing, partial metadata, tag and member order, duplicate dictionary text, and empty strings. Recompute output indexes from survivors. Whole-blob passthrough is only a secondary fast path when everything qualifies; the main design must work efficiently when every blob contains only a few survivors.

4. **High conviction for selective counting: filter before histogramming; use indexed counters for key-only runs.**

   Counting already avoids the filter's per-occurrence string matching. [`BlobTally`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_count/tally.rs:125) collects tag spans through validation, counts local SID tuples, then resolves and evaluates distinct entries. Its shared table has 256 shards and arena-backed strings. There is no single global merge thread to eliminate.

   The remaining avoidable work is in [`count` followed by `select`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_count/tally.rs:178). If *any* expression counts pairs, every tag enters the `(kind, key_sid, value_sid)` histogram, including tags that no expression can accept. Key selection is then repeated across distinct values sharing a key.

   **The stronger design:** use the dictionary predicate from item 1 before updating counters.

   For every tag occurrence:

   - Increment its indexed key counter if a key-row expression accepts it for that kind.
   - Enter its pair in the pair histogram only if a pair expression accepts it.

   Key counters can be arrays indexed by SID, with kind-specific acceptance and a touched-slot list. That removes a hash-table lookup per occurrence from key-only counting. Pair histograms remain sparse and contain only accepted pairs.

   The decoder should also permit counting during its validation traversal. Today's `TagRuns` records array positions, and `walk_tags` traverses arrays after validation has already inspected them. Blob-local provisional counters could be discarded on validation failure and merged only after acceptance.

   **Why this could matter:** a rare requested pair currently pays local histogram costs for unrelated tags in every surviving blob. Rejecting those before hashing attacks the hot loop directly. Plain key counting also gets a much simpler accumulation operation.

   **Risks:** key and pair rows are independent; one occurrence may count in both. Overlapping expressions must not double-count a row. Duplicate dictionary strings must still merge by text across slots and blocks. Even key-only runs must observe invalid value indices because they determine where an element's visible tags end.

5. **High conviction for large OSC output: replace collection followed by serial gzip with a bounded output pipeline.**

   The production OSC path already avoids owned OSM elements. Nevertheless, [`tags_filter_osc_to`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/osc.rs:216) collects every batch's nine rendered XML groups before writing anything. Its memory grows with the entire retained output.

   [`write_groups`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/osc.rs:400) subsequently runs output through one `GzEncoder`. Parsing/rendering finishes before this serial compression phase begins.

   There is also wasted rendering before selection. [`ElementBuf`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/emit.rs:36) formats coordinates, metadata, references, members, and escaped tags before `FilterBytes::matches` decides whether the element survives.

   **The stronger design:** separate validation and selection from rendering, then pipeline rendering, compression, and ordered output.

   Workers should inspect the recorded lexical element, validate it, and evaluate its tags. Render only retained elements. Deletes remain unconditional survivors.

   Compress group chunks on workers. Maintain ordered streams for the nine action/kind groups, streaming the earliest writable group and spooling later groups with bounded memory. Emit document framing and concatenate chunks in the existing group order.

   [`merge_changes::OscWriter`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/merge_changes/mod.rs:557) already produces independently compressed gzip members, and its streaming coordinator already supports bounded output with spill files. The needed compression and output mechanisms exist nearby.

   **Why this could matter:** it removes a serial compression tail, overlaps work, avoids formatting rejected elements, and makes memory independent of retained output size.

   **Risks:** arbitrary action/kind interleaving prevents streaming every retained element straight to its final position. Preserve canonical XML ordering and escaping, late-input failure behavior, and publication only after success. Independently compressed chunks can reduce compression ratio. A single-member gzip input also retains a serial inflate limit; parallelizing output does not remove that.

There are several worthwhile **local changes**, with narrower impact than those rewrites:

- **Preserve ID ranges in the filter catalog.** [`CatalogEntry`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:1299) retains kind but discards `min_id` and `max_id`. Use [`IdSet::any_in_range`](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:800) to prune dependency and output schedules. `getid` already does this. Scattered dependencies may still touch nearly every node blob, so this is not a substitute for a cheaper decoder.
- **Avoid the unconditional node output scan.** Pass 2 sets `want_nodes = true` even when all node inclusion sets are empty. A query with no matches still schedules node blobs for a second decode.
- **Apply `-R` blob filtering before payload reads.** Its reader currently reads compressed blobs before [`should_skip_blob`](/home/folk/Programs/pbfhogg/crates/lib/src/read/pipeline.rs:82) rejects them. Move rejection to header admission, preserving the required framing checks.
- **Use the finite tag-key index for more predicates.** Substring and wildcard keys currently disable key-based blob filtering. The stored key set can conservatively answer whether any key satisfies a substring predicate, or whether any tags exist. Missing or unusable tagdata must continue to mean "decode."
- **Honor two-kind count filters in scheduling.** `tags_count::Plan` derives its standalone kind filter only for a single kind. A two-kind selection can still exclude the third kind's blobs.

A larger **medium-confidence counting rewrite** would replace mutex-shared shards with partition owners receiving bounded batches of selected counts. That could reduce shared-table cache traffic and stop decode workers performing table growth while holding shard locks. I would rank it below predicate-first tallying: the current table is already sharded, and owner queues introduce transport costs and dictionary-lifetime problems. Likewise, parallel result sorting is useful for huge retained row sets, but does not address the per-tag scan.

Two correctness findings also deserve attention:

- **Negative IDs can silently disappear in referenced filtering.** Wire parsers accept signed IDs, while [`IdSet::set`](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:532) silently ignores negative values. The referenced path records matches through those sets; `-R` writes matches directly. From the inspected path, a matching negative-ID element can therefore survive `-R` and disappear with reference inclusion. I found no rejection in that path; this is a code-derived finding, not a reproduced test.
- **Filtering can lose subsecond timestamps.** Both metadata conversion helpers used by filtering divide millisecond timestamps by 1000 before rebuilding output. See [`element_metadata_impl`](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:533) and [`dense_node_metadata_impl`](/home/folk/Programs/pbfhogg/crates/lib/src/owned.rs:587). Under the metadata-preservation contract in the request, that is a correctness defect. A selective wire encoder should preserve timestamp precision instead of inheriting this conversion.
