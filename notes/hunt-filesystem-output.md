I found three clear contract defects and two additional boundary hazards. These are from source inspection only. I changed no files and ran no builds or tests.

1. **[P1] A `.buckets-level*` directory can hide a backup from deletion protection.**

   [scratch.rs:878](/home/folk/Programs/pbfhogg/crates/lib/src/scratch.rs:878) skips those directories entirely during `protected_entries`. Given this existing tree:

   ```text
   index/
     geocode_header.bin
     .buckets-level9/
       .pbfhogg-backup-123-0.previous/
   ```

   replacement passes the protection check. After publication, [output.rs:2166](/home/folk/Programs/pbfhogg/crates/lib/src/output.rs:2166) recursively deletes the old tree, including the nested backup. No concurrent mutation is needed.

   This breaks [ADR-0017's promise](/home/folk/Programs/pbfhogg/decisions/0017-outputs-are-published-by-the-library.md:27) to refuse protected entries anywhere inside a directory, and the promise that a kept backup is never deleted. The local helper documentation accurately describes its shortcut; that shortcut is wrong against the stronger safety contract. Allowing removal of the bucket directory itself should not exempt its descendants from inspection.

2. **[P2] The index replacement validator accepts arbitrary directories based on one filename.**

   [replaceable_index_dir](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/mod.rs:330) accepts a nonempty directory whenever `geocode_header.bin` exists. That entry can be an empty file, an unrelated file, a directory, or a symlink to an existing object. No header contents or index structure are examined.

   Consequently, an unrelated directory containing that name passes `index_output(..., Replace, ...)`; a successful build replaces it wholesale and deletes its contents.

   This contradicts the function's explicit promise that overwrite permission replaces an index or an empty directory, never an arbitrary directory. The validator is the wrong end of this boundary: `OutputDir` faithfully trusts the acceptance decision it receives. Its actual acceptance criterion is a filename convention, substantially weaker than the documented protection.

3. **[P2] `same_file` destroys the original I/O error while adding the pathname.**

   The `named` closure in [path_guard.rs:105](/home/folk/Programs/pbfhogg/crates/lib/src/path_guard.rs:105) formats the original error into a string and constructs a replacement `io::Error`.

   For example, a canonicalization failure with `EACCES` retains `PermissionDenied`, but loses the original error object and its errno. Later conversion into `pbfhogg::Error` cannot recover either.

   This directly breaks the [error provenance contract](/home/folk/Programs/pbfhogg/reference/pipeline.md:967): typed errors keep their causes, and intermediate layers do not stringify them. The implementation should retain the original error in a typed path wrapper.

4. **Additional boundary hazard: the library index builder can delete its input PBF.**

   [build_geocode_index_to](/home/folk/Programs/pbfhogg/crates/lib/src/geocode_index/builder/mod.rs:229) checks whether scratch lies inside the destination, but does not check the input.

   A library caller can rebuild an existing index using `index/source.pbf` as input, with scratch outside the index. The build reads that input successfully; publication then replaces the index directory and deletes `source.pbf` with the old generation.

   The CLI prevents this through its containment preflight. The documented refusal is in the CLI contract, so I am flagging the library behavior separately rather than claiming that contract explicitly covers this API. It is nevertheless a destructive difference between the two entry points.

5. **Additional partial-failure hazard: scratch cleanup can abort an already-unwinding process.**

   On removal failure, [RunScratch::cleanup](/home/folk/Programs/pbfhogg/crates/lib/src/scratch.rs:383) calls the caller's diagnostic sink directly, including when invoked by `Drop` during panic unwinding. [Diagnostics::emit](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:2433) does not contain that callback's panic.

   An existing panic, a scratch removal failure, and a panicking diagnostic sink therefore produce a double panic and process abort, interrupting remaining cleanup. This is conditional, but concrete. The output layer already recognizes this exact hazard and suppresses cleanup callbacks during unwinding in [emit_cleanup](/home/folk/Programs/pbfhogg/crates/lib/src/output.rs:2228); scratch lacks the corresponding protection.
