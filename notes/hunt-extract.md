I found four behavioral defects and one documentation mismatch. These are source-level findings; I changed no files and ran no builds or tests.

1. **[P1] Extraction can turn corrupt coordinates into valid coordinates elsewhere.**  
   Complete-ways and smart classify nodes through [unchecked columnar conversion](/home/folk/Programs/pbfhogg/crates/lib/src/read/columnar.rs:20), called from [pass 1](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/smart.rs:413). Multi-extract uses the same conversion.

   A node with `lat_offset = 429496729600`, raw latitude zero and longitude zero has latitude `429.4967296` degrees. Division by 100 yields `4294967296`; narrowing to `i32` produces zero. An extraction around `(0,0)` selects it, and [the write pass](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/common.rs:610) writes it at `(0,0)`. Truthful indexdata without a bbox is sufficient; no false metadata claim is needed.

   This breaks the stated refusal-over-silent-corruption contract. Sorted single-extract simple already refuses this input through its checked scanner. The general reader's unchecked behavior is documented, but extraction is the wrong consumer boundary: it converts that unchecked result into newly written geographic data.

2. **[P1] Negative IDs disappear instead of being refused, breaking reference completion.**  
   Extraction inserts IDs and dependencies into `IdSet`, whose [documented behavior](/home/folk/Programs/pbfhogg/crates/lib/src/idset.rs:527) silently discards negative IDs.

   Consider an in-region node `1`, an out-of-region node `-1`, and way `10` referencing `[1,-1]`. Complete-ways selects the way, discards `-1` while collecting its dependencies, then publishes the way without the existing node it references. Smart inherits the same failure. See [dependency collection](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/smart.rs:486) and [membership-based emission](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/common.rs:604).

   Simple also behaves inconsistently: an in-region negative node disappears through filtered emission but survives inside a verified raw-copy blob.

   This breaks both the stated project-wide rejection policy and complete-ways' completeness claim. `IdSet` keeps its own contract; extraction fails to enforce its precondition. ADR-0002's rejection title and its narrower enforcement policy also conflict with the stated project-wide guarantee.

3. **[P2] `--directory` can publish outside its promised output root.**  
   [Rebasing](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/mod.rs:408) preserves relative paths verbatim and joins them beneath the override. With `-d /work/results`, an output named `../escaped.pbf` becomes `/work/results/../escaped.pbf`, which writes `/work/escaped.pbf`. Absolute paths beneath the old root can retain escaping `..` suffixes too.

   This contradicts [DEVIATIONS.md's explicit guarantee](/home/folk/Programs/pbfhogg/DEVIATIONS.md:470) that every configured output stays under the override root. Normal output validation does not repair this destination choice; with overwrite enabled, it can replace a file outside that root.

4. **[P2] Absolute outputs lose their subdirectories when the config path is relative.**  
   [The configured root](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/mod.rs:349) remains relative when the config argument is relative. Rebasing then compares an absolute output against that relative root using `strip_prefix`, which necessarily fails.

   For example, from `/work`, config `cfg/extract.json` with `"directory":"out"` and output `/work/cfg/out/part/a.pbf`, rebased under `/new`, produces `/new/a.pbf`. The documented result is `/new/part/a.pbf`.

   This violates the promised preservation of subpaths beneath the configured directory. It can also turn distinct configured destinations into duplicate filenames and refuse an otherwise valid config.

5. **[P3] The documented single-pass paths reread and usually decompress payloads twice.**  
   The public API says simple multi-extract [reads the input once](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/mod.rs:561). Instead, [node classification](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/multi.rs:412) finishes before a separate [node write pass](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/multi.rs:518); ways and relations repeat that pattern. Single-extract simple does likewise. Verified raw copies avoid the second decompression, but still reread the frame.

   The documentation is wrong here. This also exposes a structural optimization opportunity: classify and encode each blob together, then let the ordered consumer install membership and write frames before advancing to the next kind. That would remove repeated payload work. I have not measured its payoff.

One additional surprising geometry policy: [ring-area validation](/home/folk/Programs/pbfhogg/crates/lib/src/commands/extract/polygon.rs:559) precedes antimeridian unwrapping. Consequently, `[(179,0),(-179,2),(180,1),(179,0)]` passes validation although its unwrapped geometry is collinear. Conversely, some nonzero-area unwrapped triangles are refused because their wrapped coordinates have zero area. The recorded raw-ring refusal policy explains the latter, so I have not counted expanding that accepted input set as a defect.
