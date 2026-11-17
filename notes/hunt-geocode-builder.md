Source review only. No files changed, builds performed, or tests run.

1. **[P1] The overwrite guard can authorize deletion of an unrelated directory.**  
   [replaceable_index_dir](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/mod.rs:330) accepts any directory where `geocode_header.bin` exists. That entry can contain arbitrary bytes, be a symlink, or even be a directory. No header validation follows before publication replaces the entire destination and deletes its backup.

   This breaks the documented [index-or-empty restriction](/home/folk/Programs/pbfhogg/docs/cli/commands.md:2041). An unrelated directory containing an empty subdirectory named `geocode_header.bin` is a concrete counterexample. The builder's eligibility check is wrong; the output layer faithfully executes its authorization. Recognition should establish that the destination is an index, while still permitting replacement of older formats.

2. **[P1] Out-of-domain coordinates can wrap into valid locations and be published.**  
   [Pass 2](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass2.rs:678) calls `decimicro_lat()` and `decimicro_lon()` before checking the geographic domain. Those accessors [narrow with `as i32`](/home/folk/Programs/pbfhogg/crates/lib/src/read/elements.rs:50).

   With default coordinate framing, a raw latitude of `4_294_967_296` represents `429.4967296` degrees. Narrowing turns it into zero, which `pack_coord` accepts. An addressed node can consequently become an address at latitude zero.

   This directly breaks the promise that [out-of-domain nodes fail the build](/home/folk/Programs/pbfhogg/docs/cli/commands.md:2067). The general accessors document their permissive behavior; the builder is the wrong end of this boundary. It needs checked scaling and narrowing before domain validation, as the existing node scanner already provides.

3. **[P1] Missing nodes create invented street segments and admin boundaries.**  
   [resolve_way_into](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass2.rs:427) removes unresolved references with `filter_map`, then treats the remaining coordinates as one continuous way.

   For source refs `A, missing-B, C`, it writes segment `A-C`. A query on that chord can return a zero-distance street even though no such segment exists in the source. For an admin ring, removing missing vertices can leave a smaller closed polygon, which [admin assembly accepts](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/admin.rs:183).

   This breaks the [missing-geometry contract](/home/folk/Programs/pbfhogg/docs/cli/commands.md:2068): absence contributes a newly constructed edge. The reader and cell cover correctly process the counterfeit geometry they receive. Preserve gaps structurally: streets can retain contiguous resolved runs, and admin assembly must not close rings across unresolved references. Interpolation already carries completeness explicitly.

4. **[P1] A corrupt relation role becomes an outer boundary.**  
   [select_relation](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass1.rs:103) uses `m.role().unwrap_or("")`; the empty role means outer.

   Give an otherwise ordinary boundary relation an outer ring and an enclosed ring whose role index is out of bounds. The enclosed ring becomes another outer polygon instead of causing refusal. Queries inside it can return the administrative area where a hole should have excluded it.

   This violates the stated refusal policy for input-reachable faults. [CORRECTNESS.md](/home/folk/Programs/pbfhogg/CORRECTNESS.md:71) acknowledges this exact fallback, so this is a known defect, not an undiscovered implementation detail. That allowance conflicts with the stronger correctness claim: a failed lookup is not an empty role. Propagate the role error.

5. **[P2] Corrupt node and relation tags are silently read as a valid prefix.**  
   The builder uses infallible tag iterators in [node processing](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass2.rs:714) and [relation selection](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass1.rs:57). An out-of-bounds string index ends those iterators without an error.

   An address with house number and street before the corrupt pair, and a postcode or place after it, is published with those later fields absent. Relation classification likewise operates on a truncated tag set. Pass 1.5 already refuses the equivalent corruption in way tags, making the builder inconsistent across element kinds.

   The generic iterator limitation is [documented](/home/folk/Programs/pbfhogg/CORRECTNESS.md:43), but choosing it here violates the stated no-silent-wrong-answer contract. Checked iterators already exist; block validation does not resolve these indices.

