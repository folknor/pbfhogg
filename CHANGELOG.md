# Changelog

## Unreleased

### Breaking

- **`PbfWriter::into_inner` returns a `Result`.** On a pipelined writer,
  whose output file lives on the writer thread, it used to panic; it now
  returns an invalid-argument error, as the writer's other methods do.
- **apply-changes no longer claims the base's replication position.** Its
  output used to carry the base's `osmosis_replication_timestamp` and
  sequence number although it holds a later state. It now drops both (as
  osmium does) and keeps the base URL; an updater that knows the state of
  the diff it applied must stamp it with `--output-header` (library:
  `HeaderOverrides`). A tool that bootstraps from the header, such as
  `osm2pgsql-replication init --osm-file`, refuses an unstamped output
  instead of resuming from a stale position. `bench-merge` follows suit.

- **renumber writes no replication state.** Its new IDs are not the feed's,
  so a header naming the feed and position invited updaters to apply diffs
  that address other objects. osmium renumber writes none either.

- **An empty `--output-header` value omits the field.**
  `--output-header osmosis_replication_base_url=` used to write a present,
  empty base URL, and an empty timestamp or sequence number was refused; any
  of the three keys with an empty value now removes that field from the
  output, whatever the command would have written, as in osmium. It works
  on degrade's header-only passthrough too. `--generator ""` still writes an
  empty writing program. Library: `HeaderOverrides`' replication fields are
  now `FieldOverride` (`Unchanged`, `Set`, `Clear`) instead of `Option`, the
  `OutputHeaderEntry` variants carry an `Option` (`None` clears), and
  `HeaderBuilder` gains `without_replication_timestamp`,
  `without_replication_sequence_number` and `without_replication_base_url`.

- **`DiffOptions::summary` is removed.** The library never read it: the
  caller chooses between `DiffStats::write_summary` and
  `DiffStats::write_osmium_summary` itself. Delete the field from any
  `DiffOptions` literal.

