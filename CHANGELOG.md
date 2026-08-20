# Changelog

## Unreleased

### Breaking

- **`PrimitiveBlock::new(Bytes)` is removed; use
  `PrimitiveBlock::from_vec(Vec<u8>)`.** The parser extends the buffer in
  place with its inlined string-table and group entries, so a `Bytes`
  could only be handled by copying it first - a full decompressed-block
  memcpy, on the order of 1.4 MB per block, on what is otherwise a
  zero-copy path. Every internal caller had already been migrated off it;
  `new` remained as a public API whose only implementation was the slow
  path the internals were rewritten to avoid. `from_vec` is that path,
  and it is now public. Migration: decompress into a `Vec<u8>` and pass
  it by value instead of converting to `Bytes`.

### Added

- **`BlobReader::evict_page_cache(bool)` and
  `ElementReader::evict_page_cache(bool)`.** The buffered file
  constructors advise `POSIX_FADV_DONTNEED` over the consumed prefix
  after every blob, which is what keeps a sequential pass over a
  planet-sized file out of RSS - but the page cache is shared, so this
  silently leaves the file cold for anything that reads it next: a
  two-pass command, a test suite, an `osmium` running alongside. There
  was no opt-out short of switching to O_DIRECT, which is a different I/O
  mode entirely. The default is unchanged; pass `false` to leave the page
  cache alone.

  On `ElementReader` the builder can only govern the blobs it has not read
  yet: every file constructor decodes the header blob before returning, so
  a post-hoc `evict_page_cache(false)` spares the element blobs but not
  the header. `ElementReader::from_path_with_eviction(path, false)` makes
  the decision up front and is the one to use when the whole file's cache
  must be left untouched.

### Changed

- **`cat --dedupe` now bounds its frame-header allocation.** Its pass-1
  frame loop read the 4-byte big-endian length prefix and resized its
  header buffer straight from it, with neither the
  `MAX_BLOB_HEADER_SIZE` cap nor the zero-length rejection the other
  frame decoders carry. Four bytes of corrupt or hostile input therefore
  requested an allocation of up to 4 GiB before anything inspected them.
  Both guards now apply, so such input is a typed error. Every blob-frame
  decoder in the crate shares one implementation of the length prefix,
  the end-of-file policy and both size guards, so they can no longer
  drift apart.

- **A `Blob` carrying more than one payload field is rejected.** `raw`,
  `zlib_data` and `zstd_data` are three encodings of one payload, so a
  blob carrying two of them has no defined content - and the two Blob
  parsers disagreed about which one won, meaning one such blob decoded to
  two different blocks depending on which read path reached it. Both
  parsers now reject it rather than picking a winner.

  **Compatibility:** a file that read successfully before may now be
  rejected. Repeating a non-repeated protobuf field is legal wire
  encoding with last-one-wins semantics, so such a blob is unusual rather
  than provably corrupt. No known writer produces one - libosmium emits
  exactly one payload field per blob - but its *reader* does tolerate the
  shape, with a third policy again (first `raw` wins, otherwise the last
  compressed field), which is the argument for rejecting rather than for
  copying it. If you hit this, the file was not written by a conforming
  writer: re-fetch it, or re-encode it with a reader that accepts it. The
  error names the offending fields.

- **A `Blob.raw_size` that disagrees with the decompressed payload is
  rejected.** It was used only as a buffer-sizing hint, so a lying value
  was silently ignored; libosmium cross-checks the same field and now so
  do we. An absent `raw_size` remains legal.

  **Compatibility:** same shape of change - a file with a wrong
  `raw_size` used to read fine and now errors. We are stricter than
  libosmium in one direction: it sizes its output buffer to `raw_size`
  and lets zlib fail when the value is too small, but an oversized value
  succeeds there and silently yields a payload zero-padded to the
  declared length. Both directions are rejected here.

