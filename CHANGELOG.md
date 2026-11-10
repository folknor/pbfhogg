# Changelog

## Unreleased

### Changed

- `merge-changes` and `tags-filter --input-kind osc` keep the body of a
  delete - tags, way node refs, relation members and a deleted node's
  location - as osmium does, instead of writing only its identity and
  metadata. Geofabrik diffs carry the object's last state in every delete.
  A deleted node carrying only one of `lat` / `lon`, or an unusable
  value, is now refused by these two commands.
- Two-pass `tags-filter` walks the blob headers once instead of once per
  pass, about 10 % faster on planet (56 to 50 s for `w/highway=primary`,
  62 to 56 s for `r/type=route` on bygg).
- Replacing an existing directory output that the caller cannot write is
  refused before any work starts, instead of failing at publication after
  the whole run.
- `inspect` reports a decoded data blob with no elements as `empty`, with
  its own stats and anomaly reason, instead of counting it as mixed; it no
  longer makes the file read as unordered.
- `check --index` reports an empty or mixed-kind unindexed blob among
  indexed ones as a coverage violation, matching the commands that refuse
  such a file; it used to pass.
- A pipelined `PbfWriter` whose writer thread failed keeps failing: a later
  `flush` returns the original error instead of `Ok`, so it no longer
  certifies an incomplete output.
- `apply-changes` keeps metadata fields an OSC element leaves out absent in
  the output, instead of writing uid, changeset or timestamp `0`. A node
  with partial metadata is written as a plain Node message; nodes with
  complete or no metadata stay dense. A node without metadata is no longer
  given zero metadata because it shares a dense run with nodes that have
  it. `digest` accepts a block holding both node encodings.
- `--direct-io` help and the I/O capability matrix in `docs/cli/commands.md`
  now state which read passes and writes each command actually performs
  with O_DIRECT; a run prints a Note when the filesystem reports zero
  direct-I/O alignment and so falls back to buffered I/O.
- `build-geocode-index` decompresses each way blob once instead of twice
  and validates every way blob, so a tag string index past the string
  table is refused instead of cutting the way's tags short. A key repeated
  in a blob's string table no longer loses the way's coordinates. Planet
  builds need about 7 GB of extra scratch for the way store. Pass 1 now
  reads only the relation blobs instead of the whole file. Together with
  cheaper Pass 2 merges and parallel Pass 3 bucket writing, a planet
  build goes from 177 to 110 s on bygg and a germany build from 10.6 to
  6.4 s.
- `diff -v` caps keyed tag lines with the other edit lines at 1000 per
  element; past the cap the element's tag detail is one line giving the
  tag counts and saying details were omitted.

## 0.6.0 - 2026-11-09

This release makes pbfhogg safe to embed as a library and strict about its
input. Commands refuse files they would otherwise answer wrongly about,
outputs are never left partial, and the library no longer touches process
state.

### Breaking

**CLI**

- Existing outputs are refused unless `--overwrite` (`-O`) is given; a
  symlink, directory or special file is never accepted as an output.
- Exit codes: `0` success, `1` negative answer (`diff` found differences,
  `check` found violations), `2` failure, `3` `--direct-io` or `--io-uring`
  unavailable, `64` usage error, `130` / `143` after SIGINT / SIGTERM. See
  "Exit codes" in `docs/cli/commands.md`.
- Every command except `cat` refuses a partially indexed input; `cat` (and
  the new `cat --reindex`) is the repair. `check` instead stops the affected
  check with exit `1` (JSON `partially_indexed`) and still runs the others.
- Every short flag has one meaning across subcommands; letters that meant
  something else on one command are long-only there. `getid --verbose-ids`
  is now `-v, --verbose`.
- A flag the rest of the command line makes ineffective is a usage error
  (exit `64`) instead of a warning or ignored. This includes `--force` where
  no indexdata is needed, `check --direct-io` under `--refs` / `--locations`,
  and `--max-errors` under `check --refs` without `--show-ids`.
- `--type` takes one comma-list grammar everywhere and refuses an unknown
  element type.
- OSC outputs are framed by name: `.osc` is plain XML, `.osc.gz` gzip, any
  other name is refused.
- Input that used to be read partly or silently misread is refused:
  malformed elements and dense arrays, a `Blob` with two payloads or a wrong
  `raw_size`, zero-length frames, truncated or malformed OSC (including an
  unreadable member `role`, elements outside the change-file hierarchy, and
  malformed children of `<delete>` objects in every OSC command), and an
  OSMHeader blob anywhere but first.
- `inspect tags` needs indexdata only with `--type` or with expressions that
  leave an element type out; `tags-filter -R` no longer needs it when its
  expressions cover every element type.
- `add-locations-to-ways` and `apply-changes --locations-on-ways` fail on an
  unresolvable way node instead of writing `(0, 0)`;
  `--ignore-missing-nodes` restores the old behaviour.
- `apply-changes` and `merge-changes --simplify` refuse a version regression;
  `--allow-version-regression` opts out.
- `tags-filter` and `inspect tags` expressions follow osmium's matcher (`*`
  prefix or substring match, comma alternatives, whitespace trimmed), which
  changes what some expressions select.
- `inspect tags` counts as `osmium tags-count`: with no expression or a
  key-only one (`highway`) it prints key rows of two fields; `=` expressions
  count pairs as before. For the old output pass `'*=*'` or `'highway=*'`.
- `time-filter` refuses input without timestamps.
- `check --refs --show-ids` lists at most `--max-errors` missing references
  (default `100`, `0` for no limit); the count stays exact.
- `degrade --block-cap` is now `--elements-per-blob`, accepted only with a
  flag that re-encodes.
- `extract` complete-ways and `--smart` also write every relation that has
  an included relation as a member, recursively, as osmium does (a route
  brings its route master). On a bbox they now match osmium's element set
  exactly; `--smart` decides on a relation's first `type` tag.
- `extract` and `export --bbox` edges are inclusive and rounded half away
  from zero to a decimicrodegree, as in osmium, instead of widened outward;
  `--config` edges agree with `-b`.
- `extract` polygons use osmium's integer membership rule; the old test
  dropped about half the boundary nodes, and on OSM boundaries the output
  now matches osmium. A polygon crossing the antimeridian that circles a
  pole or spans 360 degrees is refused.
- A relative `"directory"` in `extract --config` resolves against the config
  file's directory.
- `diff --format osc` exits `1` when the inputs differ; `diff -v` prints
  positioned edit scripts with quoted, escaped keys, values and roles.
- `inspect` text output quotes and escapes file text; `inspect --json` is
  `schema_version` 2 (`ordering.sequence` replaced by `segments_head`).
- `apply-changes` no longer copies the base's replication state, and
  `renumber` writes none.
- The writing program is `pbfhogg/<version>`.
- `renumber` moves orphan references to `start + count + old_id` instead of
  keeping an old ID that may name another element.
- `removeid` and `sort` summaries count only the elements they decoded.
- `cat --clean` writes elements in input order and no longer needs indexdata.
- `build-geocode-index` writes index format 5 (rebuild existing indexes),
  splits `--force` into `--overwrite` and `--allow-unindexed`, caps
  `--admin-level` at 10, `--street-level` / `--coarse-level` at 19 and the
  search radii at a fixed number of cell widths; an index exceeding that no
  longer opens.
- Indexdata is format v3; earlier releases read such files as unindexed.
- `add-locations-to-ways` now defaults to `--index-type auto` (library:
  `IndexType::default()` is `Auto`). Auto picks external only for sorted,
  indexed input whose estimated node store exceeds 80 % of the available
  page-cache budget, and otherwise runs sparse, the previous default. When
  available memory cannot be determined (every non-Linux host, containers
  without a visible cgroup v2 mount) or is below the sparse run's fixed
  3 GiB headroom, auto now keeps sparse and skips its estimate, instead of
  choosing external, so a small file no longer picks up external's
  scratch-disk demand. Every default run prints an `auto-selected index
  type` note. Pass `--index-type sparse` for the previous behaviour.