6. **[P2] The builder can successfully publish an index with nothing queryable.**  
   The [empty-index gate](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/mod.rs:714) counts interpolation source records before resolution.

   Use two untagged nodes and one way tagged only `addr:interpolation=all`, with no streets, addresses, or admin polygons. The source interpolation record passes the gate. Resolution [retains it with zero endpoints](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/interp.rs:202), and the reader correctly refuses to answer from it. The build nevertheless succeeds.

   This contradicts both the gate's comment and the promise to refuse [nothing queryable](/home/folk/Programs/pbfhogg/docs/cli/commands.md:2227). The gate needs to judge usable output after resolution, rather than the presence of staging records.

7. **[P2] Negative replication timestamps silently become enormous positive timestamps.**  
   [build_generation](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/mod.rs:435) casts the header's signed timestamp directly to `u64`. A header timestamp of `-1` becomes `18_446_744_073_709_551_615`, which `Reader::replication_timestamp()` returns unchanged.

   This breaks the [claim that this metadata describes the input header](/home/folk/Programs/pbfhogg/docs/guide/advanced.md:183). The builder checks representability for the neighboring replication sequence but not the timestamp. Preserve signed timestamps or refuse values the index format cannot represent.

8. **[P2] Public geometry calls can hang indefinitely while unwrapping longitude.**  
   [unwrap_ring_longitudes](/home/folk/Programs/pbfhogg/crates/lib/src/geo.rs:452) repeatedly adds or subtracts 360. With an infinite longitude, subtraction never changes the value. A sufficiently large finite longitude, such as `1e308`, has the same problem because subtracting 360 rounds back to itself.

   For example, `point_in_ring_with_antimeridian` with ring `[(0,0), (INFINITY,0), (0,1)]` never returns. `point_in_polygon` and `simplify_ring` also reach this code.

   These public signatures document no finite-coordinate precondition or nontermination behavior. This is a public API fault, not a PBF-builder trigger: builder coordinates cannot reach it. Validate the domain or use bounded normalization that cannot stop making progress.

9. **[P2] Public integer longitude normalization overflows on its accepted type.**  
   [wrapped_longitude_delta_e7](/home/folk/Programs/pbfhogg/crates/lib/src/geo.rs:507) adds half a turn before reducing modulo a turn. `i64::MAX` overflows that addition. With overflow checks it panics; without them it returns the wrong longitude because wrapping modulo `2^64` changes the subsequent longitude remainder.

   [normalize_longitude_e7](/home/folk/Programs/pbfhogg/crates/lib/src/geo.rs:516) inherits the defect. Its documented operation is folding an unwrapped `i64` longitude, with no narrower precondition. Reduce modulo the full turn before adding the half turn. Current builder callers stay within a safe range.

Other discrepancies and surprising behavior:

- **“Negative IDs are rejected project-wide” overstates this builder's behavior.** Negative-ID address nodes can be indexed successfully. Negative way references are [skipped and counted](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass1_5.rs:90), rather than rejected, even when their nodes exist. ADR-0002 already weakens the blanket claim through site-specific enforcement, but does not inventory this geocode behavior. The description and enforcement documentation need reconciliation.
- **Building addresses can land far from the building at the antimeridian.** The [literal arithmetic longitude mean](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/pass2.rs:514) mixes positive and negative longitudes without unwrapping. This follows the documented mean-of-nodes policy, so it is a surprising policy consequence rather than an additional contract defect.
- **Admin ranking area uses a different longitude frame from containment.** [Area calculation](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/admin.rs:213) uses raw longitudes, while containment unwraps antimeridian crossings. A wider dateline-crossing rectangle can therefore have a smaller stored ranking area than a narrower one. The documentation explicitly defines ranking by stored planar area, so changing this would change the current policy.
