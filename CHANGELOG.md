# Changelog

## Unreleased

### Breaking

- **`apply-changes` no longer limits one base blob's re-cut output, and
  `ErrorKind::OverflowReplayBudget` is gone.** A base blob whose rewritten
  output overflows a block, or grows past 64 MiB, is now written a second
  time with its frames streamed to the output, so it is never held in
  memory whole. Many changed elements sharing a large string that each
  need a block of their own, or more than 256 MiB of large changed values
  in one blob, used to fail the run (exit `2`) and now apply. Library
  callers matching `ErrorKind::OverflowReplayBudget` must drop the arm;
  the harness's `overflow_replay_budget` kind is gone with it. An element
  too large for a block alone is still refused before publication.

- **`export` runs on every core, stops at a failed write, and believes
  kind claims.** Features are now formatted on the read's decode workers
  and written in input order, so export is many times faster (Germany
  about 3 minutes to under 10 seconds), with byte-identical output. A
  failed write, such as a closed stdout pipe, now ends the run at once
  instead of decoding the rest of the input. On indexed input a blob whose
  indexdata claims relations, or under `--type` the kind not selected, is
  skipped without decoding, and a decoded blob holding a kind its claim
  does not name is refused (exit `2`); `cat --reindex` repairs such a file.
  A refused element still leaves the features before it written.

- **Simple multi-extract decodes each blob once on sorted indexed input,
  and refuses an inexact ID range it relies on.** `extract --simple
  --config` now selects and writes every region from one read,
  decompression and decode per blob when a kind's indexdata ID ranges are
  pairwise disjoint, as single-region simple extract already did: planet
  with five longitude strips about 68 s to 52 s. Such a blob whose ID range
  or count is not exact for what it holds is now refused (exit `2`, no
  region published); `cat --reindex` repairs it. Inputs whose ranges
  overlap or carry no indexdata run as before.

- **`extract --simple` decodes each blob once on sorted indexed input, and
  refuses an inexact ID range it relies on.** A single-region simple
  extract of a sorted input now reads, decompresses and decodes each blob
  of a kind once, rather than once to classify and again to write, when the
  kind's indexdata ID ranges are pairwise disjoint: about 9 % faster at
  planet with a continent box. Such a blob whose ID range or count is not
  exact for what it holds is now refused (exit `2`, nothing published);
  `cat --reindex` repairs it. Inputs whose ranges overlap (history files)
  or carry no indexdata run as before.

- **`sort` and `cat --dedupe` keep `LocationsOnWays` through a rewrite.**
  When every input declares `LocationsOnWays`, a way blob in an overlap run
  (or internally unsorted, or restructured) is now re-encoded with its inline
  coordinates, and the output keeps the declaration; before, the coordinates
  were dropped, the declaration removed and `header.way_metadata_loss`
  warned. The prepass features are still dropped and warned. A rewritten way
  that has refs but no coordinates in such an input is now refused (exit
  `2`) rather than written into a file that declares it located. A merge of
  inputs that do not all declare the feature, and `time-filter` (whose
  coordinates would not be as of the cutoff), drop them as before.

- **`renumber` keeps the `LocationsOnWays` declaration.** Renumber always
  carried each way's inline coordinates beside its renumbered refs, but
  wrote a header without `LocationsOnWays` and warned
  `header.way_metadata_loss`. An input declaring it now yields an output
  declaring it, with no warning for it, so way export, `diff --locations`
  and `digest --form content-locations` accept the output. The prepass
  features are still dropped and warned.

- **OSC readers refuse what XML forbids lexically, as osmium does.**
  `apply-changes`, `merge-changes` and `tags-filter --input-kind osc`
  now refuse (exit `2`) an undeclared entity reference anywhere (also
  with an external DTD, which osmium would skip), a bare `&` or literal
  `<` in an attribute value, attributes run together (`k="a"v="b"`),
  attribute and unknown element names that are not XML names, a DOCTYPE
  after the root, repeated, lowercase or with a malformed external
  identifier, an XML declaration that is not first, text, CDATA or a
  reference outside the root, `--` in a comment, `]]>` in text and a
  processing instruction targeting `xml`. Before, all of these were read.

- **`check --ids` details come in a fixed order.** Under `--full` the
  duplicates of a kind used to sit among its order violations wherever the
  blob a worker happened to blame them on lay, so the list, and what
  `--max-errors` kept, could change from run to run. Each kind now lists its
  order violations in file order, a blob's step down from the previous blob
  before its own, then its duplicates in canonical ID order. Totals are
  unchanged.

- **`inspect --nodes` is the same at every `-j`.** FOR windows used to
  carry across whichever blobs the scheduler gave one worker, so the
  bit-width histograms, block count and size estimate changed with
  scheduling and `-j`. Windows now close at every blob boundary, a blob's
  tail being its own partial block, which is what a per-blob codec would
  pay; expect about one more block per node blob. The size estimate now
  packs a partial block at its own length instead of charging 128 values,
  is labelled `Estimated FOR coordinate bytes`, and prints GiB, as it
  always computed, instead of GB. `CoordStats` gains `packed_bytes` and
  `bytes_per_block()`.

- **Geocode candidates are bound to their reader and rank like
  `Reader::query`.** `Candidates` now holds `AddressCandidate`,
  `StreetCandidate`, `AdminCandidate` and `InterpolationCandidate` values,
  each borrowing the reader that produced it; the first three deref to the
  old match types, and none can be built or edited by a caller.
  `Candidates::into_result` takes no reader argument, and
  `Reader::interpolate` is replaced by
  `InterpolationCandidate::house_number`, so a candidate can no longer be
  resolved against a different index. `into_result` used to keep the first
  listed of two equal-area same-level admin polygons and compared rounded
  meters, so it could disagree with `query()`; both now share one total
  order. `query()` itself now breaks exact distance ties by record
  identity instead of search order.

- **cat's blob counts say what happened to each blob and what was
  opened.** `CatStats::blobs_decoded` is renamed `blobs_reencoded` (input
  blobs rebuilt from their elements), and `blobs_rewritten` and
  `blobs_payloads_opened` are new. A `cat --repair` blob repaired in place
  used to be counted nowhere, so a run could print `0 blobs passed
  through`; it now counts as rewritten. A blob reframed with fresh
  indexdata still counts as passed through, but its decompression is no
  longer hidden. The summary reads `Rebuilt elements from N input blobs
  (M elements written); R blobs rewritten; K blobs passed through`, with
  zero clauses dropped, then `Opened the payloads of P input blobs` when
  blobs that passed through were opened to index or validate them. The
  sidecar counter `cat_blobs_decoded` is now `cat_blobs_reencoded`, beside
  new `cat_blobs_rewritten` and `cat_blobs_payloads_opened`.

- **`degrade --unsort` refuses `--elements-per-blob 1`.** At a cap of 1
  every blob holds one element, and single-element blobs of distinct IDs
  never overlap, so the swap only permuted blobs: `sort` restored the
  order without its overlap rewrite, the workload `--unsort` exists to
  produce. The CLI now exits 64 and the library's `Unsort::CrossBlob`
  returns an invalid-argument error below a cap of 2, as
  `--unsort-intra` already did.

- **Compression levels are checked types.** `Compression::Zlib` and
  `Compression::Zstd` carry `ZlibLevel` and `ZstdLevel` instead of bare
  integers; build them with `Compression::zlib(level)` /
  `Compression::zstd(level)`, which refuse a level the backend does not
  support. A hand-built `Compression::Zlib(10)` used to panic inside the
  writer on its first blob; that value can no longer be constructed.
  `ParseCompressionError` is renamed `CompressionError`. The parser and
  `--compression` now accept every zstd level the linked zstd library
  supports, including negative levels below `zstd:-7`, and refuse levels
  outside its range instead of letting zstd clamp them.

- **`inspect --json` is `schema_version` 5: the header replication
  timestamp prints as a UTC timestamp.** JSON
  `header.replication.timestamp`, the text `Repl:` line and
  `--get header.replication.timestamp` printed the raw seconds, unlike
  every other timestamp inspect prints and unlike `osmium fileinfo`. All
  three now print `YYYY-MM-DDTHH:MM:SSZ`, which
  `--output-header osmosis_replication_timestamp=` accepts back; a year
  outside 0000 to 9999 prints expanded.

- **`inspect --json` was `schema_version` 4: `id_ranges.<type>.monotonic`
  is `null` when the index-only report cannot decide it.** A blob's
  claimed ID range does not say in what order the blob holds its IDs, yet
  the index-only `--id-ranges` report answered `yes` whenever consecutive
  ranges ascended, so a file unsorted inside a blob, or holding negative
  IDs, read as monotonic until decoded. It now says `no` where the claims
  prove descent, `yes` only when every blob claims one element, and
  `unknown` (`null`) otherwise; `--distrust-metadata` decodes and answers.

- **`inspect --extended` judges `data.objects_ordered` (`Ordered:`) by
  canonical OSM order over the element stream.** It compared IDs signed
  numerically, so a file `sort` wrote with negative IDs read as unordered,
  and it took any mixed-kind block for disorder, so a block holding a node
  group then a way group read as unordered. It now says whether the
  decoded elements strictly ascend by type and then canonical ID, the
  order `check --ids` accepts.

- **`inspect --json` was `schema_version` 3: `elements.tagged_nodes` is
  `null` when unmeasured.** The default index-only path never counted
  tagged nodes (indexdata has no tag count) yet published `0`, so an
  indexed file with tagged nodes read as having none. It is now `null`
  there; a number is always a decoded count, available with
  `--distrust-metadata` or any decoding flag.

- **`inspect --indexed` is a verdict of its own, and `inspect --get` prints
  only its value.** `--indexed` now refuses (exit 64) `--nodes`,
  `--blocks`, `--id-ranges`, `--locations`, `--anomalies`, `--extended`,
  `--get` and `--distrust-metadata`. A combined run used to print the
  first-blob verdict line before the report even under `-q`, and to
  overwrite the JSON report's `indexed` with the first-blob answer; the
  report's own `Indexed:` line answers across every data blob. `--get` now
  refuses `--nodes` and `--id-ranges`, which only added a report around
  the value.

- **`inspect --get` with a `header.*` or `file.*` key refuses
  `--distrust-metadata` (exit 64) beside `--extended` or `--locations`
  too.** Those flags send the key through the full scan, where the refusal
  used to lapse although the value still comes from the header or the file
  itself.

- **A header-only file is indexed on every `inspect` route.** The
  full-decode scan (`--extended`, `--locations`, `--distrust-metadata`)
  reported it as not indexed, so `--extended --get indexed` said `false`
  where `--get indexed` said `true`. The extended text report on a file
  with no objects now prints both metadata coverage lines as `(none)`
  instead of omitting them.