**Library**

- MSRV is 1.99 (library and CLI), up from 1.96.
- Commands take an explicit `&Host` (process actions, resources,
  diagnostics, page-cache policy, executor, invocation); the library never
  mutates process state, reads the environment or writes to stderr
  (ADR-0016). It also sets no allocator policy; call
  `process::cap_malloc_arenas` or `process::set_mmap_threshold` yourself if
  you want one.
- Command outputs are an `Output`, `OutputDir` or `OutputSet`, staged and
  published only on success. Scratch-using commands take a `RunScratch`.
- `BoxResult` is gone: fallible functions return `pbfhogg::Result`, with
  `ErrorKind`, file / blob / offset location, and `Error::report`.
- Write entry points take `WriteOptions` with an `OutputBackend`; job counts
  are a `Parallelism`. `PbfWriter::to_path*` take an `Executor`,
  `write_raw_copy` takes an `&InputFile`, `into_inner` returns a `Result`.
- `pbfhogg::ElementKind` replaces `OscElementKind`, `ElemKindTag`,
  `DefaultType` and `ShowElementType`.
- Report types write into an `io::Write` instead of stdout.
- `BlobReader` and `ElementReader` evict the page cache only when asked
  (`evict_page_cache(true)`).
- `merge_changes::merge_changes` takes a `MergeMode` (`Stream { scratch }`
  or `Simplify { allow_version_regression }`) in place of `simplify: bool`.
- Removed: `IndexedReader`, `IdRanges`, `IdSet::count_below` and
  `count_in_range`, `BlockBuilder::with_element_cap`,
  `HeaderBuilder::from_header`, `PrimitiveBlock::new`,
  `inspect::show_element`, `DiffOptions::summary`,
  `MergeStats::blobs_skip_decompress` and `blobs_scan_only`.
- Signature changes: element caps are `NonZeroUsize`; `inspect` takes
  `InspectOptions`, `check_refs` takes `CheckRefsOptions` (`show_ids:
  Option<ShowIds { max_errors }>`), `check` calls return `CheckOutcome`;
  `derive_changes` takes `timestamp: Option<u64>`; type filters are a
  `TypeFilter`; `HeaderOverrides` fields are `FieldOverride`;
  `HeaderBuilder::optional_feature` takes `impl Into<Feature>`;
  `tags_filter::osc::tags_filter_osc` takes a `Parallelism`.
- `BlockBuilder` refuses (and poisons itself on) an element it cannot encode
  correctly instead of panicking or writing it wrong.
- `extract::Region::Polygon` holds a prepared `extract::Polygon` built by
  `Polygon::new` from `PolygonRings`; the `polygons` and `bbox` fields are
  gone (`Polygon::bbox` replaces the latter). `Region::contains` and
  `parse_bbox` quantize to decimicrodegrees inclusively, matching `extract`.
- `tags_count::tags_count` counts keys or pairs as `inspect tags` does and
  returns `TagCounts { rows, stats }`; `TagCount::value` is an
  `Option<String>` (`None` for a key row). It and `tags_filter::tags_filter`
  refuse unindexed input only where the new `requires_indexdata` methods on
  their options say so.
- Mode-only settings moved inside their mode, so ignored combinations can no
  longer be written: `TagsFilterOptions::add_referenced:
  Option<AddReferenced { remove_tags, parallelism }>` (replaces
  `omit_referenced`, `remove_tags`, `jobs`), `GetidOptions::add_referenced:
  Option<AddReferenced { remove_tags }>`,
  `MergeOptions::locations_on_ways: Option<LocationsOnWays {
  ignore_missing_nodes }>`, and `DegradeFlags::reencode: Option<Reencode {
  block_cap, unsort, strip_locations, drop_ids }>` with `unsort` an
  `Option<Unsort>` (`CrossBlob` / `IntraBlob`); `degrade::degrade` no longer
  takes a `block_cap`.

### Added

- `build-geocode-index` indexes `addr:place` addresses and interpolation
  ways between them, and steps `addr:interpolation` digits `1` to `9`. The
  reader's `street` fields on `AddressMatch`, `InterpolationMatch` and
  `InterpolationCandidate` become `Option<&str>` beside a new `place`; an
  address with an empty `addr:street` and no place is no longer indexed.
- `digest`: an encoding-independent digest of a PBF's header and elements
  (`xxh128 v2`; `--form content` gives the element-only `v1`).
- `check --index` verifies blob indexdata and tagdata; `check --locations`
  verifies a LocationsOnWays file's inline coordinates.
- `cat --reindex` rewrites false indexdata and tagdata losslessly;
  `--distrust-metadata` on `sort`, `tags-filter` and `inspect` ignores false
  sort claims and indexdata.
- `degrade --corrupt-tagdata`, `--corrupt-indexdata` and
  `--corrupt-indexdata-kind` write well-formed but false hints.
- A global `-q, --quiet`.
- `--tmp-dir` on `add-locations-to-ways`, `diff --format osc`,
  `build-geocode-index` and `merge-changes`; `--staging-dir` on every command
  that writes an output.
- Ctrl-C and SIGTERM stop a run cleanly, removing its staging and scratch;
  leftovers of unfinished runs are reported (never removed).
- `getid` and `getparents` accept `node/123`-style IDs.
- `apply-changes` and `tags-filter --input-kind osc` accept uncompressed
  `.osc`; `apply-changes -j 1` runs, and `tags-filter --input-kind osc`
  accepts `-j`.