- **A text input that is not UTF-8 is `ErrorKind::InvalidData`, not `Io`.**
  The `getid` / `getparents` ID files, `extract` configs and region files
  used to report non-UTF-8 content as an I/O error; they now report it as
  malformed input, as the `tags-filter` expressions file already did, still
  naming the path and keeping the decoder's error as the cause. A library
  caller matching on `ErrorKind::Io` for that case must match
  `InvalidData`. The expressions file's message now reads `reading the
  expressions file: failed to read <path>: ...` instead of `could not read
  expressions file '<path>'`.

- **`diff --format osc` exits 1 when the inputs differ.** It used to exit 0
  whatever it found, unlike a text `diff`; it now answers the same way
  (`0` identical, `1` different) and still publishes its `-o` in both
  cases. A script that treated any non-zero exit from an OSC diff as a
  failure must accept `1`.

- **`--type` is one comma-list grammar on every command, and `getid
  --verbose-ids` is now `-v, --verbose`.** `--type` takes any subset of
  `node,way,relation` through a single parser. `inspect tags`, which counts
  one kind, now refuses a list of more than one with a message saying it
  takes exactly one; `export` no longer takes an enum and accepts `node`,
  `way` or `node,way`, refusing `relation` with a message naming what it
  accepts. `getid --verbose-ids` is renamed `--verbose` (short `-v`, the
  spelling `diff` already used); the old name stays as a hidden flag that
  refuses and names `-v, --verbose`, so a script fails loudly instead of
  silently losing the diagnostic.

- **Every OSC producer frames its output by the output name, and refuses
  other names.** `merge-changes`, `tags-filter --input-kind osc` and
  `diff --format osc` (and the library functions behind them) write plain XML
  to a name ending `.osc`, gzip to a name ending `.osc.gz`, and refuse any
  other name before any work, with a usage error naming both suffixes. This
  breaks `tags-filter --input-kind osc` and `diff --format osc` runs that
  wrote gzip to an `.osc` name (they now write plain XML), `merge-changes`
  runs whose name ended `.gz` but not `.osc.gz`, and every run that wrote to
  some other suffix. The match is case-sensitive: `.OSC` is refused. A
  non-UTF-8 name is judged like any other (it used to be written plain by
  `merge-changes`). New public function `osc::output_is_gzip` carries the
  rule.

- **A partially indexed input is refused instead of run on an index it
  lacks.** A file whose first data blob carries indexdata and a later one
  does not used to pass the first-blob indexdata check. Every command that
  already reads every blob header to plan its work now refuses it with the
  missing-indexdata error, located at the first data blob without
  indexdata: sort, apply-changes, repack, degrade, tags-count,
  extract, add-locations-to-ways, build-geocode-index,
  `inspect --nodes`, `inspect tags`, `check --refs`,
  `check --ids` and digest. `--force` and `--allow-unindexed` do not lift
  this refusal; `pbfhogg cat` re-indexes such a file. A wholly unindexed
  input is handled as before. `ErrorKind::MissingIndexdata` gains a
  `partial` field that marks this case.

- **`cat --clean` writes in input order, needs no indexdata, and takes
  `--force` only with `--type`.** It used to run one pass per element kind,
  so an unsorted input came out regrouped as nodes, ways, relations, and a
  blob without indexdata was decompressed once per kind; an unindexed input
  therefore needed `--force`, and a partially indexed one was refused. It
  now decodes every blob once and writes the kept elements in the order it
  read them (a sorted input stays sorted). Without `--type` it neither asks
  for indexdata nor accepts `--force`; with all three kinds in `--type` it
  does not ask either (`--force` is accepted there and does nothing); with a
  narrower `--type` it still asks, since it skips blobs of other kinds on
  their indexdata. A partially indexed input is accepted in every `cat`
  mode. The summary's "Decoded N blobs" now counts input blobs decoded
  rather than blobs written.

- **Element caps are `NonZeroUsize`.** `repack::repack`'s
  `elements_per_blob`, `degrade::degrade`'s `block_cap`,
  `BlockBuilder::with_element_cap_prealloc` and
  `block_builder::DEFAULT_ELEMENT_CAP` take or are a `NonZeroUsize`, so a
  zero cap cannot be passed rather than being refused at run time.

- **`PbfWriter::write_raw_copy` takes a `&read::input::InputFile`** (an open
  file together with the path it came from, `InputFile::open(path)`) instead
  of an `&Arc<File>`, so a failed copy names the input and the byte range.

- **Library errors name no CLI command or flag.** Refusals that used to
  carry `pbfhogg ...` recipes or `--flag` advice in their text are typed
  instead, and a front end adds its own advice: new `ErrorKind::Unsorted`,
  `IndexdataMismatch`, `MissingNodeLocations`, `BlobTooLarge` (with the new
  `OversizePart`) and `VersionRegression`. `MissingIndexdata`'s text no
  longer includes the `pbfhogg cat` recipe. Code that matched on error text
  should match on the kind. The `pbfhogg` CLI prints the same advice as
  before, now naming the actual input.

- **Errors name their files.** A failed open or read of an input (PBF, OSC,
  ID list, GeoJSON, extract config, expressions file, geocode index file)
  says `failed to open <path>` or is located `(in <path>, ...)`; a failed
  pread in a parallel scan names the file, blob and byte offset; a failed
  write says `writing <output>: ...` (an `ENOSPC` used to arrive as a bare
  "No space left on device"). An `io::Error` returned by an `Output` writer
  now carries the library error inside it, so its `raw_os_error()` is
  `None`; the original OS error is reachable through
  `pbfhogg::Error::from(err).kind()`.

- **Diagnostic wording changes:** `add-locations-to-ways`' automatic choice
  reads `auto-selected index type <type> (...)` (was `--index-type`),
  `repack`'s unused-cap note reads `the element cap of N per blob never
  fired`, and `degrade`'s summary lists the transformations applied by name
  (`applied: unsort strip_locations`, was `--unsort --strip-locations`).

- **`HeaderBuilder::from_header` is removed.** Build the header from the
  fields you want it to declare (`bbox`, `source`, the replication setters,
  `historical`, `sorted`). To carry an input header's bounding box as it was
  read, use the new public `HeaderBuilder::input_bbox`: unlike `bbox`, it
  does not refuse an inverted or out-of-range box. The commands derive
  theirs through one output header policy that is not public API.

- **The writing program is now `pbfhogg/<version>`** (the library's package
  version), as osmium writes `osmium/<version>`; it was `pbfhogg`.
  `HeaderBuilder::writing_program` and `--generator` still override it.
  An empty `source`, replication base URL or writing program (`--generator
  ""`, `--output-header osmosis_replication_base_url=`) is now written as a
  present, empty field and kept, not dropped.

- **Every short flag now has one meaning across all subcommands.** Shorts
  that meant something else on one command are long-only there:
  `cat --clean` (was `-c`), `extract --config` (was `-c`) and `--simple`
  (was `-s`), `renumber --start-id` (was `-s`), `getparents --add-self`
  (was `-s`), `inspect --extended` (was `-e`), `inspect tags --sort` (was
  `-s`), `tags-filter --remove-tags` (was `-t`) and `--invert-match` (was
  `-i`), `getid --remove-tags` (was `-t`). The old letter is refused on that
  command with an error naming the long flag, so an old command line never
  silently means something new. The vocabulary is in
  `reference/cli-reference.md`, "Short flags". New shorts:
  `build-geocode-index -o` (`--output-dir`), `export -t` (`--type`) and
  `-b` (`--bbox`), `check -q` (`--quiet`).

- **A flag the command line makes ineffective is refused, not warned about
  or ignored**, as a usage error (exit `2`) before any input is opened:
  `-O` without `-o` (`export`, `diff`); `cat --force` without `--type`,
  `--clean` or `--dedupe`; `degrade --force` without a decode-path flag;
  `extract --simple --force`; `tags-filter --force` with `--invert-match` or
  `--distrust-metadata`; `tags-filter -R` with `--remove-tags` or any
  explicit `-j` (was a warning, and `-j 0` was accepted); `getid
  --remove-tags` without `-r` (was a warning); `add-locations-to-ways
  --force` with `--index-type external` or `--inject-prepass`, and
  `apply-changes --force --locations-on-ways` (all three used to be refused
  only on unindexed input, after the input was opened; on indexed input
  `--force` did nothing); `diff --tmp-dir` with text `-j 1` or `-v`; `diff
  -q` with `-c`, `-v` or `-s`; `check -q` with `--show-ids` or
  `--max-errors`; and in OSC-mode `tags-filter`, `--compression`,
  `--direct-io` and `--force`, which were accepted and discarded. `getid`
  and `getparents` with no ID source, `tags-filter` with no expressions and
  `degrade` with no transformation flag are refused by the argument parser.

- **Flags are checked before any input is opened.** Malformed values of
  `--output-header`, `--start-id`, the `time-filter` timestamp, `--bbox`,
  `--drop-ids` and `-j`, malformed expressions and positional IDs, and
  `--direct-io` / `--io-uring` in a build without the feature, are refused
  as usage errors before the command touches its input (a missing feature
  used to be found when a reader or writer first opened, minutes into some
  runs). `apply-changes` refuses an unsorted base, or `-l` on a base without
  `LocationsOnWays`, before parsing the OSC, and `-j 1` before anything.
  `add-locations-to-ways` and `diff` check their output before making their
  scratch directory, so a missing output directory reports the output.

- **`-j/--jobs` means one thing everywhere**: `0` (the default, unchanged)
  sizes the worker pools automatically, `1` runs sequentially, `N` caps them
  at `N`; one help text on every command. `tags-filter -j N` now also caps
  its final decode-and-filter pool, which ignored `-j` before.

- **Library: `pbfhogg::Parallelism { Auto, Fixed(NonZeroUsize) }`** replaces
  the jobs parameters: `TagsFilterOptions::parallelism`,
  `TagCountOptions::parallelism`, `MergeOptions::parallelism`,
  `DiffOptions::parallelism`, and the `parallelism` argument of
  `merge_changes`, `derive_changes` and `node_stats`
  (`Parallelism::from_jobs(n)` maps the old `0`-is-auto count). `diff` and
  `derive_changes` now resolve `Auto` themselves (one shard per core). New
  refusals, before any input is opened: `TagsFilterOptions::validate`
  (`omit_referenced` with `remove_tags` or a fixed parallelism),
  `MergeOptions::validate` (one worker; `force` with `locations_on_ways`),
  `AltwOptions::validate` (`force` with `IndexType::External` or
  `inject_prepass`), `getid` with `remove_tags` but not `add_referenced`,
  and `diff` with `verbose` and a fixed shard count above one.
  `ExportOptions::new` takes a parsed `Option<Bbox>`. New:
  `OutputHeaderEntry` (one parsed `--output-header` entry) with
  `HeaderOverrides::from_entries`, `tag_expr::validate_expressions`, and
  `OutputBackend::is_compiled`.

- **`build-geocode-index --force` warns** that it also grants
  `--overwrite`, naming `--overwrite` and `--allow-unindexed` as the flags
  to spell instead. It keeps working.

- **Ctrl-C and SIGTERM now clean up and exit 130 / 143.** The first SIGINT
  or SIGTERM stops any command at its next checkpoint: its output staging
  and scratch directory are removed, nothing is published (an existing
  output is left untouched), and it exits `130` (SIGINT) or `143` (SIGTERM)
  with `Error: interrupted by SIGINT: ...`. Before, the default disposition
  killed the process and left staging files and, for
  `add-locations-to-ways --index-type external`, `diff` and
  `build-geocode-index`, possibly hundreds of GB of scratch for the next run
  to report. A second signal exits at once with the same convention,
  leaving the rest for the next run's leftover scan. A signal that arrives
  after the run completed no longer changes its result. Scripts that
  matched the shell's own `130` keep working.

- **`Host` gains a `cancel: pbfhogg::host::Cancel` field**, and
  `Output::publish`, `OutputDir::publish` and `OutputSet::publish` take
  `&Cancel`, as does `Output::writer`. A struct literal adds
  `cancel: Cancel::new()`; callers that never cancel pass `&host.cancel`.
  `Cancel::cancel` from any thread makes a running command stop at its next
  checkpoint, remove its staging and scratch, publish nothing and return
  `ErrorKind::Cancelled` (`Error::is_cancelled` looks through context and
  `io::Error` carriers); `Cancel::check` gives a caller's own code between
  library calls the same error. The library never installs a signal
  handler; the caller decides what trips the token. A publication that has
  started is never interrupted.

- **Scratch is one hidden directory per run, and the library takes it as an
  argument.** `altw::add_locations_to_ways`, `altw::external::external_join`,
  `diff::diff`, `diff::derive::derive_changes` and
  `geocode_index::builder::build_geocode_index` take a
  `pbfhogg::scratch::RunScratch` by value, made with
  `RunScratch::create(parent, &host)` (the parent must exist and be writable;
  create it before opening any input). Everything the run writes as scratch
  goes in `parent/.pbfhogg-run-<pid>-<random>`, which is removed when the
  run ends; a removal that fails is reported through the host's diagnostics
  with the leftover's path. `Host::scratch_parent` is gone, and so are the
  `diff-par-*`, `derive-*`, `.pbfhogg-external-join-*` and
  `.pbfhogg-sparse-index-*` names. Scratch no longer lands beside the input
  (parallel `diff` used to write next to the old PBF) or inside the geocode
  index directory's staging copy.

- **Existing outputs are refused unless `--overwrite` (`-O`) is given.**
  Every command that writes a file or directory - PBF output, `merge-changes`
  and `diff --format osc` OSC, `diff -o`, `export -o`, every output of
  `extract --config` (all checked up front, so a late region never fails
  after early ones were extracted), and `build-geocode-index --output-dir` -
  now stops before doing any work when the output path already exists, with
  `Error: output <path> already exists and replacing it was not requested;
  pass --overwrite (-O) to replace it` and exit 2. `--overwrite` restores the
  old replace-in-place behaviour, matching osmium's `-O`. Scripts and
  benchmarks that rerun into the same path must add it. For
  `build-geocode-index` an existing empty directory is now refused too
  without `--overwrite`; its legacy `--force` still grants both overwrite and
  unindexed input.

- **A symlink is never accepted as an output**, even with `--overwrite`: it
  used to be silently replaced by a regular file. The error names the link
  and its target. An existing directory or special file (`-o /dev/null`) is
  likewise refused for a file output.

- **Library command entry points take a `pbfhogg::Output` instead of an
  output `&Path`, and publish it themselves.** `Output::new(path,
  Overwrite::Refuse | Overwrite::Replace, &diagnostics)` applies the rules
  above (refusals
  are the typed `ErrorKind::Output(OutputError::Exists | Symlink | NotAFile
  | ...)`, whose text names no CLI flag) and stages the file; the entry point
  writes the staging file and renames it into place only when it succeeds,
  so a failed library call no longer truncates or leaves a partial file at
  the path it was given. Changed: `sort::sort`, `cat::cat`,
  `cat::dedupe::merge_pbf`, `repack::repack`, `degrade::degrade`,
  `time_filter::time_filter`, `renumber::renumber_external`, `getid::getid`
  and `removeid`, `getparents::getparents` and
  `getparents_with_min_blobs`, `extract::extract`,
  `tags_filter::tags_filter`, `tags_filter::osc::tags_filter_osc`,
  `apply_changes::merge`, `altw::add_locations_to_ways`,
  `altw::external::external_join`, `merge_changes::merge_changes` and
  `diff::derive::derive_changes`. `extract::extract_multi` now takes the
  regions and a `pbfhogg::OutputSet` (`extract_multi_with_display_paths` is
  gone). `geocode_index::builder::build_geocode_index` takes a
  `builder::IndexOutput`, an `OutputDir` that only `builder::index_output`
  builds, so an existing non-index directory can never be replaced; and
  `BuildConfig` loses `output_dir`, `overwrite` and `force` (overwrite
  permission is the `OutputDir`'s; unindexed input is `allow_unindexed`).
  `path_guard::OutputFile` is gone.

- **Publication syncs the output, and an output must be written through
  one finished writing session to be published.** `Output::publish` (and the
  `OutputDir` and `OutputSet` ones) now `fsync`s the staged file before
  renaming it into place, so a writeback error stops the publication instead
  of surfacing after the previous output is gone; the PBF writers the
  commands use no longer sync at their own end of stream. An output is
  written through `Output::pbf_writer` (a `PbfWriter` on the chosen
  `OutputBackend`, finished by a successful `flush`), `Output::writer` (an
  `OutputWriter` for anything else, replacing `Output::create`, which handed
  out a bare `File`; finished by `OutputWriter::finish`) or
  `OutputDir::writer` (a `DirWriter` whose `finish` declares the directory
  complete). `publish` refuses, with `OutputError::Unfinished`, an output no
  writer was opened on (writing to `Output::path` behind the session's back
  is no longer published), one whose writer was not finished, and one whose
  writer reported any error, even if the caller carried on. The public
  `PbfWriter::to_path*` constructors still sync at `flush`.
  `Output::before_publish` runs a callback when publication starts.
  `Output::new`, `OutputDir::new`, `OutputSet::files` and
  `builder::index_output` take the `Diagnostics` that cleanup trouble is
  reported to (`output.cleanup` warnings), so it cannot be dropped by
  omission; pass `Diagnostics::discard()` to opt out.

- **Publication never deletes what pbfhogg never deletes.** An output named
  like, or inside, a `.pbfhogg-tmp-*`, `.pbfhogg-backup-*` or
  `.pbfhogg-run-*` entry (or an older version's scratch name) is refused
  (`OutputError::ReservedName`). Replacing a directory output (the geocode
  index) is refused before any work, and again before the swap, while
  anything of that kind is anywhere inside it, live run directories
  included (`OutputError::HoldsLeftovers`, naming the entries); before this,
  `build-geocode-index --overwrite` deleted them with the old index. Only an
  index's `.buckets-level*` scratch trees still go with it. A replaced
  output's backup is made durable before the new output is put in place, and
  removed only once the replacement's directory sync succeeded: when that
  sync fails, `OutputError::Unsynced` now names the kept backups instead of
  having already deleted them.

- **`bench-merge`'s `elapsed_ms` stops before publication.** It measures the
  merge, its write and its flush; the fsync, the rename and the directory sync
  are outside it. Sidecar phase markers end before them too, and the new
  `PUBLISH_START` / `PUBLISH_END` pair brackets them, so they show up in
  `--durations` instead of inside the last phase. Wall-clock around the whole
  process includes them, as it always did. Numbers recorded before this change
  include the publication and are not comparable.

- **Errors keep their kind and their cause end to end; `BoxResult` is gone.**
  The functions that returned `BoxResult` (the command entry points,
  `osc::parse_osc_file`, `osc::load_all_diffs`, `geocode_index::Reader::open`,
  `build_geocode_index`, `geo::Bbox::try_new`, `tag_expr::read_expressions_file`
  and the rest) and every `PbfWriter` method (previously `io::Result`) now
  return `pbfhogg::Result`; `?` into an `io::Error` or a `Box<dyn Error>` still
  works. Functions with a small error type of their own that was not `BoxResult`
  keep it: the `FromStr` impls of `TypeFilter`, `Compression` and `IndexType`
  still return their own parse errors. Also changed: `FromStr for ElementKind`
  and for `getid::ElementRef` now have `Err = pbfhogg::Error` (was `String`);
  `commands::parse_rfc3339_utc` returns the new public `TimestampError` (was
  `String`); `HeaderBuilder::build`, `BlockBuilder::take`,
  `DirectWriter::create`, `FileWriter::buffered`, `FileWriter::direct`,
  `FileWriter::sync`, `FileReader::buffered` and `FileReader::open` moved from
  `io::Result` to `pbfhogg::Result`. `HeaderBuilder::build` now reports an
  invalid bounding box as `ErrorKind::InvalidArgument`, so code matching
  `io::ErrorKind::InvalidInput` on it must match the new kind. `ErrorKind` gains
  `InvalidArgument`, `InvalidData`, `Internal` and `External`, each with a
  message and an optional typed cause, and what used to surface as a string or
  as `Io` wrapping `ErrorKind::Other` is now classified: `Io` means a real I/O
  failure, a malformed input is `InvalidData`, a bad request is
  `InvalidArgument`, a broken invariant or caught panic is `Internal`. Matches
  on `ErrorKind` need the new arms. `Error` is now `Clone`, gains
  `Error::context`, `Error::external` and `Error::report`, and loses
  `into_kind`. `Display` shows only the error's own context, message and
  location (no longer its source's text, which used to print twice); `report()`
  renders the whole chain with locations at the end, and the CLI prints it.
  `DirectReader::open` and `FileReader::direct` return the typed
  `PlatformUnsupported` directly rather than inside an `io::Error`. Errors now
  name the true blob ordinal (the header blob is blob 0) and byte offset in
  more places; for example an `apply-changes` failure in a base blob now ends
  `(in base.osm.pbf, blob 1, at byte offset 123)` where it used to read
  `base blob seq 0 at byte offset 123`. Fixes the external
  `add-locations-to-ways` backend flattening an output-side `--direct-io` or
  `--io-uring` refusal in stage 4: it now exits 3 like the other commands, not 2.

- **Library write entry points take `WriteOptions`, and the output backend is
  an `OutputBackend`.** The positional `compression, direct_io, io_uring,
  force` tail is replaced by one `&pbfhogg::write::WriteOptions { compression,
  backend, direct_reads, force }`, and the `(direct_io, io_uring)` bool pair by
  `pbfhogg::write::OutputBackend { Buffered, Parallel, Direct, Uring }`
  (`OutputBackend::from_flags(direct_io, io_uring)` keeps the old rule that
  both flags together mean io_uring; `direct_reads` is the input-side
  `--direct-io`, which stays independent of the backend). Changed signatures:
  `cat::cat`, `getid::getid` and `removeid`, `getparents::getparents`,
  `repack::repack`, `degrade::degrade`, `extract::extract`, `extract_multi`
  and `extract_multi_with_display_paths`, `time_filter::time_filter`,
  `renumber::renumber_external`, `sort::sort` (now takes `&WriteOptions`;
  `SortOptions` is gone), `cat::dedupe::merge_pbf` (likewise;
  `MergePbfOptions` is gone) and `altw::external::external_join`. The option
  structs `apply_changes::MergeOptions`, `tags_filter::TagsFilterOptions` and
  `altw::AltwOptions` replace their `compression`, `direct_io`, `io_uring` and
  `force` fields with one `write: WriteOptions`. CLI behavior is unchanged.

- **One element-kind type, `pbfhogg::ElementKind`.** `osc::OscElementKind`
  and `getid::ElemKindTag` are replaced by `pbfhogg::ElementKind { Node, Way,
  Relation }` (ordered node < way < relation, with `as_str`, `ordinal`,
  `type_char` and `FromStr`). `CompactDiffOverlay::deleted_ids` and
  `is_deleted`, and the kind of `getid::ElementRef`, take it; so do
  `getid::parse_ids_with_default_type` and
  `parse_ids_from_file_with_default_type` (replacing `getid::DefaultType`) and
  `inspect::show_element_to` (replacing `inspect::ShowElementType`). Rename
  the old variants one for one; CLI behavior and output are unchanged.

- **`HeaderBuilder::optional_feature` takes `impl Into<Feature>`** instead of
  `&str`. `&str` and the new `pbfhogg::block::Feature` constants
  (`HeaderBlock::LOCATIONS_ON_WAYS` and friends) work unchanged; a `&String`
  argument no longer deref-coerces and needs `.as_str()`.

- **Unavailable `--direct-io` / `--io-uring` now exits 3, not 2.** A
  filesystem that refuses `O_DIRECT` at open, or a host without usable
  io_uring (old kernel, sandboxed `io_uring_setup`, `RLIMIT_MEMLOCK` below
  the registered buffers), is reported as
  `Error: platform feature direct-io|io-uring unavailable: ...` with exit
  code 3, so scripts can retry without the flag. Every other failure still
  exits 2. In the library the same refusals are the new
  `ErrorKind::PlatformUnsupported { feature, source }`; `DirectReader::open`,
  `PbfWriter::to_path_direct` and `to_path_uring` carry it inside their
  `io::Error` (`raw_os_error()` is no longer `EINVAL` for an `O_DIRECT`
  refusal), and converting to `pbfhogg::Error` restores the typed variant.

- **`build-geocode-index` publishes a complete index or nothing.** Every
  build is written to a `.pbfhogg-tmp-*` staging directory beside
  `--output-dir` and swapped into place only after it and its smoke test
  succeed, so a failed `--overwrite` rebuild leaves the old index intact and
  readers already open keep working. The output directory is replaced as a
  whole: it must be new, or (with `--overwrite`) an existing index or empty
  directory; a directory holding anything else is refused rather than
  deleted. Allow disk space for two generations during a rebuild.

- **`build-geocode-index --force` is split into `--overwrite` and
  `--allow-unindexed`.** Replacing an existing index no longer also waives
  the indexdata requirement, and accepting unindexed input no longer grants
  permission to replace an index. `--force` keeps working as shorthand for
  both; `BuildConfig` gains the matching `overwrite` and `allow_unindexed`
  fields. An `--output-dir` that is a symlink is now refused (publication
  would have replaced the link with a real directory), and on Linux so is an
  existing directory that is a mount point (the final rename would have
  failed only after a full build).

- **`build-geocode-index --admin-level` is capped at 10** (was 19). The
  admin flood fill grows 4x per level, and level 19 asked for billions of
  cells on a large country: a hang and an OOM rather than a refusal. Levels
  above 10 are now refused before any output is created.

- **`BuildStats` gains `smoke_test: SmokeOutcome`.** Callers can distinguish a
  passing query, a query with no address or street match (including its
  coordinate in decimicrodegrees), and a skipped query when the index has no
  address points. Downstream `BuildStats` struct literals must provide the
  new field.

- **Multi-extract config paths no longer depend on the working directory.**
  A relative `"directory"` in the config resolves against the config file's
  directory, and `-d` rebases every output under its directory: outputs
  below the configured root keep their relative path, absolute outputs
  outside it keep their file name. Previously a relative directory followed
  the process working directory, and `-d` silently left absolute outputs
  where the config put them.

- **Commands take an explicit `&Host`** (`pbfhogg::host`), which supplies the
  caller's process actions, resource probes and diagnostics; there is no
  default. The library no longer calls `mallopt`, raises `RLIMIT_NOFILE` or
  reads `BROKKR_MARKER_FIFO` unless the caller asks: `ProcessActions` hooks
  fire at the same program points the CLI used to mutate the process, and
  sidecar markers are inert until the caller opens a FIFO with
  `debug::initialize_marker_fifo(path)`; the library never reads
  `BROKKR_MARKER_FIFO` itself.
  `debug::cap_malloc_arenas`, `debug::set_mmap_threshold` and
  `debug::malloc_trim` moved to `process::`. Thread counts and the altw
  `auto` memory budget come from `host::Resources` (`MachineProbe` or
  `Fixed`) instead of probing the machine directly.

- **The library writes nothing to stderr.** Every former line is an event on
  the caller's `host::Diagnostics` handle (`Progress`, `Warning`, `Note`,
  `Summary`, `Developer`), so callers can capture or redirect each run's
  output separately. `print_summary` and `print_osmium_summary` are replaced by
  `write_summary` and `write_osmium_summary` into an `io::Write`.
  `has_indexdata`, `has_tagdata` and `osc::parse::load_all_diffs` take
  `&Diagnostics`. The unrecognised-indexdata warning is once per
  `Diagnostics` handle (shareable through `OnceKeys`), not once per process.
  `BlobReader` and `ElementReader` gain a `.diagnostics(d)` builder.
  CLI benchmark readers attach the same handle, retaining malformed-indexdata
  warnings in modes that parse indexdata.

- **Public file readers no longer evict the page cache by default.**
  `BlobReader::from_path` and `open`, and `ElementReader::from_path` and
  `open`, used to drop each input's pages behind the read head, which made
  any second pass, concurrent reader or other process on the host read cold.
  They now leave the page cache alone. Opt in for one-pass reads of files
  larger than RAM with `.evict_page_cache(true)` or
  `ElementReader::from_path_with_eviction(path, true)`. Commands follow the
  new `Host::page_cache` field (`PageCachePolicy::Evict` or `Keep`); the CLI
  keeps evicting as before.

- **The `PbfWriter::to_path*` constructors take an `Executor`**
  (`host::Executor::Ambient` for rayon's ambient pool, or
  `Executor::Pool(Arc<rayon::ThreadPool>)` to confine framing to a pool of
  your own). Commands run their parallel work on the new `Host::executor`.

- **Report types write into a caller-supplied `io::Write` instead of printing
  to stdout.** A library does not own its host's stdout. Renamed, each now
  returning `io::Result<()>`:
  - `InspectReport::print_report` / `print_report_filtered` become
    `write_report(&mut w, ..)` / `write_report_filtered(&mut w, ..)`;
  - `NodeStatsReport::print_report` becomes `write_report(&mut w)`;
  - `check::index::IndexReport::print_human` becomes
    `write_human(&mut w, file_name)`;
  - `VerifyIdsReport::print_human` becomes `write_human(&mut w, file_name)`.

  `inspect::show_element` (which wrote to stdout) is removed, replaced by
  `inspect::show_element_to(path, ty, id, direct_io, &mut w)`. Write errors
  in the inspect block tables used to be silently discarded and are now
  returned.

- **Geocode index format version 3 widens admin entry and vertex byte offsets
  to 64 bits.** Version 2 indexes are rejected at `Reader::open` with a message
  naming `build-geocode-index` as the remedy; there is no migration, and an
  existing index must be rebuilt. This removes the 4 GiB limits on
  `admin_entries.bin` and `admin_vertices.bin`.

- **`geocode_index::Reader::query` and `candidates` now return `Result`.** NaN,
  infinite, and out-of-range latitude or longitude are rejected with a typed
  `QueryError` instead of being converted into arbitrary S2 cell IDs.

- **Coarse geocode fallback now fills each missing result class independently.**
  A fine street no longer prevents a coarse address and postcode from being
  considered, and missing interpolation results also trigger coarse lookup.

- **Geocode distances, snap points, and cell coverage now use the short
  longitude arc across the antimeridian.** Addresses, streets, interpolation
  segments, and interpolation lengths straddling +/-180 degrees are no longer
  silently discarded as almost a world away. The snapped point on a street or
  interpolation segment is now interpolated in the same unwrapped frame the
  distance used, so it lands on the antimeridian rather than on the opposite
  meridian - previously the resulting partial length ran roughly half the globe
  against a segment length of a fraction of a degree, and interpolated house
  numbers on such a segment could be wildly wrong. The builder's S2 segment
  sampler unwraps too, so a segment crossing the dateline covers the cells it
  actually passes through instead of a band around the whole planet. The
  builder's interpolation endpoint matcher is the last member of the family: it
  now measures endpoint-to-address distance on the short arc as well, so an
  address point just across the dateline from a way endpoint is preferred over
  a farther one on the same side (the raw subtraction also overflowed `i32`
  there).

- **Geocode coarse fallback for interpolation now depends on whether a fine hit
  resolves, not on whether one exists.** A nearby interpolation way with
  unusable endpoint house numbers used to yield no result AND suppress the
  coarse interpolation that would have produced one. `candidates()` no longer
  returns an interpolation candidate that `interpolate()` cannot resolve, so
  `query()`, `candidates()`, and `Candidates::into_result()` agree.

- **`--max-admin-vertices` is now a local-boundary budget rather than one cap
  applied uniformly to every admin level.** Levels 5 through 2 retain
  progressively more vertices, up to 16 times the configured value for
  countries. This reduces permanent border displacement. Douglas-Peucker can
  still introduce self-intersections, but the larger high-level budgets reduce
  both its tolerance and affected border band; they do not close that residual.

- **Corrupt geocode strings and out-of-bounds fixed-record reads no longer
  become empty names or zero-coordinate records.** The low-level decoders now
  distinguish missing data from real empty values, and query consumers skip a
  missing defensive read instead of fabricating a match.

- **The geocode index builder now rejects an index with no queryable records,
  and readers reject invalid S2 levels, mismatched fixed-record
  file lengths, and out-of-bounds entry lists.** These conditions previously
  produced an empty but apparently successful index, trusted corrupt metadata,
  or could panic while querying a hostile index.

- **`build-geocode-index` now caps `--street-level`, `--coarse-level` and
  `--admin-level` at 19, down from the S2 limit of 30.** Cell covering samples
  each way segment at a fraction of the cell width, so the per-segment cost
  doubles with every level; past 19 a single long or antimeridian-crossing
  segment can require billions of samples, which is a build that never
  finishes. The levels are validated before any input is read and a violation
  exits 2. Readers are unaffected and still accept indexes written at levels up
  to 30.

- **`apply-changes --locations-on-ways` now fails when a way node reference
  cannot be resolved.** Unresolvable references were written as `(0, 0)`,
  which is Null Island - a real coordinate - so the placeholder vertices
  were indistinguishable from genuine geometry in the output PBF. Pass
  `--ignore-missing-nodes` for exactly the previous behaviour. This brings
  the command into line with `add-locations-to-ways`, which withdrew the
  same deviation, and replaces the corresponding open item in
  `reference/osmium-parity.md` with a recorded deviation.

- **Polygon extracts reject GeoJSON vertices outside the WGS84 domain.**
  Latitude must be within -90 and 90, longitude within -180 and 180. Matches
  the `--bbox` domain guard.

- **A GeoJSON FeatureCollection now contributes every polygonal feature.**
  Only the first feature's geometry was used, so a multi-feature region file
  silently extracted a fraction of the requested area. Applies to both
  `--polygon` and the `polygon` key in a `--config` entry.

- **`add-locations-to-ways` now fails when a way node reference cannot be
  resolved.** Unresolvable references were filled with `(0, 0)` and counted
  in the stderr summary as `missing locations`. `(0, 0)` is Null Island, a
  real coordinate in the Gulf of Guinea, so the placeholder vertices went
  into a well-formed PBF that no reader could distinguish from genuine
  geometry, and the only signal was a stderr line that does not survive a
  pipeline. Pass `--ignore-missing-nodes` for exactly the previous
  behaviour, including the summary line. This withdraws a documented
  deviation from osmium, which has always failed by default.

- **`add-locations-to-ways` refuses a way blob whose coordinate framing is
  not the PBF default.** The wire-level reframe path used by both the
  `sparse` and `external` backends writes way coordinates in
  decimicrodegrees while copying the source block's `granularity`,
  `lat_offset` and `lon_offset` verbatim. The reader reconstructs
  `lat = lat_offset + granularity * value`, so a block declaring
  `granularity = 1000` produced way coordinates ten times too large - as
  valid PBF that reads back cleanly, with nothing downstream able to notice.
  Blocks are now rejected unless `granularity` is 100 and both offsets are
  zero, which is what every osmium and pbfhogg producer writes.
  `pbfhogg cat --clean` re-encodes an odd input into the default framing.

- **`add-locations-to-ways` refuses a referenced node whose coordinates are
  outside the geographic domain.** A latitude past +/-90 or a longitude past
  +/-180 degrees is representable in the wire format but is not a place;
  writing it through would relocate the vertex silently. Both the `sparse`
  and `external` backends fail with the same error, so `--index-type`
  remains a performance knob rather than a semantic switch. Nodes not
  referenced by any way are unaffected.

- **`extract --bbox` rejects coordinates outside the geographic domain.**
  Latitude must be within -90 and 90, longitude within -180 and 180. Only
  `min < max` was checked before, and `--bbox 200,0,300,1` then saturated to
  `i32::MAX` bounds inside the integer conversion and produced an empty
  extract with exit code 0 - a wrong answer reported as a successful run.

- **`time-filter` now refuses input whose elements carry no timestamps.**
  Every timestamp accessor ended in `unwrap_or(0)`, so an element with no
  `Info` - which is what `osmium --drop-metadata` and several pbfhogg write
  paths produce - reported timestamp 0. Zero is at or below every plausible
  cutoff, so the snapshot gate kept the element and counted it as "before
  cutoff", and the command printed `N versions scanned, N <= cutoff, N
  elements written` having filtered nothing whatsoever. The result was a
  full copy of the input, reported as a successful filter, with nothing
  downstream able to tell it apart from a cutoff that genuinely kept
  everything. Both the history and snapshot paths now error, naming the
  first element that lacks a timestamp. Re-run against an input that still
  carries element metadata.

- **A truncated OSC is now a hard error in every command that reads one.**
  A `.osc.gz` cut at a gzip member boundary decompresses cleanly, so
  quick-xml simply reported end-of-input and `apply-changes`,
  `merge-changes` and `tags-filter --input-kind osc` all treated the
  partial document as the whole change stream - `apply-changes` wrote a
  partially-applied PBF and exited 0. The parser now requires the closing
  `</osmChange>` tag (the self-closing `<osmChange .../>` empty-document
  form is still accepted).

- **An OSC element outside any `<create>`, `<modify>` or `<delete>` section
  is now refused.** The three OSC parsers previously gave three different
  answers: the change overlay treated it as a create and applied it,
  `merge-changes` errored for the self-closing form and silently dropped
  the other, and `tags-filter` dropped it. None of them could know which
  change the producer meant.

- **An OSC metadata attribute that is present but unparsable is now an
  error.** `version="abc"` used to be recorded as `version = 0`. Absent and
  unparsable are both distinct from the value zero, and the new
  version-precedence gate compares these numbers.

- **`merge-changes` and `tags-filter --input-kind osc` now reject a created
  or modified `<node>` without usable coordinates.** Both used
  `unwrap_or(0.0)` plus a saturating `as i32`, so a coordinate-less node
  was silently placed at 0/0 - a real location in the Gulf of Guinea - and
  `lat="1e30"` was clamped to `i32::MAX`. `apply-changes` already rejected
  these; now all three agree. Deletes still need no coordinates.

- **Version precedence: a change that does not advance an object's version
  is refused.** Neither the change overlay (later parse wins) nor
  `merge-changes --simplify` (`BTreeMap` insert, input order wins) ever
  consulted `version`, so precedence was purely positional. Applying the
  same OSC twice, or listing inputs out of sequence order - which is easy
  to do by accident - silently regressed objects to older versions and
  reported success. Both now refuse an incoming change whose version is at
  or below the version it would replace, naming both versions. Elements
  that claim no version are unaffected, since there is nothing to compare.
  Opt out with `apply-changes --allow-version-regression`,
  `merge-changes --allow-version-regression`, or
  `CompactDiffOverlay::allow_version_regression(true)`.

  A `<delete>` participates in the gate on both sides. It is version-gated
  like any other change, and it leaves a *versioned tombstone* rather than
  simply dropping the object from the live index - otherwise a delete
  erased the object's version history and the very next stale change had
  nothing to be compared against. Delete node 1 version 9 then modify node
  1 version 8, in one OSC or across two, and version 8 was resurrected
  past the gate. The tombstone carries forward the higher of the delete's
  own claim and the version the live record held.

- **`tags-filter --input-kind osc` no longer strips element metadata from
  the changes it keeps.** Every surviving element was re-emitted with no
  metadata at all, so filtering a change stream silently erased `version`,
  `timestamp`, `changeset`, `uid`, `user` and `visible` from every change
  that passed the filter - deletes included. A filter is expected to drop
  what does not match, not to anonymise what does. It also re-emitted tag
  keys, tag values and member roles as raw attributes rather than escaping
  them, so a tag value containing a quote or an ampersand produced
  malformed XML here and well-formed XML from `merge-changes` for the same
  input; both commands now share one writer.

- **`osc::load_all_diffs` errors on a directory whose files all failed to
  match.** It accepted exactly `<sequence>.osc.gz`, so a directory of
  `changes-4705.osc.gz`, plain `.osc`, or Geofabrik's nested layout matched
  nothing, returned an empty overlay, and printed a cheerful "Loaded 0
  diffs" - after which the caller applied no changes and reported success.
  An empty directory is still a legitimate no-op.

- **`tags-filter` / `inspect tags` expressions now follow osmium's matcher,
  which changes what existing expressions select.** Both commands are
  advertised as osmium equivalents, but the parser understood only exact
  matches, comma lists on the value half, and a key ending in `:*`.
  Everything else osmium accepts parsed as an exact match on a literal
  containing a star and matched nothing at all - `highway=res*`,
  `name=*strasse`, `highway=*`, `amenity,shop` each produced empty output
  with no error to notice. Now, on **both** halves of an expression:
  `foo*` is a prefix match, `*foo` and `*foo*` are substring matches
  (osmium has no suffix matcher), a bare `*` matches anything, and a comma
  list is a set of alternatives. Spaces around each half and around each
  comma-list element are stripped, so `highway = primary` from an
  `--expressions` file names the key `highway` rather than `"highway "`.
  The expression now splits at the **first** `=`, so `a=b!=c` means key
  `a`, value `b!=c`. Inversion is marked by a `!` that is the last
  character of the *raw* key, tested before the space stripping above, as
  `get_tag_matcher` does: `highway !=x` inverts, while `highway ! = x`
  names the literal key `highway !`. Any expression containing `*` or a
  comma in its key, or surrounding spaces, will now select elements it
  previously did not.

- **`PbfWriter::write_raw_copy` takes an `&Arc<File>` instead of a `RawFd`.**
  In pipelined mode the splice is queued into the writer thread and runs
  later, so a bare descriptor number had no lifetime tie to anything: a
  caller who closed the input before `flush()`, or whose fd number was
  recycled by an unrelated `open`, got a silent copy of whatever now sat at
  that descriptor - wrong bytes, no error, on the one path whose purpose is
  byte-exact passthrough. Migration: wrap the input `File` in an `Arc` and
  pass a reference to it.

- **`pbfhogg::inspect::inspect` takes an `InspectOptions` struct** instead of
  five positional `bool`s, which is where the new `distrust_metadata`
  setting lives. Migration is mechanical: name the fields.

- **`check --refs` (`RefCheckResult`) gained an `untrackable_ids: [u64; 3]`
  field** and a `total_untrackable()` accessor.

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

- **The writer now refuses to produce a file it would have to corrupt or
  truncate, where it previously produced one and exited zero.** Four
  cases, all of which used to succeed silently:
  - A framing task dropped without sending its blob - leaving the output
    short by an arbitrary number of blobs - is now an error on every
    backend. Previously the default backend (used by every command without
    an I/O flag) did not check at all, and the `--io-uring` and parallel
    pwrite backends checked only for a gap that stranded *later* blobs in
    the reorder buffer. A gap at the tail strands nothing, so a lost final
    blob - or a single-blob write that vanished entirely - passed all
    three. The writer is now told the final blob count before the stream
    closes and refuses to finalise a file that is short of it.
  - `BlockBuilder::add_relation` with a member whose type is
    `MemberType::Unknown` now poisons the builder, and the next `take()` /
    `take_owned()` fails. The member used to be written as a **node** - a
    file that is valid PBF and semantically wrong, which no reader,
    `check --index`, or osmium cross-validation catches.
  - `BlockBuilder::add_way_with_locations` with mismatched `refs` and
    `locations` lengths now poisons the builder the same way. In release
    builds the encoder zipped the two slices, so the way was written with
    refs missing; only a `debug_assert_eq!` stood in the way.
  - `DirectWriter::write` after `DirectWriter::flush` is now an error. It
    used to append after a mid-file zero gap and silently drop the final
    partial buffer.

- **`PbfWriter` surfaces the writer thread's real I/O error.** When the
  writer thread died - ENOSPC, EIO, a short io_uring write - the next
  `write_raw` / `write_raw_owned` / `write_raw_chunks` / `write_raw_copy`
  returned a synthetic "writer thread terminated" and the actual cause was
  discarded by `Drop`. These paths now join the thread and return its own
  error, kind and message intact.

- **`HeaderBuilder` writes `source` (HeaderBlock field 17).** `build`'s doc
  had always listed it; no code path emitted it, so every command that
  rewrote a header through `HeaderBuilder` dropped the field. New
  `.source()` / `.without_source()` builder methods, and `from_header` now
  copies it. Related: replication timestamp / sequence number are emitted
  unconditionally when set, so sequence number 0 is no longer silently
  dropped.

- **Writing to a `PbfWriter` after `flush()` returns an error instead of
  panicking.** The `flush` doc claimed "subsequent writes go through the
  direct (non-pipelined) path"; for every pipelined constructor the file
  had been moved into the writer thread, so that path hit an `expect`. The
  doc has been corrected and the panic replaced with an `io::Error`.

- **An OSC `<node>` in a `<create>` or `<modify>` section must now carry
  usable `lat` and `lon`.** A missing, empty, unparsable, out-of-range or
  non-finite coordinate used to be accepted and stored at exactly (0, 0).
  That is a real coordinate in the Gulf of Guinea, so the corrupted node was
  indistinguishable from a genuine one, and under `--locations-on-ways` the
  (0, 0) propagated into every way referencing it. Such a node is now a parse
  error naming the node ID and the attribute. Nodes genuinely at 0/0 are
  unaffected, and `<delete>` nodes still need no coordinates.

  "Out of range" means the geographic range, not merely what fits an `i32`
  after scaling to decimicrodegrees. `lat="100"` scales to 1_000_000_000,
  which an `i32` holds comfortably, so a representation check alone accepted
  it. Latitude must now be within -90 to 90 and longitude within -180 to 180,
  inclusive, checked before the cast so the conversion is exact rather than
  saturating.

- **A relation member of an unknown type is now refused rather than written
  to OSC as `type="node"`.** The substitution produced well-formed OSC in
  which the member pointed at a different object - node 42 instead of
  unknown-type 42 - which nothing downstream could detect. `derive-changes`,
  `diff --format osc` and the compact OSC overlay all error instead, matching
  the treatment the PBF write path already gives an unknown member type.

- **`apply-changes` refuses to write an unsorted PBF under a
  `Sort.Type_then_ID` header.** It stamps that header unconditionally while
  gating only on the *base* header's claim, and a header can lie (`degrade
  --unsort` produces such files). The output is now checked as it is written,
  against a running high-water mark in canonical OSM ID order.

- **`diff` and `derive-changes` reject an input whose blobs contradict its
  sorted header.** A blob of an earlier element kind turning up after that
  kind's phase had ended used to be skipped, discarding it and every element
  in it silently; a mixed-type block had its other-kind elements consumed by
  whichever phase read it first and lost. Both now error.

  Both the sequential and the parallel path enforce this, and they needed
  separate work. The sequential path walks blobs in file order and sees the
  disorder structurally; the parallel planner buckets every blob by kind
  before any ordering is examined, so file order never reached it. Until both
  were closed, `pbfhogg diff` on the same pair of files answered "blobs are
  not in canonical order" with exit 2 at `-j 1` and "Files are identical"
  with exit 0 at the default `-j`. The parallel walk now additionally rejects
  a per-kind blob list whose ID spans go backwards - a layout its own
  sharding assumes cannot occur.

- **`diff` and `derive-changes` reject a blob whose stored `max_id`
  understates its contents.** The block-pair merge takes its merge boundary
  from the two blobs' claimed last IDs; a short claim put the boundary
  mid-block and dropped the rest of that block, silently, with no count
  anomaly - the count that would have disagreed comes from the same wrong
  metadata. The residual decision is now made from what the element walk
  actually saw, and a claim contradicting the block is an error naming the
  file. Per ADR-0013 this needs no new flag: the merge no longer sizes
  anything from metadata, so it stops needing the trust rather than gating
  it.

- **`IndexedReader` and `IdRanges` are removed.** They kept a separate blob
  index, populated ID ranges lazily by decoding blobs, and ignored the
  indexdata pbfhogg writes. No command used them. For targeted ID lookups use
  the `getid` library entry point, or `ElementReader` with a blob filter.

- **`IdSet::count_below` and `count_in_range` are removed.** Use `rank` for
  counts below an ID, and subtract ranks to count a range. For an inclusive
  range ending at `i64::MAX`, use `total_count` as the upper bound.

- **`BlockBuilder::with_element_cap` is removed.** It preallocated as if the
  input were unbounded, so a large cap drove huge allocations. Use
  `with_element_cap_prealloc(cap, bound)` with a real bound such as the input
  size. The default cap is now the public `block_builder::DEFAULT_ELEMENT_CAP`;
  `degrade::DEFAULT_BLOCK_CAP` stays as an alias of it.

- **`check::refs::check_refs` takes a `CheckRefsOptions`** (embedding the
  shared `check::CheckOptions`) in place of positional flags, and honours its
  `max_errors`. `VerifyIdsOptions` moves its `max_errors` and `direct_io`
  fields into the same embedded `check: CheckOptions`;
  `check::index::CheckIndexOptions` is now an alias of `CheckOptions`.

- **`MergeStats::blobs_skip_decompress` and `blobs_scan_only` are removed.**
  Nothing incremented them. The apply-changes summary line drops the two
  always-zero sub-counts and reads
  `Blobs: N passthrough (M index-hit), R rewritten (of T total)`.

- **`CompactDiffOverlay`'s deleted-ID sets are private.** Read them through
  `deleted_ids(OscElementKind)` and `is_deleted(..)`; inserting into them
  directly could leave the overlay's index and the sets disagreeing.

- **`diff::derive::derive_changes` takes `timestamp: Option<u64>`** in place
  of `update_timestamp: bool`. Pass the epoch seconds to stamp deletes with,
  or `None` to leave them unstamped.

- **An OSMHeader blob anywhere but first is an error in every command.**
  A file made by appending one PBF's bytes to another's carries a second
  OSMHeader. `sort`, plain `cat` and the library readers (`BlobReader`,
  `ElementReader`) used to skip it, reading the second file's data under the
  first file's header and silently dropping what the second declared
  (`HistoricalInformation`, required features, bbox); plain `cat` even kept
  the first header's `Sort.Type_then_ID` over the unsorted result. Every
  reader now refuses it, as `digest` and `cat --dedupe` already did and as
  osmium does. Pass the original files as separate inputs instead
  (`pbfhogg cat a.osm.pbf b.osm.pbf -o merged.osm.pbf`), which reconciles
  their headers.
  For `BlobReader::new_seekable`, "first" means the construction position,
  even at a nonzero offset containing an embedded PBF. Later seeks keep
  that stream start; revisiting its header is allowed, while seeking to a
  different header is rejected. Reported byte offsets remain absolute.

### Added

- **Library: `read::input::PinnedInput` keeps a multi-pass read on one
  file.** A command that reads its input in several passes opens it several
  times, and each open used to resolve the path again, so a file renamed
  over the input mid-run was read partly from each. A `PinnedInput`, taken
  once per input, makes every later open a fresh descriptor of the pinned
  file: on Linux with `/proc` a replaced or unlinked input does not affect
  the run; on other Unix hosts an open that finds another file at the path
  fails with the new `ErrorKind::InputReplaced` (off Unix nothing is
  verified). Only regular files and block devices
  can be pinned. `FileReader::open` and `InputFile::open` now take an
  `impl Into<InputSource>` (a `&Path`, `&PathBuf`, `&Arc<Path>` or
  `&PinnedInput`); a caller passing `&&Path` dereferences it. The commands
  adopt it one by one; until a command does, it still reopens by path.

- **A run says when passthrough copies left the kernel fast path.** When
  the kernel refuses `copy_file_range` (cross-filesystem output, FUSE or
  NFS, seccomp, a non-regular output) and passthrough ranges are copied
  through userspace instead, a `Note:` line now gives the number of ranges,
  the bytes requested and the first refusing errno. It used to show only in
  sidecar counters. Library: `write::report_writer_index_loss` is renamed
  `write::report_writer_degradations` and emits this note as well as the
  tag skip-index warning; rename the call.

- **Plain `cat` says when it rebuilt or omitted a block.** An unindexed
  blob whose block no single index can describe (no elements, a group with
  more than one element field, kinds changing across groups) is rebuilt
  into single-kind blocks, normalised as `--type` and `--clean` write them,
  or omitted when it holds no elements, and the summary counts these only
  as decoded. A `Note:` line now gives how many were
  rebuilt and omitted and the rules that applied (dense nodes, coordinates
  truncated to decimicrodegrees, whole-second timestamps, versionless
  metadata discarded, unknown fields dropped). Output is unchanged.

- **A global `-q, --quiet` on every command: the exit code is the
  answer.** It drops every `Note:`, progress line and closing summary
  (including `inspect tags`' `N distinct tag values` and `extract --config`'s
  per-region lines) and keeps every `Warning:` and the `Error:` line. It
  never changes what is written to `-o`, nor a report on stdout (`inspect`,
  `digest`, `export`, the `inspect tags` table); `diff -q` and
  `check --quiet`, which already existed, are this flag now and still print
  nothing at all, and a quiet `inspect --indexed` prints no verdict line
  either (it refuses `--json` with `-q`). It goes before or after the
  subcommand. `diff --format osc`
  now accepts it (it was refused) and writes its output without the summary;
  `getid -v` refuses it. The `bench-*` key=value blocks are unchanged.

- **`BlobReader::leading_header` is public.** It takes the first blob and
  returns the missing-header error for an empty input or one that opens with
  anything other than an OsmHeader, so a caller iterating blobs itself can
  refuse a header-less file instead of reading it as empty.

- **`BlobReader::new_named(reader, path)`** builds a streaming reader whose
  errors name `path` and locate at byte offsets counted from the reader's
  first byte, without requiring `Seek`. **`Blob::locate_error`** is public,
  so a decode error on a blob can carry the same location.

- **`BlobReader::cancel` and `ElementReader::cancel`** attach a
  `pbfhogg::host::Cancel` to a reader built by hand; it then stops with
  `ErrorKind::Cancelled` at the next blob once the token is tripped.
  Readers the commands open take `host.cancel` already.

- **`--tmp-dir <DIR>` on `add-locations-to-ways`, `diff` and
  `build-geocode-index`** chooses where the run's scratch goes, for when the
  output's volume is small or slow (`--index-type external` needs hundreds of
  GB at planet scale). The directory must exist and be writable and is checked
  before any input is opened, so a bad value fails at once. The default is the
  output's directory, as before; a `diff` with no output file uses the current
  directory (it used to use the old input's directory). Output staging files
  stay beside the output regardless.

- **Leftovers of runs that never finished are reported.** A command that
  writes an output now prints a `Warning:` for each unfinished run directory,
  old-style scratch file or directory, hidden `.pbfhogg-tmp-*` staging file
  and `.pbfhogg-backup-*` backup it finds in the output's directory (and in
  `--tmp-dir`), with its size and why it is believed stale, and never removes
  any of them. A backup is worded separately: it can be the only copy of a
  previous output. The scan is also available to library callers as
  `scratch::find_stale`, which returns typed values. It also looks where older
  versions of each command left scratch: every directory an
  `extract --config` output goes in, the old input's directory for `diff`,
  and for `build-geocode-index` everything inside the index directory it is
  about to replace, at any depth (via `scratch::find_stale_in_index`, which
  also reports older builders' `.buckets-level*` trees; `--overwrite` deletes
  those with the old index, and the report says so), and the earlier
  `.geocode-build-*` staging siblings. A backup that cannot be removed after
  a successful replacement is reported as a warning instead of staying
  silently. A run directory whose lock cannot be probed (another user's, an
  unexpected file type, a lock the filesystem refuses) is reported as "in
  use or not, unknown; do not delete", never as stale; the probe opens the
  lock read-write, which NFS needs for an exclusive lock.

- **`PbfWriter::to_path_sync`: synchronous file output that syncs on
  `flush()`.** It creates the file, owns a 256 KiB buffer and compresses on
  the calling thread, and `flush()` syncs the file like every pipelined
  path constructor. `multi-extract` writes each region through the same
  synchronous writer over its staged output, so its outputs are synced (at
  publication) when the command finishes (they were not before) and each
  output gets a 256 KiB buffer instead of 8 KiB; allow for that buffer per
  region when extracting hundreds of regions at once.
  `PbfWriter::new` is unchanged: it only calls the wrapped writer's `flush`.

- **`build-geocode-index` reports the data it drops.** Way node references
  with no coordinate (common at an extract's border), admin relation member
  ways with no geometry, dropped admin relations, interpolation ways with
  unresolved endpoints and negative node references each produce a warning
  with a count and a bounded sample of the OSM IDs involved. The smoke-test
  warning now names the queried coordinate and the index directory.

- **`getid` and `getparents` accept named element IDs** (`node/123`,
  `way/456`, `relation/789`) alongside `n123` / `w456` / `r789`, on the
  command line and in `--id-file`, matching what `inspect --show` already
  accepted. The library parser is `getid::ElementRef` (`FromStr`). Bare
  numeric IDs still need `--default-type`; see DEVIATIONS.md.

- **`pbfhogg digest`: an encoding-independent digest of a PBF's element
  content.** Two files get the same digest exactly when
  `diff --suppress-common` reports them identical (barring a 128-bit hash
  collision), whatever their compression, blob boundaries, dense or plain
  nodes, string-table order, header or indexdata - so a stored digest of one
  side can stand in for re-reading it. Output is `xxh128 v1 HEX` plus a
  count and digest per element kind; the version changes whenever the
  canonical form does. The command refuses, printing nothing, any input on
  which diff's answer would not be a function of element content: unsorted
  or duplicate IDs, mixed blocks, partial or inaccurate indexdata, a second
  header, unresolvable string references. Library: `pbfhogg::digest::digest`.

- **Fallible tag iterators.** `tags_checked()` on `Node`, `Way`, `Relation`
  and `DenseNode` returns `CheckedTagIter` / `CheckedDenseTagIter`, which
  yield an `Err` for a key or value index past the string table where
  `tags()` silently ends the tag list.

- **Builds against musl (`x86_64-unknown-linux-musl` and friends).** The
  glibc-only allocator tuning calls (`mallopt` arena caps, `malloc_trim`,
  `mallinfo2`) are now gated on `target_env = "gnu"` and become no-ops
  elsewhere, so static `FROM scratch` binaries can depend on pbfhogg.
  Contributed by Steffen Trog (#2). musl's default allocator is markedly
  slower under pbfhogg's multithreaded allocation load; consider a
  replacement global allocator such as mimalloc in musl builds.

- **`apply-changes` and `tags-filter --input-kind osc` accept uncompressed
  `.osc` input.** Both wrapped every input in a gzip decoder unconditionally
  and failed on plain XML with a raw "invalid gzip header", while
  `merge-changes` sniffed the magic bytes and handled both. Compression is
  now detected from the file's leading bytes in one shared place, so all
  three agree. `osc::load_all_diffs` correspondingly accepts
  `<sequence>.osc` alongside `<sequence>.osc.gz`.

- **`pbfhogg::write::emit_writer_counters()`.** Publishes the `writer_*`
  sidecar counters, at most once per process. Emission used to happen inside
  `PbfWriter::flush`, but the counters are a process-global total, so a
  command that opens several writers - `multi-extract` opens one per region
  and writes to all of them at once - published the same running totals once
  per region, which reads as a per-region breakdown and is not one. Library
  consumers who want the counters should call this once on the way out, as
  the CLI now does; the counters remain process totals and per-writer
  attribution is not available. A process that never wrote anything emits
  nothing at all, so the read-only commands - which now also reach the exit
  hook - do not publish a set of zeroed `writer_*` counters.

- **`pbfhogg check --index`: verify a file's blob-level metadata against the
  blobs themselves.** `BlobHeader.indexdata` (element kind, ID range, count,
  spatial bbox) and `BlobHeader.tagdata` (the blob's tag key set) are hints
  every fast path in this crate believes without opening the blob - `cat
  --type` passes a blob through raw on the declared kind, `renumber` assigns
  IDs from the declared count, `tags-filter` skips whole blobs on the tag
  index - and nothing in the format signs them. `--distrust-metadata` let a
  consumer decline to believe them, but there was still no way to ask whether
  they were true. The new selector decompresses every blob, re-derives the
  metadata from the payload, and reports each disagreement with its blob
  ordinal and byte offset; `--json` carries the same verdict. It exits `1`
  when it finds mismatches and `2` when it cannot run.

  It is opt-in rather than part of the default `--ids` + `--refs` pair,
  because it decompresses the whole file, and passing it suppresses that
  default. There is deliberately no `--distrust-metadata` on it: a verifier
  that consulted the metadata to decide what to look at would be worthless,
  so it installs no blob filter and decompresses every blob unconditionally.
  Absence is not a claim - a blob with no indexdata or no tagdata is not
  reported - and the bbox and tag-key checks are containment rather than
  equality, so an honest third-party file cannot be reported as lying.
  Metadata that is present but unreadable is reported, however: an indexdata
  of an unrecognised length or version, a malformed tag index, and a tag index
  attached to a blob whose group mixes element kinds (where no key set can be
  complete, and where `tags-filter` would skip the blob on the strength of it).
  Ordinary reads treat all three as "no metadata, decode the blob", which is
  the right call for a reader and no answer at all from a verifier. See
  [the CLI reference](reference/cli-reference.md#check).

- **`--distrust-metadata` on `tags-filter` and `inspect`.** Both commands
  make decisions from per-blob sidecar metadata that nothing validates
  against the payload it describes: `tags-filter` skips whole blobs on
  their tag index, and `inspect` reports counts, ID ranges and per-block
  stats straight out of `indexdata` whenever every blob has some. A file
  whose tag index omits a key its elements carry therefore loses those
  elements from `tags-filter` output silently - a wrong answer, not a slow
  one - and `inspect`, the tool you reach for when a file is suspect,
  reports that same suspect file's claims as measurements. The new flag
  decodes every blob instead, so the answer depends only on element data.
  On `tags-filter` it also waives the indexdata precondition; on `inspect`
  it applies to the scanning report and is rejected alongside `--indexed`,
  `--nodes` and a `header.*` `--get` key, which do not consult metadata
  contents at all.

- **`degrade --corrupt-tagdata`, `--corrupt-indexdata` and
  `--corrupt-indexdata-kind`.** The existing `--strip-*` flags only
  *remove* a hint, and a blob with no hint is decoded rather than trusted -
  so no degraded file could ever exercise a *wrong* hint, which is the
  failure mode that matters. These write a well-formed hint that lies: a
  tag index naming a key the blob does not carry; an index overstating the
  blob's count and max ID by 1,000,000; and a rotated element kind
  (node to way to relation to node) with the counts left honest. The kind
  is the field per-kind blob schedules select on, so it is the corruption
  that makes a consumer skip a blob outright rather than misreport it,
  which is why it is a flag of its own. All three are header-only rewrites,
  mutually exclusive with their `--strip-` counterparts and with any flag
  that re-encodes blobs.

- **`BlobReader::evict_page_cache(bool)` and
  `ElementReader::evict_page_cache(bool)`.** The buffered file
  constructors advise `POSIX_FADV_DONTNEED` over the consumed prefix
  after every blob, which is what keeps a sequential pass over a
  planet-sized file out of RSS - but the page cache is shared, so this
  silently leaves the file cold for anything that reads it next: a
  two-pass command, a test suite, an `osmium` running alongside. There
  was no opt-out short of switching to O_DIRECT, which is a different I/O
  mode entirely. The default has since flipped to keeping the page cache
  (see Breaking); pass `true` to evict.

  On `ElementReader` the builder can only govern the blobs it has not read
  yet: every file constructor decodes the header blob before returning, so
  `ElementReader::from_path_with_eviction(path, enabled)` is the one that
  makes the decision before the header is read.

### Fixed

- **`BlockBuilder` refuses an element its block cannot take instead of
  panicking.** Library: an `add_*` call on a block that is at its element
  cap or holds another element kind (the case `can_add_node`, `can_add_way`
  and `can_add_relation` report) used to panic. The element is now refused
  as a way with mismatched refs and locations already was: the builder is
  poisoned and every later `take` returns an error naming the element and
  the reason, so no block is written after it. The writing guide wrongly
  said the builder flushes on its own; it does not, and the guide's
  complete example now checks `can_add_way` and propagates errors.

- **`digest` reads its input once, and accepts a stream.** It used to open
  the input twice (a walk of every blob header, then the decoding pass), so
  an input renamed over between the two could have one file's blocks
  checked against the other file's indexdata, and a FIFO could not be
  digested at all. It now makes every check in its one decoding pass,
  through one open: a replaced input cannot mix two files into one digest,
  and `<FILE>` may be a FIFO or `/dev/stdin`. A partially indexed file is
  refused as before, located at the same blob, and a blob of unknown type
  is refused as before, now located at that blob; on a file with several
  faults, which one is reported can differ, since
  a partially indexed file is no longer refused before any block is
  decoded. Library: `digest::digest` takes `impl Into<InputSource>` (a
  path as before, or a `&PinnedInput` the caller took, opened through as
  it is; digest takes no pin of its own).

- **`check` reads one input file even if it is replaced mid-run.**
  `--ids` opens the input twice (the header and indexdata check, then the
  schedule walk its workers read through), and `--ids --full` took its
  allocation size from a separate stat of the path; a bare `check` runs
  `--ids` and then `--refs`, and `--index` alongside another check reads
  the input once more. Each of those resolved the path again, so an input
  renamed over during the run could be checked partly from each file, with
  no error. `--ids`, and any run of more than one check, now pins the input
  when it starts: on Linux with `/proc` a replaced or deleted input does
  not affect the run; on other Unix hosts the run fails with
  `ErrorKind::InputReplaced`. On those runs an input that is a FIFO, socket
  or character device is now refused up front; `--index` alone still reads
  a FIFO. `--refs` takes its allocation size from the open it reads.
  Library: `check::verify_ids::verify_ids`, `check::refs::check_refs` and
  `check::index::check_index` take `impl Into<InputSource>` (a path as
  before, or a `&PinnedInput` the caller took, used as it is);
  `verify_ids` pins a path itself, the other two open once and do not.

- **`inspect` reads one input file even if it is replaced mid-run.** The
  default report walks the blob headers and, on a blob without indexdata,
  starts again with a full decode; `--nodes` probes for indexdata, then
  walks the node blobs; `--indexed` or `--nodes` alongside the report each
  read the input again; and the full decode and `--get file.size` took the
  size from a separate stat of the path. Each of those resolved the path
  again, so an input renamed over during the run could be reported partly
  from each file, with no error. Every run that reads the input more than
  once now pins it when it starts: on Linux with `/proc` a replaced or
  deleted input does not affect the run; on other Unix hosts the run fails
  with `ErrorKind::InputReplaced`. The file size now comes from the open
  that read the file. On those routes (the default report, `--nodes`, and
  any report with `--indexed` or `--nodes`) an input that is a FIFO,
  socket or character device is now refused up front. `--extended`,
  `--locations` and `--distrust-metadata` without `--indexed` or
  `--nodes`, `--indexed` alone, a header `--get` key and `--show` read the
  input once and take no pin; the report and a header `--get` key still
  read a FIFO (its size reported as 0). Library: `inspect::inspect` and `inspect::node_stats::node_stats`
  take `impl Into<InputSource>` (a path as before, or a `&PinnedInput` the
  caller took, used as it is), and the new `inspect::inspect_header`
  returns the header block and the file size from one open.

- **`time-filter` reads one input file even if it is replaced mid-run.** On
  a snapshot input (no `HistoricalInformation`) every run opens the input
  twice (the header read, which picks the path, then the schedule walk the
  filtering workers read through), and each open used to resolve the path
  again, so an input renamed over during the run could have its header read
  from one file and its blobs filtered from another, with no error. The
  input is now pinned when the run starts: on Linux with `/proc` a replaced
  or deleted input does not affect the run; on other Unix hosts the run
  fails with `ErrorKind::InputReplaced` and publishes nothing. Because the
  path is only known once the header is read, the input is pinned on the
  history path too, which reads it once: an input that is a FIFO, socket or
  character device is now refused up front on either path, including a
  history file piped in through `/dev/stdin`. Library:
  `time_filter::time_filter` takes `impl Into<InputSource>` (a path as
  before, or a `&PinnedInput` the caller took, used as it is).

- **`degrade` reads one input file even if it is replaced mid-run.** The
  decode path (`--unsort`, `--unsort-intra`, `--strip-locations`,
  `--drop-ids`) opens the input three times (the indexdata check, the header
  read and the schedule walk the re-encoding workers read through), and the
  header-only passthrough opened it twice (the header, then every frame
  again from the start); each open used to resolve the path again, so an
  input renamed over during the run could be degraded under one file's
  header from another file's blobs, with no error. The decode path now pins
  the input when the run starts: on Linux with `/proc` a replaced or deleted
  input does not affect the run; on other Unix hosts the run fails with
  `ErrorKind::InputReplaced` and publishes nothing. On the decode path an
  input that is a FIFO, socket or character device is now refused up front.
  The passthrough now reads the header and every frame through a single
  open, so it needs no pin and reads a FIFO or `/dev/stdin` (buffered
  reads), which its second open could not, a FIFO serving no byte twice.
  Library:
  `degrade::degrade` takes `impl Into<InputSource>` (a path as before, or a
  `&PinnedInput` the caller took, used as it is on either route).

- **`repack` reads one input file even if it is replaced mid-run.** Every
  run opens the input three times (the indexdata check, the header read and
  the schedule walk the re-encoding workers read through) and reads its
  size, and each of these used to resolve the path again, so an input
  renamed over during the run could be re-encoded under one file's header
  from another file's blobs, with no error. The input is now pinned when
  the run starts: on Linux with `/proc` a replaced or deleted input does
  not affect the run; on other Unix hosts the run fails with
  `ErrorKind::InputReplaced` and publishes nothing. An input that is a
  FIFO, socket or character device is now refused up front. Library:
  `repack::repack` takes `impl Into<InputSource>` (a path as before, or a
  `&PinnedInput` the caller took, used as it is).

- **`inspect --tags` (tags-count) reads one input file even if it is
  replaced mid-run.** Every run opens the input twice (the indexdata check,
  then the schedule walk the counting workers read through), and each open
  used to resolve the path again, so an input renamed over between them was
  checked as one file and counted as the other, with no error. The input is
  now pinned when the run starts, after the expressions are accepted: on
  Linux with `/proc` a replaced or deleted input does not affect the run;
  on other Unix hosts the run fails with `ErrorKind::InputReplaced`. An
  input that is a FIFO, socket or character device is now refused up
  front. Library: `tags_count::tags_count` takes `impl Into<InputSource>`
  (a path as before, or a `&PinnedInput` the caller took, used as it is).

- **`build-geocode-index` reads one input file even if it is replaced
  mid-run.** Every build opens the input four times (the indexdata check,
  the replication header read, the relation pass and the schedule walk the
  later passes share), and each open used to resolve the path again, so an
  input renamed over during the build was indexed partly from each file,
  with no error. The input is now pinned when the build starts: on Linux
  with `/proc` a replaced or deleted input does not affect the build; on
  other Unix hosts the build fails with `ErrorKind::InputReplaced` and the
  existing index, if any, is left as it was. An input that is a FIFO,
  socket or character device is now refused up front (it could not serve
  the repeated reads before either). Library: `build_geocode_index` still
  takes the path in `BuildConfig::input_path` and pins it itself.

- **`diff --format osc` (derive-changes) reads each input file from the
  file it started on, even if one is replaced mid-run.** Every run opens
  each input two or three times (the header and indexdata read, then the
  parallel header walk, the sequential block reader or the streaming
  reader; a parallel run over negative IDs adds the sequential reader),
  and each open used to resolve the path again, so an input renamed over
  during the run was derived partly from each file, with no error. Both
  inputs are now pinned before either is read, once the output name is
  accepted, a path given twice sharing one pin. On Linux where
  `/proc/self/fd` can reopen the pin, a replaced or deleted input does not
  affect the run; elsewhere on Unix a later open that finds another file
  at the path fails the run with `ErrorKind::InputReplaced` (a deleted
  input fails that open), and the output is not published. An input that
  is a FIFO, socket or character device is now refused up front, in
  either position, before the other is read. Library:
  `diff::derive::derive_changes` takes each input as
  `impl Into<InputSource>` (a path as before, or a `&PinnedInput` the
  caller took, used as it is).

- **`diff` reads each input file from the file it started on, even if one
  is replaced mid-run.** Every run opens each input two to four times (the
  header and indexdata read, then the parallel header walk, the
  sequential block reader or the streaming reader; a parallel run over
  negative IDs adds the sequential reader, and `-t` excluding the later
  kinds adds a header walk of the rest of the file), and each open used to
  resolve the path again, so an input renamed over during the run was
  compared partly from each file, with no error. Both inputs are now
  pinned before either is read, a path given twice sharing one pin, so
  `diff a a` compares one file with itself. On Linux where
  `/proc/self/fd` can reopen the pin, a replaced or deleted input does not
  affect the run; elsewhere on Unix a later open that finds another file
  at the path fails the run with `ErrorKind::InputReplaced` (a deleted
  input fails that open), and no `-o` output is published. An input that is a FIFO, socket or character device is now
  refused up front, in either position, before the other is read.
  Library: `diff::diff` and `diff::diff_stats` take each input as
  `impl Into<InputSource>` (a path as before, or a `&PinnedInput` the
  caller took, used as it is); a run refused mid-way may already have
  written text to the caller's writer.

- **`cat` reads each input file from the file it started on, even if one is
  replaced mid-run.** Every mode opens each input two to four times (the
  indexdata check under `--type`, the header read, an indexdata probe when
  an input declares a way enrichment, then the blob reader or the
  `--clean` schedule walk; `--dedupe`'s check, index pass and second
  pass), and each open used to resolve the path again, so an input renamed
  over during the run was read partly from each file, with no error. Every
  input is now pinned before any is read, a path given twice sharing one
  pin: on Linux where `/proc/self/fd` can reopen the pin, a replaced or
  deleted input does not affect the run; elsewhere on Unix the run fails
  with `ErrorKind::InputReplaced` and publishes nothing. An input that is
  a FIFO, socket or character device is now refused up front, before any
  other input is read. Plain `cat` holds its pins for the run, one
  descriptor per distinct input (it used to hold one input open at a
  time), and `--dedupe` one per argument as before, so `cat` now asks its
  host for them first: the CLI raises its soft
  open-file limit to the hard limit, and a run the limit cannot hold is
  refused before it reads anything, naming the counts. Library hosts are
  asked through `ProcessActions::fd_allowance`, which `cat::cat` and
  `cat::dedupe::merge_pbf` now call.

- **`renumber` reads one input file even if it is replaced mid-run.**
  Every run opens the input three times (the header read, the one reader
  every rewrite phase shares, and the schedule walk), and each open used
  to resolve the path again, so an input renamed over during the run was
  read partly from each file, with no error. The input is now pinned when
  the run starts: on Linux with `/proc` a replaced or deleted input does
  not affect the run; on other Unix hosts the run fails with
  `ErrorKind::InputReplaced` and publishes nothing. An input that is a
  FIFO, socket or character device is now refused up front. Library:
  `renumber::renumber_external` takes `impl Into<InputSource>` (a path as
  before, or a `&PinnedInput` the caller took).

- **`getparents` reads one input file even if it is replaced mid-run.**
  Every run opens the input two or three times (the indexdata check, the
  scan-arm estimate on indexed input, then the header walk or the full
  scan), and each open used to resolve the path again, so an input renamed
  over during the run was read partly from each file, with no error. The
  input is now pinned before any ID file is read: on Linux with `/proc` a
  replaced or deleted input does not affect the run; on other Unix hosts
  the run fails with `ErrorKind::InputReplaced` and publishes nothing. An
  input that is a FIFO, socket or character device is now refused up
  front, before a `-I` PBF is read. Library: `getparents::getparents` and
  `getparents::getparents_with_min_blobs` take `impl Into<InputSource>` (a
  path as before, or a `&PinnedInput` the caller took), and so does
  `has_indexdata`, which probes through a pin it is given.

- **`getid` reads one input file even if it is replaced mid-run.** Every
  mode opens the input two or three times (the indexdata check, the
  scan-arm estimate and the scan; `--add-referenced`'s two passes;
  `--invert`'s check and walk), and each open used to resolve the path
  again, so an input renamed over during the run was read partly from each
  file, with no error. The input is now pinned before any ID file is read:
  on Linux with `/proc` a replaced or deleted input does not affect the
  run; on other Unix hosts the run fails with `ErrorKind::InputReplaced`
  and publishes nothing. An input that is a FIFO, socket or character
  device is now refused up front, before a `-I` PBF is read. The `-I` PBF
  of `getid` and `getparents` is pinned too, so a FIFO there is refused
  rather than opened, which used to wait for a writer. Library:
  `getid::getid`,
  `getid::removeid` and `getid::parse_ids_from_pbf` take
  `impl Into<InputSource>` (a path as before, or a `&PinnedInput` the
  caller took), and the new `InputSource::pin` pins a path or shares a
  pin.

- **`tags-filter` reads one input file even if it is replaced mid-run.**
  The PBF input is opened several times per run (the indexdata check, the
  header reads, the schedule walks of pass 1, the relation closure, the
  way dependencies and pass 2, and under `-R` the check and the read), and
  each open used to resolve the path again, so an input renamed over
  during the run was read partly from each file, with no error. The input
  is now pinned when the run starts, after the expressions are accepted:
  on Linux with `/proc` a replaced or deleted input does not affect the
  run; on other Unix hosts the run fails with `ErrorKind::InputReplaced`
  and publishes nothing. An input that is a FIFO, socket or character
  device is now refused up front in every mode, including `-R` with
  `--invert-match` or `--distrust-metadata`, which read the input once and
  used to accept one. `--input-kind osc` is unchanged.

- **`extract` and `multi-extract` read one input file even if it is
  replaced mid-run.** Every strategy opens the input several times per run
  (the sortedness and indexdata checks, the schedule walks, the header
  read, the classify and write passes, and on a multi-extract that falls
  back to one extract per region, every region's passes), and each open
  used to resolve the path again, so an input renamed over during the run
  was read partly from each file, with no error. The input is now pinned
  once when the run starts, for every region of a multi-extract: on Linux
  with `/proc` a replaced or deleted input does not affect the run; on
  other Unix hosts the run fails with `ErrorKind::InputReplaced` and
  publishes nothing. An input that is a FIFO, socket or character device
  is now refused up front (it could not serve the repeated reads before
  either).

- **`sort` reads one input file even if it is replaced mid-run.** The
  input is opened several times per run (the indexdata check, pass 1's
  header walk, pass 2's passthrough source, and one reader per overlap run
  being re-sorted), and each open used to resolve the path again, so an
  input renamed over during the run was read partly from each file, with
  no error. The input is now pinned when the run starts: on Linux with
  `/proc` a replaced or deleted input does not affect the run; on other
  Unix hosts the run fails with `ErrorKind::InputReplaced` instead. An
  input that is a FIFO, socket or character device is now refused up
  front (it could not serve the repeated reads before either).

- **`add-locations-to-ways` reads one input file even if it is replaced
  mid-run.** Both index types open the input several times per run (the
  header and indexdata read, the header walks, the worker pools, and on
  the external index each stage's reader), and each open used to resolve
  the path again, so an input renamed over during the run was read partly
  from each file, with no error. The input is now pinned when the run
  starts, in `add_locations_to_ways` and in `altw::external::external_join`
  alike: on Linux with `/proc` a replaced or deleted input does not affect
  the run; on other Unix hosts the run fails with
  `ErrorKind::InputReplaced` instead. An input that is a FIFO, socket or
  character device is now refused up front (it could not serve the
  repeated reads before either).

- **`apply-changes` reads one base file even if it is replaced mid-run.**
  The base PBF is opened several times per run (the header and indexdata
  read, the header walk, the worker pool, the passthrough copy source), and
  each open used to resolve the path again, so a base renamed over during
  the run (an atomic replace by a downloader) was read partly from each
  file, with no error. The base is now pinned when the run starts: on Linux
  with `/proc` a replaced or deleted base does not affect the run; on other
  Unix hosts the run fails with `ErrorKind::InputReplaced` instead. A base
  that is a FIFO, socket or character device is now refused up front (it
  could not serve the repeated reads before either).

- **A bad user name or member role index is refused on re-encode, not
  written as an empty string.** A user string index or relation member role
  index past its block's string table (or negative) used to be re-encoded as
  an empty user or role by every command that rebuilds blocks: `sort`,
  `cat --dedupe`, `cat --clean`, `degrade`, `repack`, `time-filter`,
  `extract`, `multi-extract`, `tags-filter`, `getid`, `getparents` and
  `add-locations-to-ways`. They now fail with an error naming the element
  and the index (`node 1: user name string index 9`,
  `relation 1 member 0: role string index 9`), as the OSC writers already
  did; `cat --clean user` refuses too, since the input is corrupt whichever
  fields are kept. Tag string indices are unchanged (see CORRECTNESS.md).
  Library: `Info::user` and `DenseNodeInfo::user` report a negative index as
  `ErrorKind::WireFormat` instead of `StringtableIndexOutOfBounds` for index 0.

- **An element carrying two metadata messages is refused, not half-read.**
  A node, way or relation with its `Info` written twice, or a dense group
  with `DenseInfo` written twice, used to read as its last occurrence:
  fields only the first one set were dropped, and a corrupt first `Info` was
  never checked and could read as version 0 / timestamp 0. The shared element
  parsers (every decode, and the node, way and indexdata scanners) now refuse
  a second occurrence that carries data, naming the field, as they already
  refused a packed array split across occurrences; renumber's byte-level
  rewriter and raw blob passthrough still copy such an element unchanged. No known writer
  produces either shape; an empty extra occurrence is still accepted.
  `Node::info`, `Way::info` and `Relation::info` no longer fall back to
  default metadata on a parse failure: block construction rules it out, and
  reaching it would be a pbfhogg bug, so they panic. The block and group
  iterators (`elements`, `elements_skip_metadata`, `dense_nodes`, `nodes`,
  `ways`, `relations`) and dense node tags follow the same rule: where they
  once read such a failure as no dense nodes, absent dense metadata, a
  skipped element or a shortened tag list, they now panic.

- **`getid` accepts a partially indexed input in every mode.** A file whose
  first data blob carries indexdata and a later one does not was refused by
  `getid --add-referenced` when way IDs were requested, and as a
  `-I`/`--id-osm-file` ID file by `getid` and `getparents`, while every
  other `getid` mode ran on it. Every `getid` path decodes a blob that
  lacks indexdata, so the output is the same as from the file fully
  indexed; these now run too. A primary input whose first data blob lacks
  indexdata still needs `--force` (an `-I` file never did), and
  `getid --invert` still refuses an unindexed blob holding ways under a
  header that declares way enrichment.

- **`apply-changes --force` writes a wholly indexed output.** On an
  unindexed base, the blobs the diff rewrote got indexdata and the ones it
  left alone were copied without it, so the output was partially indexed and
  `sort`, `diff`, `tags-filter`, `extract` and `apply-changes` itself refused
  it. Untouched blobs now keep their compressed payload under a header
  carrying indexdata and tagdata derived from it, as plain `cat` does. A blob
  no single index describes is re-encoded, and one with no elements is
  dropped. A blob whose elements span more than one kind is now refused
  (exit 2, nothing published) instead of being written with creates and
  modifications of its second kind out of type order or duplicated; re-encode
  such a base with plain `cat` first.

- **`check --ids` reports a node blob after a relation blob when the file has
  no ways.** Type order was checked only as node before way and way before
  relation, so a file with no ways and its relations ahead of its nodes
  passed. The node/relation pair is now checked directly. Every type-order
  violation also named its two kinds the wrong way round: a node blob past a
  way blob printed `type order: way after node` and, under `--json`,
  `"found": "way", "after": "node"`. It now reads `node after way`, with
  `found` the misplaced kind. The indexdata
  trust model (what every command believes about indexdata, tagdata and the
  sorted flag, what a false claim costs, and that `check --index --ids` is
  the admission test for a file from elsewhere) is now stated in one place,
  `reference/indexdata-trust.md`.

- **`add-locations-to-ways --index-type auto` judges indexdata the way the
  backends do.** It used to look at the frame right after the header, of any
  type, while the indexdata check that follows looks at the first data blob,
  so the two could disagree: a header-only file was "indexed=false" to the
  selection note, and an extension frame ahead of the data decided the
  answer. Both now read the first data blob's indexdata. The selection no
  longer reads that blob's payload, so a malformed payload there is no longer
  refused at selection; it is up to whichever pass reads it.

- **`diff --format osc` on input without indexdata keeps every metadata
  field.** Creates and modifies derived from a file without blob indexdata
  carried only the version and lost timestamp, changeset, uid and user, which
  the indexed path printed, so the OSC depended on whether the inputs were
  indexed. Both sequential paths now print elements the same way, byte for
  byte, and the text `diff` (verbose details included) likewise. Two edge
  behaviours moved with it: a derived element whose tag, role or user name
  points past its block's string table, or a modified element whose tag or
  role does in a verbose diff, is now refused on every path (the indexed path
  used to print it shortened); and on unindexed input an element with a bad
  tag or role index that is only compared or deleted is no longer refused:
  it is reported as different (or deleted), as on indexed input.
- **`--direct-io` and `--io-uring` keep the alignment the device requires.**
  Direct I/O assumed 4096-byte alignment everywhere, so on a device or
  filesystem requiring more (a block device with a logical block size above
  4 KiB) the open succeeded and the first transfer failed with a bare
  `Invalid argument`. The alignment is now read per file from `statx`
  (`STATX_DIOALIGN`, Linux 6.1+, glibc builds) and used when it exceeds
  4096; every host requiring 4096 or less, or not reporting, does exactly the
  I/O it did before. A reported alignment the 256 KiB direct I/O buffers
  cannot keep (not a power of two, or larger than the buffer) is refused at
  open as `platform feature direct-io unavailable` (exit 3).
- **The sequential `diff` refuses blobs out of order within a kind instead
  of answering wrongly.** The sequential block-pair merge (`diff -j 1`,
  `-v`, every run on input holding negative IDs, and the sequential
  `--format osc`) checked that kinds come in order but not that blobs within
  a kind do: old node blobs `[100,200]` then `[1,50]` against a correctly
  ordered file with the same nodes reported 50 created and 50 deleted on
  identical content, where the parallel path refused the file. Both paths
  now refuse it as unsorted, also inside a kind a `--type` filter excludes.
- **The unindexed `diff` and `diff --format osc` no longer drop a misplaced
  block.** Without indexdata, a block of an earlier kind after a later one
  (a node block after the ways) was passed over by every phase and silently
  left out of the answer, and element order within a kind was never
  checked. Both are refused as unsorted now, whatever `--type` selects.
- **`diff --type` refuses the same malformed files at every `-j`.** A filter
  excluding the last kinds (`-t node`) made the sequential path stop reading
  after the selected kinds, so a truncated tail, a later data blob without
  indexdata or tail blobs out of order were refused only at `-j` above 1;
  the sequential path now walks the remaining blob headers. In the other
  direction, `-j 1` parsed the envelope of an excluded kind's blob it read
  past and refused a malformed one the parallel path never reads; no path
  interprets an excluded kind's blob bodies now.
- **Valid blob layouts with mixed-sign IDs are no longer refused as
  unsorted.** The blob order check took 0 as the first element of a blob
  whose ID range spans negative and positive IDs, so a blob holding -2 and 1
  after a blob holding -1 (canonical order 0, -1, -2, ..., 1, 2, ...) was
  refused: `apply-changes` refused to write such output and the
  `add-locations-to-ways` external join refused such way and relation blobs.
  Only disorder the ID ranges prove is refused now.
- **A `BlobHeader` field with the wrong wire type is refused by every
  reader.** A known field (type, indexdata, datasize, tagdata, waymembers)
  carrying a wire type its schema does not allow was skipped by some read
  paths and misread or refused by others, so whether a file was refused
  depended on the command and its flags. It is a wire-format error on every
  path now.
- **`build-geocode-index --overwrite` no longer deletes an input kept
  inside the index directory.** Replacing an index swaps the whole
  directory and deletes the old one, and the alias check never ran for
  this command, so `build-geocode-index idx/in.osm.pbf -o idx --overwrite`
  published the index and deleted `in.osm.pbf` (a symlink there was
  deleted the same way). An `--output-dir` that contains the input, or the
  entry naming it, is now refused before any work with `output <dir>
  contains input <path>`, exit 2.
- **A version below 1 is no longer printed as a version.** A node written
  without metadata into a dense block whose other nodes carry it is stored
  as version 0 with zeroed fields (as libosmium also writes it), and
  `diff --format osc` deleted it as `version="0"` (`version="1"` under
  `--increment-version`), created or modified it with `version="0"
  timestamp="1970-01-01T00:00:00Z" changeset="0" uid="0"`, and `export
  --metadata` gave it `@version` 0 (or -1 for an Osmosis absent slot).
  OSC output, the text diff's `vN`, `inspect --show`, the `inspect
  --extended` metadata census and `export --metadata` now print a version
  only when it is 1 or more. For a dense node without a version, the other
  metadata fields print only where non-zero, so the padding prints nothing
  while a real versionless node keeps its timestamp, changeset, uid and
  user; an `Info` keeps every field it carries. New library accessors
  `Info::claimed_version()` and `DenseNodeInfo::claimed_version()` apply
  the same rule. See the correctness guide.
- **`cat` indexes every blob it can decode.** A blob whose block no single
  index can describe (no elements, a group carrying more than one element
  kind, or kinds that change across groups - all legal PBF, written by other
  tools) used to pass through `cat` unindexed, so following the "generate an
  indexed PBF first" advice on such a file got the same refusal again. Those
  blobs are now re-encoded into single-kind blocks, in decoder order, and
  indexed; an element-less block is dropped. Re-encoding goes through the
  block builder, so these blobs take the same normalisation as `cat --type`
  and `--clean` (dense nodes, coordinates at decimicrodegree granularity,
  whole-second timestamps). A block the decoder rejects is a located error
  instead of a silent pass-through. When a run both decodes and copies
  blobs, its summary reads `Decoded N blobs, wrote M elements; K blobs
  passed through`.
- **`diff` on a partially indexed input is the missing-indexdata error.** A
  file indexed on its first data blob but not on a later one used to fail
  mid-merge with a generic "block-pair merge requires indexdata"; both diff
  paths (and `--format osc`) now refuse it with the same error and advice
  as an unindexed input, located at the first unindexed blob.
- **`check --index` fails a partially indexed file.** A file whose data
  blobs carry indexdata on some and not others now exits 1 with a violation
  at the first unindexed blob, instead of exiting 0 with a count. A wholly
  unindexed file still passes, and so do blobs no index could describe.
- **Raw passthrough copies fall back to userspace wherever the kernel
  refuses `copy_file_range`.** Only a cross-device copy (`EXDEV`) used to
  fall back; on FUSE and some NFS mounts (`EOPNOTSUPP`), pre-4.5 kernels
  (`ENOSYS`), container seccomp profiles (`EPERM`) and non-regular outputs
  such as `/dev/null` (`EINVAL`) the run failed instead. The fallback is
  taken only before any byte of a range has moved, never between two ends
  that are the same file or onto an appending descriptor. Interrupted
  reads, writes and copies are retried everywhere, and the io_uring copy
  path accepts a short read instead of failing on it.
- **Frame errors no longer invent an offset.** A `BlobReader` built with
  `new` on a reader with no known position used to report truncation "from
  offset 0"; it now names no offset. Its length-prefix read failures keep
  the frame's offset, and a failed seek keeps its target.
  `BlobReader::blob_from_offset` reads a valid frame after an earlier failed
  read instead of reporting "no blob" there.
- **`add-locations-to-ways --index-type external` on an input whose header
  is not sorted** is the unsorted-input error naming the file, with the
  CLI's sort advice.
- **`add-locations-to-ways --index-type external` refuses indexdata ranges
  that contradict the sorted header.** Node blobs out of order or
  overlapping, under a header claiming `Sort.Type_then_ID`, used to surface
  as "node references could not be resolved" (or zeroed coordinates under
  `--ignore-missing-nodes`); they are now the unsorted-input error, located
  at the offending blob, before any join work. The same scan refuses way
  and relation blobs out of order and a node blob whose range holds
  negative IDs, which the external join cannot handle (the sparse index
  can). Ranges are exact by contract, so a file whose indexdata overstates
  `max_id` is refused too.
- **`diff -j 1`, `diff -v` and a quiet text `diff` no longer need a
  writable current directory.** A text diff whose command line guarantees
  the sequential path makes no scratch directory, so it runs from a
  read-only directory and no longer creates and removes a `.pbfhogg-run-*`
  there. A quiet text diff still shards, but only counts: it no longer
  writes every shard's text to scratch (tens of GB at planet scale) to
  throw it away, and it refuses `--tmp-dir`, which has nothing to place.
  Every other run that may shard still makes its directory at startup and
  fails there when the location is unusable. Library callers of
  `diff::diff` pass `None` for the scratch argument in the one-shard case;
  the new `diff::diff_stats` returns the same counts as an unsuppressed
  `diff` without producing text, and takes no scratch at all.
- **A file without a leading OSMHeader is the missing-header error in
  `check --index`, `check --refs` and `inspect --show`.** `check --index`
  reported a clean pass and `inspect --show` found or missed elements on
  such a file, and `check --refs` (and any command on the shared per-kind
  schedule scan) silently dropped a leading data blob and then reported
  missing references.
- **The unindexed-input refusal advises only a switch that works.** It no
  longer says `Or pass --force` for `add-locations-to-ways --index-type
  external` or `--inject-prepass` and `apply-changes --locations-on-ways`,
  where `--force` is refused, and `build-geocode-index` names
  `--allow-unindexed`. The library error itself no longer names any flag.
- **`merge-changes` and `apply-changes` name an OSC they cannot open**,
  instead of a bare `No such file or directory`.
- **`add-locations-to-ways` can no longer hang when its output write
  fails.** The in-memory backends' rewrite pass over indexed input and the external backend's
  final stage kept the decoded-block channel open after the writer
  returned an error, so decode workers blocked on a full channel and the
  run never ended; the error is now reported.

- **A header-only PBF counts as indexed.** A valid file with a header and no
  data blobs has no blob lacking indexdata, so every command that wants
  indexdata (`sort`, `repack`, `getid`, `tags-filter`, `apply-changes`,
  `add-locations-to-ways`, `extract`, `cat --type`, `inspect --nodes`, ...)
  now accepts it without `--force`, and `inspect --indexed` prints
  `Indexed: yes` and exits 0. They used to refuse it with advice to generate
  an indexed PBF.

- **A missing first header is one error that names the file.** A zero-byte
  file, or one whose first blob is not the OSMHeader, now stops every command
  with `PBF file does not start with an OsmHeader blob` and the file's path,
  exit 2, before any question about indexdata. Seven different spellings,
  most naming no file, are gone. `inspect --indexed` exits 2 for such a file
  instead of answering `Indexed: no` with exit 1. `inspect`, `extract
  --simple`, `renumber`, `diff` and `bench-write` used to read a header-less
  file as empty or as unsorted instead of refusing it. Library: the error is
  `ErrorKind::MissingHeader` with the file in its location.

- **Every input file an output could clobber is checked, not only the
  positional ones.** `getid` and `getparents` `-i` and `-I`, the `-e`
  expressions file of `tags-filter` and `export`, `extract --polygon`, the
  `extract --config` file and the `polygon_file` entries it names are all
  refused as an output (`output ... aliases input ...`, exit 2) before any
  work. `getid -I ids.osm.pbf -o ids.osm.pbf -O` used to replace the ID file.
  Library: `extract::ExtractSlot` gains a `polygon_file` field.

- **Output headers describe the output.** Every PBF-writing command now
  derives its header from one rule (`reference/cli-reference.md` "Output
  header"); what changed:
  - `extract` no longer declares `Sort.Type_then_ID` on the extract of an
    unsorted file (every strategy, and `--config`). Downstream commands that
    trust the flag (`apply-changes`, `diff`, `renumber`, `digest`,
    `cat --dedupe`) were reading an unsorted file as sorted.
  - `cat`, `cat --type`, `sort` of sorted input and `getid --invert` of node
    or relation IDs copy way blobs whole, and now keep the
    `LocationsOnWays`, `pbfhogg.WayMembers-v1` and
    `pbfhogg.SharedNodePins-v1` declarations of the input; they used to drop
    them while copying the blobs that carry them, so `export` refused a
    `cat` of an enriched file. A `cat` of several inputs keeps
    `LocationsOnWays` when every input declares it, and never the prepass
    features.
  - `degrade`'s header-only flags with `--generator` or `--output-header`
    no longer rebuild the header: each override replaces only its own field,
    so the prepass declarations, custom features, writing program and
    unknown fields survive as they do without an override.
  - `apply-changes` declares no bbox: its OSC can place nodes outside the
    base's box. (Its replication fields are still the base's; set them with
    `--output-header`.)
  - `time-filter` writes the earlier of the input's replication timestamp
    and the cutoff (it used to write the cutoff even when later than the
    data), and drops the sequence number when the output is no longer at the
    input's replication state.
  - With several `cat` inputs, the replication timestamp and sequence number
    are kept only when the inputs agree on the timestamp, sequence number and
    base URL together.
  - A carried bbox is widened onto the decimicrodegree grid when off it, so a
    re-encoded node on its edge stays inside.
  - A raw-copying command whose first data blob carries indexdata but a later
    way blob does not now fails instead of writing that blob without the
    enrichment its header already declared.
  - `tags-filter --input-kind osc` refuses `--generator` and
    `--output-header`, which have no PBF header to land in, instead of
    ignoring them.
  - `--output-header osmosis_replication_timestamp` accepts
    `YYYY-MM-DDTHH:MM:SSZ` as well as UNIX seconds, like `time-filter`.

- **No output path ever holds a partial or abandoned file.** Every output,
  through the CLI and through the library alike, is written to a hidden
  `.pbfhogg-tmp-*` staging file beside it and renamed into place only after
  its final flush and every check on it succeed; a failed or dropped run
  removes the staging file and leaves an existing output untouched. Without
  `--overwrite` the final rename is exclusive, so an output that appears
  while a long run is in flight is not clobbered either. `extract --config`
  publishes its outputs as a group and rolls back the ones already in place
  if a later one cannot be published, instead of leaving a half-replaced
  set. `merge-changes`, `tags-filter` on OSC input and `export -o` now check
  their final buffered write and sync the file before publishing; the gzip
  OSC writers could previously report success after a failed last write.
  `build-geocode-index` syncs the new index's files before it replaces the
  old index.

- **Overlapping and regional diffs merge and apply instead of failing the
  version gate.** `apply-changes` and `merge-changes --simplify` refused any
  change whose version did not exceed the highest already seen for the
  object, so an equal version aborted the whole command. Equal versions are
  normal input: overlapping replication diffs repeat boundary changes, and
  regional extract diffs (Geofabrik's, or `diff --format osc` without
  `--increment-version`) delete an object leaving the region at the version
  it already had - Geofabrik germany seq 4705 modifies a node at version 12
  and seq 4706 deletes it at version 12, so every range spanning them
  failed. An equal version is now admitted and the later change wins by
  position; only a lower version is refused, and its error names the
  likely causes (inputs out of sequence order, inconsistent diff series)
  instead of "applying the same diff twice". `--allow-version-regression`
  is unchanged.

- **`apply-changes` no longer mislabels tags in rewritten blocks whose
  string table repeats a string.** Rewritten elements copy the input block's
  string indices, which assumed the output table matched the input slot for
  slot; a duplicate entry, or an empty string after slot 0, shifted every
  later index, so tags named the wrong strings with no error. Every input
  slot is now preserved.

- **`add-locations-to-ways --index-type auto` respects container memory
  limits and refuses an unreadable first blob.** The page-cache budget is
  capped by the cgroup v2 headroom (`memory.max - memory.current`) of the
  process's group and its visible ancestors, so a memory-limited container
  on a large host no longer routes a large input to sparse and thrashes.
  When the limits cannot be read the budget is unknown and sorted, indexed
  input routes external, as it already did without `/proc/meminfo`. A
  first data blob that cannot be read or parsed is now an error instead of
  routing sparse as "not indexed".

- **`--io-uring` is refused when the kernel's opcode probe fails.** A failed
  probe used to be treated as support; it is now the same
  `platform feature io-uring unavailable` refusal (exit 3) as a probe that
  reports no `WriteFixed`. Kernels 5.1 to 5.5, which predate the probe but
  support `WriteFixed`, still run.

- **Plain (non-dense) `Node` messages are read by every node pass.**
  `add-locations-to-ways` (both index backends), the external join and
  single-region simple `extract` refused a node blob holding plain nodes
  with a wire-format error, and `multi-extract` with only bbox regions
  silently left such nodes out of every region. Both encodings are now
  read. The wire-level node passes share the checked coordinate scaling
  `apply-changes --locations-on-ways` already used; `multi-extract`
  classifies plain nodes from the decoded block, as its element-by-element
  path always has. pbfhogg itself writes
  only dense nodes; `osmium cat -f pbf,pbf_dense_nodes=false` produces
  such files.

- **A block `granularity` or `date_granularity` outside `int32` is refused.**
  Both were narrowed with a bare cast, so a value such as `2^32 + 100` read
  as `100` and every coordinate or timestamp in the block was scaled by a
  different, plausible value with no error. The decoder and the blob-index
  scanner now reject the block.

- **`check --index` reports a duplicated `BlobHeader.tagdata` field.** A
  blob carrying two non-empty tagdata fields has no usable tag index, but
  `check --index` counted it as a blob without tagdata and passed it. It is
  now reported as malformed tagdata and counted among the blobs with
  tagdata.

- **Blocks built by `BlockBuilder` list an empty-string tag key in their tag
  index.** The key was left out, so a blob whose elements carried a tag with
  key `""` advertised a key set without it: `tags-filter` skipped that blob
  for an expression needing the empty key (a wrong answer), and
  `check --index` reported it as `TagKeysMissing`. Files written by
  re-encoding commands such as `repack` before this fix keep the short tag
  index until rewritten.

- **`build-geocode-index` refuses an index its format cannot address instead
  of writing a corrupt one.** Past 4 GiB of interned strings the string-pool
  offsets wrapped and later names pointed at the wrong text; address-point,
  street, interpolation, admin-polygon and cell counts, and the replication
  sequence, were narrowed to `u32` the same way. Each now fails the build
  with a message naming the limit.

- **`extract --polygon` refuses a degenerate ring** (fewer than three
  positions, or zero area) instead of producing an empty extract and exiting
  0. Open rings are still accepted and closed implicitly.

- **`inspect --show` prints nothing to stderr when the element is absent**,
  matching the exit-1 contract; the exit code alone carries the answer.

- **A malformed data block is refused instead of being written without an
  index.** Plain `cat` and `PbfWriter::write_primitive_block` used to treat
  a block the index scanner could not parse as merely unindexable and copy
  it through; `check --index` passed such a block when it carried no stored
  metadata. All three now fail.

- **`apply_changes::merge` refuses an output path that is the base input**
  (same file, symlink or hard link) before opening anything. The CLI already
  refused it; a library caller would have truncated the base.

- **`add-locations-to-ways --index-type external` accepts ways with no
  refs.** A zero-ref way used to desynchronise the per-way refcount sidecar
  and fail the run, while the sparse backend accepted the same input; which
  backend `auto` picked decided whether a legal file was accepted.

- **An apply-changes scanner or worker panic is returned as an error**
  carrying the panic message, instead of being dropped when the drain also
  failed or escalating into a process panic.

- **Multi-extract progress lines name the final output files** instead of
  the hidden `.pbfhogg-tmp-*` scratch names.

- **Single-pass multi-extract reports each region once.** The library's
  per-region `extract.multi.slot_counts` progress event is gone; the CLI
  summary is the one report, and library callers read the returned
  `ExtractStats`.

- **`build-geocode-index` output is reproducible.** Pass 2 merged blob
  results in worker-completion order, so record order, string offsets and
  which duplicate admin way survived could differ between two builds of the
  same input. Results are now merged in file order. A node that appears
  more than once with conflicting coordinates now fails the build instead
  of an arbitrary copy winning; identical repeats are still accepted.

- **The decoder refuses two block shapes it used to misread.** A
  `PrimitiveGroup` carrying more than one `DenseNodes` message, where every
  node after the first message was silently dropped, and a Node, Way or
  Relation tag string index above `u32::MAX`, which was narrowed to a
  different and possibly valid string. Both are now wire-format errors in
  every reader, in indexdata generation and in the raw node and way scanners.

- **A dense-node tag string index above `i32::MAX` is refused** when a block
  is decoded, instead of being narrowed to a different and possibly valid
  string.

- **Plain `cat` names the input file and byte offset** when a blob without
  indexdata fails to scan, instead of printing the scanner's message alone.

- **Element IDs whose deltas wrap are read the same way everywhere.** A
  block the writer produces for IDs `i64::MIN` then `i64::MAX` decodes to
  those IDs; indexdata generation (so `PbfWriter` itself), the
  add-locations-to-ways node and way scanners and the apply-changes node
  extractor used to refuse it as an overflow.

- **Geocode indexes no longer carry duplicate coarse-cell entries** for
  segments crossing more than four coarse cells.

- **`HeaderBuilder::build` refuses a caller-set bbox that is non-finite, out
  of range or inverted**, instead of writing `0,0,0,0` or a nonsense header.
  A bbox copied from an input header is still passed through as read.

- **`derive-changes --update-timestamp` stamps every delete in a run with the
  same timestamp**, taken once at start. It used to read the clock per
  delete, so one run's output straddled seconds and differed between `-j 1`
  and `-j N`.

- **`inspect FILE tags OTHER` is refused** instead of silently inspecting
  `OTHER`, and `extract` with no `--bbox`, `--polygon` or `--config` is now a
  usage error at argument parsing.

- **`check --json` reports the real file name** (converted lossily if it is
  not UTF-8) instead of the placeholder `input.osm.pbf`.

- **`export`'s invalid-way count no longer includes ways outside the bbox**;
  the bbox filter now runs before way geometry is built, which also skips
  that work for rejected ways.

- **A delete followed by a stale re-create of the same object in an OSC no
  longer drops the delete.** The re-create was refused by the version gate,
  but the delete had already been cleared, so the object survived.

- **`check --max-errors` now bounds the reference check, and `0` means
  unlimited in every check.** The reference check collected every missing
  reference whatever the cap, so `--show-ids` on an extract with millions of
  dangling refs grew without bound; `verify_ids` with a cap of 0 kept no
  details at all.

- **`add-locations-to-ways -o out.osm.pbf` works with a bare relative output
  name.** The sparse backend failed with "failed to query free space for
  sparse index in :" because the output's parent directory was the empty
  path; scratch for `diff` and `derive-changes` had the same gap.

- **Truncated inputs are caught on more read paths.** An O_DIRECT read that
  skips to a page-aligned point past the end of a truncated file now errors
  instead of passing; a truncated raw frame no longer leaves the reported
  offset past the failure; and opening a missing file through the header
  walker keeps `io::ErrorKind::NotFound`.

- **A geocode index with a non-finite or negative radius in its header is
  refused on open**, instead of opening and answering "nothing here" for
  every query.

- **`degrade` bounds its preallocation by the input size** whatever the
  block cap, its "needs at least one flag" error names `--drop-ids`, and its
  help lists every transformation flag. The largest block cap no longer
  overflows the unsort predicate in debug builds, and an unfinished boundary
  swap no longer counts the held final element twice.

- **Sorted reader assertions handle history and canonical OSM ID order.**
  Both iteration paths allow repeated node IDs in history files, and debug
  assertions accept negative IDs in canonical order.

- **Blob indexdata and tagdata are generated and read strictly.** The
  scanners that derive a blob's index used to stop at the first bad tag and
  skip groups they failed to parse, writing a tag index that omitted keys
  (so `tags-filter` skipped blobs that matched) and ID ranges and counts that
  understated the blob. They now parse every element with the decoder's own
  parsers and produce an exact index or none. Non-dense node IDs were read
  without zigzag decoding, so blobs of plain nodes were indexed with the
  wrong ID range (node 3 recorded as 6); `cat` wrote those ranges into its
  output. On the read side, a 26-byte indexdata whose version byte claims v2
  or v3 is no longer zero-padded into a bbox at (0, 0) that spatial filters
  trusted, and an inverted ID range or a repeated indexdata or tagdata field
  now reads as no index.

- **`renumber` shifts orphan references past the renumbered range.** A way
  ref or relation member pointing at an object absent from the input used to
  keep its old ID, which could equal the new ID of a different element, so
  the reference silently pointed at the wrong object. An orphan with old ID
  X now becomes `start + count + X` for its kind; the old ID is recoverable
  by subtraction. This changes the IDs of orphan references in the output.
  Relation member types are now read as int32, as every reader reads them:
  a raw type value of 2^32 or above used to be treated as unknown and kept
  its old ID, while readers saw a node member.

- **`renumber` refuses input it would renumber wrongly, and no longer hangs.**
  New IDs are assigned by position but references resolved by rank, which
  only agree when IDs strictly increase; a file whose header falsely claimed
  to be sorted, or that carried a duplicate ID, got ways and relations
  pointing at the wrong elements, exit 0. Renumber now checks the order
  itself and refuses such input, and also refuses groups mixing element
  kinds, repeated `DenseNodes` or `id` fields, and delta sums that overflow,
  each of which used to drop, duplicate or wrap IDs silently. Any error
  partway through used to hang the run once more than a channel's worth of
  blobs remained; it now ends it with that error. `--start-id` values below
  1 are refused (osmium counts downward from a negative start). A lying
  indexdata `max_id` no longer drives the node set's pre-allocation, and
  per-stage timing counters, which truncated each blob to whole
  milliseconds, are now in nanoseconds.

- **apply-changes and `merge-changes --simplify` enforce one version rule.**
  Both refuse a change whose `version` does not exceed the highest version
  ever claimed for that object in the run, deletes included, and a change
  without a `version` no longer resets that floor. Before, `merge-changes
  --simplify` compared only against the previous change, so a stale version
  after an unversioned change was accepted, and apply-changes forgot a live
  record's version when an unversioned change overwrote it. Both commands
  now print the same refusal.

- **`merge-changes --allow-version-regression` without `--simplify` is a
  usage error.** It used to be accepted and do nothing, since only
  `--simplify` compares versions.

- **List files now parse the way osmium's do.** In a tag-expressions file
  (`--expressions`), a tab is kept as part of the key instead
  of being trimmed, and a type prefix that is only valid once spaces are
  removed (`  w/highway`) is an error rather than a literal key that
  matches nothing. In a `getid` ID file, an inline comment or trailing text
  after a space (`n123 # note`) is now accepted, and a tab is now an error.

