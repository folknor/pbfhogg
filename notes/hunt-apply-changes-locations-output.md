The strongest immediate opportunity is a rewrite of way refresh around wire edits, combined with a coordinate join driven by explicit requests. The largest longer-term opportunity is a maintained reverse dependency index that removes the whole-base way search.

The current code already fuses coordinate extraction into the node phase and refresh discovery into the way phase. There is no separate full-file refresh pass to eliminate. The remaining waste is inside those phases: unconditional node decoding, repeated examination of way payloads, rebuilding whole blobs after sparse edits, and tying coordinate readiness to ordered output progress.

These are architectural hypotheses from code inspection, not measured speedup claims.

**1. Replace detection followed by rebuilding with a single way-edit operation. This is the first rewrite I would make.**

For an indexed way blob outside the OSC way ranges, `handle_candidate` decompresses it and calls `detect_stale_wire`. Detection stops at the first stale way. A hit then goes through full `PrimitiveBlock` construction, overlap classification, and `rewrite_block_parallel`. During rewriting, surviving ways run `evaluate_way` again; changed ways then run `refreshed_locations`, decoding refs and probing the OSC again. The detection result retains only a stale verdict, not the work needed to perform the edit. See [streaming.rs:696](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/streaming.rs:696), [refresh.rs:496](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/refresh.rs:496), and [element_writes.rs:284](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/element_writes.rs:284).

More consequentially, one stale coordinate sends **every surviving way in that blob** through `BlockBuilder`. The raw writers already preserve packed tags, refs and Info bytes, which is good, but they still reconstruct each message, check its strings, copy it through builder buffers, and flush at the element cap. Each subsequent base block pre-seeds the source string table again. `StringTable::pre_seed` copies and hashes its entries; this is substantially more work than preserving an untouched way message and its existing table. See [rewrite_block.rs:112](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/rewrite_block.rs:112), [block_builder.rs:1688](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:1688), and [string_table.rs:135](/home/folk/Programs/pbfhogg/crates/lib/src/write/string_table.rs:135).

I would replace this with one operation that merges OSC edits and refresh edits against the decompressed wire payload:

- Copy surviving, unchanged Way messages as complete byte spans.
- For surviving ways with refresh hits, retain the hit positions and replacement coordinates and produce their new location fields once.
- Delete or replace OSC-touched ways in the same traversal, and interleave creates there.
- Preserve the source string-table indices; append strings needed by OSC records.
- Produce output blocks according to encoded size and compression economics, rather than routing every surviving element through the general builder's element cap.

For a refresh-only blob, tags, metadata, refs, IDs and element count do not change. Its string table and existing tag index can stay intact. Only coordinate fields, enclosing message lengths, compression and framing need change.

This has a concrete precedent: [altw/reframe.rs:59](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/reframe.rs:59) already edits way payloads without `BlockBuilder`. The reader also supports projections during validation through [BlockBuffer::project](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1150), retaining validated bytes for subsequent use. Apply-changes should use those capabilities as starting points, while designing its own coherent merge-and-refresh kernel.

The payoff is plausible because it removes work proportional to all elements in affected blobs, including repeated string-table construction. It also avoids multiplying framing and scheduling work when a source blob becomes many builder blocks.

The main risks are validation and framing correctness. Clean refresh scans should keep their lightweight path; making every clean blob undergo full tag and metadata validation could erase savings. Dirty blobs must receive the required validation without restarting the edit traversal. Non-default coordinate framing, date granularity, partial metadata, repeated refs and blob-size limits all need deliberate handling. The existing ALTW reframe cannot simply be called unchanged: it rejects non-default coordinate framing.

**2. Make OSC-way coordinate resolution a planned sparse join, rather than a scan of every node plus several hash maps.**

`NodeLocationIndex` already knows every distinct node referenced by surviving OSC ways and removes nodes supplied or deleted by the OSC. Nevertheless, scanner routing forces **every node blob** through decompression in location mode. Each node is coordinate-decoded and checked, then tested against `needed_set`; the blob subsequently undergoes full parsing and overlap classification. This happens even when no OSC way needs a base coordinate. See [node_locations.rs:43](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/node_locations.rs:43), [scanner.rs:243](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/scanner.rs:243), and [streaming.rs:796](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/streaming.rs:796).