- Builds against musl. Contributed by Steffen Trog (#2).
- Notes when a run falls back from `copy_file_range`, loses parallelism or
  blob skipping, when `cat` rebuilds or omits a block, and when a `--force`
  run reads input without indexdata.
- Library: `Invocation::cancel` and `Invocation::seal`; per-invocation
  counters and sidecar streams (`MarkerFifo::call`); `host::FdPool`;
  `PinnedInput`; `PbfWriter::to_path_sync`; `BlobReader::leading_header` and
  `new_named`; `tags_checked`; `diff::diff_stats`; `inspect::inspect_header`.

### Changed

- `getid --add-referenced` follows relation members recursively, then the
  nodes of every way requested or pulled in, as `osmium getid -r` does; it
  used to add only the nodes of requested ways. `--remove-tags` strips every
  pulled-in object that was not itself requested.
- `check --ids` judges type order on files without indexdata too: a file
  whose kinds are out of order now fails (exit `1`) where it used to pass.
- `apply-changes --locations-on-ways` refreshes the inline coordinates of
  ways the OSC does not touch, and refuses missing nodes before writing.
- `diff` runs one engine at every `-j`: a text diff writes no scratch and
  refuses `--tmp-dir`, and `-v`, blob skipping and `--direct-io` work at any
  `-j`. `diff --format osc` to `.osc.gz` changes gzip member boundaries
  (decompressed output unchanged).
- `merge-changes` takes `--tmp-dir` only with several inputs, without
  `--simplify` and at `-j` above `1`, and refuses it elsewhere (exit `64`).
  Its output has one gzip member and action section per batch (same
  changes, same order).
- `repack` writes the remainder of an input blob holding at least the cap as
  its own block instead of merging it with following smaller blobs.
- `inspect tags` ends with `K key rows and P key/value rows counted, R
  passed the count thresholds` on stderr in place of `N distinct tag
  values`.
- In `sort` and `cat --dedupe` overlaps, the earliest input's copy of a
  duplicate survives, and a blob claimed sorted that descends internally is
  refused instead of reordered.
- `cat --dedupe --direct-io` and `sort --direct-io` read their second pass
  `O_DIRECT` too, so the page cache stays clean; merging a file with itself
  is slower this way.
- Default Linux builds use `copy_file_range`, readahead hints and scratch
  eviction without the `linux-direct-io` or `linux-io-uring` feature, and
  fall back to userspace copies where the kernel refuses `copy_file_range`.
- A command that reads its input more than once pins it against renames and
  refuses a FIFO, socket or device.
- Plain `cat`, multi-region `extract --config` and `add-locations-to-ways
  --index-type external` check the open-file limit (and, for external,
  scratch space) up front instead of failing partway. `--index-type auto`
  respects cgroup memory limits.
- `--direct-io` and `--io-uring` use the alignment the device reports.
- Warnings start with `Warning:` and notes with `Note:`.
- A blob over the format's size limits is refused at write time
  (`OversizePart`); an oversize tag index is dropped instead.
- Geocoding handles the antimeridian, fills each missing result class from
  the coarse fallback, keeps more vertices for high admin levels, and builds
  reproducibly.

**Performance** (planet unless another dataset is named; on bygg, except
the first four entries, which were recorded without a host)

- `check --index` 494 s -> 29 s and `cat --reindex` 444 s -> 44 s (worker
  pool instead of one thread). Both accept pipe or FIFO input when
  `check --index` is run alone. The sequential read runs ahead within a
  bounded window, so a worker error can be delayed while the calling thread
  is blocked reading more input. Reporting it can require enough input to
  fill that window, EOF or a read error; merely writing more bytes need not
  unblock it. Cancellation does not interrupt a blocking read. Combining
  `check --index` with another check requires a regular file.
- Europe unindexed `sort --force` 180 s -> 30 s, with about 125 MB more
  peak memory; `cat --dedupe` scans unindexed blobs the same way.
- Cold header walks (`inspect`, `getid`, `tags-filter`, `extract` and others)
  read ahead on files of small blobs: europe cold `inspect` 46 s -> 10 s,
  denmark cold `inspect` 0.64 s -> 0.22 s. Planet `check --refs` takes
  about 20 s (`53360af4`).
- `digest` 107 s -> 63 s (`--form content` 60 s -> 53 s).
- `extract --complete` peak anon 4.4 GB -> 2.8 GB and `--smart` 8.1 GB ->
  2.9 GB on a Europe-sized box; `--simple` multi-extract of five regions
  9.8 GB -> 3.9 GB; multi-region `--config --simple` compresses in
  parallel (5 regions about 128 s).
- Two-pass `tags-filter` with no matched relation skips its way-dependency
  pass: `w/highway=primary` 70 s -> 56 s.
- `apply-changes` on a daily diff about 92 s at 1.9 GB peak anon
  (`--compression zstd:1`), and with `--locations-on-ways` about 264 s at
  3.0 GB. The summary's memory line now reads `OSC diff heap`.
- `cat`, `check`, `degrade`, `renumber`, `repack`, `time-filter` and
  `build-geocode-index` no longer cap glibc malloc arenas. `repack` about
  148 s and `degrade --strip-locations` about 153 s (from 164 s), peak anon
  1.7 GB -> 0.8 GB; `build-geocode-index` 177 s, `cat --clean` about 154 s,
  `time-filter` about 132 s.
- Text `diff` without `-c` of near-identical files no longer prints common
  lines on one core (denmark 1.3 s -> 0.65 s); base against base plus one
  daily diff 284 s at 1.8 GB. `diff --format osc` to `.osc.gz` deflates on
  every worker and keeps scratch compressed: planet against planet 47 days
  later 156 s at 1.25 GB.
- `merge-changes` encodes in parallel batches: seven daily diffs 3.7 s at
  123 MB peak anon, seven under `--simplify` 5.4 s at 12.0 GB.
- `tags-filter --input-kind osc` runs multi-threaded: a daily diff in about
  2.4 s.
- `cat --dedupe` runs on `sort`'s engine: planet merged with itself about
  206 s at 945 MiB peak anon (`53360af4`).
- `check --ids` about 17 s, `getparents` about 16 s, `inspect --nodes`
  about 19 s (`53360af4`).
  `check --ids --full` on an ordered planet peaks at about 450 MB instead of
  2.3 GB.
- `inspect`'s full-decode modes use every core: `--extended` about 27 s at
  0.3 GB. `inspect tags '*=*'` about 37 s.
- `extract` with a 9694-vertex polygon on denmark takes 0.35 s.

### Fixed

- `apply-changes` could silently write wrong tags, roles and user names into
  blobs rewritten after a wholly deleted blob on the same worker, from a
  valid base and diff.
- `apply-changes` refuses a rewritten blob with a string table index past
  its table, or a table not starting with the empty string, instead of
  copying it (where it could name an unrelated string, or panic).
- `apply-changes` no longer writes out-of-order creates, duplicated upserts
  or mislabelled tags, keeps non-default block framing, and with `--force`
  writes a wholly indexed output.
- Malformed blob tagdata (out of order, repeated key, trailing bytes) is
  treated as absent; `tags-filter`, `inspect tags` and filtered reads could
  silently skip a blob holding matches.
- Tagdata now lists an explicit empty key; on files written by earlier
  versions, `tags-filter` and `inspect tags` no longer skip blobs for an
  expression that can match the empty key. `add-locations-to-ways
  --index-type external` without `--keep-untagged-nodes` can still drop a
  node blob whose only key is empty on such files; `check --index` finds
  them and `cat --reindex` repairs.
- Commands that decode a blob refuse false indexdata instead of answering
  from it; indexdata and tagdata are generated exactly or not at all.
- `sort` and `cat --dedupe` no longer write unsorted output stamped sorted.
- `extract` complete-ways and `--smart` on sorted input with a bbox dropped
  every plain (non-dense) node and the ways and relations it matched.
- `extract` on unsorted input no longer drops ways and relations.
- A GeoJSON FeatureCollection contributes every polygonal feature.
- `add-locations-to-ways` no longer treats a node at `(0, 0)` as missing,
  and reads plain (non-dense) nodes.
- `add-locations-to-ways --index-type external` refuses input past 2^32 way
  blobs or about 2^40 way node refs (exit 2), where it silently wrote
  coordinates into the wrong ways. Highly compressible input reaches this
  far below planet size.
- `inspect --extended` metadata coverage counts fields as `osmium fileinfo
  -e` does (uid 0, changeset 0 and empty user are not available).
- `diff` handles negative IDs, `i64::MIN`, empty blocks and corrupt
  elements correctly.
- `renumber` refuses input it would renumber wrongly, and no longer hangs on
  an error.
- `check --ids` uses canonical OSM ID order, and `check` no longer panics on
  IDs past its bitsets.
- `tags-filter --input-kind osc` keeps element metadata and escapes XML;
  `merge-changes` keeps the metadata of an element without a version.
- A truncated zlib blob no longer hangs; decompression bombs are bounded.
- Output headers describe the output (no sorted flag on an unsorted
  extract, enrichment features kept, a multi-input `cat` bbox covering every
  input).
- A reader exiting early on piped output, or a closed stderr, no longer
  panics or aborts the run.
- `build-geocode-index` no longer deletes an input kept inside the index
  directory, and refuses an index its format cannot address.
- `build-geocode-index` lists streets, interpolation ways and admin polygons
  under every S2 cell they pass through; about one street segment in six
  missed a cell. Rebuild to gain the cells.
- Geocode queries search the whole fine and coarse radius instead of about
  one cell width (about one street answer in twelve over Germany was wrong
  or missing), and measure segments correctly near the poles. No rebuild
  needed. The reader refuses an index with out-of-order cell files.
- `build-geocode-index` takes interpolation end numbers only from addresses
  on the end nodes, and leaves unresolved a way with disagreeing ends, a
  non-integer number, wrong parity or an unknown `addr:interpolation`; it
  used to borrow a nearby house's number. Fewer ways resolve (Germany 7 of
  78). Rebuild for the full fix.
