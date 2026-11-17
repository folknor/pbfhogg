Static review only. No files changed, and no builds or tests run. I found the following defects.

1. **[P1] Recovery can attribute one source's bytes to another source.**  
   [bind_identity](/home/folk/Programs/pbfhogg/crates/datasets/src/download.rs:88) rewrites the dataset-wide identity when the current target is absent. It does not check older recoverable raw files.

   A concrete sequence: acquisition of `asia/georgia` publishes raw on day A, then dies before registration. On day B, acquisition of `north-america/us/georgia` uses a different filename, rewrites the identity, and registers successfully. Acquiring snapshot A for that US dataset then adopts the leftover Asian raw because the identity now matches the US source.

   This breaks the [source identity and safe recovery contract](/home/folk/Programs/pbfhogg/reference/datasets.md:126). The code is wrong: one mutable identity cannot safely describe all retained unregistered publications.

2. **[P1] Generating a missing indexed PBF can overwrite another registered file.**  
   [generate_indexed](/home/folk/Programs/pbfhogg/crates/datasets/src/download.rs:145) checks filenames only within the selected dataset, using literal string equality. Ordinary primary acquisition and new snapshot acquisition do not call the host-wide destination check.

   For example, Denmark's registered raw can be `shared.osm.pbf`, while another dataset registers `shared-with-indexdata.osm.pbf`. Downloading Denmark with its indexed entry missing replaces that other dataset's bytes while leaving its existing pin unchanged. Absolute and alternate path spellings also bypass the local check.

   This violates registered-file preservation and the promise that acquiring a snapshot leaves existing generations unchanged. Publication needs the same ownership checks on every acquisition path.

3. **[P1] Download staging cleanup can delete registered data.**  
   [acquire_raw](/home/folk/Programs/pbfhogg/crates/datasets/src/download.rs:134), [generate_indexed](/home/folk/Programs/pbfhogg/crates/datasets/src/download.rs:160), and OSC acquisition call `clear()` on predictable staging names without checking registry ownership.

   A registered snapshot may legitimately name `.partial-denmark-new.osm.pbf`. Acquiring snapshot `new` unlinks that file before starting its writer. Even refresh's host-wide check examines final destinations only.

   This breaks preservation of registered files, including on failure: a subsequent transfer error leaves the old registry pointing at deleted data.

4. **[P1] The registry writer can truncate registered data through its temporary path.**  
   [replace_file](/home/folk/Programs/pbfhogg/crates/devkit/src/registry.rs:510) uses `File::create` on the fixed `.CONFIG.tmp` name. It neither checks ownership nor refuses symlinks.

   With host `data = "."`, a registered raw file named `.brokkr.toml.tmp` is accepted by acquisition. The next registry commit truncates that raw, writes TOML into it, then moves it away. A pre-existing temporary symlink likewise makes `File::create` truncate its target.

   This violates registered-file preservation and the writer's claim that failures before commit preserve existing bytes. The temporary file must be established as writer-owned before opening it for truncation.

5. **[P2] The alias resolver does not resolve dangling symlinks.**  
   [publication_path](/home/folk/Programs/pbfhogg/crates/datasets/src/layout.rs:62) treats `canonicalize` returning `NotFound` as an absent filename. If `alias -> destination` exists but `destination` does not, it returns the path to `alias`, rather than its target.

   Consequently, refresh and promotion can fill a destination already registered through that alias, bypassing collision protection and potentially installing bytes inconsistent with the registered pin.

   This contradicts both the helper's documented behavior and promotion's [promise not to fill absent registered destinations](/home/folk/Programs/pbfhogg/reference/dataset-promotion.md:55). The resolver is wrong.

6. **[P2] The collision inventory omits registered polygons and PMTiles.**  
   [DatasetView and Generation](/home/folk/Programs/pbfhogg/crates/devkit/src/registry.rs:33) retain only PBF and OSC entries. Download and promotion then use that incomplete view as their inventory of registered paths.

   A configured polygon at a promotion staging path is therefore invisible to [the protection check](/home/folk/Programs/pbfhogg/crates/datasets/src/promote.rs:125), and promotion deletes it. An absent registered polygon destination can similarly be filled. PMTiles registrations have the same omission.

   This breaks the unqualified registered-path protection contract. The typed acquisition view is insufficient as a filesystem ownership inventory.