- **`extract` no longer trusts indexdata for raw-copied node blobs.** A blob
  is copied through without re-encoding only after its decoded nodes are
  all confirmed inside the region, so a blob whose indexdata bbox lied can
  no longer leak out-of-region nodes into the output; such a blob is
  filtered normally, with a warning. The node counts in the summary are
  now always counted from decoded data, never from indexdata.

- **`apply-changes --locations-on-ways` no longer loses coordinates
  silently.** Node blobs of plain (non-dense) nodes had their coordinates
  dropped, surfacing later as unexplained missing nodes; both encodings are
  now read, and a corrupt coordinate is an error naming the blob's sequence
  number and offset. A way written before node coordinates were available
  (possible when some blobs lack indexdata) used to lose its inline
  coordinates; it is now an error. A way referencing a node the same diff
  deletes used the node's old position and counted it as found; it now
  counts as missing, so the run fails unless `--ignore-missing-nodes` is
  passed. A worker error on a node blob no longer hangs the run. The
  `node blobs scanned` statistic now counts node blobs only.

- **`cat --dedupe` reconciles the output header across every input**, as the
  other cat paths already did: it used to take the first input's header, so
  the output advertised that input's bounding box and replication fields
  over data from all of them. Every input must now carry exactly one
  OSMHeader, as its first blob; a missing, late or repeated header is an
  error (a headerless input also used to skip the sortedness check). The
  summary no longer counts elements of blobs copied through undecoded as
  merged; their indexdata claim is reported on a separate line. Multi-input
  `cat` keeps the header's `source` when every input agrees on it, and the
  library `cat` with no inputs returns an error instead of panicking.