- `build-geocode-index` reads interpolation ways as OSM tags them (street
  from the end addresses when no `addr:street`, which was most of them),
  splits a way at interior address nodes, and stops treating a way with
  `addr:housenumber` as interpolation. Rebuild to gain this.
- `build-geocode-index` stores strings holding a NUL byte whole instead of
  truncating at the NUL.
- Concurrent library runs no longer share scratch.
- io_uring writers opened one after another no longer fail with ENOMEM.

### Dependencies

- `protohoggr` 0.4.0 to 0.5.0 (shorter encoding of large dense `uid` and
  `user_sid` deltas), `s2` 0.1.0 to 0.2.0, `zstd` 0.13 to 0.14, `hotpath`
  0.21.2 to 0.30.0, `quick-xml` 0.41.0 to 0.42.0, `libc` 0.2.186 to
  0.2.191; new: `xxhash-rust` 0.8, `hashbrown` 0.17.

## 0.5.0 - 2026-07-16

### Breaking changes

- `add_locations_to_ways` now takes `AltwOptions` instead of positional
  configuration arguments. Its CLI gains `--inject-prepass`, which emits the
  opt-in `pbfhogg.WayMembers-v1` and `pbfhogg.SharedNodePins-v1` metadata.
  The public read API adds `Blob::way_members`, `Way::shared_node_pins`, and
  `BlobReader::set_parse_waymembers` for those artifacts.

- **Minimum supported Rust version raised 1.87 → 1.96** (library and CLI). Required by the move to `if let` chains and `as_chunks` in the read and command paths.

### Added

- `export` streams tagged nodes and tagged enriched ways to newline-delimited
  GeoJSON or a wrapped FeatureCollection. It supports element and tag filters,
  property selection, vertex-based bounding boxes, OSM metadata properties,
  polygon area detection and winding correction, stdout or guarded file
  output, and rejects way export when `LocationsOnWays` is unavailable.

- `degrade --drop-ids <N:SEED>` deterministically removes exactly N elements
  from the output, selected by a global (hash, kind, id) ordering seeded by
  `SEED` (splitmix64-based, fully specified so selection is reproducible
  byte-for-byte across builds and hosts). Ways and relations that still
  reference a dropped id become dangling references, giving `check --refs`
  a well-defined, reproducible dangling-reference count to validate against.
  `N` is an exact count (not a rate); `N == 0` is rejected, and `N` greater
  than the input's element count is a hard error once the true count is
  known. Because dropping elements changes blob framing, `--drop-ids` always
  forces the decode path (it cannot ride the blob-level passthrough), and it
  composes with every other transformation flag (`--strip-locations`,
  `--strip-indexdata`, `--strip-tagdata`, `--unsort`, `--unsort-intra`).

- `degrade --strip-tagdata` clears the per-blob `BlobHeader.tagdata` tag key
  index on every OsmData blob, so `tags-filter` runs its no-hint fallback
  path against the output. Like `--strip-indexdata` it is a header-only
  passthrough that leaves the blob payload, sortedness, and `indexdata`
  intact (a tagdata-stripped file is still indexed), and it composes with
  every other transformation flag. On the passthrough path both
  `--strip-tagdata` and `--strip-indexdata` now preserve all other
  `BlobHeader` fields byte-for-byte - the untargeted hint keeps its exact
  original bytes (a v1 blob index stays v1 rather than being re-serialized
  to v2), and `pbfhogg.WayMembers-v1` and any unknown header fields pass
  through unchanged. Only the field the flag targets is cleared.

- `degrade --strip-bbox` clears the file-level `HeaderBlock.bbox` so the
  output declares no bounding box, for exercising `inspect`'s bbox handling
  and downstream/external-consumer tolerance of a file with no declared
  extent. It is entirely a header-level change - no OsmData blob is
  touched - and composes with every other transformation flag. On the
  passthrough path it is a header-only strip: `source`, a non-default
  `writingprogram`, custom optional features, replication metadata, and
  unknown/extension fields all survive byte-for-byte alongside the bbox
  removal.

- `multi-extract`'s node classification now prunes candidate regions with
  a CSR grid (3600x1800 cells of 0.1 degree over the region bounding
  boxes) instead of testing every node against every region. The grid
  engages only at 16 or more regions and only within a 256 MiB coverage
  budget; below the threshold, or over budget (e.g. many overlapping
  near-planet-wide regions), classification falls back to the prior
  linear scan unchanged. Output is byte-for-byte identical to the linear
  scan in every case - the grid only narrows which regions get tested
  per node, it never decides a match itself.

### Changed

- `add-locations-to-ways --index-type auto` is now scale-aware. It
  previously picked `external` for every sorted + indexed input, which
  misroutes small and medium inputs (denmark: sparse 5.8 s vs external
  12.3 s; north-america: sparse wins by 26 %) - external's fixed
  scratch round trips only pay off once the sparse store outgrows the
  page cache. Auto now estimates the sparse store from per-blob
  indexdata node counts (8 bytes per node) and picks `external` only
  when the input is sorted + indexed AND the estimate exceeds 80 % of
  the host's available RAM; the threshold is computed at runtime, so
  the same file can route differently on differently-sized hosts (that
  is the point). The routing decision and both numbers are printed to
  stderr. When the estimate is unavailable (missing indexdata mid-file,
  no `/proc/meminfo`), sorted + indexed inputs fall back to `external`
  as before. Explicit `--index-type sparse|external` is unaffected.

- `build-geocode-index`'s interpolation endpoint resolver replaces its
  transient hashmap spatial index with a sorted CSR built in parallel,
  and parallelizes endpoint resolution across interpolation ways.
  Single-run measurement (plantasjen): planet interp-resolve phase
  30.6 s -> 2.76 s (~11x); Europe ~15 s -> 1.29 s (~11.6x). The win is
  the CSR data-structure replacement, not the endpoint parallelism
  (92 ms at planet, negligible against the 2.64 s CSR build).
  Resolved-way counts are unchanged (Germany 71/78).

- `diff` now runs parallel by default: `-j` defaults to `0`
  (auto-pick from available cores) instead of `1` (sequential), for
  both `--format text` and `--format osc`. Measured at planet scale
  the parallel path is 9.5x faster on text (2134 s -> 227.5 s) and
  7.6x on osc, with byte-identical output. `-v/--verbose` diffs
  always take the sequential path (per-field detail lines are
  sequential-only), so verbose output is unchanged. Trade-off: the
  parallel path writes shard temp files next to the output (~30 GB
  text / ~45 GB osc XML at planet scale, removed on completion);
  pass `-j 1` to restore the scratch-free sequential path. The `-j`
  help text previously claimed parallelism was text-only; it applies
  to both formats.

- `repack` now preserves `LocationsOnWays`: when the input header declares
  the feature, the output re-advertises it and every inline way-node
  coordinate round-trips exactly, instead of being silently dropped. Inputs
  without the feature are unchanged (no coordinates, no flag). `repack` still
  drops `pbfhogg.WayMembers-v1` and `pbfhogg.SharedNodePins-v1` and now warns
  specifically about those two.

- Full-scan transforms now execute in the decode workers for `getid`
  `--add-referenced`, getparents FullScan, single-pass `tags-filter`, and the
  decode-all `add-locations-to-ways` fallback. On the 8k blob encoding,
  getid improved 7.7%, getparents 6.5%, and `tags-filter -R` 7.0%; getid's
  primary-input pass-2 peak RSS fell from 1.18 GB to 596 MB. The old
  64-block command batches and `PBFHOGG_CMD_BATCH_BYTES` override are gone.

