The biggest opportunities are **compiling admin containment into the index**, **removing the builder's single-owner merge**, and **replacing source-ordered query records with a page-packed spatial layout**. These justify substantial rewrites. The current implementation already has parallel decoding and a compact coordinate join; adding workers or adjusting buffers leaves its strongest bottlenecks intact.

This is a code-only assessment. I read the scoped implementation and its decode, geometry and ALTW dependencies, without opening project Markdown, history or performance records. I made no changes and ran no measurements. Payoff assessments below are structural hypotheses, not measured speedups.

1. **Compile admin containment into spatial state. This is the highest-conviction query rewrite.**

   The builder already does much of the expensive work needed to answer admin queries cheaply. It covers polygon boundaries, flood-fills interior cells and records whether a cell lies wholly inside a polygon. But [the reader searches nine cells and ignores every interior flag](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:1850). Each containment test then [allocates and reconstructs all polygon rings from mmap coordinates](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:1949). Antimeridian handling can allocate another unwrapped copy.

   This charges polygon-size work per lookup even for points deep inside a country. Ranked lookup also lacks the attempted-polygon deduplication that the raw candidate path has: a non-containing polygon repeated across neighboring cells can undergo the same failed test repeatedly.

   I would replace this with a versioned, hierarchical admin index carrying two kinds of state:

   - Regions with constant membership carry the containing polygon IDs, including the precomputed smallest-area winner at each level.
   - Boundary regions carry membership state plus a prepared edge index for exact containment.

   Queries use their own region. Whole-region membership is authoritative under the new format's construction invariant. Boundary queries evaluate only relevant original edges, using latitude intervals or a compact edge hierarchy, rather than reconstructing and scanning entire rings. Keep original edge endpoints and the existing crossing expression so this remains an acceleration of today's predicate.

   The build side should change with it. [The present flood fill](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass3.rs:1074) runs full polygon containment tests for seeds and visited cells. Its work includes a cells-times-vertices component, and parallelism stops at one task per polygon. A huge polygon remains one large task. Instead, prepare each ring once, refine cells intersecting its boundary, classify boundary-free regions and retain large uniform subtrees without enumerating every interior leaf. Assembly, preparation, classification and serialization can proceed per completed relation rather than waiting for a planet-wide polygon vector.

   **Why the payoff is plausible:** it removes both repeated query geometry scans and repeated build-time classification of uniform interiors. Large polygons become cheap in proportion to their local boundary complexity.

   **Risks:** preserve holes, even-odd behavior for self-intersections, implicit closing edges, antimeridian retries, simplification output, stored `f32` area and polygon-index tie breaking. Simply enabling today's flags is insufficient: flags on neighboring cells say nothing about the query's own position, and format version 7 does not distinguish the stronger construction invariant.

2. **Replace the builder's global ordered merger with independently owned output partitions. This is the strongest build rewrite.**

   [Pass 2](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass2.rs:588) parallelizes node processing and way resolution, but its useful output converges on one consumer. That consumer interns address strings, encodes records, assigns offsets and counts, writes geometry, collects interpolation metadata and inserts admin geometry into a global map.

   Worker-side hashing and [the cloned string pool](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass2.rs:794) reduce work on that thread, but retain the architecture: one owner must process every surviving address and street. Faster workers eventually produce more waiting.

   I would have workers emit complete, independently writable fragments:

   - Assign logical identity from source position and local ordinal, separate from physical storage location.
   - Intern strings locally or into independently owned hash partitions. Resolve local symbols to final offsets with a parallel remapping step. Compare bytes on hash collisions.
   - Write geometry and metadata from the producing workers.
   - Generate spatial records while coordinates are already available, rather than writing final source-ordered geometry, reopening it and traversing it again for cell assignment.
   - Use fragment counts and prefix sums wherever the retained format requires globally contiguous IDs or offsets.

   This permits stable logical order without serial ownership of every record. String bytes need not retain first-occurrence physical placement, provided string equality, optional-value semantics and observable query identities remain intact.

   The spatial aggregation should be part of this redesign. [Stage A](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass3.rs:289) repeatedly materializes chunk-wide assignments, repartitions them, writes both bucket trees and waits at chunk boundaries. [Stage B](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass3.rs:963) already sorts and encodes in parallel, but returns entire encoded buckets to one ordered writer. Partitioning by the top byte of the cell ID also gives large geographic regions disproportionate ownership.

   Replace those fixed buckets with partitions sized by actual record volume, represented by worker-private runs. Each partition owns its sort, encoding and output. Prefix sums establish final extents; workers write those extents directly, or the versioned format retains independently addressable partition files. The serial remainder should process descriptors and counts, not every address, string and geometry byte.

   **Why the payoff is plausible:** this removes an Amdahl limit and avoids the write, reread, decode and repartition cycle between geometry production and spatial indexing.

   **Risks:** changing physical order can change equal-distance winners. Preserve logical source identity and explicitly reproduce tie behavior. Admin duplicate-way replacement, interpolation section identities and string-offset equality used during interpolation resolution also need deliberate treatment. A concurrent global interner alone would leave most of this bottleneck standing.