- **`build-geocode-index` no longer races or panics on hostile input.** A
  node duplicated across blobs made two workers write one coordinate slot
  concurrently, which was undefined behaviour, and an out-of-range slot was
  an out-of-bounds write in release builds. A node ID above its blob's
  indexdata `max_id` panicked a worker; it is now an error naming the node.
  Way refs above every node ID in the file are dropped as dangling, as they
  always effectively were. With `--force` on input without indexdata, blob
  kinds and the node-ID bound are now derived by decoding, instead of a
  bound of 0.

- **A huge element ID no longer aborts the process.** One way ref or
  relation member near `i64::MAX` made the internal ID set try to allocate
  terabytes in `add-locations-to-ways --index-type sparse`, and the same set
  backs `tags-filter`, `getid` and `extract`. Memory now follows the number
  of distinct IDs, so such a ref is an ordinary missing location. Under
  `--index-type external`, a node blob whose indexdata claims a `max_id` the
  bucket layout cannot encode is now an error naming the alternatives, not a
  panic.

- **`add-locations-to-ways --index-type external` counts descriptors the
  process already holds** before sizing its shard workers. It budgeted from
  the soft `RLIMIT_NOFILE` alone, so a library caller holding many files, or
  a CLI run that inherited them, could pass the up-front check and then fail
  with `EMFILE` partway through stage 1. It now takes fewer workers or fails
  up front, naming how many descriptors were open. Breaking for library
  callers: `FdAllowance::descriptors` is now the count a run may open on top
  of those already open, the limit and open count move to
  `FdAllowance::probe`, and `FdAllowance::host_grant` lets a host running
  concurrent joins hand out a share it reserves.