- **A corrupt element inside a blob is now a hard error instead of a
  silently dropped element.** This is a breaking behaviour change for
  anyone reading damaged or hostile PBFs. The rule that a truncation is a
  hard error rather than silent data loss was implemented at the frame
  layer and stopped at the blob boundary; below it, every element-level
  decode failure was swallowed. A malformed `Node`, `Way` or `Relation`
  was skipped, a malformed `DenseNodes` group of up to 8000 nodes became
  an empty iterator, a dense group whose `lat` array was shorter than its
  `id` array truncated the node run, a wire error ended the element
  stream mid-group, and a corrupt `Info` became default metadata
  indistinguishable from absent metadata. `pbfhogg cat corrupt.pbf -o
  out.pbf` therefore exited 0 having written fewer elements than it read.

  Every `PrimitiveBlock` is now structurally validated at construction:
  group framing, each element sub-message, each element's `Info`, every
  packed array on those elements, the parallel-array invariants their
  iterators zip on (`keys` against `vals`, a way's `lat`/`lon` against
  its `refs`, a relation's `roles_sid`/`memids`/`types`), the
  `DenseNodes` message, and the equal length of its packed `id`/`lat`/
  `lon` arrays. Any failure is an error from the read path, so the
  command exits non-zero. There is no lenient mode. Files that pbfhogg
  read "successfully" before may now fail - that is the point: the
  earlier success was reporting a subset of the data as if it were all of
  it.

  The two dense-node arrays that array-length equality could not express
  are now covered as well. `DenseNodes.keys_vals` is one flat array of
  zero-terminated per-node tag runs, so it is walked once per block and
  must describe exactly as many runs as there are ids: a short array used
  to leave the trailing nodes untagged and a long one used to discard its
  tail, both silently. Every array in `DenseNodes.denseinfo` must now be
  either absent or exactly as long as the id array - a short one used to
  make metadata vanish for every node from that point on. A `denseinfo`
  carrying only some of its five metadata arrays is rejected too, because
  it produces the same silent loss here.

  Two encodings stay legal, because both occur in the wild: an absent
  `keys_vals` (no node in the group has tags), and a final tag run that
  ends at the end of the array without its `0` terminator.

- **A `BlobHeader` frame declaring zero length is rejected.** Such a
  frame consumes four bytes and describes nothing, so a run of zero bytes
  used to read as a stream of empty blobs that every consumer skipped: a
  valid header followed by 64 zero bytes read as 16 extra blobs, exit 0,
  empty output. A `BlobHeader` carrying no `type` field is rejected for
  the same reason.

### Fixed

- **Packed repeated fields written in the unpacked wire form are no
  longer read as empty.** Protobuf allows a packed repeated field to be
  written either as a packed segment or as individually tagged varints,
  and libosmium accepts both for every packed field of `Node`, `Way`,
  `Relation`, `DenseNodes` and `DenseInfo`. Our parser matched only the
  packed segment; the unpacked form was skipped and the field decoded as
  empty. For `DenseNodes.keys_vals` that meant "no node in this block has
  tags" - a whole block's tags lost, with no error, from a spec-legal
  encoding that the reference implementation reads correctly. The
  unpacked form is now accepted for every packed field on the read path,
  with no copy.

  In the same pass, a packed field that appears twice with data in both
  occurrences is now a hard error naming the message and field. The spec
  says concatenate; this parser used to keep the last occurrence and drop
  the rest, silently, as libosmium still does. No known writer emits the
  shape, and the read path cannot express a concatenation without giving
  up the borrowed slices that make strict validation free, so it is
  rejected rather than silently truncated.

- **The last tag of a dense node survives a `keys_vals` run that ends
  without its delimiter.** The dense iterator derived the end of a tag
  run by subtracting one byte from its cursor, assuming the run always
  ended at a `0` delimiter. When the array ran out instead, that chopped
  the last byte off the final value varint and the tag was dropped with
  no signal.

- **A panicking pipeline thread no longer reads as a clean end of file.**
  `ElementReader::into_blocks_pipelined` delivers blocks over a channel
  whose only sender lives on the pipeline thread, so an unwind there
  closed the channel exactly as normal completion does - and the
  iterator's `Drop` joins and discards the panic. A crashed pipeline
  therefore looked like a file that had simply ended, with a zero exit
  code. The iterator now yields an error in that case.

- **A `BlobHeader.indexdata` of an unrecognised length is reported.** A
  length that is neither 26 nor 42 is still ignored - the blob reads as
  "no index" and consumers conservatively fall back to a full decode,
  which is the safe direction - but it was completely invisible, so a
  systematically corrupt index silently degraded every filtered read with
  no way to tell from the outside. One warning per process now says so.

- **A truncated or corrupt zlib blob no longer hangs.** A deflate stream
  that ended before its final block left the inflate loop consuming
  nothing, producing nothing and never reaching stream end - spinning at
  100 % CPU with no output, no timeout and no diagnostic, and presenting
  as a deadlock when it happened on a parallel decode worker. It is now a
  typed error naming the byte counts at the point of failure.

- **Decompression bombs are bounded during inflation.** The 32 MiB
  decompressed-content cap was only checked after decompression had run
  to completion, so a blob whose compressed size passed the pre-read
  guard could inflate to tens of GB (far more with zstd) and exhaust
  memory before the guard fired. Inflation now stops one byte past the
  cap for both zlib and zstd and reports `MessageTooBig`. Relatedly, the
  `Blob.raw_size` capacity hint - an unvalidated `int32` - is clamped to
  the same cap before it reaches `Vec::reserve`, where it could
  previously request 2 GiB.

- **A malformed dense block no longer panics `extract --strategy smart`
  or `multi-extract`.** The columnar decoder built its id, lat and lon
  arrays in three independent loops and then indexed lat and lon by the
  id count, so a dense group with a truncated or absent `lat` array
  panicked with index-out-of-bounds on untrusted input. Such a group is
  now rejected at block construction, and the columnar node count is the
  shortest of the three columns so positional indexing cannot go out of
  range regardless.

- **A failing run no longer destroys a pre-existing file that shares the
  output name.** Every command that writes a `--output` used to open the
  destination immediately, truncating it before the first byte of new
  output existed, and a mid-run error left a partial file under the name
  the user chose. Pointing `-o` at an unrelated file cost you that file.

  This now applies to *every* command with an output, not only `export`
  and `renumber`: `sort`, `cat` (both plain and `--dedupe`), `repack`,
  `degrade`, `renumber`, `tags-filter` (PBF and OSC), `export`, `diff`
  (text `-o` and `--format osc`), `getid`, `getparents`,
  `extract` (single and `--config` multi-extract),
  `add-locations-to-ways`, `time-filter`, `apply-changes` and
  `merge-changes`. Each writes to a scratch sibling in the destination's
  directory and renames it into place only once the run has succeeded, so
  the destination is untouched until then and is replaced by a single
  `rename` - atomic on Unix. The one exclusion is `bench-write`'s sibling
  `bench-merge`, a synthetic benchmark, where the rename would land in the
  region being measured.

  A multi-extract extracts all-or-nothing: no region is published unless
  every region succeeded. Publishing them is not a transaction, though -
  it is a sequence of renames, and there is no N-way atomic rename. Every
  destination is now checked immediately before the first one is
  published, so the ordinary reasons a rename fails (the destination is a
  directory, its parent has gone) refuse the whole publication with
  nothing written. Another process changing a destination between that
  check and the rename that reaches it can still leave a partial set.

  Three side effects worth knowing. While a run is in flight the output
  directory holds a dot-prefixed `.pbfhogg-tmp-*` file next to the
  destination, and a hard kill leaves it behind. A destination whose
  *directory* is not writable now fails, where overwriting an existing
  writable file in a read-only directory used to work. And the output
  does not appear at its final path until the run finishes, so anything
  polling for the file's existence to gauge progress will no longer see
  it grow.

- Errors from the blob framing layer now name the input file and the blob
  index, not only the byte offset: `... (in planet.osm.pbf, blob 4312, at
  byte offset 91827364)`. On `cat a.pbf b.pbf`, `apply-changes` and
  `merge-changes` an offset alone did not say which input failed. New
  accessors `Error::source_path()` and `Error::blob_index()` expose them;
  both are `None` for readers built from a bare `Read`, which have no
  path to report, and the blob index is only stamped while iteration is
  sequential from the stream start - a seek away from offset 0 drops it
  rather than reporting a count that no longer matches the position. Failing to open an input now names the path as well
  (`failed to open <path>: ...`) instead of a bare
  `No such file or directory (os error 2)`.

- A dense node whose metadata carries an out-of-range user string index no
  longer loses its entire metadata record. Version, timestamp, changeset,
  uid and visibility are preserved and only the user name falls back to
  empty, so a single bad string-table reference in an input no longer
  strips authorship from the surrounding node on the way through.

### Breaking changes

- Library: errors detected by the blob framing layer now carry the byte
  offset at which they were found, reachable via `Error::byte_offset()`
  and appended to the `Display` output as ` (at byte offset N)`. A
  wire-format or blob error from a 90 GB file previously said nothing
  about where. `ErrorKind` is unchanged; code matching on error *text*
  for these variants may need updating. For a truncated blob the offset
  is the exact byte that could not be read, not the start of the frame
  it sits in, and it is reported identically by the full-read path and
  by the header-only structural walk `next_header_skip_blob`. Offsets
  are present only for readers that know their stream position
  (`BlobReader::from_path` / `open` / `new_seekable`, and everything
  built on them - not `BlobReader::new`, which wraps an arbitrary
  reader).

- **Exit codes now distinguish "the answer is no" from "the command
  failed".** A command that ran to completion and answered negatively
  exits `1` - `diff` with differences, `inspect --indexed` on a
  non-indexed file, `inspect --show` on an absent element, `check` with
  integrity violations. Every failure path now exits `2` (matching what
  clap already returned for a usage error). Previously both meanings
  shared `1`, so `pbfhogg diff a.osm.pbf missing.osm.pbf` was
  indistinguishable from "the files differ". Scripts branching on `1`
  for the semantic answer, or on any non-zero for failure, are
  unaffected; a script reading `1` as "failed" must widen to `!= 0`.
  See the exit-code table in `reference/cli-reference.md`.

- `inspect` rejects the flag combinations it used to accept and discard.
  `--show` is now mutually exclusive with every other `inspect` flag
  (it returned early and threw the rest away), and `-j/--jobs` and
  `--force` require `--nodes`, the only mode that reads them.

- `diff` rejects an explicit `-j/--jobs` together with `-v/--verbose`.
  Verbose output is sequential-only, so the job count was accepted and
  ignored; `tags-filter` already rejected the equivalent `-R` + `-j`.

- Library: `cat::cat`, `diff::DiffOptions`, `check::verify_ids::
  VerifyIdsOptions` and `tags_count::TagCountOptions` take a typed
  `Option<TypeFilter>` instead of an unvalidated type string, and
  `VerifyIdsOptions` loses its lifetime parameter. `TypeFilter` is now
  public and implements `FromStr`, which rejects unknown types instead
  of quietly matching nothing.

- Library: `getid::GetidOptions` gains `report_found` and
  `getid::GetidStats` gains `found`.

- `ErrorKind::MissingFeature("LocationsOnWays")` no longer names CLI
  flags in its `Display` output; the `--type node` hint is added by the
  CLI. Same for the `dense` index-type parse error.

### Changed

- `getid --verbose-ids` no longer decodes the finished output PBF a
  second time to work out which requested IDs were missing. The found
  set is collected on the write path, so the diagnostic no longer
  roughly doubles the command's runtime at planet scale.

- Enumerated flag values are parsed once, at the argument boundary, into
  a typed value: `--compression`, `--type`, `--clean`, `--index-type`,
  `inspect tags --sort` and the `bench-*` modes. `--help` now lists the
  accepted values and shell completion offers them, and the error
  wording is consistent across commands. `inspect tags --sort` keeps
  `count` and `name` as aliases; `--index-type dense` keeps its
  migration pointer at `sparse` as a hidden value. `-t/--type` rejects
  an unknown or misspelled element type on every command that takes it,
  rather than letting it degrade into a filter that matches nothing.

- Commands that write a PBF now reject an output that aliases one of their
  inputs before opening any writer, rather than truncating the input and
  failing partway through. The check resolves symlinks and compares device
  and inode, so `pbfhogg sort in.osm.pbf -o link-to-in.osm.pbf` is caught,
  not just a literal repeat of the same path. `extract --config` additionally
  rejects two config slots whose outputs resolve to the same file; previously
  only lexically identical output paths were caught, so differently spelled
  paths and symlinked outputs opened two writers on one file and produced
  truncated, interleaved output.

- `--type` is now validated at parse time on `cat`, `diff`, `check` and
  `inspect tags`. An unrecognised element type is a usage error naming the
  offending value instead of a silently empty filter that produced an empty
  output file.

- `build-geocode-index` validates its configuration before doing any work:
  S2 levels must be in `0..=30`, the coarse level must be shallower than the
  street level, and the search radii must be finite and non-negative. Values
  outside those ranges previously panicked in debug or produced a silently
  wrong index in release.

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
