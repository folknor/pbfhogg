I would rewrite the extract execution core and the export pipeline. The strongest opportunities remove repeated decoding and per-region work. Point-in-polygon arithmetic and coordinate formatting come after those.

This is static code analysis, without measurements, edits, or consulting project documents or history. Payoff assessments below are hypotheses grounded in the work the code performs.

One scope discrepancy matters: this checkout implements GeoJSON and GeoJSON sequence export of nodes and ways. It requires LocationsOnWays for way export, ignores relations, and contains no GeoParquet implementation. Ring assembly currently serves the geocode builder, not export. Those are missing capabilities relative to the scope described, rather than existing export hot paths.

1. **High conviction: replace the per-region extraction driver with one shared dependency engine.**

   The largest scaling problem is in [extract_multi_to](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/mod.rs:658). Shared multi-region execution is available only for sorted simple extraction without attribute cleaning. Complete-ways, smart, and cleaned simple extraction run the entire single-region extraction separately for every region.

   Consequently, adding regions multiplies input decompression, coordinate classification, reference scanning, and dependency resolution. Even disjoint regions repeatedly decode the same planet. Cleaning attributes also disables sharing despite having no effect on spatial membership.

   I would build one engine whose records carry region membership masks:

   - Classify node coordinates once into region masks.
   - Compute each way's initial mask by unioning its referenced nodes' masks.
   - Compute directly matched relation masks from node and way masks.
   - Propagate relation masks to parents for complete-ways and smart.
   - For smart, propagate completion masks to direct member ways and nodes, then from those extra ways to their nodes.
   - Emit each region's selected records in the promised order.

   The existing [Membership](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/membership.rs:604) already supplies useful compressed masks. It is a starting point, not a requirement to preserve the current execution shape. In particular, keep **initial spatial membership separate from completion membership**. Otherwise added nodes or ways can accidentally seed additional relations, changing smart's result. Likewise, parent relations must not acquire the completion behavior of directly matched multipolygons.

   This is potentially the largest payoff for multi-region complete-ways and smart: shared source work becomes largely independent of region count. Output encoding, compression, and bytes still scale with actual fan-out.

   The principal risks are overlapping-region memory growth, correct parent closure through cycles, and distinguishing the several reasons an element belongs to a region. These are explicit dataflow problems and justify a coherent rewrite.

2. **High conviction: make sorted simple extraction genuinely decode once.**

   The implementation's “single-pass” name hides two payload sweeps per kind. [Simple extraction](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/simple.rs:417) classifies nodes, rereads them to write, classifies ways, rereads them to write, then does the same for relations. [Multi-extract](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/multi.rs:354) repeats that structure.

   The node-to-way-to-relation barriers are useful. The classify-to-write barriers within each kind are unnecessary for ordinary sorted snapshot data.

   Each worker should read, validate, classify, encode, and compress its input block in one task. It returns completed output frames and a compact membership contribution. Membership construction can consume contributions independently of output ordering; frames retain input ordinals for ordered delivery. Freeze node membership before starting ways, and way membership before relations.

   Simple relations need no final relation membership table: they do not participate in recursive selection. They can be classified and emitted directly.

   This removes the second read and decompression of nearly every rewritten block, plus the second element walk. It also avoids writing by looking up a decision that the same worker just computed. Compression already runs in workers in the current extract write paths; the opportunity is fusion around that existing parallel work.

   **Risk:** current selection is ID-based. Repeated IDs or history versions can cause a match from one occurrence to select other occurrences. A fused implementation must reconcile equal-ID runs, including runs crossing block boundaries, wherever that behavior belongs to the supported contract. It must also handle sorted inputs without homogeneous indexed blocks.

