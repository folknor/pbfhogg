I found three behavior defects. These are source-traced findings at `63d4ec1d`; I changed no files and ran no builds or tests.

1. **[P2] Renumber rejects valid plain nodes, including pbfhogg's own output.**

   **Broken claim:** [renumber all element IDs](./docs/cli/commands.md#renumber), with no dense-only restriction.

   Every node group goes through `single_dense`, which treats a plain `Node` as a foreign kind and refuses it. A sorted, honestly indexed block containing one plain node therefore fails renumber. The error incorrectly describes this single-kind group as mixed.

   This breaks an existing producer-consumer boundary: [`BlockBuilder::add_node_partial`](/home/folk/Programs/pbfhogg/crates/lib/src/write/block_builder.rs:802) deliberately writes plain nodes when dense encoding cannot preserve metadata presence. For example, metadata containing an explicit UID of zero and no version takes that route.

   **Wrong end:** renumber's node rewriter. It must handle both node encodings. See [`single_dense`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:401).

2. **[P2] Renumber rejects empty ways that its own builder writes.**

   **Broken claim:** renumber remaps all element IDs and cross-references; an empty reference list is supported by the library's way API.

   [`encode_way`](/home/folk/Programs/pbfhogg/crates/lib/src/write/encode.rs:222) omits field 8 when `refs` is empty. Renumber nevertheless requires that field and returns `reframe_ways: no refs field`. Thus a sorted, indexed file produced using `add_way(..., &[], ...)` cannot be renumbered.

   There is also a self-incompatibility: renumber accepts an explicitly present empty refs field, then its replacement encoder omits that field. The successful output fails a second renumber.

   **Wrong end:** [`reframe_way`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:703). An absent repeated field must mean an empty list, as the producer already implements.

3. **[P2] Sort orders history versions only when blob layout happens to trigger rewriting.**

   **Broken claim:** [`sort` is equivalent to osmium sort](/home/folk/Programs/pbfhogg/crates/lib/src/commands/sort/mod.rs:1), also stated in the parity table. [The vendored osmium contract](/home/folk/Programs/pbfhogg/research/osmium-tool/man/osmium-sort.md:24) orders equal IDs by ascending version.

   Consider one isolated history blob containing `way 7/version 2`, then `way 7/version 1`, under a header without the sorted claim. Pass 1 compares only IDs, finds no descent, and leaves the blob eligible for passthrough. Sort succeeds with versions still descending. `--distrust-metadata` does not fix this.

   Put those records in overlapping blobs, or introduce an ID inversion that forces rewriting, and the sweep instead emits version 1 before version 2. The result therefore depends on blob boundaries and unrelated disorder.

   **Wrong end:** the sort classification uses a weaker ordering than the rewrite path. See [the ID-only scan](/home/folk/Programs/pbfhogg/crates/lib/src/blob_meta/scan.rs:287), [rewrite selection](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge.rs:1036), and [version sorting](/home/folk/Programs/pbfhogg/crates/lib/src/commands/blob_merge/sweep.rs:879). The reference statement that [sort does not reorder versions](/home/folk/Programs/pbfhogg/reference/indexdata-trust.md:185) is also incorrect: it sometimes does.

Other concrete contract/documentation mismatches:

- **Relation counts are not checked against indexdata.** [The trust table](/home/folk/Programs/pbfhogg/reference/indexdata-trust.md:215) promises that renumber refuses a decoded count mismatch. Node and way stages do; [relation assembly](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/relations.rs:183) never compares `outcome.count` with `task.element_count`. A relation blob claiming zero while containing one relation succeeds. The documentation is wrong here: relation numbering uses observed ranks, so the claimed count does not control its IDs.

- **The overlap trust description describes the retired algorithm.** [It says](/home/folk/Programs/pbfhogg/reference/indexdata-trust.md:157) intra-entry order is unchecked and repaired by the heap, with claims verified before subsequent output. The current sweep refuses lazy-cursor descents and checks the final count/range on cursor exhaustion. The documentation should describe the current streaming validation.

- **Deliberate protobuf refusals are missing from the promised deviation registry.** Renumber explicitly refuses legal unpacked fields and split packed segments in [`wire_rewrite.rs`](/home/folk/Programs/pbfhogg/crates/lib/src/commands/renumber/wire_rewrite.rs:48). These choices are described in `reference/truncation-handling.md`, but absent from `DEVIATIONS.md`, which the CLI deviations page points to.

- **The CLI's block-rebuilding description has stale metadata semantics.** [It says](/home/folk/Programs/pbfhogg/docs/cli/commands.md:1278) nodes become dense, versionless metadata is discarded, and missing fields become defaults. [The current encoder](/home/folk/Programs/pbfhogg/crates/lib/src/commands/cat/mod.rs:736) preserves partial metadata and can emit plain nodes, consistent with the newer deviation record.

The structural issue worth addressing is renumber's assumption that node input means DenseNodes. Its rewrite layer needs to cover the element representations the library actually produces. Sort likewise needs one ordering contract shared by classification and rewriting.