- **`osc::load_all_diffs` refuses entries it cannot load.** A
  sequence-named entry whose metadata could not be read (an unsearchable
  directory, a dangling symlink) was silently left out, so the loader
  could return a short or empty overlay as success. It is now refused,
  as are a FIFO, socket or device named as a diff and two diffs with the
  same sequence number (`4705.osc` and `4705.osc.gz`). A non-UTF-8 name
  now counts as unmatched for the zero-match refusal, and directory
  listing errors name the directory.
- **A header bbox must appear once with all four coordinates.** A
  `HeaderBlock.bbox` missing a coordinate read that coordinate as 0, and a
  repeated bbox replaced the earlier one, so a complete box followed by an
  empty one reported `(0, 0, 0, 0)` to `HeaderBlock::bbox`, `inspect` and
  output headers. Both shapes are now refused when the header is decoded.
- **A malformed OSC attribute is refused wherever it sits.** The OSC
  readers stopped at the attribute they wanted, so a malformed or
  duplicated attribute after it (`<nd ref="1" broken/>`, `<member
  type="node" ref="1" role="a" role=b/>`) was accepted, and merge-changes
  and tags-filter OSC wrote the element back without it. Every command
  now refuses it, on `tag`, `nd` and `member` in any section, and on the
  start tags no command reads: the root, the section tags, `<bbox>` and
  `<bounds>`, and unrecognised root-level elements. The error reads
  "malformed attribute on <ELEMENT>".

- **A BlobHeader with an absent or zero `datasize` is refused.** The
  header parse used to read an omitted `datasize` as zero and accept an
  explicit zero, so such a frame passed as an empty body: header walks,
  raw passthrough (`cat`, `degrade`, `getid`, `check --index`) and the
  unknown-type skip accepted it, an extension frame could be silently
  skipped, and only a full decode failed, as an empty blob. Both are now
  refused for every blob type, as libosmium refuses them, before the
  header-position rule. The negative-size error now reads "blob header
  datasize must be positive". `BlobHeader::get_blob_size` is always
  positive.

- **Rebuilt blocks round non-default-granularity timestamps down and refuse
  overflow.** A command that rebuilds a block from a source block whose
  `date_granularity` is not the default 1000 used to scale its timestamps
  with wrapping arithmetic and truncate toward zero, so an overflowing
  value was written as a different valid timestamp and -1 ms became the
  epoch. The timestamp is now scaled exactly and rounded down to the whole
  second (as `time-filter` and the OSC writer round), an overflow is
  refused with an error naming the element, and a versionless dense node
  whose claim rounds to 0 s is written as a plain node so the claim is not
  read back as padding. Default-granularity input, which is every known
  producer's, is unchanged. The precision contract for rebuilt blocks is
  now stated once, in the CLI reference's "PBF rebuild precision"; repack's
  round-trip claim is qualified to it.

- **`time-filter` refuses a dense node whose timestamp is padding.** A
  dense node with no version (0 or the Osmosis -1) and timestamp 0, which
  is what a writer stores for a node without metadata in a block whose
  other nodes carry it, used to pass as the Unix epoch and be kept below
  every cutoff. It is now refused as a node that carries no timestamp,
  like metadata-stripped input. A versioned node at the epoch
  and a versionless node with a non-zero timestamp are still filtered.

- **Default `tags-filter` refuses history input.** Without `-R` it
  selects objects by ID, so on a file holding several versions of one
  object it wrote every version of a matched ID as a match, kept a
  non-matching version's tags under `--remove-tags`, and wrote
  non-matching way versions whose nodes were never collected. It now
  refuses, with exit 2, a PBF declaring `HistoricalInformation`, and a
  PBF repeating an ID whose records disagree about the expressions.
  Repeats that agree still run. Use `-R` on history files, or filter a
  `time-filter` snapshot.

- **`altw::external::external_join` is removed.** It published `(0, 0)`
  placeholders for unresolvable way node references with no way to
  refuse them, and skipped `AltwOptions::validate`. Call
  `altw::add_locations_to_ways` with `index_type: IndexType::External`
  instead; it takes the same settings in `AltwOptions` (with
  `write.force` off, which the external index refuses) and refuses
  missing nodes unless `ignore_missing_nodes` is set. The external index
  now refuses missing nodes as soon as its node join has counted them,
  before encoding and writing the output, so a refused run ends earlier.

- **A node at two coordinates is refused, not resolved by chance.**
  `add-locations-to-ways` and `apply-changes --locations-on-ways` now
  fail with exit 2 and publish nothing when a node a written way needs
  occurs more than once at different coordinates. The coordinate used
  to depend on the index type and, for the sparse index and
  apply-changes, on worker scheduling. Identical repeats are still
  accepted, `--ignore-missing-nodes` does not waive the refusal, and an
  out-of-domain repeat is refused under every index type (the external
  index used to judge only the first record).

- **A caught worker panic is `ErrorKind::Panicked` and exits 101.** A
  panic on a worker, writer or decode thread that pbfhogg catches at its
  join used to come back as `ErrorKind::Internal` and exit 2, the same
  status as a refusal of the input. It is now the new kind
  `ErrorKind::Panicked { who, payload }`, `Error::is_panic` finds one
  under any wrapping, and the CLI exits 101 for it, as for an uncaught
  panic, ahead of every other status. `Internal` now means a broken
  internal invariant only. Either is a defect worth reporting; a script
  that treated 2 as "pbfhogg refused this input" no longer sees panics
  there. The library harness reports such a call's outcome kind as
  `panicked`.
- **Geocode index format 9; admin lookup about five times faster.** The
  reader now looks up admin boundaries in the query point's own admin cell
  only, and accepts a boundary that cell marks as containing all of its
  queries without a point-in-polygon test, where it used to test every
  candidate of the nine surrounding cells. On the Germany box (100,000
  points) a query went from about 130 us to about 26 us; admin answers
  are unchanged except for points exactly on a boundary. Format 9 makes
  the admin cell lists a binding cover, so the reader refuses format 8
  and earlier: rebuild existing indexes. A producer other than pbfhogg
  must meet the cover rules stated at `AdminEntry` in the format module.
  An admin area with a ring whose antimeridian-unwrapped longitudes pass
  100,000 degrees (one winding round a pole hundreds of times) is dropped
  with a warning and answers no query.
- **`build-geocode-index` refuses an index whose only content is
  interpolation ways.** With no address points, streets or admin polygons,
  no interpolation way can resolve a number, yet such a build used to
  publish an index that answers nothing. It now fails with the empty-index
  error, exit 2.
- **The geocode reader refuses corrupt index content it used to answer
  around.** `Reader::query` and `Reader::candidates` fail with the new
  `QueryError::DanglingReference` or `QueryError::DanglingSegment` when a
  cell entry they read names a record past its file; before, the entry was
  skipped and the query answered with no result or a farther one.
  `geocode-query` exits 2 naming the points line. `Reader::open` now also
  refuses admin cells out of order or repeated, an admin polygon area that
  is NaN, infinite or negative, an admin level outside 2 to 10 and the
  postal level, a nonempty entry list over an empty record file, and a
  string pool whose first entry is not the empty string. Indexes built by
  pbfhogg are unaffected. `QueryError` gains variants, so an exhaustive
  match on it must add arms.
- **apply-changes refuses a history base, and applies a hidden change as a
  delete.** A base PBF declaring `HistoricalInformation` exits 2 before the
  OSC is parsed; before, every version of a changed object was overwritten
  with the new state. Make a snapshot with `time-filter` first. A `<create>`
  or `<modify>` with `visible="false"` now removes the object (as osmium
  does) instead of writing an element marked invisible into the snapshot.
- **time-filter output no longer declares `HistoricalInformation`.** It
  holds one version of each object; before, a history input's declaration
  was carried, so the snapshot read as a history file.
- **OSC input holding a character XML 1.0 forbids is refused.**
  `apply-changes`, `merge-changes` (with or without `--simplify`) and
  `tags-filter --input-kind osc` exit 2 on an OSC document containing a
  C0 control character other than tab, newline and carriage return, or
  U+FFFE / U+FFFF, written literally or as a character reference
  (`&#1;`, `&#xFFFE;`), anywhere in the document - including attributes
  and text no command reads and changes a filter would drop - as osmium
  does. Before, `apply-changes` wrote such a value into the output PBF.
  `&amp;#1;` (escaped text) and references inside comments or CDATA stay
  legal. A `<!DOCTYPE>` with an internal subset (`[...]`) is now refused;
  a bare `<!DOCTYPE osmChange>` or an external identifier is still read.
- **apply-changes refuses changes older than the base.** Where a change
  replaces or deletes a base element, and the highest version the changes
  claim for that object (deletes included) is below the base element's
  version, the run fails with the same version-regression error the
  changes' own ordering check gives, naming both versions. Before, the change
  was applied regardless, so a stale diff silently downgraded or deleted
  newer objects; osmium keeps the base element instead. Equal versions,
  and elements or changes without a version, still apply.
  `--allow-version-regression` restores last-change-wins, and is needed
  to apply a content-derived patch back to an older snapshot. Library:
  `ErrorKind::VersionRegression` gains an `in_base` field.
- **build-geocode-index refuses an unresolvable boundary member role.** A
  way member of an admin or postal boundary relation whose role string
  index is negative or past the block's string table now fails the build
  (exit 2), naming the relation, member position and index, and leaves any
  existing index in place. Before, the role read as empty and so as outer,
  turning a would-be hole into another area of the boundary.
- **A tag string index past the block's string table is refused.** Every
  command that decodes a block now fails (exit 2) on a tag key or value
  index that names no string, as osmium does. Before, the tag list ended
  silently at that tag, so repack, cat, degrade, sort, the filters,
  extract, export, the geocode builder and add-locations-to-ways could
  write elements with fewer tags or select on a shortened list, and
  `inspect tags` counted only the tags before it. Indexdata generation
  and `check --index` refuse the same blobs. Library: `tags_checked()`,
  `CheckedTagIter` and `CheckedDenseTagIter` are removed; `tags()` now
  always yields every tag of an accepted block.
- **Blob and HeaderBlock fields are recognised by wire type as well as
  field number.** A known field number carrying a different wire type is
  skipped as an unknown field, as protobuf and osmium do, and an invalid
  wire type is refused (exit 2). Before, a Blob payload tagged with an
  invalid wire type still decoded, and a wrongly typed header field could
  become a replication timestamp. A Blob holding a second payload in
  `lzma_data`, `OBSOLETE_bzip2_data` or `lz4_data` beside its `raw`,
  `zlib_data` or `zstd_data` is refused instead of decoding the supported
  one, and a lone payload in one of those codecs now names the
  unsupported compression instead of reporting an empty blob.