- Classify-schedule commands (`check-refs`, `tags-filter` two-pass, and
  the other `parallel_classify_*` consumers) now issue a bounded
  `POSIX_FADV_WILLNEED` prefetch over the blob-body ranges the scan is
  about to read, in a 256 MiB sliding window. This reclaims the
  page-warming the 2026-04-20 `POSIX_FADV_RANDOM` header-walk swap gave
  up at mid-size scale: europe `check-refs` -6.2 %, `tags-filter`
  -5.7 %. Planet is not over-prefetched; the window stays bounded to
  what workers are about to consume.

- `add-locations-to-ways --index-type sparse` advises the coordinate
  store `MADV_RANDOM` once pass 1 has populated it, so pass 2's
  genuinely-random lookups stop paying kernel readahead speculation
  (~43 KB of disk read per major fault drops to ~4.8 KB). At europe
  scale, where the store straddles the page-cache budget, the matched
  A/B measured pass-2 disk read -57 % (600 GB to 260 GB) and total
  wall -6.5 %. The win grows with cache pressure up to a point but
  inverts under severe external memory pressure, where the
  no-readahead trade multiplies synchronous fault round-trips; inputs
  whose store fits comfortably in RAM are unaffected either way.

- The sequential read path (`ElementReader::for_each`, `Blob::decode`,
  `Blob::to_primitiveblock`, `IndexedReader` iteration) no longer pays a
  whole-buffer copy per decoded block: decompression now lands directly in
  the block's owned buffer instead of being copied out of a temporary.
  Two behavior notes ride along. `for_each` now skips non-OSMData blobs
  without decoding them, matching the pipelined path - a malformed
  *repeated* header blob mid-file no longer surfaces a decode error on
  either path. And a Raw (uncompressed) blob of exactly 32 MiB is now
  accepted uniformly; previously the pipelined path rejected the exact
  boundary while sequential accepted it.

- `getparents` and `getid` include mode now select between the pread
  header walk and a full-file scan from a bounded OSMData blob-count
  estimate. This preserves the walker's win on low-blob-count planet
  encodings while recovering full-scan performance on high-blob-count
  encodings such as Geofabrik extracts.

- The pipelined reader (`for_each_pipelined`,
  `for_each_block_pipelined`, `into_blocks_pipelined`) now bounds
  decode-in-flight memory at the `decode_ahead` knob (default 32 blocks).
  Previously the decode stage admitted the entire file at disk
  rate, so decoded-block memory grew with file size (21.5 GB peak observed
  on a 19 GB input). Dropping `PipelinedBlocks` early, or returning an
  error from the block closure, now stops the pipeline within about
  `decode_ahead` blobs instead of reading and decompressing the rest of the
  file in the background.

### Fixed

- **`extract --config` and GeoJSON polygon coordinates now parse floats
  identically to CLI `--bbox` strings.** serde_json's default float
  parsing is best-effort and can land 1 ULP off the correctly-rounded
  value on high-precision inputs (e.g. `12.639999999999999` parsed as
  the f64 of `12.64`); after the floor/ceil decimicrodegree conversion
  that ULP becomes a whole coordinate step, so the byte-identical bbox
  could include a boundary node when passed via `-b` but exclude it via
  `--config` or a GeoJSON file. Found as a persistent 1-node
  multi-extract-vs-sequential mismatch at a denmark strip boundary
  (node 5446477279 at longitude 12.6399999, sitting exactly on a
  `12.4 + 4*0.06` strip edge). Fixed by enabling serde_json's
  `float_roundtrip` feature; the parse-cost increase only touches
  config/GeoJSON files, not PBF data.

- **`degrade`'s header-only passthrough no longer silently drops OSMHeader
  fields.** With no `--generator`/`--output-header` override,
  `--strip-indexdata` and `--strip-tagdata` previously rebuilt the output
  header through `HeaderBuilder::from_header`, which drops `source`,
  custom optional features, and unknown/extension fields, and resets a
  non-default `writingprogram` to `pbfhogg` - none of which those flags
  were meant to touch. The passthrough path now forwards the input
  `HeaderBlock` payload verbatim (field-identical; the outer Blob envelope
  is still re-compressed) via a surgical wire-level field stripper, so
  only the field(s) the active strip flags actually target change. The
  lossy `HeaderBuilder` rebuild still runs when `--generator`/
  `--output-header` is passed, since that is an explicit request to
  rewrite the header.

- **Reverse geocoding no longer attributes near-boundary points to the
  wrong admin region via the interior-cell hint.** The admin-cell "interior"
  bit is a builder-emitted hint (a cell whose S2 center sampled inside the
  polygon); `search_admin_ranked` and `search_admin_all` in
  `src/geocode_index/reader.rs` treated it as a point-in-polygon bypass
  (`is_interior || admin_polygon_contains(...)`), so a query point just
  outside a boundary - but adjacent to a genuinely interior-flagged
  neighbor cell - was accepted into that region without ever being tested
  against its geometry. In ranked mode this could even prefer the
  wrong-side polygon over the correct one when it happened to have a
  smaller area. The reader now always runs the point-in-polygon test; the
  interior bit is read but no longer bypasses it. Only queries within one
  admin-cell width of a boundary (~8 km at the default admin cell level)
  were affected.

- **`repack` no longer emits non-monotonic elements across blob
  boundaries.** When the target cap was smaller than the input's per-blob
  element count (a shrink), the merge thread wrote worker "full" blocks
  directly while routing the central builder's coalesced tail-blocks
  through a delayed buffer, so an earlier input blob's lower-ID tail could
  land after a later blob's higher-ID full block. The output then violated
  the `Sort.Type_then_ID` ordering its own header still advertised, which
  broke downstream consumers that trust the sorted flag (sort fast path,
  streaming diff) - and `--as-snapshot` promoted the mis-ordered file into
  the dataset graph. The merge thread now drains the central stream before
  each run of direct full-block writes, restoring global order. Pure grows
  (cap larger than every input blob) were never affected and their
  cross-blob coalescing is unchanged. Trade: on a coalescing shrink the
  output blob count now depends on the input-blob boundaries rather than
  being exactly `ceil(elements / cap)` - each input blob whose element
  count is not a multiple of the cap emits its tail as its own
  possibly-under-cap block instead of packing tails across boundaries.

- **Oversized declared `BlobHeader.datasize` is now rejected before
  allocation.** The read path capped only the *decompressed* blob content
  (32 MiB), so a hostile or corrupt `datasize` could drive the
  pre-decompression compressed-body allocation to an arbitrary size ahead of
  that guard. A new 32 MiB cap (`MAX_BLOB_DATASIZE`) rejects such input with
  the typed `BlobError::DataSizeTooBig` at every read-side site (`BlobReader`,
  `read_raw_frame`, `read_blob_header_only`, `HeaderWalker`). The value is not
  a written format limit - the spec caps only the uncompressed block (32 MiB)
  and the `BlobHeader` (64 KiB) - but the de facto interoperability bound: the
  reference reader (OSM-binary) applies a single 32 MiB `MAX_BODY_SIZE`
  directly to `datasize` and rejects anything at or above it, so mirroring it
  keeps pbfhogg-accepted files readable by the reference implementation.
  Well-formed files are unaffected - only adversarial declared sizes error.