Retained coordinates travel through a local hash map, a worker slot map, and a final merged map. OSC-way emission then collects refs into a fresh vector, performs map lookups, collects a second vector of coordinates, and passes both to the builder. See [streaming.rs:1261](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/streaming.rs:1261), [drain.rs:967](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:967), and [element_writes.rs:47](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/element_writes.rs:47).

I would give each distinct requested node an ordinal and each OSC-way reference a corresponding ordinal or resolved slot. Sorted requests would be intersected with node-blob ranges before scheduling:

- A node blob needed by neither the ordinary merge nor coordinate requests remains passthrough.
- A decoded node blob resolves its sorted request slice while performing the core merge's node traversal.
- Coordinates go directly into the ordinal array with explicit presence tracking.
- OSC ways consume those slots, preferably as location payloads prepared before final encoding.

This removes repeated hashing and map merging, but the larger opportunity is avoiding node decompression and parsing where no request requires it. When requested IDs are spread widely enough to touch most node blobs, that part of the benefit shrinks. I would not promise sparse I/O for every daily diff.

The existing ALTW sparse store demonstrates direct coordinate slots and explicit presence encoding in [sparse.rs:140](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/sparse.rs:140). Its current `RankedIdSet` is not a complete replacement here because it stores nonnegative IDs; apply-changes' OSM ordering includes negatives. An ordinal representation must preserve that support.

Risks include duplicate-node winner selection, missing-node accounting, geographic checks and keeping `(0,0)` distinguishable from absence. Skipping formerly inspected blobs also requires an explicit decision about malformed-input refusals; savings must not come from accidentally changing promised exit behavior.

**3. To remove the dominant whole-way search, maintain node-to-way dependencies. This is a larger storage redesign.**

Any OSC node upsert enables `refresh_ways`, which forces all indexed way blobs through decompression and reference probing. The blocked Bloom filter makes individual misses cheaper, but does not remove the work of reading, decompressing and traversing the entire way population. See [rewrite.rs:182](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/rewrite.rs:182), [scanner.rs:249](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/scanner.rs:249), and [refresh.rs:238](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/refresh.rs:238).

For an arbitrary PBF without dependency metadata, that search is fundamentally necessary. Way IDs and their ranges do not tell you which node IDs they reference.

For repeated updates, I would maintain a compressed reverse index from node IDs to stable way or logical block identities. Changed node IDs would then identify candidate way blobs directly. Those candidates would be unioned with ordinary apply-changes candidates and processed once by the wire-edit operation above.

The design needs stable identities plus a current identity-to-file-range directory. Raw byte offsets alone are unsuitable because earlier output changes shift later offsets. Way creates, deletes and reference-list changes update postings; coordinate-only refreshes leave dependencies unchanged. Index publication must be atomic with the corresponding PBF, and an absent or mismatched index must fall back to scanning.

This is the largest asymptotic opportunity: refresh discovery becomes proportional to changed-node dependencies instead of all way references. However, its initial construction still requires reading all references, postings may be large, and geographically scattered changes can still reach many compressed blobs. Full PBF output also retains its copy/write floor.

I would choose exact compressed postings over assuming a small per-blob Bloom summary will solve this. Large blobs and large change sets can make such summaries admit most blobs.

The existing `WayMembers-v1` field is not this index: [way_members_payload](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/reframe.rs:464) encodes per-way relation membership, not node-reference dependencies.

**4. Separate coordinate readiness from output retirement, and use one output ordering authority.**

The scanner stops at the first non-node descriptor until the drain has processed every earlier node item and merged coordinate maps. Workers publish extracted coordinates before sending their node output, so coordinate production finishes earlier than the condition that releases way work. Backpressure in node output submission therefore delays way decompression and refresh discovery unnecessarily. See [scanner.rs:292](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/scanner.rs:292), [streaming.rs:888](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/streaming.rs:888), and [drain.rs:380](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:380).

Output then passes through another sequence of ownership transfers:

1. Workers retain all compressed frames for one source blob.
2. The drain reorders whole-blob results and emits gap or trailing creates.
3. The drain submits each rewritten frame individually to `PbfWriter`.
4. The writer assigns another sequence and performs ordered delivery.
5. The parallel backend assigns offsets and dispatches writes.