- **A block with two non-empty string tables is refused.** Every command
  that decodes a block, indexdata generation, `check --index`, renumber
  and add-locations-to-ways' way rewriting now fail (exit 2) on a
  `PrimitiveBlock` carrying a second string table with data. Before, the
  last table silently replaced the earlier one, so tags could resolve to
  the wrong strings. Blobs copied through without decoding (already
  indexed blobs in `cat`, relation blobs in add-locations-to-ways) are not
  inspected. An extra empty table is ignored, now by renumber too, which
  used to refuse it.
  add-locations-to-ways also no longer drops an empty string table from
  its output, which had made that output unreadable to a second run.
- **A node coordinate the PBF framing puts outside the representable range
  is refused, not wrapped.** cat, repack, sort, `cat --dedupe`, degrade,
  tags-filter, time-filter, getid, getparents, apply-changes and
  add-locations-to-ways now fail (exit 2) on a decoded node whose
  granularity, offset and raw value scale past an `i32` decimicrodegree,
  and on an inline way location they re-encode that does so. Before, a
  block with `lat_offset` 429496729600 wrote its node at latitude 0.
  diff and digest refuse such a compared node instead of matching it
  against one at the wrapped position; export skips it as outside the
  geographic domain.
  Blobs copied without decoding are not judged. Library: on `Node`,
  `DenseNode` and `WayNodeLocation`, `decimicro_lat()` / `decimicro_lon()`
  now return `Result<i32>` and `nano_lat()` / `nano_lon()` `Result<i64>`,
  scaled exactly; `lat()` / `lon()` never wrap; `decimicro_lat_checked()`
  and `decimicro_lon_checked()` are removed (use `.ok()`);
  `WayNodeLocation` no longer implements `PartialEq`.
- `diff --format osc --increment-version` exits 2 without publishing an
  OSC if a deleted element is already at version 2147483647.

- **`diff --format osc` refuses values its own OSC reader would refuse.**
  A created or modified node outside latitude -90 to 90 or longitude -180
  to 180, or with a coordinate that does not fit the PBF representation,
  and an element timestamp outside years 0000 to 9999 now fail the run
  (exit 2, nothing published). Before, derive printed `lat="91"`, a
  five-digit or signed year, or a wrapped coordinate or date, and
  `apply-changes` then refused the file. The library's delete timestamp
  override past `9999-12-31T23:59:59Z` is now `InvalidArgument` before
  any input is opened. Before, it was clamped and written unreadable.
- **`tags-filter --input-kind osc` writes the kept changes in input
  order.** It used to write every create, then every modify, then every
  delete, so a delete followed by a re-create of the same object at the
  same version came out the other way round and applying the filtered
  file deleted the object; a delete followed by a higher-version create
  was refused by `apply-changes` as a version regression. Output now
  matches `merge-changes` without `--simplify`: sections follow the
  input's actions and may repeat, and a `.osc.gz` output is several
  concatenated gzip members (one per batch) rather than one; `gzip -d`
  and osmium read it whole, but a reader that stops after the first
  member (flate2's `GzDecoder`) sees only the document's start. Memory no
  longer grows with the kept output.
- **Geocode index format is now 8; existing indexes must be rebuilt.**
  The last release wrote format 5. Version 6 replaced the admin vertex
  budget with `--admin-tolerance`, version 7 records the input PBF's
  XXH3-128 and size in the header, and version 8 stores the input
  header's replication sequence and timestamp verbatim, signed and with
  their presence. `Reader::open` refuses any other version with an error
  telling you to rebuild with `build-geocode-index`; there is no in-place
  upgrade.
- **`Reader::replication_sequence` and `Reader::replication_timestamp`
  return `Option<i64>`** (were `u32` and `u64`). `None` means the input
  header had no value, where both used to read as 0. A negative input
  timestamp used to come back wrapped to a huge positive number, and a
  sequence outside the u32 range failed the build; both are now kept as
  the input holds them. `format::Header` loses its `format_version`
  field: `to_bytes` always writes the current version.
- **`build-geocode-index` no longer joins a way across a missing node.**
  On an extract that drops some of a way's nodes, a street is split into
  one street per stretch of consecutive present nodes instead of gaining a
  straight segment over the gap (which queries could match at zero
  distance), a `building` way with a missing node gives no address point,
  and an admin boundary whose outer or inner member ways are not all
  present with every node contributes no area, where before it could
  close a smaller ring or lose a hole. Complete input builds the same
  index as before.
- **The ID sets in `getid::ElementIds` (`node_ids`, `way_ids`,
  `relation_ids`) are signed and their methods renamed.** They hold any
  `i64`, negative IDs included. `set` / `set_if_new` are now `insert` /
  `insert_if_new`; `iter` is gone in favour of `iter_canonical` (OSM
  `id_order`: `0`, then `-1, -2, ...`, then `1, 2, ...`) and
  `iter_numeric_from`; `any_in_range` is `any_in_numeric_range`; the
  atomic setters are `insert_atomic`, `insert_atomic_if_new`,
  `try_insert_atomic` and `try_insert_atomic_if_new`. Inserting a
  negative ID now stores it, where it was silently dropped before.
- **getid (including `--invert`) and getparents handle negative IDs
  exactly.** Negative query IDs are accepted from positional arguments
  and `--id-file` (they were refused, exit 64 or 2), and from
  `--id-osm-file` (they were silently dropped, so a source of negatives
  alone was refused as holding no ID). `-r` now follows negative way
  refs, relation members and relations in relations, which it silently
  dropped. Matching is by the signed ID, where osmium matches by absolute
  value. `parse_ids` and the other library parsers accept negative IDs.
  A bare negative number under `--default-type` needs no `--`.
- **tags-filter handles negative IDs exactly in its default
  (reference-including) mode.** A matching negative node, way or
  relation, and the negative way refs, relation members and nested
  relations it pulls in, are now written; before, they were silently
  dropped (`-R` and OSC input were already exact). Selection is by the
  signed ID, where osmium folds referenced IDs onto their absolute value.
- **extract handles negative IDs exactly in every strategy and in
  `--config`.** Negative nodes inside the region, the ways and relations
  they match, complete-ways' outside way nodes, smart's member ways and
  nodes, and parent relations are now written; before, they were silently
  dropped, so complete-ways could publish a way without a node it
  references, and `--simple` kept negative nodes only from node blobs it
  copied raw. Matching is by the signed ID, where osmium matches by
  absolute value.
- **check handles negative IDs exactly.** `--ids --full` no longer fails
  an ordered file holding negative IDs (every negative was an
  `id_out_of_range` violation, exit 1); it finds negative duplicates by
  their signed ID. The `id_out_of_range` JSON violation type and the
  library's `IdViolation::IdOutOfRange` are removed. `--refs` (and the
  default `check`) and `--locations` check negative IDs where they
  refused the file (exit 2); `--locations` now reports conflicting
  records and, with `--agreement`, disagreeing inline copies of negative
  nodes.
- **add-locations-to-ways handles negative IDs exactly with both index
  types.** A way ref to a negative node gets that node's location; with
  the sparse index it was refused as unresolvable (exit 2), or written as
  `(0, 0)` under `--ignore-missing-nodes`, and the external index refused
  any node blob below zero (exit 2). An untagged negative relation-member
  node is kept, and `--inject-prepass` marks negative member ways and pins
  negative shared nodes; all three were silently dropped. Members are
  matched by the signed ID, where osmium's `--keep-member-nodes` matches
  by absolute value.
- **add-locations-to-ways `--index-type external` joins node IDs of any
  size.** Node IDs past about 2^38 from zero, which it refused (exit 2),
  now take a 16-byte scratch record per way node ref instead of 8.
- **add-locations-to-ways `--index-type auto` routes on more than the
  header estimate.** An input whose indexdata the external index would
  refuse (ranges out of sorted order), or whose node IDs need its wide
  scratch record, now runs sparse, where auto used to route some of them
  to a refusal. When auto runs sparse against a known memory budget, the
  exact store is judged again after the first pass, counting referenced
  IDs the file lacks and negative-ID sets, and an over-budget store
  switches to external with a second `Note:` on stderr.
- **build-geocode-index uses negative admin ways and node refs.** A
  boundary whose member way has a negative ID, and every way whose nodes
  have negative IDs, now get their geometry; before, negative admin ways
  were left out (their boundary was dropped as having no geometry) and
  negative node refs were skipped with a `skipped N negative node refs`
  warning, which is gone. Indexdata claiming a node `max_id` below a
  negative node it holds is now refused like any understated claim.
- **renumber accepts negative IDs and a `--start-id` below 1, as osmium
  does.** Negative node, way and relation IDs, way refs and relation
  members are renumbered by canonical order (`0`, then `-1, -2, ...`, then
  `1, 2, ...`), which the input must follow; they were refused (exit 2). A
  negative start counts down from it (`-1, -2, ...`), and 0 means 1; both
  were refused. `--start-id -5` parses in the space form. Library:
  `RenumberOptions` takes any start the same way.
- **renumber names orphan references differently.** A way ref or relation
  member to an element absent from the input is now mapped one to one onto
  the IDs outside its kind's renumbered range, below or above it; on
  positive input from start 1, an orphan X becomes X plus the number of
  input elements of its kind above X. The old `start + count + X` shift
  collided for negative orphans and refused an orphan near the largest ID,
  which now renumbers. The old ID can no longer be recovered by
  subtraction.
- **`geo::assemble_rings` takes ring members and can refuse.** It now takes
  `&[RingMember]` (each member carries its node IDs as well as its
  coordinates) and returns `Result<Vec<Ring>, RingRefusal>`, where it took
  bare coordinate chains and returned rings. Members join by node ID, not
  by coordinate. The bounded variant and its step budget are gone.

### Added

- **Occurrence count for missing node refs in ways.** `check --refs`
  appends the number of way node refs to missing nodes when it exceeds
  the distinct missing IDs (`Missing node refs in ways: 1 (2
  references)`), as it already did for relation members, and JSON adds
  `missing_node_ref_occurrences` (library:
  `RefCheckResult::missing_node_ref_occurrences`). That is the figure
  `osmium check-refs` prints as `Nodes in ways missing`; before, it could
  not be recovered from pbfhogg's output.

- **`has_negative_ids` report fields.** The ID check reports whether
  any element ID of a selected kind is negative (`has_negative_ids` in
  `check --ids --json`, a `Negative IDs:` line in its text, and the
  public `VerifyIdsReport::has_negative_ids`), and `inspect --extended`
  reports it for every decoded element (`data.has_negative_ids` in
  `--json` and as a `--get` key, `Negative IDs:` in its text).