- **`PbfWriter::to_path` onto a FIFO, socket or character device no longer
  fails `flush()`.** Every byte was written, then the final `fsync` returned
  `EINVAL` because such targets cannot be synchronized, and `flush()` reported
  that as a failed write. On Linux that one case now counts as success;
  regular files and block devices still propagate every `fsync` error.
  `bench-write` now flushes its writer and reports write errors instead of
  dropping it.

- **`diff` no longer calls a corrupt element identical.** A tag whose string
  index points past the block's string table used to end the tag list
  silently, and a bad role index read as an empty role, so two such elements
  could compare common. Diff and `diff --format osc` now resolve every
  string strictly: the streaming path errors, the indexed paths report the
  element modified.

- **`diff` paired node `-9223372036854775808` with node
  `-9223372036854775807`.** The canonical ID order folded `i64::MIN` onto
  `-i64::MAX`, so the merge treated the two as one object and, comparing
  content only, could report them common.

- **`diff` rejected some spec-valid blocks as mixed.** An empty
  `PrimitiveGroup` beside a group of ways made the block classify as mixed,
  which the streaming merge refuses. Empty groups are now neutral.

- **`diff` silently dropped the relations after an empty node block.** On
  unindexed input, a block holding an empty `DenseNodes` message between the
  ways and the relations ended the relation phase early, so every later
  relation went unreported (same for `diff --format osc`). Blocks with no
  elements are now skipped.