3. **Build one page-packed spatial search structure over points and segments. This is the larger end-to-end rewrite.**

   Today's street lookup follows:

   `cell table -> segment-reference list -> way record -> coordinate file`

   [The reader performs these accesses for every candidate](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:1797). The referenced geometry follows source way order, so geographically adjacent candidates need not occupy adjacent pages. The same segment can be examined through several cells, and fine and coarse indexes store overlapping reference populations.

   Search also performs binary searches for neighborhood cells, then may walk empty grid cells while computing geometric cell bounds. This is real work even when there is little nearby data.

   I would replace the two complete reference trees with a bulk-loaded, page-packed spatial hierarchy over address points and original street/interpolation segments. Leaves contain the coordinates needed for distance evaluation directly. Internal nodes carry conservative bounds and class-presence information. Ranked lookup explores promising nodes first, maintaining separate best distances for addresses, streets and interpolation.

   There is a useful semantic simplification here: **ranked lookup needs one search within `max(fine_radius, coarse_radius)`.** If a fine-radius candidate exists, the nearest candidate within the larger radius must also lie inside the fine radius. If none exists, coarse fallback determines the result. This remains true when the configured coarse radius is smaller than the fine radius.

   The current [two-stage ranked search](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:750) therefore does not require two physical indexes. The raw candidate API still needs its per-class fine-then-coarse selection rules.

   My layout target would be:

   - A small resident upper directory.
   - Page-sized leaves containing complete point and segment search records.
   - Local result strings or a small local dictionary for common values.
   - Compact references to compiled admin membership.
   - Explicit overflow handling for dense regions and long geometry.

   This makes one or two **data-page** touches plausible for ordinary queries after the upper directory is resident. It is not a universal bound: dense cities, three classes with distant winners, boundary geometry and long strings can require more pages.

   **Why the payoff is plausible:** it removes dependent random accesses, repeated segment evaluation and empty-cell traversal. It can also eliminate fine-cell covering and duplicate fine/coarse bucket production for ranked search, making this a build-through-query improvement.

   **Risks:** bounds must be conservative for the existing query-latitude cosine metric, including polar and periodic-longitude behavior. Keep original endpoints for final distance and snapping; geometric clipping or a spherical replacement can change answers. Equal-distance traversal ties need explicit compatibility handling.

   The raw API exposes segment duplicates, ordering and interpolation record indices. Those cannot disappear casually. Its compatibility enumeration can use compact occurrence metadata while ranked lookup uses the packed hierarchy. That compatibility cost belongs outside the ranked hot path.