- **`pbfhogg geocode-query INDEX POINTS`** answers reverse geocoding
  queries from the command line: one `LAT LON` per line of `POINTS`, one
  JSON object per query on stdout (nearest address, street and
  interpolation, admin areas per level), the answers of `Reader::query`.
  Until now only the library could query an index.
- **A geocode index records its input PBF's whole-file XXH3-128 and
  size**: `Reader::input_xxh3_128()` (formatted `{:032x}`, the same hex as
  any whole-file XXH3-128 of the file, as brokkr pins and elivagar's
  `input.xxh3_128` record) and `Reader::input_bytes()`, so a consumer can
  prove an index was built from its PBF; replication sequence and timestamp
  never could. Index format is now 7 (rebuild existing indexes). The build
  reads the input once more, sequentially beside its passes (no measurable
  wall cost at planet), and refuses an input whose size changed during the
  build.
- **`geocode_index::reader::Reader::open_dir(dir: impl AsFd)`** (Unix)
  opens an index from a directory handle the caller holds. On Unix
  `Reader::open` now also opens the directory once and every index file
  relative to it, so an open racing a `build-geocode-index --overwrite`
  publication reads one build or fails instead of possibly mixing files of
  two builds. An index file that is a symlink is now refused.
- **`diff --locations`** also compares inline `LocationsOnWays` way
  coordinates: a way whose tags and refs match but whose location list
  differs (in length, or at any position at 1e-7 degrees, truncated toward
  zero as libosmium reads it) is modified, and `-v` prints each differing
  position. Both inputs must declare `LocationsOnWays`; refused with
  `--format osc`. Applies to `-q`. **`digest --form content-locations`**
  (`v4`) is its digest: equal exactly when `diff --locations -c` reports the
  files identical. Together they compare the inline coordinates of two
  producers (pbfhogg and osmium `add-locations-to-ways`, say) without their
  metadata or header. Library: `DiffOptions::locations`, a new `locations`
  argument to `diff::diff_stats`, and `digest::Form::ContentLocations`.
- **`cat --repair`** rewrites a file every other command refuses for a
  repeated `Info` or `DenseInfo` (`Node.info appears twice with data` and
  its kin) into a valid one, merging each as protobuf merges a repeated
  message. Every data blob is validated; well-formed ones are written as
  plain `cat` writes them, repaired ones recompressed with their other
  `BlobHeader` fields kept, and a `Note:` counts what was merged. Combines
  with `--clean` (and `--type` with `--clean`). New library entry
  `cat::repair` and `CatStats::blobs_repaired`. Plain `cat`, `--clean` and
  `--type` are unchanged without the flag.
- **`Relation::members_into`** resolves a relation's members into a reusable
  `Vec<MemberData>` ready for `BlockBuilder::add_relation`, refusing a role
  string index that is negative or past the string table with an error naming
  the relation, the member and the index. It is the conversion every
  re-encoding command uses, now public so `BlockBuilder` users need not map
  `RelMember::role` errors by hand.
- **`ElementReader::resources`** supplies the `host::Resources` the reader
  sizes its pipelined and `par_map_reduce` workers from, so a reader opened
  without a host no longer has to probe the machine; `host::Fixed` gives the
  same worker counts everywhere. The setter probes nothing and changes nothing
  else about the reader, and an explicit `decode_threads` still wins. Readers
  that do not call it keep their sizing.
- **`PrimitiveBlock::element_kinds`** returns the exact set of element kinds
  a block holds, as a new `ElementKindSet` (`has`, `is_empty`, `len`,
  `single`, `iter`), recorded by the validation every block already passes,
  so it costs nothing. Route blocks by kind with it rather than with
  `block_type()`, which labels each group by its first field only: an empty
  dense message in front of ways labels a way block as nodes, a ways-first
  group can carry relations, and `Mixed` also covers changeset and unknown
  groups.

### Changed

- **`cat`, `extract`, `multi-extract`, `tags-filter`, `getid`,
  `getparents` and snapshot `time-filter` cut an output block too large
  for one blob instead of refusing it.** A re-encoded block can pass the
  32 MiB payload limit, or its encoded body the 32 MiB blob size, even
  though its input blob fit (a rebuilt string table, a codec change). Such
  a run used to fail with exit `2`; the block is now cut between elements
  as `sort` cuts one, each piece indexed from its own content. A single
  element too large for a blob, and a cut that needs the last few bytes
  below the limit, are still refused. `repack` and `degrade`
  keep refusing, and so does a repaired payload `cat --repair` frames
  under its input header.