- **`apply-changes` printed its merge summary twice.** The library wrote
  it and the CLI wrote it again; library callers rendering the returned
  stats themselves got a stray copy too. The library no longer writes it.

- **`diff` with negative IDs at the default `-j`.** The parallel shard
  planner compares IDs as raw numbers, which orders negative IDs wrongly;
  release builds could mis-shard silently and the blob-order check could
  refuse valid files. Indexed inputs holding any negative ID now take the
  sequential block-pair merge.

- **A `PbfWriter` driven from inside its own rayon pool no longer
  deadlocks.** Writing, flushing or dropping a writer from a worker of the
  pool its framing runs on - including a one-thread pool - used to block
  that worker on work only it could run. The writer now waits
  cooperatively there. A framing panic under the pool's non-aborting
  `panic_handler` surfaces as an error from `flush`; with no handler, rayon
  aborts the process, which is now the documented disposition.

- **Concurrent library runs no longer share or delete each other's scratch.**
  Two `add-locations-to-ways`, `diff` or `diff --format osc` runs in one
  process named their scratch files by process id alone, so they wrote into
  and cleaned up the same files. Each run now owns its own scratch directory
  (see the scratch entries above). Scratch creation errors name the path;
  sequential derive also removes fragments after partial initialization or a
  panic.