4. **Ride the ALTW coordinate join at its resolved-coordinate seam. Do not materialize an ALTW PBF first.**

   The indexed build already has a good basic input-read shape:

   - Read relations to select admin membership.
   - Read ways once, collecting selected references and writing a compact way store.
   - Read nodes once, collecting coordinates and node addresses.
   - Resolve selected ways from scratch.

   Thus, its three typed scans amount to roughly **one full planet payload decode**, not three full planet decodes. [Schedule construction](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass1_5.rs:722) is a framing walk. The input digest adds a separate whole compressed-file read. Unindexed classification currently adds decoding work before the typed scans.

   One payload decode is sufficient with dependency-first random access and intermediate storage. A single physical forward stream is also possible, but requires retaining coordinates before knowing the referenced subset, and retaining way information before relations select admin members. It trades rereads for larger state.

   The current rank-indexed join already resembles [ALTW sparse](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:252). The opportunity is to consume resolved way batches directly, then produce spatial fragments immediately. For memory-bounded builds, adapt ALTW's [ID-bucket join](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/stage2.rs:345) and slot reordering, ending at resolved coordinates before its PBF encoding and payload handoffs.

   The neighbor must provide selected-way coordinates in original reference order, explicit missing-reference information, and a node-address extraction path. Important differences prevent using the current engine unchanged:

   - Geocode needs addresses from every node blob, including blobs containing no selected way references.
   - Bucket-straddling blobs must not duplicate address extraction.
   - Geocode rejects conflicting duplicate coordinates; sparse ALTW currently stores coordinates without that comparison.
   - Geocode drops missing coordinates and tracks interpolation completeness. Substituting zero coordinates changes results.
   - The external engine requires sorted indexed input, while geocode supports a broader input path.

   **Verdict:** reuse the join algorithms and consume their results directly. Keep rank lookup where its working set fits; the external join is a bounded-memory alternative with extra scratch traffic, not an automatic throughput win. Implement the geocode path first; generalizing a shared API can follow.

5. **Several smaller changes are worthwhile, but they are below those rewrites.**

   **Fuse dense-node validation, decoding and address extraction.** Pass 2 uses ordinary elements, unlike the way extraction projection. Dense arrays are traversed during validation, and [the dense iterator scans each tag run to find its extent](/home/folk/Programs/pbfhogg/crates/lib/src/read/dense.rs:189) before `node.tags()` decodes it again.

   Existing [projection pools](/home/folk/Programs/pbfhogg/crates/lib/src/scan/classify.rs:1416) provide a starting point. A stronger decoder extension would validate and consume node arrays together, classify address keys once per string-table entry, and emit coordinates plus selected address fields directly. This affects billions of nodes, so it is more substantial than removing allocations. Merely switching to a projection still leaves repeated array walks; the useful neighbor change is validated values delivered during the checked decode. The raw ALTW tuple scanner is insufficient because it omits tags and has different coordinate-refusal behavior.

   **Persist interpolation measurements.** [An improving interpolation candidate can trigger a full section-length scan](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:931), followed by another prefix walk for its number. Store total length and per-segment prefix length during building. Compute each section's values in the same accumulation order; subtracting global prefixes can change floating-point rounding and inferred house numbers. Also stop spatially indexing unresolved sections that can never answer. This is a strong targeted change, but interpolation prevalence limits its overall value.

   **Deduplicate failed admin attempts immediately.** Match the raw path's attempted-polygon tracking in ranked lookup. This is a useful local improvement while the compiled admin index is being replaced.

   **Parallelize CLI query batches and avoid JSON value trees.** [The CLI](/home/folk/Programs/pbfhogg/crates/cli/src/geocode_query.rs:82) queries serially, constructs nested `serde_json::Value`s and retains the complete serialized report in memory. `Reader` is already `Send + Sync`. Ordered parallel batches and direct serialization can improve million-query throughput; spatially grouping a batch can improve locality further. Preserve input order, output bytes and the existing no-output-on-interruption behavior. Publishing answers incrementally would change that behavior.

Two correctness findings should remain visible during this work.

- [`Candidates::into_result()`](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/reader.rs:146) keeps the first equal-area admin match, while ranked lookup explicitly chooses the lower polygon index. A valid index presenting equal-area polygons in reverse index order can make these APIs disagree. This is a code-level inconsistency, not something I executed.
- Building address placement is the arithmetic mean of resolved way coordinates, including a repeated closing vertex. It can lie outside a concave building, and an antimeridian-crossing building can average near longitude zero. Preserve that behavior during a throughput rewrite unless placement semantics are changed separately.

Finally, [the query instrument](/home/folk/Programs/pbfhogg/crates/bench/src/geocode.rs:128) measures serial queries over a repeated, warmed, uniform-in-bbox point set. It does not establish cold-page behavior, address-heavy throughput or CLI throughput. Its optimizer sink consumes only `admin.len()`; full results should escape optimization before trusting large query improvements. Those limitations especially matter when evaluating the packed layout, whose main benefit may be fewer page accesses rather than cheaper arithmetic.