3. **High conviction: export should produce complete byte chunks on workers.**

   [export](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/mod.rs:149) calls `ElementReader::for_each`. That API performs reading, decompression, block construction, and callback execution sequentially. Geometry construction, filtering, coordinate formatting, JSON escaping, metadata formatting, and writing therefore all lie on one serial path.

   Switching to `for_each_pipelined` alone leaves most export work in one consumer. The stronger change is:

   **read compressed block -> worker validates and exports all its features -> ordered sink writes completed chunks.**

   The repository already has [for_each_fused_block and for_each_fused_buffer](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:368), which move transformations onto decode workers. They can support a concrete first implementation without designing a general export framework.

   Workers should own reusable coordinate, property, and output buffers. Build final feature bytes directly into a chunk instead of constructing a geometry string, constructing a properties string, then copying both into [FeatureWriter's feature buffer](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/writer.rs:25). GeoJSON sequence framing is straightforward. FeatureCollection workers can return comma-separated feature chunks with feature counts; the sink handles punctuation between nonempty chunks.

   Push type and conservative expression filters into scheduling. Export currently decodes relations it cannot emit and decodes unwanted element types. Existing `BlobFilter` and [expression-to-blob-filter logic](/home/folk/Programs/pbfhogg/crates/lib/src/commands/tags_filter/mod.rs:41) provide most of the machinery. Reuse the relevant logic carefully: export accepts empty expressions, while tags-filter gives them different semantics.

   This removes a whole-command serialization bottleneck. Once workers produce final chunks, the remaining serial component is predominantly output bandwidth.

   Risks include bounding chunks by bytes rather than just task count, preserving feature and property order, and stopping promptly on write failure. A very large way must not make one admission slot hold an unbounded amount of text.

4. **Strong architectural opportunity: resolve complete-ways and smart dependencies without repeatedly decoding their payloads.**

   Complete-ways classifies nodes, ways, and relations, then rereads everything for emission. Smart adds another full way classification sweep to discover the node dependencies of relation-selected ways. Its [extra-way resolution](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/smart.rs:621) scans the full way schedule even though it only needs refs for `extra_way_ids`.

   The underlying dependency chain is real:

   `region nodes -> initial ways -> directly matched relations -> extra ways -> extra nodes`

   It does not require retrieving those dependencies by decoding the same source payload repeatedly.

   I would read relation topology early, retaining its member IDs, roles needed by the strategy, type classification, parent edges, and output payload. That identifies every way which could become a smart completion dependency. After node classification, read ways once:

   - Compute initial spatial membership.
   - Collect outside-node dependencies.
   - Retain refs and payload for candidate completion ways.
   - Retain or encode payload for initially selected ways.

   Then resolve direct relation matches, parent closure, and smart completion over those retained records. Finally emit nodes and the selected way/relation payloads.

   The source work looks like this, excluding header scans and passthrough:

   | Strategy | Current payload sweeps by kind | Stronger design |
   |---|---|---|
   | Simple | Nodes twice, ways twice, relations twice | Each kind once |
   | Complete-ways | Classification, then emission for every kind | Nodes twice; ways and relations once, retaining selected payload |
   | Smart | Nodes twice, ways three times, relations twice | Nodes twice; ways and relations once, retaining candidate topology and payload |

   These count schedule sweeps, not identical byte volumes: spatial pruning affects classification, and emission schedules can be broader.

   **Fundamentally, simple needs one source traversal on sorted input. Complete-ways and smart need information from later elements before finalizing earlier nodes.** They therefore need either a node revisit or retained node payload. Smart does not fundamentally need three planet traversals: retaining relevant topology removes the extra resolution traversal. With arbitrary materialization, even one source read is possible, but that merely transfers work to retained data.

   The practical design should retain compact topology and selected payload, with bounded scratch where necessary. Its risk is trading decompression for scratch writes and reads. That is attractive for a small extract and less certain for near-world outputs. I would make the full change and judge its end-to-end behavior, rather than merely replacing the scanner inside the existing extra-way pass.

   At the scheduling seam, retain ID ranges and observed selection summaries. `IdSet::any_in_range` already exists, but extract's `BlobDesc` discards the range information. Sparse dependency sets should produce sparse emission schedules. Empty dependency sets should cause no resolution work.

5. **High conviction on avoidable work, workload-dependent payoff: replace element rebuilding with selection over wire data, and redesign fan-out ownership.**

   [extract_block_pass2](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/common.rs:578) and smart's writer reconstruct selected elements through ordinary `BlockBuilder` methods. Ways materialize refs, relations materialize members, tags are resolved to strings and interned again, and metadata passes through decoded representations. Multi-extract repeats the rebuilding for every destination.

   Extraction mostly selects existing records. It should preserve unchanged encoded fields.

   There are concrete precedents already available:

   - [Pre-seeded string tables](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1360).
   - [Raw way and relation field writers](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1656).
   - Apply-changes' raw surviving-element writers.
   - ALTW's wire-level block reframing.

   The stronger implementation is a validated block selector. Keep source string indices, copy unchanged way/relation fields, and rebuild only fields selection or cleaning changes. Dense-node filtering still requires coordinated filtering and redelta encoding of IDs, coordinates, metadata, and tags. It does not require repeated string lookup and hashing.

   For multi-region output, group destinations with identical row selections and cleaning behavior. Encode and compress each distinct selection once, then share its immutable frame. This helps overlapping and nested extracts without depending on entire source blobs fitting inside a region.

   The current [MultiLocal](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/multi.rs:918) also flushes every region's builder at every input-block boundary. Sparse matches can therefore create small output blocks with poor compression and high framing overhead. Output packing should span input blocks, using ordered batches large enough to produce worthwhile blocks.

   Finally, [write_regions](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/multi.rs:1012) serially drains every destination from one aggregate result. One region's write stalls all others, and admission credit covers an input blob's entire compressed fan-out. Use independent destination ordering and write queues under one shared byte budget. Preserve per-file order without imposing one global delivery barrier across unrelated files.

   Risks are output block size, unused source string-table inflation, metadata presence and timestamp normalization, and maintaining correct indexdata/tagdata. Extract also deliberately strips way enrichment today; copying raw ways indiscriminately would change that behavior.

6. **Strong hypothesis for polygon workloads: compile exact region membership into a spatial subdivision.**

   Polygon classification already uses integer coordinates and latitude bands. In [Rings::contains](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/polygon.rs:226), however, every point inside the envelope walks its band's segments and performs exact crossing tests. The multi-region grid only prunes region bounding boxes; it does not prove polygon membership.

   I would prepare an adaptive spatial subdivision whose leaves contain:

   - A proven region membership mask for areas with no relevant boundary.
   - A short list of unresolved regions and boundary segments for boundary cells.

   Most nodes away from boundaries then need a spatial lookup and a mask load. Exact integer predicates remain for boundary cells. Overlapping regions share the lookup, and ordinary bbox regions can participate in the same structure.

   This changes the dominant operation from repeated point-against-edge work to point location. It is especially promising for complicated administrative boundaries and many regions. Preparation cost is amortized across billions of nodes.

   Correctness is the hard part: boundary cells must conservatively include touching edges and endpoints, retain the existing planar parity versus feature-union rules, and handle periodic longitude images. Approximate raster membership or polygon simplification would not satisfy this contract.

   At the decode seam, polygon classification should consume coordinate columns as bbox classification already does. Falling back to general element iteration because a region is a polygon is unnecessary.

   I would prioritize this after removing duplicate decode. It cannot eliminate the mandatory coordinate scan, and broad blob bboxes may offer little pruning on ID-sorted data.

**Export should ride the ALTW coordinate join, without producing an intermediate ALTW PBF.**

Existing export already receives inline coordinates, so it currently performs no node lookup. For ordinary PBF export, the right integration is to put the export sink directly after coordinate resolution.

The sparse backend can expose a completed coordinate store to geometry workers. The external backend can deliver ordered per-way coordinate spans or columns from its join/router, before converting them into PBF location fields. Export should consume those coordinates directly. Producing an enriched PBF first pays coordinate delta encoding, PBF compression, intermediate I/O, decompression, and coordinate decoding solely to cross a command boundary.

Filtering must happen before join-request generation wherever possible. For relation geometry, include needed member ways even when they would not become standalone features.

For future GeoParquet support, the same workers should build geometry and property columns directly, with parallel row-group encoding and an ordered file assembler. Going through GeoJSON would introduce an unnecessary text representation. There is no existing GeoParquet writer here to assess for compliance or throughput.

The join seam must expose missing-coordinate status explicitly and preserve positional association with refs. ALTW's output policy must not silently become export's invalid-geometry policy.

The medium-value local changes are clearer but smaller:

- [Property serialization](/home/folk/Programs/pbfhogg/crates/lib/src/commands/export/properties.rs:8) allocates a JSON string per key/value and builds reserved/emitted hash sets per feature. Append escaped text directly, cache escaped string-table entries per block, and use reusable duplicate-key tracking.
- Export reads metadata even when metadata output is disabled. Avoid that work.
- [Coordinate formatting](/home/folk/Programs/pbfhogg/crates/lib/src/coord_fmt.rs:28) uses general floating formatting for every ordinate. Integer decimal emission is attractive for ordinary e7 coordinates, but must preserve existing rounding for non-default PBF framing. `decimicro_*` truncation is not an equivalent replacement.
- `is_area_way` scans refs, followed by coordinate collection and more geometry scans. Fuse coordinate decoding with bbox, closure, distinct-position, and area summaries. Retain coordinates only where orientation requires them.
- `text_quote.rs` is not in export's machine-output path. Its escaping rules differ from JSON serialization; optimizing or substituting it does not address this bottleneck.

Additional findings worth flagging:

- **Export continues decoding after a write failure.** Its callback records `first_err`, then subsequent callbacks return immediately, but sequential `for_each` still reads and decompresses the remaining input. A broken pipe can therefore waste the rest of a planet traversal. A fallible consumer fixes this directly.
- **Extract followed by export has a costly composition seam.** Extract re-encodes ways without inline locations, while export requires them. Even an enriched input loses the prerequisite. Direct join-to-export integration is particularly valuable here.
- **Negative IDs silently lose extraction membership.** Both `IdSet` and multi-region membership discard negative IDs. Supported negative-ID inputs need a representation that preserves them.
- **Coordinate acceptance differs between extract paths.** Simple's raw node scanner uses checked scaling; columnar and element accessors use wrapping arithmetic and narrowing. Extreme framing values can therefore fail under one strategy and produce coordinates under another.
- **Ring assembly has a separate worst-case problem.** [assemble_rings_search](/home/folk/Programs/pbfhogg/crates/lib/src/geo.rs:940) backtracks over endpoint choices while copying coordinates into speculative rings. The public API is unbounded; the geocode caller uses a placement budget. Search over segment/orientation descriptors and materialize coordinates only after selecting a ring. Preserve the existing traversal result wherever promised. This can remove severe pathological costs, but it is not a current GeoJSON export throughput opportunity.