- **Piping CLI output into a reader that exits early no longer panics.**
  `pbfhogg inspect x.pbf | head`, and the same with `check`, `diff`,
  `export`, `tags-count` and `inspect --show`, used to die with "failed
  printing to stdout: Broken pipe" and exit 101. The CLI now stops writing
  and exits 0, as standard Unix tools do. `check` and `inspect --indexed`
  work out their verdict before writing anything, so they keep their exit
  code; streaming `diff` exits 0 when the pipe closes. Other stdout write
  errors, such as a full disk behind `> file`, still exit 2. `export`, `diff`
  and `tags-count` output to stdout is now block-buffered instead of
  line-buffered.

- **A failed parallel write no longer keeps writing after it reports
  failure.** When one worker of the default parallel output writer failed or
  panicked, the writer returned the error while its other workers were still
  writing to the output file, so a caller that removed or reused the path
  could race them. Every worker is now stopped and joined before the error is
  returned, and the error carries the panic's message instead of a bare
  "writer thread panicked". The parallel gzip writer used by
  `diff --format osc` reports the first failure, with its message, in the
  same way.

- **Parallel `diff` no longer leaves `diff-par-*.txt.tmp` files behind** when
  writing its output fails, for example when the reader of a stdout pipe goes
  away. The same leak in `diff --format osc` is fixed too.

- **Geocode indexes now accept legal non-dense Node messages and retain way
  vertices at Null Island.** The coordinate store uses the same explicit
  latitude-bias presence bit as `add-locations-to-ways`.

- **Geocode cell coverage is complete for configured S2 levels and long
  segments, emits each cell once, and seeds administrative interiors from
  actual inside cell centers.** A 256-step cap previously truncated the cover
  of any long segment. Concave boundaries no longer lose all interior coverage
  when their vertex average lies outside the polygon, and boundaries whose
  interior splits into regions that are disconnected at cell resolution now get
  coverage in every region rather than only the one containing the seed.

- **`geocode_index::Reader::open` now validates every offset-into-a-file
  relationship in the format**, not just the geo-cell entry lists: admin cell
  entry lists, street and interpolation way node spans, admin polygon vertex
  spans, and string-pool offsets. A truncated or hostile index file previously
  opened successfully and then returned silently incomplete results, since
  every read path stops at the end of the mapping and reports nothing rather
  than an error. An interpolation way whose type byte is not one of the
  three values the format defines (all, even, odd) is refused the same way;
  it used to be read silently as even/odd.

- **`extract` and `multi-extract` no longer include points just outside a
  `--bbox`.** Point membership was decided against integer bounds widened by
  `floor`/`ceil`, which is correct for choosing candidate blobs and wrong as
  the answer: a node up to one decimicrodegree outside the requested box
  matched. The widened bounds are now confined to blob selection, and every
  membership decision - element-by-element classification, the columnar
  classifier used by `smart` and `multi-extract`, and the raw-blob
  passthrough proof - uses bounds that agree exactly with the floating-point
  comparison. Affects all four strategies and `export --bbox`.

- **`assemble_rings` backtracks at ambiguous junctions.** The greedy
  assembler followed the first available continuation and could consume
  segments into a dead-end chain, losing a ring that was assemblable.
  Backtracking can take exponential time on a relation whose member ways
  share many endpoints, so `build-geocode-index` runs it under a step
  budget that every relation without a branching endpoint fits by
  construction. A relation that exceeds it is refused whole, counted among
  the dropped admin relations, and reported in a warning; every relation
  that fits assembles exactly as the unbounded search would. The public
  `geo::assemble_rings` stays unbounded.

- **`ring_crosses_antimeridian` checks the implicit closing edge.** An
  unclosed ring whose only crossing was between its last and first vertex
  was treated as not crossing, so the point-in-polygon path skipped
  longitude unwrapping.

- **`altw --index-type sparse` no longer leaves its temporary store behind
  when preallocation fails.**

- **`extract` on an unsorted input no longer silently drops ways and
  relations.** The unsorted fallback shared by the `simple`, `complete` and
  `smart` strategies classified nodes, ways and relations in a single
  file-order sweep. That ordering is only sound on a sorted file - and this
  is the branch taken when the file is not sorted. A way stored before the
  blob holding its bounding-box nodes was tested against an empty node set
  and never matched; likewise a relation whose members came later. Those
  elements were simply absent from the output, with no error, no warning
  and no count. The fallback now sweeps the file once per element kind.
  Sorted inputs are unaffected - they already took the three-phase path.

- **A node at exactly `(0, 0)` is no longer reported as a missing location
  by `add-locations-to-ways`.** Both backends spelled "absent" as the
  coordinate `(0, 0)`, which is Null Island. The `sparse` store now carries
  an explicit presence bit (latitude is stored biased by 90 degrees, freeing
  bit 31), and the `external` backend counts a reference as resolved when
  its id was found rather than when the coordinate it found was non-zero.
  The two backends previously disagreed about the same input.

- **Admin-boundary holes crossing the antimeridian are assigned to the
  right outer ring.** The geocode index builder decided hole ownership with
  the plain ray cast rather than the antimeridian-aware variant used
  everywhere else, so holes in relations spanning +/-180 degrees (Russia,
  Fiji, the Antarctic claims) went to the wrong outer or were dropped.
  Ownership is also now decided from several vertices spread around the
  hole instead of from its first vertex alone, which for an admin boundary
  is very often a node shared with the outer ring - exactly on the
  boundary, where ray casting is undefined.

- **Hostile PBF metadata can no longer corrupt or crash the node scanner.**
  `granularity` was read straight off the wire and multiplied by every
  accumulated coordinate delta with no validation and no overflow check:
  a value above `i64::MAX` became a negative granularity, a huge one
  panicked in debug and wrapped in release, and the final narrowing to
  `i32` truncated an out-of-range coordinate into a plausible position
  somewhere else on Earth. Granularity is now bounded, the delta
  accumulators are checked, and the narrowing errors instead of
  truncating. The way-ref scanner's reference accumulator is checked too.
  This scanner feeds `add-locations-to-ways` pass 1, the external
  backend's stage 2, and simple-extract node classification.

- **A refused `pbfhogg::altw::add_locations_to_ways` no longer leaves an
  output behind.** The missing-locations policy can only be evaluated once a
  complete output exists - the external backend does not learn its resolved
  count until its last stage - so the write necessarily precedes the
  refusal. Under the CLI that was already harmless, but a library caller was
  handed an error together with a well-formed PBF at the path it named, full
  of fabricated `(0, 0)` vertices, and a pre-existing file of that name had
  already been destroyed. The write now goes to a scratch path that is
  published only after the policy check passes, on the same
  destination-dependent shape `merge_changes` uses, so the CLI's own guard
  is not duplicated. The error message no longer names an output path,
  because there no longer is one.

- **A failed `pbfhogg::commands::merge_changes::merge_changes` no longer
  overwrites the destination it was given.** The single-input path created
  the output file and streamed XML into it during the parse, so a truncated
  or otherwise invalid input left the destination holding a partial
  document - and a pre-existing file of that name was already truncated
  before the first byte of new output existed. Library callers only: the
  CLI has been safe since every user-named `--output` started going through
  a scratch sibling that is renamed into place on success. The library now
  guards its own writes, choosing a plain remove-on-failure guard when it
  created the file and a scratch-and-rename when it would overwrite an
  existing one, so nothing is guarded twice.

- **`apply-changes` no longer emits an out-of-order create.** A pure create
  whose ID fell inside a base blob's ID span matched no element in that blob,
  so the precise overlap check called the blob a false positive and shipped it
  verbatim - leaving the create to be emitted after larger IDs, in a file
  whose header stamps `Sort.Type_then_ID`. A blob is now rewritten whenever an
  upsert lands in its range, and the create is interleaved at its sorted
  position. Base 10/20/30 with creates 5/15/35 produced `5, 10, 20, 30, 15, 35`
  and now produces `5, 10, 15, 20, 30, 35`.

- **Zero-valued element metadata survives into derived OSC.** The writer
  emitted `version`, `changeset` and `uid` only when positive, so `uid="0"` -
  the anonymous mapper that old OSM data is full of - and a zero changeset
  vanished; when every field was zero the element shipped with no metadata
  attributes at all, and applying it back produced a PBF element with no info
  block. Presence, not truthiness, now decides. This closes the gap in the
  `--format osc` lossless-roundtrip guarantee at those margins.

  The same guarantee now holds for an element whose `Info` omits `version`
  entirely. Every `Info` field is independently optional on the wire, but the
  three borrowed writers emitted metadata only when `version` was present, so
  a valid element carrying a timestamp, changeset, uid or user but no version
  lost all of it. Each field is now emitted on its own presence.

- **`apply-changes` checks trailing creates for ordering too.** Creates
  emitted after the last base blob go through a second output channel that
  the sortedness guard did not cover, leaving it blind to exactly the last
  thing written.

- **The compact OSC overlay no longer truncates arena offsets to 32 bits.**
  Per-kind arena offsets were stored as `u32`; past 4 GiB in one arena - about
  85 M node changes, reachable on a catch-up diff or a multi-OSC accumulation -
  the offset wrapped and the index pointed at an arbitrary earlier record, so
  `apply-changes` wrote one element's data under another's ID. Offsets are now
  `usize` (no size cost: both are 16 bytes after padding in the index map), and
  the remaining `u32` fields in the record layout - tag/ref/member counts, tag
  value lengths, and the string interner's offsets, lengths and IDs - error on
  overflow instead of truncating.

- **A zero-length block can no longer produce a `Blob` message with no fields
  at all.** The payload field (`raw`, `zlib_data`, or `zstd_data`) was written
  with a skip-if-empty encoder, so an empty uncompressed payload serialized to
  an empty `Blob` - indistinguishable from a truncated or corrupt one, and
  carrying no indication of which encoding it used. The payload field is now
  always emitted. No in-tree caller can reach an element-free block, so this
  closes a latent malformed-output path rather than fixing observed breakage;
  output for every non-empty block is byte-identical.