- **A block too large for one blob that cannot be cut is refused naming
  the input it came from.** On every command that cuts such a block
  (`sort`, `cat`, `extract`, `tags-filter`, `getid`, `getparents`,
  `time-filter`, `add-locations-to-ways`, `renumber`), the remaining size
  refusal used to come from the writer later, naming only the output; it
  is now raised as the block is built, with the same text and exit `2`,
  plus the input file and the blob ordinal or frame offset (for `sort` and
  `cat --dedupe`, the input of the block's first element). Library
  callers see `ErrorKind::BlobTooLarge` with that location attached.
  Unsorted `extract --simple` also locates its decode failures at the
  blob.

- **`extract --simple` on unsorted indexed input is about twice as fast,
  and writes no node from a blob outside the region.** Each classify sweep
  now decodes only the blobs that claim its kind, and the write pass
  decodes on every core and skips node blobs whose indexdata bbox misses
  the region (Denmark's unsorted copy about 7.4 s to 3.3 s). A repeated
  node ID selected inside the region used to be written again from such a
  blob; it no longer is, matching sorted input.

- **`extract --config` shares its passes across regions for every
  strategy.** On sorted input, complete-ways (the default), `--smart` and
  cleaned `--simple` multi-extract used to extract the regions one after
  another, each with its own passes over the input. Cleaned simple now
  takes the same single pass as uncleaned simple; complete-ways and smart
  classify nodes, ways and relations once for all regions, resolve way
  completion, relation parents and smart relation completion per region,
  and write every region in one pass. Each region's output and summary
  line are the same as before. These routes hold a writer per region for
  the run, so they ask for one descriptor per region up front, and they
  write through buffered outputs (`--direct-io` applies to their input
  reads only), as the uncleaned simple single pass does. The banner names
  the strategy (`[multi-extract] single-pass: N regions, smart strategy`).
  Planet 5-strip complete-ways and smart multi-extract are about 4.6x
  faster. Unsorted input still extracts one region at a time.

- **History `time-filter` runs on every core.** A history input's versions
  are now selected and encoded on the read's decode workers; only objects
  whose versions straddle a blob boundary are joined in order. A planet-sized
  history file fell from about 20 minutes to about 2m40s. Output is the same
  elements, but blocks now follow the input blobs (each input blob's
  survivors in blocks of up to 8000) instead of being packed across them.
  Only a surviving version's payload is read, so a bad user or role string
  index on a superseded version is no longer refused. An order or version
  refusal now names the blob it was found in.
- **A bare `check` reads an unindexed file once too.** The ID and
  reference checks used to read an unindexed file in turn; one pass now
  validates each blob once for both, the reference check re-reading only
  the blobs it re-reads alone. Unindexed planet fell from about 37-42 s to
  21 s.
- **A bare `check` reads an indexed file once.** The ID and reference
  checks used to read every node and way blob each; they now share one
  header walk and one read per blob, each check still reporting, stopping
  or failing on its own. Planet fell from about 37-44 s to 21 s. Library:
  `check::pair::check_ids_and_refs` runs the pair.
- **Default `tags-filter` reads relation blobs at most twice, however
  deeply relations nest.** The relation-membership closure used to decode
  every relation blob once per nesting level; it now builds the
  relation-to-relation graph in one read and walks it in memory, as
  `getid --add-referenced` does. The closure phase of a planet
  `r/type=route` filter fell from about 3.9 s to 0.9 s (about 5 % of the
  run); output is unchanged.
- **An oversize-blob refusal no longer advises fewer elements per blob
  where the command offers no way to do that.** The error states the size
  and the limit; `repack` and `degrade` still add `; lower
  --elements-per-blob`. Library callers matching `ErrorKind::BlobTooLarge`
  see the same kind, with the remedy clause gone from its text.
- **`apply-changes` names its re-cut allowance when it runs out.** An input
  blob whose oversize-block re-cut passes 256 MiB of blocks used to be
  refused with the first oversize error alone; it is now refused (exit `2`,
  as before) with a new `ErrorKind::OverflowReplayBudget` naming the
  allowance, whose source is that oversize error.
- **`renumber` applies one rule to block and group fields it does not
  interpret.** Way blobs used to carry unknown PrimitiveBlock and
  PrimitiveGroup fields (including an obsolete `changesets` group field)
  through verbatim while node and relation blobs dropped them; every blob
  now drops them, as osmium renumber does. A way or relation field that is
  not a length-delimited message is now refused instead of being copied or
  dropped.
- **`build-geocode-index` holds about 2 GB less memory after Pass 2 at
  planet.** Admin boundary way geometry is kept in two flat buffers that
  go back to the OS when assembly ends, instead of millions of small
  allocations the heap kept resident for the rest of the build. The
  index is unchanged.
- **`build-geocode-index` assembles admin boundary rings by node ID and
  refuses ambiguous topology.** Member ways join where they share a node,
  not where two nodes share a coordinate, so a retrace, a figure-eight or
  four ways between the same two nodes no longer pass as a ring or pair by
  member order. A relation whose outer or inner ways do not form an
  unambiguous set of disjoint rings, or that lists one way in both roles,
  is withheld whole and counted under the new warning
  `geocode.pass2.admin_ring_topology`, which replaces
  `geocode.pass2.admin_ring_budget` (the search budget no longer exists).
  More relations can be withheld than before (on planet, about 300 more of
  roughly 3,000), and the build is slower and uses more memory (planet
  wall about 8 s, peak RSS up about 4 GB).
- **`merge-changes --simplify` writes survivors in canonical OSM order**
  inside each create, modify and delete group: `0`, the negative IDs by
  magnitude, then the positives, as osmium orders objects. It used to write
  them in numeric order, so `n-7` came before `n-1`. Positive-only changes
  come out as before.
- **`PrimitiveBlock::for_each_element`, and so `ElementReader::for_each` and
  `for_each_pipelined`, read a group carrying both plain and dense nodes
  dense nodes first,** as `elements()` already did, so `time-filter` on a
  history file now reads in the same order as other commands. Only
  malformed groups holding several element fields see a difference; such a
  group is read in kind order, not field order, unlike libosmium (recorded
  in DEVIATIONS.md).

- **`geocode_index::builder::build_geocode_index` takes its input as its
  first argument, and `BuildConfig::input_path` is gone.** The input is an
  `impl Into<InputSource>`, as on every other multi-pass entry point, so a
  caller that already pinned the input passes the `PinnedInput` and the
  build reads that file. `altw::external::external_join` takes its input
  the same way instead of a `&Path`. Passing a path behaves as before.

- **The hidden, unsupported `getparents --full-scan-min-blobs` test flag
  and `getparents::getparents_with_min_blobs` are gone from ordinary builds.**
  They now exist only in builds compiled with the `pbfhogg_test_hooks`
  compiler cfg; elsewhere the flag is refused as unknown. Ordinary
  `getparents` dispatch is unchanged.
- **The doc-hidden `test-hooks` Cargo feature is removed** from `pbfhogg`
  and `pbfhogg-cli`. Test fault injection (`pbfhogg::test_hooks`) and the
  CLI's `PBFHOGG_TEST_*` environment hooks now compile only under the
  compiler cfg `pbfhogg_test_hooks` (`RUSTFLAGS="--cfg pbfhogg_test_hooks"`),
  so no feature selection or feature unification, including a registry
  `cargo install`, can build them into an executable without that cfg. A build that named
  `--features test-hooks` now fails as an unknown feature.
- **The `bench-read`, `bench-write`, `bench-merge` and hidden
  `bench-geocode-query` subcommands are removed from the `pbfhogg` binary.**
  They were benchmark instruments, not user commands, and only ever called
  public library API; scripts that invoked them now get an
  unknown-subcommand error. The read and write workloads are measured
  through the library harness (`pbfhogg-harness`, scenarios `decode-*` and
  `encode-*`), the merge and geocode query instruments live in the
  unpublished `pbfhogg-bench` executable.

- **`build-geocode-index` refuses to publish an index whose smoke test
  fails.** The smoke test used to query the first address point and pass on
  any address or street, only warning on a miss and publishing anyway; it now
  checks that the first address point is listed in its own fine and coarse
  cell, and a miss fails the build with an internal error, leaving any
  existing index in place. `SmokeOutcome::NoMatch`, the
  `geocode.smoke_test.no_match` warning and the
  `geocode_smoke_test_no_match` sidecar counter are gone.
- **Gzip checksums use the CPU's carry-less multiply where it has one.**
  They always ran a table-driven fallback before. Planet `merge-changes`
  over seven daily diffs drops from 3.6 to 2.8 s, and with `--simplify` from
  6.2 to 4.2 s. `apply-changes` on planet is unchanged.
- **`merge-changes` and the OSC `tags-filter` with `-j N` /
  `Parallelism::Fixed(n)` run on the host's executor** instead of building a
  pool of their own: `n` now caps how many of their jobs run at once there
  (encode jobs, simplify's emit chunks and sort), so a library caller's
  `Executor::Pool` confines them at every setting. Their worker threads are
  no longer the call's to tear down; a sink callback running in those jobs
  runs on an executor worker that outlives the call.
- **Every thread the library starts is joined before the call or session
  that started it ends**, thread-local destructors included. Scoped worker
  sets used to be waited for only to the end of their main functions.
- **PBFs whose DenseInfo carries only some columns are read**, as osmium
  writes them under `add_metadata` (for example
  `add_metadata=version+timestamp`, or a history file written without
  metadata, which carries `visible` alone). They used to be refused.
  Re-encoding commands write such nodes dense with the same columns. A
  present column of the wrong length is still refused. New public
  `DenseNodeInfo::columns()` and `DenseColumns`; an absent column reads as
  0 (`visible` as true, the user as empty).
- **Nodes with partial metadata are written dense where that keeps what
  they claim.** A run of plain or OSC nodes sharing one partial field set
  (for example version, timestamp and visible) is written as a dense group
  with exactly those DenseInfo columns when that is smaller than the plain
  `Node` messages, instead of always plain. Nodes whose dense slot would
  read differently (a versioned node without `visible`, a versionless node
  with a zero uid or changeset, an empty user or `visible` true) stay
  plain. Complete-metadata output is byte-identical.
- A dense node with the Osmosis version `-1` keeps its metadata: printing
  outputs (`diff --format osc`, `inspect --show`, `export --metadata`,
  `digest`) show its real timestamp, uid and user, and re-encoding writes it
  with version 0 instead of dropping the whole slot. A versioned dense
  node's changeset `-1` is read as 0 everywhere, as re-encoding already
  wrote it. **`digest --form full` is now `v3`**: digests of files with
  such nodes change, so stored `v2` reports do not compare.
- `DenseNode::info` borrows from `&self` rather than `&'a self`.
- **`build-geocode-index`: `--max-admin-vertices` is replaced by
  `--admin-tolerance <METERS>`** (default 20, `0` keeps every vertex), and
  the geocode index `FORMAT_VERSION` is bumped to 6: existing indexes must
  be rebuilt. Admin boundaries were simplified to a vertex count, so the
  boundary could move by any distance the count demanded, kilometres on a
  large ring, and a query well inside one area could resolve to its
  neighbour. A query farther than the tolerance from a boundary now gets the
  answer the unsimplified boundary gives, at every admin level. Index size
  and admin lookup cost now follow the tolerance rather than a fixed budget.
  An admin ring that fits inside the tolerance as a whole (a small exclave,
  a sliver along a shared boundary, or the matching hole in its neighbour)
  is simplified at a tolerance its own width allows instead of collapsing,
  so a query inside it keeps that area rather than getting the neighbour.
  Equal-area admin candidates at one level now resolve deterministically.
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
- Every other command that re-encodes elements (`cat`, `sort`, `repack`,
  `degrade`, `extract`, `tags-filter`, `time-filter`, `getid`,
  `getparents`, `add-locations-to-ways`) now keeps the metadata fields of
  a plain node, way or relation exactly as the input carries them. An
  absent uid, changeset, timestamp or user is no longer written as `0` or
  empty, and an `Info` without a version keeps its other fields instead of
  being dropped whole, which used to lose its user and turn a deleted
  element visible. A version or changeset of `-1` is read as absent and
  stays absent. `--clean` zeroes a field the element carries and leaves an
  absent one absent. A plain node with version 0 stays a plain node.
  Dense nodes are unchanged.
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

### Fixed

- **Multi-extract copies a node blob raw to each region its single
  extract would.** `extract --simple --config` copied a node blob raw
  only when it lay inside every region, and otherwise rebuilt it in the
  default framing. A region holding a blob that another region did not
  then lost what the raw copy keeps: a coordinate granularity or offset
  finer than a decimicrodegree, sub-second timestamps and the blob's own
  header. The raw copy is now decided per region, so each region's
  output equals its single-region extract on such input too.
- **Multi-extract reads each region's node blobs as its single extract
  does.** On sorted indexed input, `extract --config` read a node blob
  whose indexdata bbox met any region and then selected and wrote its
  nodes for every region. On input with a node ID in several blobs
  (history-shaped), a region could get that ID's occurrence from a blob
  its single-region extract never reads, and an understated bbox claim
  let a region select, and with complete-ways or `--smart` match ways and
  relations through, nodes its single extract skips. Each node blob now
  counts only for the regions its bbox meets, and one that meets none is
  not read. Inputs with each node ID in one blob are unaffected.

- **`apply-changes` writes a lone metadata-less node plain when dense
  would not fit a block.** A node without metadata that overflows the
  32 MiB block limit alone in the dense form is retried as a plain node,
  which claims the same and is a few bytes smaller; before, such a node
  (including an untouched plain node from the input, rewritten dense)
  failed the run (exit `2`). And one oversized output block that needs
  the source string table no longer costs the other oversized blocks of
  the same input blob their smaller re-cut.

- **A bbox edge's exponent is never capped.** `extract` and `export` bbox
  edges saturated their exponent at 100000, so a mantissa padded with
  more digits than that (`1` with 100001 zeros, then `e-100001`, exactly
  `1`) read ten times too large. The cap now grows with the token, so every
  spelling is read at its exact value.

- **Geocode admin rings on one line no longer publish as areas.** A
  boundary ring whose points all lay on one line (all at one latitude, say)
  encloses nothing, yet was stored, counted towards the empty-index check
  and listed in cells while answering no query. `build-geocode-index` now
  refuses such a relation as a ring-topology refusal
  (`geocode.pass2.admin_ring_topology`), judging collinearity with
  longitudes unwrapped across the antimeridian, where `+180` and `-180`
  are then one point. A ring that simplification would flatten onto a
  line is stored unsimplified. The library's `geo::assemble_rings` refuses
  these rings with `RingRefusal::DegenerateRing`. The format is unchanged;
  rebuild to get the new answers.
- **Geocode admin ranking measures antimeridian rings in their own
  frame.** A ring crossing the antimeridian stored the area of its raw
  longitudes, so a narrow dateline polygon could outrank a wider one the
  wrong way. `build-geocode-index` now measures it unwrapped, as
  containment tests it. The format is unchanged; rebuild an index to get
  the new scores.
- **Geocode hole ownership and polar rings no longer depend on member
  order.** Which outer ring owned a hole was a majority vote over five
  vertices sampled from wherever the assembled hole started, so reordering
  a relation's members could move a hole to another outer or drop it,
  notably when the hole shared nodes with its outer. Ownership is now decided by one exactly
  classified point of the hole, chosen by coordinates alone, skipping
  points on the outer's boundary. A ring winding round a pole was
  contained through a closing edge at the latitude of whichever vertex it
  started at; `build-geocode-index` now refuses its relation with a
  `geocode.pass2.admin_start_dependent_ring` warning, as it does for a
  ring whose unwrapped longitudes span a full turn or that has an edge of
  exactly 180 degrees of longitude. The former
  `geocode.pass2.admin_unbounded_frame` drop is gone: no ring it caught
  survives assembly now. The format is unchanged; rebuild to get the new
  answers.
- **Geocode snap and distance agree on half-turn segments.** For a street
  or interpolation segment spanning exactly 180 degrees of longitude, the
  distance could be measured on one half-world path and `snap_lat_e7` /
  `snap_lon_e7` placed on the other. The reader now projects onto both
  paths and reports the nearer one's point and distance.
- **`BlobReader::seek` resumes a reader a failed read stopped.** After
  `next()` or `next_header_skip_blob()` returned an error, a successful
  `seek` still left both returning `None`, while `seek_raw` and
  `blob_from_offset` resumed. Every successful seek now clears the sticky
  error; a failed seek clears nothing.
- **`diff`'s byte-equality shortcut also compares the declared
  `raw_size`.** Two blobs with identical compression and compressed bytes
  but different declared sizes were skipped as equal, though decoding
  refuses at least one of them, or treats them alike only when one
  declares no size. Such a pair is now decoded, so the refusal surfaces.

- **`apply-changes` re-cuts oversize blocks without the strings the
  changes removed.** When a rewritten block outgrew 32 MiB, the blocks it
  was cut into each carried the source block's whole string table. A
  surviving element that grew (refreshed `--locations-on-ways` coordinates)
  beside a large string the changes deleted was refused although it fit a
  block with its own strings, and survivors alternating with large changed
  values could repeat that table past the 256 MiB recovery budget and be
  refused too. Re-cut blocks now carry only the strings their elements use;
  whatever the old re-cut recovered is still recovered. An element too large
  for a block with only its own strings, or a blob whose changes need more
  than 256 MiB of blocks, is still refused before publication.
- **`renumber` cuts a blob its new IDs grow past 32 MiB.** A far or
  negative `--start-id` widens every ID, and refs alternating between live
  and orphan elements widen their deltas, so a blob near the payload limit
  could come out over it and the run failed with a size error. Such a blob
  is now written as several blobs, cut between elements, each with its own
  index; every other blob is written as before. Where keeping the input's
  string table in every piece cannot fit, a dense node group is cut between
  nodes and each piece carries only the strings it uses; a compressed blob
  whose body would expand past 32 MiB is cut too. Still refused: a single
  element too large for a blob with only its own strings, and a blob whose
  elements carry fields outside the format where cutting needs those
  changes.
- **`sort`, `cat --dedupe` and history `time-filter` cut an output block
  past 32 MiB.** They pack records from several input blobs into new
  blocks of up to 8000 elements, so input whose blobs all fit (large
  relations or tag values spread over overlapping or many blobs) could
  build a block too large for the format, and the run failed with a size
  error, nothing published. Such a block is now cut between elements into
  blobs that fit, each indexed from its own content, as `renumber` cuts
  its blobs; every other block is written as before. Still refused: a
  single element too large for a blob with only its own strings.
- **`add-locations-to-ways` cuts a way blob its coordinates grow past 32
  MiB.** Each way blob was written as one output blob, so long ways whose
  nodes lie far apart could take an input blob that fit over the payload
  limit, and the run failed with a size error, nothing published. Such a
  blob is now cut between ways into blobs that fit, each indexed from its
  own content, under every index strategy. Under `--inject-prepass` each
  piece carries the `pbfhogg.WayMembers-v1` bits of its own ways, and a way
  blob with more ways than one blob header holds bits for is cut by way
  count instead of failing on the header. Still refused: a single way too
  large for a blob with only its own strings.
- **An incompressible block near 32 MiB gets the typed size refusal under
  zlib.** The zlib encoder reserved about 0.1 % over the input and failed
  with an internal "did not complete in one call" error when level 1's
  output grew past that, before the blob-size check could name the
  problem. It now reserves the backend's worst case and continues when
  short, so such a block is refused as too large, or written when it fits.
- `diff --format osc -o changes.osc.gz` caps gzip compression workers at
  the requested `--jobs` count when either input lacks indexdata.
- The `merge-workload` bench reads timing from a dedicated stdout line,
  so input paths in diagnostic output cannot supply a fake timing.

- `renumber --compression zlib` and `--compression zlib:6` now use level
  6. Omitting the CLI flag still uses level 1. The library honors the
  compression in `WriteOptions`, whose default is level 6.

- `PbfWriter::write_header` now returns a cancellation error before
  writing any bytes when its invocation is already cancelled.

- Corrected the osmium parity guides to use `inspect --extended` and
  osmium's `header.boxes` key, and to distinguish osmium's whole-file
  `show` pager from the single-element lookup in `inspect --show`.

- Documented that `inspect --show` refuses `--distrust-metadata`.

- **`add-locations-to-ways` accepts indexed way blocks in a non-default
  coordinate framing.** A way block whose `granularity` was not 100 or
  whose coordinate offsets were nonzero failed the run on indexed input,
  though the unindexed `--force` path accepted it. The way block is now
  written in the default framing, keeping its `date_granularity`. Such a
  block that also holds nodes is still refused (exit 2), and so is a way
  block whose `date_granularity` does not fit `int32`.

- **`add-locations-to-ways --index-type external` counts the nodes of a
  node blob it drops whole.** Removing untagged nodes, the external join
  skips a blob of only untagged non-member nodes without decoding it, and
  its nodes were left out of the summary: a file of only such nodes
  reported `0 nodes read, 0 written, 0 dropped`. They now count as read
  and dropped by the blob's indexdata count, matching the sparse index.
  `Stats::merge` now returns a `Result`, refusing as invalid data a
  statistic that overflows `u64` from claimed counts instead of wrapping.

- **`degrade --corrupt-tagdata`, `--corrupt-indexdata` and
  `--corrupt-indexdata-kind` no longer publish an oversized `BlobHeader`.**
  The replacement hint could grow a `BlobHeader` already near the 64 KiB
  limit past it (a large WayMembers or unknown field, or a 26-byte v1 index
  rewritten as 42 bytes). Such a blob is now refused with a header-size
  error (`OversizePart::RewrittenHeader`, a new variant) and nothing is
  published.

- **`apply-changes --direct-io` and `renumber --direct-io` read the whole
  input `O_DIRECT`.** The flag reached only the front or header read; the
  header walks, the worker preads and (apply-changes) the copied ranges
  stayed in the page cache. Every PBF input read is now direct; the OSC
  read stays buffered. Planet apply-changes wall is unchanged.

- **A `--direct-io` input replaced mid-run by a FIFO, or by a file on a
  filesystem without direct I/O, fails as a replaced input, not exit 3.**
  Where inputs reopen through their path (no usable `/proc/self/fd`), the
  refused `O_DIRECT` open was reported as direct I/O being unavailable.

- **An O_DIRECT read refuses a blob payload cut after the input was
  opened, when the payload ends on an aligned offset.** The skip checked
  the length captured at open, so the walk could end cleanly short of the
  declared payload.

- **`merge-changes` and `tags-filter --input-kind osc` accept a hidden node
  without coordinates.** A `<create>` or `<modify>` node with
  `visible="false"` (an OSM deleted version, which carries no location)
  applied directly but was refused by both re-emitters. They now treat it
  as a delete's location: none, or both usable coordinates; half a location
  is refused. It keeps its section and `visible` attribute.

- **`export --metadata` writes only RFC 3339 `@timestamp` values, with
  milliseconds kept.** A timestamp in year 10000 or later printed with a
  five-digit year, and a sub-second one lost its fraction, so 1 ms printed
  as the epoch. A year outside 0000 to 9999 is now refused, naming the
  element, and a sub-second timestamp prints its exact milliseconds as
  `.SSS`. Whole-second timestamps in years 0000 to 9999 print as before.

- **`inspect --extended` keeps the timestamp range's milliseconds and its
  extreme values.** The range truncated toward zero, so 1500 ms printed as
  one second and -1 ms as the epoch; it now prints the exact `.SSS` when an
  extreme is not a whole second. A sole timestamp of the largest
  representable millisecond, or a box whose every latitude was the largest
  value, was counted but omitted from the text, JSON and `--get` output.

- **A cancelled `into_blocks_pipelined` iterator stops at the next call.**
  Cancelling the invocation used to let `next()` keep returning blocks the
  pipeline had already queued, and a pipeline that had finished reading
  ended in a clean `None` with the cancel never reported. A `next()` that
  begins with the invocation cancelled now returns `Cancelled` at once,
  whatever was read ahead (a queued error included), and the iteration
  ends; any error item now ends the iteration too.

- **A panic payload whose destructor panics no longer aborts the process
  at teardown.** A caller `Read` or `Write` that panicked with such a
  payload could abort when a pipelined block iterator, a writer or a
  worker pool joined the thread during another unwind, or turned the
  promised `Panicked` error into a second panic. Caught payloads that are
  not a plain message are now forgotten instead of dropped.

- **A panicking `DiagnosticSink` can no longer abort the process.** A
  panic in a sink during cleanup that ran while another panic unwound
  (a failed scratch removal, or a `SummaryWriter` resending its line
  from its destructor) was a double panic and aborted. A sink panic is
  now contained like an `InstrumentSink`'s: the sink is switched off for
  that `Diagnostics` and its clones, the run carries on, and the new
  `Diagnostics::sink_failed` reports the stream incomplete. Output
  cleanup warnings are now also delivered while a panic unwinds, where
  they used to be skipped. `SummaryWriter::flush` now delivers the
  pending unterminated fragment as one event instead of doing nothing.

- **`DirectWriter` stops at its first I/O error.** A failed final flush
  used to make every later `flush` return `Ok` without writing the tail
  or truncating the padding, and a write whose buffer flush failed counted
  bytes it never took, so a later flush could zero-extend the file or
  write a prefix twice. Now a failure is reported again by every later
  `write` and `flush`, `write` returns only bytes it actually accepted,
  and a failed writer's file is left as it is on drop.

- **A failing stderr or `--help` write is an I/O error, exit 2.** A stderr
  write that failed other than by its reader going away (`2>/dev/full`, a
  full disk behind `2>file`) panicked and exited 101, the defect status. It
  now stops the diagnostics, lets the command finish, and turns a completed
  run's `0` or `1` into `2`; a failure status stands. `--help` and
  `--version` whose text cannot be written exit `2` instead of `0`. A closed
  pipe on either stream is unchanged.

- **`inspect --show` no longer misses an element under a false sorted
  header.** On a file claiming `Sort.Type_then_ID` it stopped at the first
  blob whose ID range lay past the target, so blobs out of order under that
  claim could make it report a present element as absent (exit 1). A
  lookup now ends only at the element or at the end of the file. Blobs
  that indexdata rules out are skipped without reading their payloads, so
  a hit is usually faster than before, and a miss costs a walk over every
  blob header.

- **`--clean version` keeps a dense node's zero-valued claims.** `cat`
  and `extract` with `--clean version` wrote a versioned dense node as a
  versionless dense slot, which reads a timestamp of 0 (the epoch), a
  changeset or uid of 0 and an empty user as absent, so those claims were
  lost. Such a node is now written as a plain node carrying the fields not
  cleaned; nodes without such a value stay dense, and cleaned fields still
  claim nothing.

- **`diff --format osc` keeps a hidden element hidden.** A created or
  modified element whose PBF metadata says `visible=false` used to be
  written without the attribute, so applying the OSC made it a visible
  object. It is now written with `visible="false"`, which `apply-changes`
  (and osmium) apply as the object's deletion. An explicit `true` is still
  not written. A change of visibility alone (hidden or unhidden with
  unchanged content) used to get no OSC entry at all; it is now a modify
  of the new record carrying its new version. The text `diff` still
  compares content only.

- The CLI reference now explains when `diff` and `diff --format osc`
  report a sequential fallback for unindexed input. If you rely on that
  note, pass `--jobs 2` to get it even on a host that reports one worker.

- **An element timestamp whose millisecond scaling overflows is refused,
  not wrapped.** `time-filter`, `export --metadata` and `digest --form
  full` used to read such a timestamp wrapped to an arbitrary value: a
  plausible date that time-filter kept or dropped by a meaningless
  comparison, or exactly 0, which on a versionless dense node read as
  padding. Each now refuses it as invalid data naming the element, as
  `diff --format osc` already did; `inspect --show` prints
  `timestamp: <invalid>`. `inspect --extended` refuses the blob holding
  one, where its metadata coverage used to drop the timestamp or its
  timestamp range to show an arbitrary date. time-filter now judges timestamps in whole
  seconds rounded down, so a pre-1970 sub-second timestamp belongs to the
  second before it rather than the one after. Re-encoding commands copy a
  timestamp at the default `date_granularity` verbatim, so such a value
  is no longer rewritten as a different one.

- **`degrade --unsort` and `--unsort-intra` perturb history input.**
  The swap could pick two versions of one ID, leaving the ID order
  unchanged and the output not unsorted at all. Both modes now move
  whole ID runs: `--unsort` moves the version at the cap and its ID's
  later versions past the next ID, and `--unsort-intra` swaps the first two IDs with
  all their versions in one blob, leaving a kind unperturbed when those
  records do not fit `--elements-per-blob`. Unique-ID input produces the
  same output as before.

- **`sort` orders the versions of one object whatever the blob layout.**
  A blob holding `way 7` version 2 before version 1 that overlapped no
  other blob was copied through with its versions descending, while the
  same records split across overlapping blobs came out ascending. Every
  blob `sort` decodes in pass 1 (an input without the
  `Sort.Type_then_ID` claim, or any input under `--distrust-metadata`)
  is now rewritten when its versions of one ID descend, so history input
  comes out in osmium sort's (type, ID, version) order. Under the claim,
  the default fast path still trusts each blob's version order.

- **`inspect --show` prints every metadata field the element carries.**
  A present timestamp, changeset or uid of 0 and an empty user name were
  dropped, so they read like absent fields, and visibility was never
  printed, so a deleted history object looked visible. They now print,
  the empty user as `user: ""`, plus a `visible: true|false` line
  whenever the element states visibility. A negative sub-second
  timestamp now floors to the earlier second instead of printing 0.

- **`export` writes only positions inside the GeoJSON coordinate range.**
  A tagged node at latitude 100, or a way carrying libosmium's undefined
  location (`214.7483647` on both axes), was written as is. A node or way
  with any emitted position outside longitude -180 to 180 or latitude -90
  to 90 is now skipped and counted; the stderr summary gains an
  `N invalid nodes` count before the invalid ways, and the library's
  `ExportStats` gains `skipped_invalid_nodes`.
- **`export` skips degenerate Polygons.** An area way whose ring has zero
  area as written - collinear positions, or distinct positions that
  rounding to seven decimals collapses - was emitted as a Polygon. It is
  now skipped and counted among the invalid ways.
- **`export` winds every Polygon exterior ring counterclockwise.** The
  orientation was judged with a floating-point area over absolute
  coordinates, which cancels to zero for small rings away from the
  origin, so such a ring could be written clockwise. It is now judged
  exactly on the coordinates as written. An area way with a coordinate
  that does not format as a decimal number is skipped and counted as
  invalid geometry.

- **`degrade --drop-ids N:SEED` counts N in distinct IDs.** On input that
  repeats a `(kind, id)`, such as a history file with one record per
  version, selection used to count records, so N could select fewer than
  N IDs, and N above the distinct-ID count passed the size check. It now
  removes exactly N distinct IDs with every record of each, refuses N
  above the distinct count before writing output, and the summary's
  `dropped M elements` (and `DegradeStats::dropped`) is the measured
  number of records removed rather than the requested N. Selection on
  unique-ID input is unchanged, seed for seed.

- **`geo` ring functions always return, and the E7 longitude helpers no
  longer overflow.** `geo::point_in_ring_with_antimeridian`,
  `point_in_polygon` and `simplify_ring` looped forever on a ring with an
  infinite or huge longitude; they now document finite WGS84 degrees as their
  domain, with unspecified answers outside it, and always terminate. Answers
  for in-domain rings are unchanged. `geo::wrapped_longitude_delta_e7` and
  `normalize_longitude_e7` are total over `i64`; before, values near
  `i64::MAX` panicked or returned a wrong longitude.

- **OSC coordinates round from their decimal text, as osmium does.**
  `apply-changes`, `merge-changes`, `tags-filter --input-kind osc` and the
  library OSC overlay read a coordinate with eight or more fraction digits
  that sits on a half-unit tie, such as `39.00704275`, as its exact decimal
  rounded half away from zero (`39.0070428`); before, floating-point scaling
  could round it down. Seven-digit coordinates were never affected. An
  exponent spelling is rounded from its exact value, which can differ from
  osmium (see DEVIATIONS.md).
- **Header rewrites keep same-number unknown fields.** `degrade`'s
  header-only path (`--strip-bbox`, `--strip-indexdata`, `--strip-tagdata`,
  `--generator`, `--output-header`) and the reframes of `cat --reindex`,
  `cat --repair`, `sort` and `cat --dedupe` removed every occurrence of the
  field number they replace. They now remove only the occurrences pbfhogg
  reads as that field (same number and wire type); one with another wire
  type, which readers skip as unknown, is copied through like any other
  unknown field.
- **inspect --blocks takes a huge limit.** A text-mode `--blocks N` whose
  doubling overflows (for example `9223372036854775808`) panicked; it now
  lists every block, as the JSON output already did.
- **renumber takes plain nodes and ways without node refs.** A node block
  holding plain (non-dense) node groups, which pbfhogg itself writes for
  nodes whose metadata a dense block cannot carry exactly (for example
  version 0 with an explicit uid 0), failed with an error calling the
  group mixed; a way with no node refs, which the writer encodes without
  a refs field, failed with `no refs field`, so renumber's own output with
  such a way could not be renumbered again. Both now renumber; plain nodes
  stay plain with their metadata intact.

- **A first data blob cut short is refused by the indexdata probe.**
  `inspect --indexed`, `has_indexdata` and every command's indexdata
  precondition answered from the first data blob's header without
  checking its payload was there, so a file ending inside that payload
  got a yes or no verdict. It is now the located truncation error (exit
  2), ahead of the missing-indexdata refusal even under `--force`.

- **OSC output never contains characters XML 1.0 forbids.** A tag,
  member role, user or metadata value holding NUL, another C0 control
  besides tab, LF and CR, or U+FFFE / U+FFFF used to be written out raw
  by `diff --format osc` (from a PBF string) and by `merge-changes` and
  OSC `tags-filter` (from an input reference such as `&#1;`). The result
  was a file that conforming XML readers, osmium's among them, reject.
  These runs now fail with exit 2, naming the element and attribute, and
  publish nothing.
- **`extract --config -d DIR` keeps every output under `DIR`.** A
  configured output such as `../x.pbf`, or an absolute output below the
  configured directory whose rest holds `..`, used to be written outside
  the override; an empty output could name the override itself. These are
  now refused before any work. With a relative `--config`, absolute
  outputs below the configured directory used to lose their
  subdirectories (and could collide); they now keep them. Library:
  `parse_extract_config_with_output_directory` refuses an absolute output
  when given a relative config path.

- **A closed stdout no longer hides a real failure.** Any failed command
  used to exit 0 silently once a stdout write had met a closed pipe
  (`pbfhogg ... | head`), even when the closed pipe was only met while
  cleaning up after another error. For example, `export --format geojson`
  on a truncated input exited 0 when its buffered FeatureCollection opening
  was flushed into the closed pipe. The status now comes from the error the
  command returned. Only a run that stopped because of the stdout write
  ends quietly with 0. Other errors keep exit 2 and print their `Error:`
  line. `inspect --get` now fails on a key with no value before it writes
  anything. A `diff` that already found differences before its final stdout
  flush now keeps exit 1.
- **A PBF requiring a feature pbfhogg does not implement is refused.** A
  header `required_features` entry other than `OsmSchema-V0.6`,
  `DenseNodes` or `HistoricalInformation` used to be ignored: the file was
  read as if it were plain, and a rewriting command wrote a header without
  the requirement. Every library header decode, and so every command that
  reads elements, now fails with the new
  `ErrorKind::UnsupportedRequiredFeature`, naming the feature, as osmium
  does (`inspect` included). Matching is exact, so prefixes osmium happens
  to accept are refused. `degrade`'s header-only passthrough, which decodes
  no block, still runs and keeps the requirement in its output header. The
  public `has_indexdata`, `has_indexdata_in` and `has_tagdata` probes, and
  so `inspect --indexed`, refuse such a file too (exit 2) rather than
  answering from the blob framing.

- **`apply-changes` no longer refuses changes whose output only needed
  more blocks.** A rewritten block keeps its source block's whole string
  table, and blocks were cut by element count alone. So a large tag value
  modified to another large value (two 17 MiB values), or two large
  creates side by side, made one block past the 32 MiB PBF payload limit,
  and the run failed (exit 2), although every element fitted on its own.
  This broke the `diff --format osc` roundtrip. Such output is now written
  as more blocks. An element that does not fit a block alone, or one base
  blob whose re-cut output would pass 256 MiB, is still refused before
  publication.
- **A sign inside an RFC 3339 timestamp is refused.** The `time-filter`
  cutoff, the `osmosis_replication_timestamp` header override and OSC
  `timestamp` attributes read by `apply-changes` accepted a leading `+`
  or `-` in a component, so `2026-01-01T-1:00:00Z` silently became
  `2025-12-31T23:00:00Z` (also when spelled `T&#45;1` in OSC). Every
  component must now be ASCII digits. Signed UNIX seconds are still
  accepted where they were. A derived OSC element dated before year 0000
  is now refused by `apply-changes` rather than read back.

- **OSC input reads character references in IDs, refs, coordinates and
  numeric or timestamp metadata.** `id="&#49;"`, `ref="&#50;"`,
  `lat="&#49;"`, `version="&#49;"` and the like were refused as not a
  number by `apply-changes`, `merge-changes` and OSC `tags-filter`; they now
  read as the value the reference spells, as any XML reader gives it.
- **`merge-changes` and OSC `tags-filter` keep a referenced newline, carriage
  return or tab in `timestamp`, `changeset`, `uid` and `visible`.** They
  wrote it back literally, so the next reader saw a space instead; it is now
  written as a character reference, as `user` already was.
- **`merge-changes` and OSC `tags-filter` write back an explicit empty
  metadata attribute.** They dropped `user=""`, `uid=""`, `changeset=""` and
  `timestamp=""`, so a blank-user claim `apply-changes` records became no
  user once re-emitted, and a change `apply-changes` refuses for its empty
  `uid` (or `changeset`, `timestamp`) applied cleanly after re-emission. Each
  is now written as read; apply-changes reads the same claim, or refuses the
  same change, on both routes. osmium omits an empty user (DEVIATIONS.md).
- **An empty `visible=""` in an OSC is read as absent.** `apply-changes`
  wrote it into the PBF as an explicit `visible` true claim, while
  `merge-changes` and OSC `tags-filter` write it as absent, so the same
  change applied after re-emission claimed nothing. Every consumer now reads
  it as no visibility claim; osmium refuses the file (DEVIATIONS.md).
- **A parallel or io_uring writer that cannot start fails at construction.**
  The parallel (pwrite) writer reported itself started before its worker
  threads existed, so a refused thread spawn surfaced only at a later write
  or the final flush; the io_uring writer did the same for a header whose
  submission failed. `PbfWriter::to_path_parallel` and `to_path_uring` now
  return those errors themselves.
- **External `add-locations-to-ways` stops promptly when its join fails.**
  Stage 4 did not watch stage 3's failure while copying passthrough blobs,
  so under `--keep-untagged-nodes` it copied the whole untagged node prefix
  (gigabytes at planet scale) before reporting the error.
- **OSC inputs may be streams.** `apply-changes`, `merge-changes` and
  `tags-filter --input-kind osc` read an OSC from a FIFO or `/dev/stdin`.
  Before, the gzip sniff sought back to the start, so a stream was opened,
  partly read and then refused. `tags-filter` without `--input-kind` no
  longer reads a stream to sniff its kind, which dropped its first four
  bytes; it routes a stream by its name. Its content sniff now needs Linux
  with `/proc`; elsewhere the kind comes from the name.
- **musl builds: refusing a FIFO input no longer swallows its writer's
  data.** The check that an input is a regular file opened the path as a
  reader there, so a waiting writer connected, wrote into it, and lost
  what it wrote when the refusal closed it.
- **`visible="&#102;alse"` (any XML-escaped `false`) now hides an element
  in `apply-changes`,** as it already did once re-emitted by
  `merge-changes` or OSC `tags-filter`, so both routes give the same
  result.
- **Replacing a directory output (`build-geocode-index --overwrite`) no
  longer deletes the contents of a filesystem mounted inside it.** The old
  directory was moved aside with its mounts and then removed recursively,
  descending into them. An output directory with any mount point below it,
  a bind mount over a single file or of the same filesystem included, is
  now refused before any work with the new `OutputError::HoldsMount` (CLI
  exit `2`), and a backup or unpublished staging tree found holding a mount
  at cleanup is kept and reported rather than removed. On Linux the check,
  and the existing refusal of an output directory that is itself a mount
  point, read the kernel's mount identity of each entry instead of matching
  paths against `/proc/self/mountinfo`, so a path spelled differently from
  the mount table can no longer slip past it.
- When the OS refuses to create a worker thread (`EAGAIN` under a thread
  or memory limit), every library command and reader returns an
  `ErrorKind::Io` error (CLI exit `2`) instead of panicking, and no longer
  risks hanging with threads it had already started waiting on the one
  that never started. This covers `apply-changes`, `diff`,
  `add-locations-to-ways`, `build-geocode-index`, the classify scans behind
  `check`, `extract`, `getid`, `tags-count` and the rest, the pipelined
  readers, `ElementReader::par_map_reduce`, `into_blocks_pipelined` and the
  pipelined `PbfWriter` constructors. A refused thread is never
  worked around by running with fewer.
- Indexed `diff` (text, `-c`, `-q` and `--format osc`) refuses, with exit
  `2`, every blob it decodes whose indexdata is not exact (a count, least
  or greatest ID that is not what the blob holds) or whose elements are out
  of canonical order, and a blob whose range spans negative and positive IDs
  that starts before the blob preceding it ends. A false range could
  report one element both created and deleted, hide elements behind a
  work-unit cut, or pair identical files as different; a false count gave
  wrong created and deleted totals. Honestly indexed sorted files diff as
  before. A byte-identical pair skipped without decoding (`-c`, `-q`,
  `--format osc`) is still believed; `check --index` verifies it.
- The `build-geocode-index` scratch report makes removal of legacy buckets
  conditional on publication removing the old index directory.
- `build-geocode-index --overwrite` replaces only a directory whose
  `geocode_header.bin` really is an index header: a regular file, not a
  symlink, a header long and starting with the format's magic. Any other
  entry of that name (an empty or unrelated file, a directory, a symlink)
  no longer authorises deleting the directory. Indexes of every earlier
  format are still recognised and can be rebuilt in place.
- Replacing a directory output no longer deletes a backup, staging entry or
  run directory that sits inside an index's `.buckets-level*` scratch tree;
  the replacement is refused naming it, as anywhere else in the directory.
- The library's `build_geocode_index` now refuses to replace an index that
  holds its own input (the path given, or the input file's last link), as
  the CLI already did, with the new `OutputError::HoldsInput`. The check is
  repeated just before publication and compares directories by identity,
  so an input moved into the index mid-build or reached through a bind
  mount is caught; a backup holding any link to the input is kept.
- `extract` no longer moves a node whose block framing puts it beyond an
  `i32` decimicrodegree. Complete-ways, smart, unsorted simple and the
  shared multi-extract route wrapped such a coordinate (a latitude offset
  of 429.4967296 degrees became 0) and could select and write the node at
  the wrong place, where sorted simple refused it; every strategy now
  fails with the same error on any such node it classifies or writes. `build-geocode-index` judges each node's
  geographic domain on the exact nanodegree value before truncation, so a
  wrapped or just-out-of-range latitude fails the build instead of being
  indexed. `Node` and `DenseNode` gain `decimicro_lat_checked`,
  `decimicro_lon_checked`, `nano_lat_exact` and `nano_lon_exact`.
- `diff` on indexed inputs no longer depends on how a repeated ID falls
  across blobs. Two identical records of one ID, split into two blobs on
  one side and packed into one on the other, were reported as one common,
  one created and one deleted (the unindexed comparison said identical),
  and `diff --format osc` turned that into a create and a delete that
  removed the object when applied. Occurrences of an ID now pair in the
  order they occur on every route. `diff --format osc` now refuses
  (exit 2, nothing published) a change to an ID that occurs more than
  once on either side: the OSC states each object once and applying it
  rewrites every occurrence, so it cannot carry a change to one of them.
  Repeats unchanged on both sides still pass.
- `apply-changes` no longer silently drops OSC creates (and modifies of
  absent IDs) for a kind that sorts before the base's first kind. A base
  starting with ways lost the OSC's new nodes, and one starting with
  relations lost its new nodes and ways. The run succeeded and wrote a
  file missing them; under `--locations-on-ways` the new ways still
  carried the dropped nodes' coordinates. They are now written in sorted
  position, before the base's first blob.
- `add-locations-to-ways --index-type external` (and Auto when it routes
  there) no longer hangs when the system refuses to start one of its
  stage 3 or stage 4 threads (thread or process limits reached). Such a
  refusal used to be able to leave stage 4 waiting forever for
  coordinates no thread would produce. The run now fails with the I/O
  error and publishes nothing.
- `add-locations-to-ways --index-type external` (and Auto when it routes
  there) no longer publishes ways with vertices at (0, 0) when it cannot
  read a scratch slot bucket back. A failed `stat` on a bucket used to be
  read as "empty", so every resolved vertex in that bucket was written as
  (0, 0) with no missing location reported, even without
  `--ignore-missing-nodes`. The run now fails (exit 2) with nothing
  published, as it also does when a bucket file is shorter or longer
  than what the run wrote to it.
- `PrimitiveBlock::from_vec` refuses a buffer over 256 MiB with
  `ErrorKind::WireFormat`. A buffer of 4 GiB or more used to wrap the
  block's internal string-table offsets, so string lookups could return
  a `&str` over bytes that were never checked as UTF-8 (undefined
  behaviour reachable from safe code). Blocks decoded from a PBF are
  capped at 32 MiB already and are unaffected.
- The io_uring writer (`--io-uring`, `PbfWriter::to_path_uring`) no longer
  mishandles a failed `io_uring_enter` after a write was queued: the write
  is now counted from the moment it is queued, so teardown waits for it
  instead of panicking on a miscount or waiting for a completion that never
  comes, and a teardown that cannot confirm the kernel is done with its
  buffers keeps them allocated rather than freeing memory a pending write
  may still copy into the output. An interrupted wait (EINTR) is retried
  instead of failing the run.
- `build-geocode-index` stores admin boundary vertices exactly: converting
  a kept vertex back to decimicrodegrees truncated a value like
  550000006.9999999 to the unit below, so even `--admin-tolerance 0`,
  documented as keeping every vertex, moved boundaries by up to a
  decimicrodegree. Rebuild an index to gain it.
- `build-geocode-index` no longer drops a named highway that also carries
  `addr:interpolation` from the street set; such a way is now answered as a
  street (and still as an interpolation range where its ends allow one).
  Rebuild an index to gain them.
- `extract --simple` on unsorted input no longer drops ways or relations
  behind a misleading block label. Its per-kind sweeps skipped blocks by
  `block_type()`, so a way after an empty dense message, or a relation in a
  ways-first group, was never matched. `digest` and `diff`'s streaming merge
  now classify blocks by the same exact kind set: `digest` accepts a
  single-kind block whose groups also hold an empty dense message or a
  changeset or unknown field, and refuses only blocks whose elements are of
  more than one kind.

- `BlockBuilder` no longer drops a way or relation whose every field is
  default (id 0, no tags, refs, members or metadata): it was written as
  nothing while the blob index still counted it. Way and relation ids are
  now always written, 0 included, as OSMPBF requires, and
  `add-locations-to-ways` keeps such a way instead of losing it, so its
  output carries an id a later run accepts.

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

- **Negative input IDs are now rejected at the input boundary** in `renumber` (every `IdSet` entry point) and in `getid` (parse-time, matching osmium's getid). Previously `renumber` could panic or produce phantom orphans, and `getid` silently dropped negative-id queries with a misleading `"no IDs specified"` message. The stance is recorded in ADR-0002, since superseded by ADR-0028.
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