- **`ElementReader::par_map_reduce` no longer buffers the whole file in
  memory.** It previously collected every compressed blob before starting
  parallel decode, requiring RAM proportional to file size (OOM on
  planet-scale inputs on 32 GB hosts). It now streams: a byte-and-count
  bounded pump feeds long-lived decode workers that fold in place, capping
  reader memory at a few hundred MB regardless of input size. Planet
  measurements: 86-98 GB inputs complete in ~54-58 s at 170-616 MB peak
  RSS (previously killed by the OOM reaper). Same signature and semantics;
  parallel reduction grouping may differ (already documented as
  order-unspecified). The `bench-read` blobreader arm also now uses the
  library's 256 KiB buffered reader instead of an 8 KiB `BufReader`.

- **`sort` no longer passes through blobs that are internally out of
  order.** A blob whose elements were internally unsorted but whose
  `(min_id, max_id)` range did not overlap its neighbours slipped past the
  blob-range overlap check: `sort` emitted a byte-identical copy stamped
  `Sort.Type_then_ID`, silently corrupting the sorted invariant. Pass 1 now
  checks intra-blob monotonicity while scanning element IDs and routes any
  internally out-of-order blob into the decode + re-encode path, so the
  output is genuinely sorted. The check covers every input whose header
  does not declare `Sort.Type_then_ID` - including indexed inputs, since
  blob indexdata alone is not proof of internal order (an unsorted file
  piped through `cat` gains indexdata without being reordered). Behavior
  change for indexed inputs without the sorted claim: pass 1 now decodes
  blob payloads to verify order (a one-line stderr notice says so).
  Declared-sorted inputs keep the header-only fast path; a header that
  claims sortedness over internally unsorted blobs violates its own
  contract and remains undetected - see CORRECTNESS.md.
- **OSC output no longer corrupts multi-line tag values.** All OSC-emitting
  paths (`diff --format osc` / derive-changes, `merge-changes`,
  `tags-filter --input-kind osc`) wrote raw newline/tab/CR characters
  inside XML attribute values. XML attribute-value normalization replaces
  those with spaces at parse time, so applying a pbfhogg-derived OSC
  silently turned multi-line tag values (e.g. `inscription`) into
  space-separated ones. They are now escaped as character references
  (`&#10;` etc.), matching osmium's OSC writer. Affected attributes: tag
  keys/values, relation member roles, user names.
- **apply-changes now carries OSC element metadata into its output.**
  The OSC parser never read `version`/`timestamp`/`changeset`/`uid`/`user`
  attributes, so every OSC-sourced created or modified element in merged
  output silently carried version 0 and no timestamp (osmium's
  applychanges preserves them). The overlay now stores a metadata block
  per element and all nine apply-changes write paths pass it through.
  Elements whose OSC carries no metadata attributes are written without
  metadata, as before.
- **`diff --format osc` now emits full element metadata** (timestamp,
  changeset, uid, user - previously version only), matching osmium's OSC
  writer and making the derive -> apply roundtrip metadata-lossless
  end-to-end. Attributes absent from the source element are omitted.
- **`degrade --unsort` now produces the documented cross-blob overlap.**
  It previously emitted an intra-blob inversion with non-overlapping blob
  ranges, so `sort`'s overlap-rewrite path never fired on the output.
  `--unsort` now packs output blobs continuously to the cap so the swap
  straddles a real blob boundary; the two adjacent blobs' ID ranges
  overlap as intended. The former intra-blob shape is available as the
  new `--unsort-intra` flag (mutually exclusive with `--unsort`), which
  keys its swap to the first two same-kind elements so it stays
  intra-blob regardless of input blob size. `--unsort-intra` requires
  `--block-cap >= 2`; a cap of 1 is now a hard error there instead of a
  silent no-op that still cleared `Sort.Type_then_ID`.

### Dependencies

- `hotpath` 0.17.0 → 0.21.1, `quick-xml` 0.40.1 → 0.41.0, `io-uring` 0.7.12 → 0.7.13, `memmap2` → 0.9.11, `rustc-hash` → 2.1.3.
- **`s2` 0.0.13 → 0.1.0** (optional, behind `geocode-reader`/`commands`). Pinned with `default-features = false, features = ["float_extras"]` so the geocode path no longer pulls s2's `serde` default.

## 0.4.1 - 2026-06-22

### Dependencies

- **`flate2` 1.0 → 1.1.9, pulling `zlib-rs` 0.6.4** (the pure-Rust zlib backend used for all blob (de)compression). This is the headline change for the release.
- `hotpath` 0.15.0 → 0.17.0, `quick-xml` 0.39.2 → 0.40.1, `libc` 0.2.185 → 0.2.186, `bytes` → 1.12.0, `serde_json` → 1.0.150. The `quick-xml` minor bump deprecated `Attribute::unescape_value`; OSC attribute parsing migrated to `normalized_value(XmlVersion::Implicit1_0)` (same behavior).

## 0.4.0 - 2026-05-09

### Breaking changes

- **`add-locations-to-ways --index-type dense` removed.** Sparse beats dense at every measured scale (japan 51.6s → 11.9s, europe dense OOMed) with no regime where dense won. The flag now errors pointing at `--index-type sparse`. Default `--index-type` changed from `dense` to `sparse`. `--index-type auto` now resolves to `external if sorted+indexed, sparse otherwise` (was: dense fallback).

### Commands

- **add-locations-to-ways** sparse: rewritten as a rank-indexed flat layout with parallel pass 1 and wire-format reframe in pass 2. Japan 20.9s → 11.9s (-43%); europe OOM → 5m59s (now competitive with external). Temp disk shrinks 2.4-2.8× (japan 5.7 GB → 2.0 GB; europe 52 GB → 29 GB). The strictly-increasing-id precondition is gone.
- **merge-changes**: parallel-drain via multi-member gzip output. Planet 7-OSC `--osc-range 4914..4920`: **267s → 55s (5.0×)**; `--simplify`: **262s → 74s (3.6×)**. 1-OSC fast path unchanged. Output is within 1% of the serial path's bytes; OSC consumers (osmium, osmosis, gzip CLI, `MultiGzDecoder`) all accept multi-member gzip. New `-j/--jobs N` caps the worker pool.
- **repack**: new command. Re-encode a PBF with a configurable `--elements-per-blob N` cap. Tags, refs, members, metadata, and DenseNodes encoding round-trip; output is type-sorted and propagates `Sort.Type_then_ID`. Primary use case: producing same-corpus-different-encoding pairs for blob-density measurement.
- **degrade**: new command. Produce a valid-but-adversarial PBF for benchmarking non-optimal code paths. Three composable transformations: `--unsort` (clear `Sort.Type_then_ID` and overlap adjacent same-kind blobs), `--strip-locations` (drop `LocationsOnWays`), `--strip-indexdata` (clear `BlobHeader.indexdata` on every OsmData blob). At least one required; `--strip-indexdata` alone is a blob-level passthrough (payload bit-identical). Planet-safe in all combinations.
- **time-filter** (snapshot path): now planet-safe. Planet 5× SIGKILL → 4m30s, 812 MB peak anon (was ~28 GB at kill). Europe peak anon 16.9 GB → 324 MB (-98%); europe wall regresses 1m32s → 2m27s - the cost of bounding cross-thread retention.
- **tags-filter** `--invert-match`: planet-safe. `-i w/highway=primary` peak anon 28.3 GB → 7.0 GB on planet; wall within noise (8m08s → 7m57s).

### Library API

- `BlockBuilder::with_element_cap(n)` constructor for callers that need
  a non-default per-block element cap. `BlockBuilder::new()` keeps the
  8000 default.

### Performance highlights