- **A large `--elements-per-blob` no longer aborts the whole write on the
  `BlobHeader` size cap.** `tagdata` (the per-blob tag key index) lists
  every distinct tag key in the block, so it grows with the element count,
  and the format caps a `BlobHeader` at 64 KiB. A dense blob pushed it over
  and the write failed with an opaque byte-count error - on
  `repack --elements-per-blob 228000`, an advertised configuration. The
  index is now **dropped** for that blob rather than the write failing.
  Dropping is the only safe fallback: `tagdata` is consumed as a skip
  filter, so a *truncated* key list would make `tags-filter` skip blobs that
  do contain the key, while an absent one already means "decode this blob".
  The cost is the skip optimisation on the affected blobs and nothing else.
  Drops are counted in the new `writer_tagdata_dropped_oversize` sidecar
  counter. An oversize `WayMembers-v1` bitmap remains a hard error, because
  its absence is a claim about blob contents rather than a missing hint;
  the message now names it.

- **A `--elements-per-blob` large enough to exceed the format's blob-body
  cap is rejected at write time instead of producing an unreadable file.**
  `repack --elements-per-blob 20000000` on Denmark wrote 143 MB blob bodies
  and exited zero; the failure only surfaced later, in a *reader*, as "blob
  header declares datasize too big". The writer now refuses, and names the
  flag responsible.

- **`FileWriter::flush` no longer means `fsync`.** It called `sync_all()`,
  so any incidental `flush()` - a wrapping `BufWriter`, a generic helper, a
  library consumer following `Write` semantics - bought a full fsync.
  `flush` now flushes; durability is available by name via the new
  `FileWriter::sync()`, which is what the pipeline's end-of-stream
  finalisation calls. Every `PbfWriter` constructed from a path
  (`to_path`, `to_path_direct`, `to_path_uring`, `to_path_parallel`) still
  fsyncs its output when `flush()` completes. A `PbfWriter::new` built by
  hand over a `FileWriter` does not: there, `flush` now means `flush`, and a
  caller who wants durability calls `FileWriter::sync()` itself.

- **`ParallelGzipWriter::flush` no longer reports success without writing
  anything.** It was a documented no-op returning `Ok(())`, so a caller who
  flushed and checked the result was told the bytes had gone somewhere while
  they sat in the internal buffer. `flush` now blocks until every buffered
  byte has been compressed, written to the inner writer and the inner writer
  flushed, and it returns any compression or destination-I/O error that
  happened along the way - including one deferred from an earlier chunk.
  Each `flush` forces a gzip member boundary, so flushing gratuitously costs
  compression ratio. `finish()` is still the call that closes the pipeline,
  joins every thread and returns the inner writer. A compress worker that
  dies mid-chunk now reports the gap through the data path, so a `flush`
  waiting on the acknowledgement barrier gets an error instead of hanging
  forever behind a seq that will never arrive.

- **A dead writer thread now stops `write_primitive_block*`, not just
  `write_raw*`.** The framing work runs in a `rayon::spawn` closure whose
  send result went nowhere, so a writer thread that died on ENOSPC stayed
  invisible until `flush()` - and nothing throttled the producer, because
  the dispatch permits are released when a closure *completes*, not when its
  result is delivered. A full disk therefore let an entire dataset be read,
  decoded and framed before anything was reported. The producer now notices
  and surfaces the writer thread's own error.

- **`check --ids` judged ID monotonicity by raw `i64` comparison while
  everything else used canonical OSM order.** Canonical order is `0`, then
  the negative IDs by ascending absolute value, then the positives - the
  order `sort`, the blob-overlap logic and the ID scanners all use. The two
  agree on positive-only files, so production PBFs were unaffected, but on a
  file carrying negative IDs (JOSM editor-local staging IDs) they disagreed
  in both directions at once: `sort`'s own correctly-ordered output was
  reported non-monotonic, and a raw-`i64`-sorted file that `sort` considers
  unsorted passed the check. The verifier is the one place that has to agree
  with the definition.

- **`check --refs` reported untrackable element IDs as missing references,
  indistinguishably from real ones.** An ID the tracking bitset cannot
  represent - a negative ID, or one above the range sized from the file's
  own indexdata - is never recorded as present, so every reference to it is
  counted missing. That over-report is deliberate (the alternative was a
  panicking worker), but it was silent: the reader could not tell "your file
  references a node it does not contain" from "your file contains a node
  this check could not track". The count is now reported on its own line and
  in the JSON output, matching how `--ids --full` already reports the same
  situation as `id_out_of_range`.

- **`cat` of multiple files advertised a bounding box that did not cover its
  own data.** All three cat paths built the output header from the first
  input alone, so the bbox described one input while the file contained
  several, and the replication fields came from whichever file was listed
  first. The header is now reconciled across every input: the bbox is the
  union and is dropped entirely if any input declares none, each replication
  field survives only if every input agrees on it, and
  `HistoricalInformation` is the OR. Single-input `cat` is unchanged.

- **`sort` and `cat --dedupe` no longer emit unsorted output stamped
  `Sort.Type_then_ID`.** Overlap-run detection compared only adjacent
  blobs in the first-key-sorted list, but range overlap is not
  transitive: with blob ranges `[1..1000]`, `[2..3]`, `[500..600]` the
  third does not overlap its predecessor and so was left out of the run,
  written out afterwards as a raw passthrough blob, and landed after ids
  the run had already emitted. Detection is now a running-maximum sweep,
  so a blob is pulled into the run if its first key falls inside the
  run's widest reach so far. For `cat --dedupe` this also restores the
  "exact duplicates removed" contract, which the excluded blob's
  elements silently escaped. Most likely to have been hit when merging
  geographic extracts, whose per-blob ID ranges are wide and sparse.

- **`check --ids --full` and `check --refs` no longer panic on IDs past
  their bitset ceilings.** Both sized their duplicate-detection bitsets to
  hard-coded ID ceilings and then crashed a worker on any ID past them -
  including every negative ID, which is legal OSM (JOSM editor-local
  staging IDs), and, before long, ordinary way IDs: the way ceiling was
  1.5e9 against a live OSM maximum already near 1.4e9. A crash on
  untrusted input is the worst outcome for the one command whose job is
  to diagnose bad files. The parallel allocation is now sized from the
  file's own indexdata, bounded by file size and floored at the live OSM
  ID space; a positive ID above it is still tracked exactly, inserted
  after the parallel phase, so honest extracts carrying global IDs are
  never flagged. Only a negative ID, which the bitsets cannot represent,
  is reported - `check --ids --full` emits a new `id_out_of_range`
  violation (`IdViolation::IdOutOfRange`) and `check --refs` counts it as
  untrackable - rather than aborting the run.

- **A blob whose `PrimitiveGroup` mixes element kinds is no longer given
  false indexdata or a false tag index.** The scanners stop at the first
  element field in a group, so a group holding both ways and relations
  was indexed as ways-only with a count covering only the ways. Nothing
  detected it - the existing mixed-kind guard compares across groups -
  and five commands acted on the lie: `cat --type relation` dropped the
  relations, `cat --type way` leaked them into a way-only output,
  `repack` never emitted them, `renumber` left their IDs unrenumbered in
  the way stream, and `tags-filter` could skip the blob entirely. Such a
  blob now carries no indexdata and no tag index, so every command
  decodes it - slower, and correct.

- **The per-blob tag index is no longer silently truncated to `u16`.**
  Both the key count and each key length were written with an unchecked
  `as u16`. A wrapped key length desynchronizes the parser into garbage
  keys; a wrapped key count is worse, producing a shorter key set that
  still parses cleanly, so `tags-filter` skipped blobs whose keys had
  been dropped and reported a cheerful "N blobs skipped by tag index"
  while omitting real matches. The encoder now refuses to emit an index
  it cannot represent, and a blob with no tag index is decoded rather
  than skipped. Neither bound is reachable from OSM data today.

- **Packed repeated fields written in the unpacked wire form are handled
  by every scanner, not just the read path.** Protobuf permits a packed
  field as either one length-delimited segment or individually tagged
  varints; `src/read/wire.rs` was taught to accept both, but the
  independent scanners still matched the length-delimited form only and
  read an unpacked array as empty. Consequences, all silent: a way's
  `refs` read as absent produced a short node index, an unpacked
  `DenseNodes.id` produced a blob with no indexdata,
  `add-locations-to-ways` emitted ways with no coordinates, and
  `renumber` copied a relation's member IDs through **un-renumbered**
  into output that claims to be renumbered. The way and dense scanners
  now route through the shared parser; `add-locations-to-ways` accepts
  both forms and normalizes on output; `renumber`'s byte-level splice
  rewriter, which cannot use the shared parser, rejects the unpacked form
  with an error naming the field. No known writer emits this encoding.

- **`renumber` and `add-locations-to-ways` reject a packed field split
  across several data-bearing occurrences instead of renumbering only the
  last one.** Protobuf gives repeated packed segments concatenation
  semantics, so `Way.refs` written as two length-delimited segments is
  one ref array. Both tools patch a single contiguous byte range, so they
  kept only the last segment's range: the earlier segment was copied
  through verbatim while the tail was rewritten, producing partially
  renumbered output - or, for `Relation.types`, a member array dispatched
  against the wrong ID spaces - with no error at all. `DenseNodes.id`,
  `Way.refs`, `Relation.memids` and `Relation.types` now each fail with
  an error naming the field; empty occurrences are still ignored. This
  matches the read path's existing duplicate-occurrence policy and makes
  pbfhogg stricter than libosmium here, which keeps the last occurrence.
  Re-encode such an input with `pbfhogg cat --clean` first.

- **`check --ids --full` and `check --refs` bound their ID-bitset
  allocation by the input file's size.** Sizing the bitset from the
  file's own indexdata (above) trusted an unverified number in both
  directions: a blob header claiming `max_id = i64::MAX` asked for an
  exabyte-scale allocation and took the process down before any element
  was read. The bitset may now never exceed the size of the input file,
  floored at the live OSM ID space, so the sizing still tracks real files
  of any scale while a small hostile one cannot request an unbounded
  allocation. An ID above the chosen bound is reported, as above.

- **`renumber` reports rather than panics when a file's indexdata
  understates its own ID range.** The node bitset is sized from
  indexdata, which nothing verifies, so a stale or hand-edited index made
  a node ID overshoot the allocation and crash a worker with an opaque
  bitset message. It is now an input error naming the problem.

### Changed

- **Default Linux builds now do the I/O the published numbers were
  measured with.** Passthrough splicing (`copy_file_range`, used by `sort`
  and `apply-changes` for untouched blobs), the sequential-readahead hint on
  buffered reads, and `add-locations-to-ways --index-type external`'s
  scratch eviction used to compile only with the `linux-direct-io` or
  `linux-io-uring` feature, so `cargo install pbfhogg-cli` and every
  default library build ran without them. They now run in every Linux
  build, and the two features mean only `O_DIRECT` and `io_uring`. Library:
  `PbfWriter::write_raw_copy` and `OutputBackend::supports_copy_range` no
  longer depend on `linux-direct-io`; off Linux, `write_raw_copy` copies in
  userspace and refuses a same-file or append-mode pair. A failed writeback
  of an external-join slot bucket now fails the run (`failed to sync slot
  bucket ...`) instead of being ignored.

- **`--force` on unindexed input says the same thing on every command.** A
  forced run, or `build-geocode-index --allow-unindexed`, prints one note
  that the run proceeds without indexdata and is slower. getid,
  apply-changes, `cat --type`, `cat --dedupe`, `inspect tags` and
  `tags-filter -R` used to say nothing, and the others each worded it
  their own way.

- **CLI warnings and notes now carry one prefix each.** Every warning on
  stderr starts with `Warning:` (previously also `warning:`, `Warning!` or an
  indented `WARNING:`), and every informational note with `Note:` (previously
  `hint:`, a bracketed command tag such as `[tags-filter]` or `[getid]`, or no
  prefix at all). Indented warnings and notes now start at column 0. Progress,
  summary and developer lines are unchanged. Scripts matching the old
  prefixes need updating. Library event text no longer carries a kind prefix
  at all: the host's sink renders `Warning:` or `Note:` from the event kind,
  so an embedding host sees the bare message.

- **Several diagnostics are sharper or no longer doubled.** `renumber`'s
  orphan-refs message is now a real `Warning:` (it was summary text that
  started with the word). `cat` names the input whose header carries
  way-level metadata that will be lost, and the unrecognised-indexdata
  warning names the file and the byte offset of the first offending blob.
  `check --ids` on an unindexed input prints its triple-scan warning without
  the extra unindexed-filter note. A failing `--io-uring` or parallel writer
  thread is reported once, through the returned error, instead of also as a
  `[uring_writer]` or `[parallel_writer]` line. `build-geocode-index` prints
  its `Index built:` counts before `Done in` rather than after it. The
  `apply-changes --jobs 1` refusal is two lines.

- **`add-locations-to-ways --index-type external` checks scratch space
  before its node join.** Once the way pass knows the exact ref count, the
  run is refused unless the scratch filesystem holds about 23 bytes per ref
  plus per-file slack, instead of failing with ENOSPC minutes to hours
  later. The figure is a worst-case bound and excludes the final output.

- **The writer refuses a block whose uncompressed payload exceeds 32 MiB.**
  Such a blob compressed into an in-limit body was previously written and
  then refused by every reader, including pbfhogg's own. It is now an error
  at write time.

- **New notes when a run silently lost parallelism or skipping.** `diff` and
  `derive-changes` say when they fall back to a sequential comparison
  (missing indexdata, `--verbose`, negative IDs); `multi-extract` says when
  its single-pass probe or region grid declines; `sort --force` on unindexed
  input warns that every blob is decompressed; a filtered read whose input
  has no indexdata says the filter cannot skip blobs; and a run whose output
  blobs lost their tag skip-index to oversized headers ends with a warning.

- **`merge-changes` with several inputs no longer holds every re-emitted
  input in memory before writing.** Members are written in input order as
  soon as they and their predecessors finish, with at most one in flight per
  worker thread. Output bytes are unchanged.

- **CLI: a closed stderr no longer aborts the command.** With stderr's
  reader gone (`pbfhogg sort in.pbf -o out.pbf 2>&1 | head`), the first
  progress line used to panic and end the run with status 101, leaving no
  output. Diagnostics now stop, the command completes, and the exit status
  is the command's own (0, 1 for a negative answer, 2 for a failure).

- **`merge-changes` no longer strips element metadata from an element that
  omits `version`.** Its metadata parser began with
  `let version = parse_i32_attr_optional(e, b"version")?;` and returned
  "no metadata" the moment `version` was absent - discarding `timestamp`,
  `changeset`, `uid`, `user` and `visible` along with it. `apply-changes`,
  reading the very same file through the other parser, kept all of them.
  Metadata is now recognised by *presence*: an element carrying any
  metadata attribute has metadata, and every attribute it carried is
  preserved. Correspondingly, an element that claims no `version` no longer
  has `version="0"` invented for it on output - the attribute is simply
  absent. This also affects the `diff --format osc` / `derive-changes`
  delete writers.

- **Indexdata is now written as format v3, which can represent a bbox at
  (0, 0).** v2 spelled "this blob has no bounding box" as four zero words -
  which is also the exact bbox of a single-node blob at Null Island, so such a
  blob read back as having no bbox and silently lost spatial filtering, with no
  way for the encoding to say what was true. v3 keeps the same 42-byte layout
  and marks absence with an inverted box instead, a value no well-formed bbox
  can take. v1, v2 and v3 are all still read, and v2 files keep their original
  reading, so existing indexed PBFs are unaffected; only newly written
  indexdata changes. A pbfhogg older than this release will not recognise v3
  and will treat such a file as unindexed - correct output, slower paths.

- **`removeid` and `sort` no longer report unverified indexdata as elements
  they wrote.** Both fold whole blobs through without decoding them, and both
  counted those blobs' elements from the `BlobHeader.indexdata` count - a
  number nothing verifies and the command never observed. The headline counts
  now cover only what was actually decoded, and the passthrough total is
  reported on its own line as coming from the file's own (unverified) claim.
  Scripts parsing `Wrote N elements` from `removeid`, or `Sorted N nodes`
  from `sort`, will see smaller numbers on files with passthrough blobs.

- **`tags-count -e <expr>` now skips blobs on the tag index.** It parses the
  same expressions as `tags-filter` but built no blob filter, so it
  decompressed every blob in the file even when the tag index proved a blob
  could not contribute a single count. Counts are unchanged.

- **`cat --clean` refuses an input without indexdata unless `--force` is
  passed.** `--clean` routes through the per-kind classify pipeline, whose
  schedules cannot tell which kind an unindexed blob holds and therefore
  schedule it as all three - so every blob was read and decompressed three
  times. The output was correct, which is why it went unnoticed; the guard
  previously fired only for `--type`. `check --ids` on an unindexed input
  warns about the same three passes rather than refusing, since `check` must
  keep working on third-party files.

- **`repack --elements-per-blob N` no longer pre-allocates from `N`
  unbounded.** `N` came straight off the command line and sized one
  `BlockBuilder` per worker plus a central one, so a pathological value was
  several unbounded allocations before a byte of input had been read. The
  per-block element cap is unchanged; only the pre-allocation is now bounded
  by what the input could actually deliver.

- **`getparents` stops decoding when a write fails.** The first write error
  was captured and reported, but the scan still ran the whole schedule to
  completion, so a planet-scale run whose first write failed paid the entire
  decode before the error surfaced.

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
  `merge-changes`, and the synthetic `bench-merge` benchmark, which no
  longer deletes a pre-existing `-o` file before it starts and commits
  outside its timed region. Each writes to a scratch sibling in the
  destination's directory and renames it into place only once the run has
  succeeded, so the destination is untouched until then and is replaced
  by a single `rename` - atomic on Unix.

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

### Dependencies

- `hotpath` 0.23.3 → 0.25.0, `quick-xml` 0.41.0 → 0.42.0. The `quick-xml`
  bump moved event attribute names and values from bytes to `str`; OSC
  parsing migrated accordingly (same behavior).
- **`s2` 0.1.0 → 0.2.0** (optional, behind `geocode-reader`/`commands`).
  Upstream removed the `float_extras` feature in favor of std's
  `f64::next_up`/`next_down`, so the pin drops it; still
  `default-features = false`.
- `zstd` 0.13 → 0.14.
- New: `xxhash-rust` 0.8 (pure Rust, `xxh3` only), for `digest`.

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
