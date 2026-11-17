Read-only audit completed. No files changed, builds performed or tests run. These findings follow from the source paths below.

1. **[P2] A broken stdout pipe can erase an input error already detected.**  
   [lib.rs:3587](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:3587) converts any ordinary error into success once the global `reader_gone` flag is set.

   Concrete case: export a PBF with a valid header followed by a truncated data frame using `--format geojson --type node`, with stdout's reader closed. Export buffers the FeatureCollection prefix, detects the truncated input, then drops its writer. That drop flushes the prefix, gets `EPIPE`, and sets the flag. The CLI consequently suppresses the original corruption error and exits 0.

   This breaks the documented malformed-input exit-2 contract. The CLI end is wrong: pipe tolerance must classify the authoritative error, rather than any failure encountered during cleanup.

2. **[P2] Stderr I/O failures become panics instead of command failures.**  
   [output.rs:164](/home/folk/Programs/pbfhogg/crates/cli/src/output.rs:164) panics for every stderr error except `BrokenPipe`. With stderr redirected to `/dev/full`, an ordinary diagnostic or final error report therefore unwinds the process and normally exits 101, bypassing the promised exit-2 classification.

   The source comment explicitly endorses this behavior, but it contradicts the public CLI contract that I/O errors are failures with status 2. The code's panic policy is wrong for an ordinary environmental failure.

3. **[P2] Quiet inspect prints output when `--indexed` is combined with another mode.**  
   `inspect FILE --indexed --extended -q` reaches [lib.rs:5207](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:5207), where both the index verdict and full report are printed without consulting `quiet`. The same happens with `--blocks`, `--id-ranges` or `--nodes`.

   The [Quiet contract](/home/folk/Programs/pbfhogg/docs/cli/commands.md:49) says quiet `inspect --indexed` prints nothing. Only the `IndexedOnly` branch implements that promise.

4. **[P2] `inspect --get` can emit more than the requested single value.**  
   At [lib.rs:5209](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:5209), the CLI prints the index verdict and node statistics before handling `get`.

   Consequently, `inspect FILE --get elements.nodes --indexed` produces an `Indexed: ...` line followed by the count. Adding `--nodes` produces a statistics report before the value. Both combinations are accepted.

   This breaks `--get`'s documented scalar-output contract. Collecting additional statistics can remain valid, but their reports cannot be added to the scalar response.

5. **[P2] Full inspect incorrectly calls a header-only PBF unindexed.**  
   [scan.rs:424](/home/folk/Programs/pbfhogg/crates/lib/src/commands/inspect/scan.rs:424) requires `total_data_blobs > 0` before setting `is_indexed`.

   For the same valid header-only file:

   - `inspect --get indexed` returns `true`.
   - `inspect --extended --get indexed` returns `false`.
   - `inspect --indexed --extended` prints an affirmative probe verdict followed by a report saying `Indexed: no`.

   The repository explicitly defines header-only files as indexed. The library's full-scan result is wrong; the CLI exposes it unchanged.

6. **[P2] `inspect --show` omits visibility despite promising all metadata.**  
   [show_element_to's contract](/home/folk/Programs/pbfhogg/crates/lib/src/commands/inspect/show_element.rs:12) promises to write all metadata. Neither [write_info](/home/folk/Programs/pbfhogg/crates/lib/src/commands/inspect/show_element.rs:196) nor the dense-node formatter prints visibility.

   An invisible history object therefore displays without its deletion state; otherwise identical visible and invisible objects produce identical output. This is a library formatting defect reached through the CLI wiring.

Two further contract discrepancies:

- **Help/version output silently accepts non-pipe write failures.** [clap_exit](/home/folk/Programs/pbfhogg/crates/cli/src/lib.rs:127) discards `err.print()`'s result, so `--help` redirected to `/dev/full` returns success. This contradicts [output.rs's promise](/home/folk/Programs/pbfhogg/crates/cli/src/output.rs:22) that non-`EPIPE` stdout failures fail the command.
- **“Negative IDs are rejected project-wide” overstates the implemented policy.** Signed `inspect --show` queries are deliberately supported. Also, [PBF ID harvesting](/home/folk/Programs/pbfhogg/crates/lib/src/commands/getid/mod.rs:333) inserts IDs into unsigned sets without rejecting negatives, silently dropping them. ADR-0002 explicitly records this narrower policy. The blanket description is incorrect; it does not describe what the CLI currently enforces.