| Operation | Dataset | Time | vs 0.3.0 |
|-----------|---------|------|----------|
| merge-changes (7-OSC range) | Planet 87 GB | 55s | -79% (5.0×) |
| merge-changes `--simplify` (7-OSC range) | Planet 87 GB | 74s | -72% (3.6×) |
| time-filter (snapshot) | Planet 87 GB | 4m30s | OOM → planet-safe |
| tags-filter `-i` (way-deps phase) | Planet 87 GB | 7m57s | 28.3 GB → 7.0 GB peak anon |
| repack `--elements-per-blob 8000` | Planet 87 GB | 6m20s | new |
| degrade `--strip-indexdata` | Planet 87 GB | 79s | new |

## 0.3.0 - 2026-04-27

### Breaking changes

- **`pbfhogg diff --summary` renamed to `--osmium-summary`.** The flag flips the stderr stats line to osmium's `Summary: left=N right=N same=N different=N` format. Migration: replace `--summary` with `--osmium-summary`, or use the unchanged `-s` short form.
- **`renumber --mode` flag removed.** External join is the only implementation.
- **`BlobReader::new_seekable<R>` and `IndexedReader::new<R>` bounds widened** from `R: Read + Seek + Send` to `R: BlobReaderSource + Send`. Custom reader types add `impl BlobReaderSource for MyReader {}`.
- **Geocode `FORMAT_VERSION` bumped to 2.** Indexes built with older versions must be rebuilt.
- **`IdSetDense` renamed to `IdSet`** and moved from `getid` to the top-level `pbfhogg::idset` module.
- **`pbfhogg::getid::IdSet` renamed to `pbfhogg::getid::ElementIds`.** Field accessors and method calls unchanged.
- **`pbfhogg::getid::removeid` adds `force: bool`** (positional, between `direct_io` and `overrides`). Pass `false` to require indexdata, `true` to permit non-indexed input.
- **`apply-changes --jobs 1` rejected.** Pass `--jobs 2` or omit `--jobs` for the default. The default's minimum is also bumped to 2 on low-core hosts.

### Commands

- **renumber**: complete rewrite to an external-join architecture. Planet 3m25s, 3.3 GB peak RSS, zero temp disk. Negative input IDs are rejected. Orphan refs (way refs / relation members absent from the input) are counted and surfaced in the summary.
- **apply-changes**: new descriptor-first streaming pipeline and parallel-pwrite writer backend (now the default). `-j/--jobs N` worker-count override. Planet daily diff 762s → 81s with the default backend; 135s with `--compression none`. `io_uring` and `--direct-io` remain opt-in.
- **build-geocode-index**: parallel Pass 2 / Pass 3. Planet 21m → 7m (-65%). Pass 1.5 peak anon 29.5 GB → 3.0 GB.
- **build-geocode-index**: now hard-errors on three on-disk-format overflow classes (admin-vertex byte offset, admin-cell entries offset, per-cell / per-way `u16` counts) instead of silently producing wrong output. Error names the offending field and points at the `FORMAT_VERSION` bump path.
- **check --refs**: parallel three-phase scan. Planet 1,225s → 70s (17.5×); Europe 426s → 34s (12.7×). Peak RSS 2.17 GB.
- **check --ids**: parallel three-phase scan in both modes. Streaming (default) was previously OOM-killed at planet; now planet-safe at 57s wall, 504 MB peak. `--full` Europe 313s → 53s (5.9×). The element-level type-order check on non-indexed input is dropped (matches the existing `--full` semantics; PBFs with indexdata get the offset-based check from both modes).
- **cat --clean** / **cat --type X --clean**: parallel three-phase scan per kind, replacing a shape that was OOM-killed at planet. Now planet-safe at 5m48s wall, 835 MB peak. Output is type-sorted (nodes, ways, relations); already-sorted input keeps its structure, unsorted input is re-sorted, mixed-type blobs are split.
- **diff** / **diff --format osc**: new `-j/--jobs N` for shard-parallel merge. Planet text 35m → 3m30s at `-j 16` (10.2×); `--format osc` 37m → 5m13s (7.1×). `-j 1` (default) keeps the sequential path.
- **getid** include mode: header-walk fast path. Planet 44s → 7s (6.2×); disk read 88 GB → 636 MB.
- **inspect** default metadata: header-walk fast path. Planet 21s → 6.5s.
- **inspect --nodes** / **inspect --tags**: now parallel, with `-j/--jobs N`. Planet `--nodes -j 16` 57s; `--tags -j 16` 2m50s. Germany `--nodes -j 8` 18.5s → 3.6s.
- **add-locations-to-ways `--index-type external`**: rewrite plus a follow-up rankless reshape. Planet 24m → 10m4s (-58%); Europe 4m52s → 4m31s. The `IdSet` rank machinery is no longer used by this command.
- **extract** (all strategies): pass-1 blob schedule reused across subsequent passes. Europe smart 254s → 181s (-29%).
- **getparents**: skip node-only blobs via `BlobFilter` when the query doesn't need nodes (~85% of blobs at planet scale).
- **derive-changes**: streams output to temp files; constant memory regardless of diff size.
- **tags-filter**: new `-j/--jobs N` on the two-pass path.

### Bug fixes