The backend already performs parallel positional writes; this is not a proposal to introduce parallel I/O. The opportunity is to remove duplicated ordering machinery and allow useful work to proceed before the drain retires a whole source blob. See [descriptor.rs:190](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/descriptor.rs:190), [drain.rs:895](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:895), [writer.rs:245](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:245), and [parallel_writer.rs:273](/home/folk/Programs/pbfhogg/crates/lib/src/write/parallel_writer.rs:273).

I would have the core establish ordered work units, including creates, before encoding. Workers would publish completed frames under those identities directly to one output coordinator. That coordinator would combine ordering, copy-range coalescing and offset assignment, then hand operations to the existing backend.

Coordinate-dependent OSC ways would wait for their coordinate slots, not for node output submission. Surviving-way refresh already depends only on the OSC probe. Relation processing has no coordinate dependency. Create encoding would become scheduled work rather than synchronous batches entered by the drain's type-transition logic; currently even its parallel batches must complete before their frames are walked and submitted in order ([stream_output.rs:147](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/stream_output.rs:147)).

This is a coherent topology rewrite. Its likely benefit is better overlap, smaller completion units and less retained output behind a slow blob. Its risks are deadlock-free admission, cancellation, failure propagation and exact create placement. The current sequence window solves real shutdown and progress problems; replacing it requires preserving those properties, not merely replacing channels.

**How I would divide the work across neighbours**

The apply-changes core should hand refresh one ordered merge plan over the same decoded bytes: surviving base-way spans, OSC replacements and creates, deletions, source framing, and verified placement information. Refresh should return coordinate-field edits within that plan. It should not rediscover which elements survive or start another element walk.

OSC ingest should expose final node states and OSC-way reference counts/iterators sufficient to construct ordinal requests. Coordinate resolution should finish before those ways reach encoding.

I would reuse ALTW's **join techniques**, not run the complete ALTW pipeline after apply-changes. Surviving base ways already carry coordinates: their refresh is an overlay patch, not a join for every vertex. Only created or modified OSC ways require coordinate resolution. ALTW's positional payload encoding is a useful model ([coord_payloads.rs:838](/home/folk/Programs/pbfhogg/crates/lib/src/commands/altw/external/coord_payloads.rs:838)); its full external pipeline would introduce unnecessary reference emission, bucket I/O and assembly passes for this workload.

**Smaller changes worth keeping separate from those rewrites**

- Submit rewritten frame groups through the existing [write_raw_chunks](/home/folk/Programs/pbfhogg/crates/lib/src/write/writer.rs:1241) API instead of reserving and sending once per frame. This reduces handoffs but leaves the architecture intact.
- Carry refresh hit positions into coordinate encoding instead of discarding them and probing refs again. This removes repeated work within edited ways.
- Eliminate the two temporary vectors in `add_osc_way` with an iterator or pre-encoded location interface. Useful, but limited to OSC-way emission.
- For default framing, a changed absolute coordinate alters its own delta and the next delta. A specialized encoder could preserve intervening packed bytes. This is a secondary refinement after removing repeated traversal and whole-element rebuilding.

I would not make Bloom-filter tuning or receiver-lock replacement the centre of this effort. Neither removes the population-wide work.

Two correctness findings deserve attention during the redesign. First, duplicate base node IDs with differing coordinates can resolve according to worker assignment: worker maps merge in slot order rather than source order, while the order guard permits equal boundary IDs. That is a concrete determinism concern, though I did not reproduce it ([drain.rs:1004](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:1004), [drain.rs:593](/home/folk/Programs/pbfhogg/crates/lib/src/commands/apply_changes/drain.rs:593)). Second, pruning refresh candidates by comparing old and new **node** coordinates would change current behavior: a tag-only node modification can still repair a stale inline way coordinate because refresh compares the way's stored coordinate directly against the OSC. Such pruning requires a proven consistency invariant.

My implementation priority would be the fused wire-edit kernel and requested-coordinate join first, followed by the output topology rewrite. The maintained reverse dependency index is the bigger strategic investment: it is the change that can eliminate the whole-base refresh search.