7. **[P2] Recovery skips the durability step needed before registry commit.**  
   Fresh publication syncs the file and its directory. However, [raw adoption](/home/folk/Programs/pbfhogg/crates/datasets/src/download.rs:128), OSC adoption, and [promotion reuse](/home/folk/Programs/pbfhogg/crates/datasets/src/promote.rs:106) perform no equivalent sync.

   If a process dies after rename but before directory fsync, retry sees the complete file and registers it without first making that directory entry durable. A later power failure can preserve the registry commit while losing the publication. Promotion can also reuse an existing equal file whose bytes were never synced.

   This breaks the documented ordering that durable publication precedes durable registration. Recovery must finish that ordering too.

8. **[P2] Distinct OSC registrations silently collapse onto one sequence.**  
   [generation](/home/folk/Programs/pbfhogg/crates/devkit/src/registry.rs:219) parses string keys into `u64` and inserts them without checking for duplicates. Valid TOML keys `011` and `11` therefore become one entry, silently discarding one file and pin.

   This bypasses the [requirement that every registered donor exist, pass its pin, and agree with other copies](/home/folk/Programs/pbfhogg/reference/datasets.md:90). Conversely, `select_osc` uses literal string keys, so a sole `011` entry exists to acquisition but is missing to a numeric range requesting sequence 11.

   The two devkit readers disagree. Sequence interpretation needs one representation, with ambiguous registrations refused.

9. **[P2] Acquisition silently ignores retired `sha256` pins.**  
   [file_entry](/home/folk/Programs/pbfhogg/crates/devkit/src/registry.rs:233) reads `xxhash` but ignores `sha256`. A file carrying only the retired pin becomes unpinned in `DatasetView`, and acquisition accepts it without checking that pin.

   The other reader, [resolve_entry](/home/folk/Programs/pbfhogg/crates/devkit/src/datasets.rs:92), explicitly refuses the same configuration, matching the [documented retirement contract](/home/folk/Programs/pbfhogg/reference/library-protocol.md:215).

   This is reachable through the supported `--config` selection of another project's registry, even when brokkr's own invocation config is valid. The registry reader is wrong.

10. **[P2] A configured FIFO blocks digest resolution before its regular-file check.**  
    [file_digest](/home/folk/Programs/pbfhogg/crates/devkit/src/datasets.rs:19) opens the path before checking its type. Opening a FIFO for reading with no writer blocks indefinitely, so the intended refusal never executes.

    `resolve_input`, OSC selection, and polygon selection reach this directly. A malformed registered input can therefore hang the development tool while it holds brokkr's machine lock, instead of returning the promised invalid-entry error. The fix belongs at the file-opening boundary; a metadata check alone would retain a replacement race.

11. **[P2] Planet acquisition prints instructions that configure the wrong replication sequence.**  
    [The post-download note](/home/folk/Programs/pbfhogg/crates/datasets/src/download.rs:792) tells every source to inspect `header.replication.sequence` and copy it into the registered PBF entries.

    The [planet contract](/home/folk/Programs/pbfhogg/reference/datasets.md:68) explicitly says that header sequence is minutely, while this tool downloads daily diffs. Following the emitted instructions configures the wrong sequence namespace. `base_seq` then trusts that configured number.

    The documentation and refusal policy are correct; the operational guidance is wrong.

12. **[P3] `replace_file` promises rollback after errors that occur after replacement.**  
    Its [public contract](/home/folk/Programs/pbfhogg/crates/devkit/src/registry.rs:495) says a failure leaves the destination unchanged. But rename succeeds before the directory open/fsync, and either later operation can fail. The function then returns `Err` with the new registry already installed.

    Here the documentation is wrong. Atomic visibility does not provide rollback after the rename commit point. The result contract should distinguish failure before replacement from replacement whose durability could not be confirmed.

The main structural opportunity is to give publication one complete registry ownership inventory and one shared acquisition/recovery protocol. Separately parsing pins, sequence keys, aliases, and ownership in each path is already producing contradictory answers at those boundaries.