- **Negative input IDs are now rejected at the input boundary** in `renumber` (every `IdSet` entry point) and in `getid` (parse-time, matching osmium's getid). Previously `renumber` could panic or produce phantom orphans, and `getid` silently dropped negative-id queries with a misleading `"no IDs specified"` message. See `DEVIATIONS.md > "Negative input IDs rejected project-wide"` for the full project-wide stance.
- **Truncated and adversarial PBF inputs now hard-error** instead of silently producing partial output. The contract covers `BlobReader::next`, `BlobReader::skip_blob_body`, `HeaderWalker::next_header`, and `FileReader::skip` (the caller-side payload-skip used by `has_indexdata`, `diff`, `cat --dedupe`, and `altw::passthrough`). Adversarial blob-header length prefixes can no longer trigger multi-GB allocations or process aborts. Truncated headers no longer produce past-EOF schedule entries that fail later inside pread workers. See `reference/truncation-handling.md`.
- **Multiple read sites** in `renumber`, `apply-changes`, and the ALTW external pipeline now reject malformed input at the boundary instead of panicking or scrambling output: truncated relation `memids` varints, DenseNodes blocks whose `granularity * offset` would overflow `i64`, and ALTW node blobs whose indexdata advertises `max_id < min_id`.
- **`apply-changes --io-uring`** on indexed input produced structurally-broken PBFs on the `CopyRange` fast-path. Fixed.
- **`apply-changes` (and `merge`)** now require a base PBF with `Sort.Type_then_ID` in the header regardless of `--locations-on-ways`. The general path previously accepted unsorted headers and could silently drop upsert creates. Migration: re-sort with `pbfhogg sort`.
- **`apply-changes --force` on non-indexed input** could silently drop upsert modifications, or emit every remaining upsert of a kind as gap-creates before a single blob. Fixed.
- **`apply-changes --force --locations-on-ways` on a non-indexed PBF** silently stripped LocationsOnWays data from base ways. Now rejected up front with a migration hint.
- **`apply-changes` drain loop** could spin indefinitely if a worker thread panicked mid-stream. The loop now breaks on channel-disconnect; the worker's panic propagates with a clear diagnostic.
- **`cat --dedupe`** silently dropped elements at kind boundaries. Fixed.
- **`sort`** silently dropped elements at kind boundaries. Fixed.
- **`inspect --show`** returned "not found" on unsorted PBFs when the target element lived in a later blob with a smaller `min_id` than a preceding blob. Fixed.
- **`inspect --direct-io`** on the indexed-PBF header-only fast path silently ignored the flag (the path uses `posix_fadvise(POSIX_FADV_RANDOM)` for equivalent cache-avoidance). Now prints a one-line stderr notice; the full-decode fallback honours the flag as before.
- **`renumber`** no longer panics in `IdSet::set_atomic` when the header advertises `Sort.Type_then_ID` but the node blobs are out of order.
- **`renumber`** removes its partial output file on every error path.
- **`removeid`** (`getid --invert`) on a non-indexed PBF silently fell through to full decode + re-encode for every blob instead of using the (unreachable-without-indexdata) raw-passthrough fast path. Now errors up front unless `--force` is passed.
- **`add-locations-to-ways --index-type external`** could silently produce wrong coordinates on PBFs with loose blob indexdata. Now hard-errors naming the offending blob.
- **`add-locations-to-ways --index-type external --force` on a non-indexed PBF** failed with a confusing error deep inside the metadata scan. Now rejected up front with a migration hint.
- **`add-locations-to-ways --index-type external`** hit `EMFILE` on a default-ulimit shell. Fixed.
- **Geocode builder** could drop admin inner rings (holes) on multi-outer relations when aggressive Douglas-Peucker simplification on the outer ring shifted the simplified boundary past the hole's first vertex. Hole containment now tests against the original (unsimplified) outer polygon; rendered geometry on disk is unchanged.
- **Geocode builder Pass 3 Stage B** silently truncated trailing partial records when a Stage A bucket file write was interrupted (ENOSPC, SIGKILL during write). Now errors with a message naming the incomplete file length.
- **Geocode `simplify_ring`** Douglas-Peucker could exceed `max_vertices` due to metric divergence between cost and marking passes. Fixed.
- **Parallel and io_uring writers** could silently truncate output on upstream framer panic. Fixed.
- **Parallel-pwrite cross-device passthrough copy** could fail on a signal-interrupted `pread` instead of retrying. Fixed.
- **`IdSet::any_in_range` blob prefilter** silently dropped any blob whose indexdata range straddles zero (negative `min`, positive `max`). Production input never triggers this; mixed-sign input (JOSM-staged data, hand-crafted PBFs) lost legitimate `getid` and ALTW external matches. Fixed.
- **Scratch / temp file leaks** on error paths in parallel `diff`, parallel `derive-changes`, geocode Pass 3 buckets, and OSC assembly aggregates are now cleaned up by `PathGuard` RAII. Concurrent `pbfhogg` processes against the same scratch directory no longer collide on temp file names (PID + process-lifetime random tag in the path).
- **`BlobReader::seek_raw`** left iteration permanently stuck after a prior `next()` returned an error. A successful `seek_raw` now clears the sticky error flag.

### Library

- **`PbfWriter::write_primitive_block_no_indexdata`.** New public method that writes an `OSMData` blob without the `BlobHeader.indexdata` / `BlobHeader.tagdata` fields, for producing byte-for-byte fixtures matching third-party tools.

### Dependencies

- **`roaring` removed.** Both remaining consumers (`check --refs`, `check --ids --full`) migrated to `IdSet`.
- `protohoggr` 0.2.1 → 0.4.0.

### Performance highlights

| Operation | Dataset | Time | vs 0.2.0 |
|-----------|---------|------|----------|
| cat (indexdata generation) | Planet 87 GB | 86.5s | -83% (5.8×) |
| cat --clean | Planet 87 GB | 5m48s | OOM → planet-safe |
| apply-changes (daily diff, default backend) | Planet 87 GB | 81s | -89% (9.4×) |
| renumber | Planet 87 GB | 3m25s | new architecture |
| build-geocode-index | Planet 87 GB | 7m12s | -65% (2.9×) |
| check --refs | Planet 87 GB | 70s | -94% (17.5×) |
| check --ids (streaming) | Planet 87 GB | 57s | OOM → planet-safe |
| diff `-j 16` (text) | Planet 87 GB | 3m30s | -90% (10.2×) |
| diff `--format osc -j 16` | Planet 87 GB | 5m13s | -86% (7.1×) |
| getid (include mode) | Planet 87 GB | 7s | -86% (6.2×) |
| inspect (default) | Planet 87 GB | 6.5s | -70% (3.3×) |
| inspect --nodes `-j 16` | Planet 87 GB | 57s | new |
| inspect --tags `-j 16` | Planet 87 GB | 2m50s | new |
| add-locations-to-ways external | Planet 87 GB | 10m4s | -58% |
| extract --smart | Europe 35 GB | 3m01s | -29% |

## 0.2.0 - 2026-04-09

First public release.

### Commands

Full PBF processing pipeline validated at planet scale (87 GB, 11.6B elements, 30 GB host):

- **cat** - passthrough with indexdata generation, type filtering with raw frame passthrough. Planet: 497s buffered.
- **sort** - Sort.Type_then_ID ordering.
- **extract** - simple, complete-ways, and smart strategies. Parallel 3-phase classification via pread workers. Raw frame passthrough for fully-contained node blobs. Columnar dense node decode for bbox classification. Planet simple: ~100s.
- **multi-extract** - single-pass N-region extract with parallel decode workers. Denmark 5-region: 1.9s, Japan 5-region: 7.3s.
- **tags-filter** - two-pass with tag index filtering, parallel classification, relation closure with way/node dependency resolution.
- **getid** - ID-range blob skip, raw frame passthrough for `--invert`, `--add-referenced` with parallel way dependency scan.
- **add-locations-to-ways** - dense, sparse, and external join index types. External join: planet 1,462s (24.4 min), 3.9x faster than dense.
- **apply-changes** - 4-phase batch merge with passthrough coalescing. Planet daily diff: 762s, 1.8 GB RSS.
- **diff** - block-pair merge-join with compressed byte comparison (skip decode for unchanged blobs). Streaming constant-memory.
- **derive-changes** - OSC generation from two sorted PBFs.
- **merge** - merge-sort multiple PBFs.
- **inspect** - blob statistics, tag counting, `--show` for single element lookup by ID.
- **check** - reference integrity checking (`--refs`).
- **build-geocode-index** - 4-pass geocode index builder. Planet: 1,346s (22.4 min), 17.8 GB RSS.
- **renumber** - sequential ID renumbering.
- **time-filter** - timestamp-based element filtering.

### Library

- `ElementReader` - sequential, parallel (rayon), and pipelined iteration modes.
- `IndexedReader` - seekable reader with blob-level index for filtered queries.
- `PbfWriter` - sync, pipelined (rayon), O_DIRECT, and io_uring write modes.
- `BlockBuilder` - iterator-based tag API, dual-buffer single-pass encoding.
- `DenseNodeColumns` - columnar dense node decode for batch classification.
- `IdSetDense` - chunked sparse bitset with O(1) set/get, rank index, bitwise OR merge.
- `geocode_index::Reader` - reverse geocoding queries via S2 cell lookup (feature-gated).

### Architecture

- Pread-from-workers: parallel blob decode via `pread(2)` with shared file descriptor, eliminating cross-thread PrimitiveBlock retention.
- `parallel_classify_phase` / `parallel_classify_accumulate`: two-function API for planet-safe parallel classification. Per-blob streaming for dense paths, per-worker accumulation for sparse paths.
- Wire-format scanners (`node_scanner`, `way_scanner`): lightweight ID/coordinate extraction without PrimitiveBlock construction.
- Raw frame passthrough: skip decompress+recompress for fully-contained blobs.
- Blob-level indexdata (v2): element type, ID range, count, spatial bbox per blob.

### Performance highlights

| Operation | Dataset | Time |
|-----------|---------|------|
| Read (parallel) | North America 18.8 GB | 22s |
| cat (indexdata) | Planet 87 GB | 497s |
| add-locations-to-ways (external) | Planet | 1,462s |
| build-geocode-index | Planet | 1,346s |
| apply-changes (daily diff) | Planet | 762s |
| extract simple | Europe 35 GB | 113s |
| multi-extract 5-region | Japan 2.4 GB | 7.3s |

### License

Dual MIT/Apache-2.0.
