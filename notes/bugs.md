# Bugs

Defects turned up by the hygiene hunt, consolidated across every scope.
Each entry records what the hunters read; none has been verified, and some
are marked "likely" by the hunter who found them.

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.
6. The higher an entry's anger index, the higher the bar for implementing
   anything as a result of it. A high-anger entry should more often than not
   end as a code comment (rule 3) rather than a code change.
7. The higher an entry's anger index, the more care is owed - exponentially -
   against performance regression from any change made for it. Before
   touching code under a higher-index entry, read
   `reference/performance-history.md` and `reference/performance.md`. The
   current code is likely not an accident.
```

Each entry carries an `Anger:` rating from 0 to 10: how likely resolving it
is to make the maintainer angry. 10 is rebuilds or contract changes on
measured paths; 9 is hot-path or contract changes that need benchmarks or
the maintainer's decision; 8 is altw, renumber and apply-changes internals
and observability contracts; 7 is geocode output and format, and CLI
behaviour; below that the work is progressively more contained.

---

## BUG-005 - Indexdata claims are folded into observed element counts

Anger: 9 - verifying indexdata costs a per-blob decode.

`sort`, `getid` and `cat --dedupe` keep a separate `passthrough_claimed`
count, and extract verifies a passthrough blob by decoding it before
copying it raw, so its counts are observed. Apply-changes still trusts
indexdata `count` for its stats on passthrough blobs, so a lying indexdata
moves its headline number with no way to tell (see HYG-061 for the trust
model).

Reported by: re-encoding, filtering and spatial, change application.

## BUG-009 - `apply-changes --locations-on-ways` does not update untouched base ways

Anger: 10 - apply-changes -l loses its splice fast path.

Decided: fix it, as its own project. Until then the CLI help, both CLI
references and DEVIATIONS.md document the current scope (OSC-touched ways
only, rerun altw for a full refresh); those docs change when this lands.

Only ways the OSC touches get fresh coordinates: `NodeLocationIndex::build_from_diff` collects refs of OSC ways
only; `rewrite_block_parallel` writes a surviving base way through
`write_base_way_local_with_locations`, which copies the old coordinate bytes;
way blobs with no diff overlap take the splice fast path. If the OSC moves
node N and does not touch way W, W goes out with N's old position, and a
deleted N leaves W with N's last coordinate. `loc_missing` cannot see it.
osmium's `apply-changes -l` recomputes from the merged node set.
`reference/pipeline.md` says the production loop re-runs altw after each
merge instead, which is probably why nobody has hit it.

Project scope: the set of base ways referencing any modified or deleted node
(a reverse index or a full way pass), routed through rewrite instead of
splice. Expect the splice fast path to be lost for many way blobs on a daily
diff, so the cost needs measuring at planet before it becomes the default;
an opt-in flag is the fallback. Enforce with a cross-validation test against
`osmium apply-changes -l` on a diff that moves a node referenced only by an
untouched way, and on one that deletes such a node.

Reported by: change application.

## BUG-021 - `apply_changes::merge` does not refuse output equal to the base input

Anger: 7 - CLI behaviour.

The library entry point never checks whether `output_pbf` is the same file as
`base_pbf`; the writer would truncate the input it is about to
`copy_file_range` from. The CLI guards it, a library caller does not. Fix:
refuse `same_file(base, output)` in the library. See HYG-030 for the wider
output-publication policy.

Reported by: change application.

## BUG-024 - `IdSet::pre_allocate` sizes from an unverified `max_id`

Anger: 8 - IdSet preallocation.

`IdSet` memory now follows the number of distinct IDs rather than their
size (small chunks are sorted arrays, IDs from 2^43 up live in an ordered
map), so `set` on an input-controlled ID no longer aborts. `pre_allocate
(max_id)` still allocates a 4 MiB bitmap per 2^25 IDs from 0 to `max_id`
before anything is validated, so a caller passing an indexdata `max_id`
that lies high allocates until it runs out of memory. `check` bounds its
argument by the file size, and renumber by `min(claim, file_len * 64)` with
IDs above it routed through `set` after its parallel pass; the geocode
builder (pass 1.5) still passes indexdata's claim unbounded and should do
the same, per CORRECTNESS.md's rule for allocations sized from indexdata.

Reported by: altw and scanning, geocode index, re-encoding.

## BUG-030 - `with_blob_filter` is silently ignored by `for_each` and `par_map_reduce`

Anger: 7 - CLI behaviour.

`ElementReader::with_blob_filter` documents that the pipeline skips
decompressing blobs whose type does not match. `for_each` destructures
`Self { blob_iter, header, .. }` and drops the filter; `par_map_reduce` never
reads it. A library user who sets a filter and calls either gets every
element, with no error. Fix: apply it in all three engines (the check is
`should_skip_blob`), or make the filter a parameter of the pipelined methods
only. Enforceable by type: a `FilteredReader` offering only the methods that
honour it.

Reported by: read path.

## BUG-037 - `StringTable::pre_seed` identity mapping is not structural

Anger: 9 - string-table remap in the rewrite hot path.

`pre_seed` feeds input strings 1..len through `add`, which dedups. The
`add_*_raw` / `add_way_raw_bytes*` paths then write the input block's raw
string indices straight into the output, assuming input index N maps to
output index N. That holds only if the input table has no duplicates and no
empty string except at 0, which neither the format nor any check guarantees.
A duplicate at index j makes `add` return i, the output table is one shorter,
and every index after j names the wrong string: wrong tags, no error. Raw sids
are never bounds-checked, and negative sids are cast with `as u32` into
`tag_key_indices`.

Fix: `pre_seed` pushes unconditionally and inserts into the index only if
absent, so identity holds by construction; or return a remap table. Test:
pre-seed from a block with a duplicate and an empty string, assert
`strings.len() == input_len`; debug-assert every raw sid `< strings.len()` in
`encode_block`.

Related: the FxHash safety note in `string_table.rs` says every string comes
from in-process data, never from untrusted PBF input. `pre_seed` and every
rewrite command's `add` take strings decoded from input PBFs and OSC XML. The
8000-element block bound may still make FxHash acceptable, but the stated
reason is wrong.

Reported by: write path.

## BUG-077 - Two osmium parity gaps not yet checked against DEVIATIONS.md

Anger: 7 - CLI behaviour.

Found while fixing the list-file readers, not verified further:

- osmium's `tags-count` (`add_matcher`) has no type-prefix syntax, while
  pbfhogg parses `w/highway` as typed in its tags-count expressions.
- osmium `getid` treats a bare ID as a node (`--default-type` defaults to
  node); pbfhogg rejects bare IDs unless `--default-type` is given.

Each is either a bug or an undocumented deviation; DEVIATIONS.md mentions
neither.

Reported by: the list-file reader fix.

## BUG-044 - Degenerate polygon rings produce an empty extract with exit 0

Anger: 7 - CLI behaviour.

`extract::parse_ring` accepts rings of any length, including 0 to 2 points
and unclosed rings; `point_in_ring` then returns `false`. A polygon file with
a degenerate exterior yields an empty extract and success: the wrong answer
reported as success that `parse_bbox`'s comment says was fixed for bboxes.

Reported by: filtering and spatial.

## BUG-046 - The geocode reader skips out-of-range entries, and one off-by-one reads the next way

Anger: 7 - geocode output.

Reader entry lists are validated for length but not content
(`way_index < street_way_count`, `segment_index + 1 < node_count`,
`addr idx < addr_point_count`, `poly_idx < admin_polygon_count`).
Out-of-range values are skipped at query time (`continue`), the "safe is not
correct" failure the `validate_way_geometry` doc argues against. A
`segment_index` equal to `node_count - 1` is worse: `two_node_distance` reads
the first node of the next way and returns a plausible wrong segment. Put the
checks in `validate_structure` (see HYG-085 on its cost).

Builder-side, the same class: Stage B's `match e.etype { ..., _ => {} }`
drops unknown entry types, and Stage A's `if let Some(rec)` and
`if let (Some(n1), Some(n2))` silently skip out-of-bounds records in files the
builder just wrote. Those are builder bugs and should be errors or
`debug_assert!` plus error.

Reported by: geocode index.

## BUG-076 - The shared node scanner silently skips plain `Node` messages

Anger: 9 - plain nodes in the shared node scanner.

`src/scan/node.rs` reads only `DenseNodes` and stops reading a group at its
dense nodes, so a plain (non-dense) `Node` is never seen, with no error, by
its users: altw (both backends' node passes) and the external join. Their
coordinates are then reported missing, or written as `(0, 0)` under
`--ignore-missing-nodes`. Multi-extract with all-bbox regions has the same
gap through `decode_dense_columns`, and silently leaves plain nodes out of
the output, while single-region simple extract errors on them.
Apply-changes now has a checked extractor for both encodings
(`apply_changes/node_coords.rs`); the scanner should take it over and every
node pass call it. pbfhogg's own writer emits only dense nodes, which is
why no fixture caught this; `osmium cat -f pbf,pbf_dense_nodes=false`
produces the input.

Reported by: the apply-changes and extract fixes.

## BUG-079 - The version gate refuses identical repeats from overlapping diffs

Anger: 10 - the version-gate contract.

The version rule refuses any change whose version is not strictly greater
than the highest claimed so far, so an identical change appearing twice is
refused. Consecutive replication diffs overlap at their boundaries: bygg's
germany seq 4705 and 4706 both carry node 135379864 at version 12 with the
same timestamp and coordinates, and `merge-changes --simplify` refuses every
germany range starting at 4705, at 3a664c7b as at HEAD. Applying an
identical change twice is a no-op, so refusing it protects nothing. The
documented promise that applying the same OSC twice is refused (DEVIATIONS.md,
"Change stream: version precedence is enforced") is the same behaviour.

Options: admit an equal version (a conflicting equal version then also
passes, last wins); admit it only when the content is identical too (a
cold-path comparison against the stored record or change); or keep it and
document `--allow-version-regression` for overlapping diffs. Needs the user's
call, since it changes a documented contract. Blocks benchmarking the
merge-changes floor on real data.

Reported by: the version-floor A/B on germany.

## BUG-080 - The decoder accepts two block shapes the index scanner refuses

Anger: 9 - decoder refusals.

Found while making the `blob_meta` scanner strict, which now refuses both:
`PackedUint32Iter` truncates a key sid above `u32::MAX` to a different,
possibly valid, sid instead of failing, and `WireGroup::dense` reads only
the first `DenseNodes` of a group where protobuf merge semantics would
combine them, silently dropping the rest. Either way the decoder yields a
different block from the one on the wire, with no error.

Reported by: the blob_meta strict-scanner fix.

## BUG-081 - Extract still trusts indexdata on the paths verification does not cover

Anger: 9 - verifying indexdata costs a per-blob decode.

Extract now decodes a passthrough candidate before copying it raw, but: a
verified raw copy keeps the input's BlobHeader, so a false `count` or ID range
in it carries into the output; a blob whose indexdata bbox claims it lies
outside the region is skipped without decoding; and every extract schedule
trusts the indexdata element kind. Verification also checks only what
building a `PrimitiveBlock` validates, not metadata arithmetic done later
during decode.

Reported by: the extract passthrough fix.

## BUG-082 - Apply-changes LocationsOnWays gaps outside the fixed paths

Anger: 9 - verifying indexdata costs a per-blob decode.

Node blobs whose indexdata labels them way or relation skip coordinate
extraction and can be spliced untouched; the `Pending` location mode protects
only ways being rewritten, not routing that trusts a wrong kind. The
plain-node rewrite path (`write_base_node_local`) still goes through
`Node::nano_lat` and the block API's granularity arithmetic, which do not
check overflow, while the new `node_coords.rs` extractor does.

Reported by: the apply-changes LocationsOnWays fix.

## BUG-083 - Measured figures in comments from the 2026-10-22 fixes

Anger: 1 - nothing to do until the figures drift.

Several comments written in that round carry measured figures (the IdSet
`set` fast path's per-ref cost, `SPARSE_MAX`'s copy volume, renumber's
`PREALLOC_IDS_PER_BYTE` denmark numbers). They are measurement provenance
today and will drift; reword when they do.

Reported by: the 2026-10-22 fix round.

## BUG-084 - Claims and paths from the 2026-10-22 round that nobody has verified

Anger: 9 - planet runs and verifies on the maintainer's hardware.

- DEVIATIONS.md's renumber cross-validation now says the differences from
  osmium are confined to orphan refs in relation member lists. That was true
  under the old pass-through policy; it has not been re-run under the orphan
  shift. Run `brokkr verify renumber --dataset denmark` and reword to match.
- Unmeasured after this round: extract's passthrough verification (germany's
  indexdata nominated no candidates, so the A/B never exercised it; needs an
  input whose node blobs carry v3 bboxes), apply-changes
  `--locations-on-ways` (the new checked coordinate extractor runs per node
  blob), and the `merge-changes --simplify` version floor (blocked by
  BUG-079 on every germany range).
- `IdSet`'s array-chunk representation was benchmarked only through altw,
  tags-filter, extract and renumber on germany and denmark; a planet altw
  cell has not been run.

Reported by: the 2026-10-22 fix round.

## BUG-075 - A repeated OSMHeader is skipped by some commands and refused by others

Anger: 9 - repeated-header policy.

`sort` and plain `cat` silently skip an OSMHeader blob that is not the
first; `digest` and `cat --dedupe` refuse one. A file with a second header
(naive concatenation of two PBFs) is therefore accepted or rejected
depending on the command. Decide once, in the reader, and have every
command inherit it.

Reported by: the cat-dedupe header fix.

## BUG-074 - Geocode builder output differs between runs of the same input

Anger: 7 - geocode output.

`parallel_classify_phase` delivers results in worker-completion order and
both Pass 2 merges ignore `seq`, so record order, string offsets, the
smoke test's first address, and which duplicate admin geometry survives
can change from build to build. A reorder buffer alone would not fix it
safely: nothing bounds how far ahead workers run of the earliest unmerged
blob (HYG-053), so the buffer could grow without limit. Separately, a node
duplicated across blobs now resolves to one complete coordinate (the
`CoordMmapShared` data race is gone, slots are atomic), but which copy wins
is not deterministic either.

Reported by: the geocode fix for the former coordinate-race entry.

## BUG-048 - Rebuilding a geocode index with `--force` truncates files under open readers

Anger: 7 - geocode output.

`--force` removes the header and `File::create`s every data file in place.
Any process with a `Reader` open on that directory has those files mmapped;
truncation turns its next page access into `SIGBUS`, and a `read_string` on
changed content hits the `expect("Reader::open validated every referenced
string")` panic. Invalidation protects only readers that open later, not
readers already open, which is the normal case for a geocoding server being
refreshed. Fix: build into a sibling directory and rename (old aside, new in,
delete old; readers keep their unlinked inodes), or write each file to
`name.tmp` and rename over. The reader-side `expect` should become an error
regardless. A failed `--force` build before Pass 2's creates also leaves the
old data files beside an absent header.

Reported by: geocode index.

## BUG-051 - The admin edge cover skips the closing segment of an open ring

Anger: 7 - geocode output.

`admin_cells_for_polygon` covers edges with `poly.vertices.windows(2)`, which
never covers last-to-first unless the ring repeats its first vertex. Whether
it does depends on `assemble_rings` and `simplify_ring`, and the format does
not say whether rings are stored closed. `geo::point_in_ring` handles implicit
closure, so the reader and the edge cover may disagree: the closing edge's
cells get coverage only if the flood fill reaches them. State the closure rule
in `format.rs`, assert it in `write_admin_data`, check it in
`validate_structure`.

Reported by: geocode index.

## BUG-052 - Geocode `StringPool` offsets wrap silently past 4 GiB

Anger: 7 - geocode output.

`StringPool::intern` uses `self.data.len() as u32`; past 4 GiB of strings,
offsets wrap and every later string points at the wrong text, with no error.
`name_offset`, `street_offset` and the rest are u32. Other counts wrap the
same way in release: `addr_point_count += 1` and `street_way_count += 1` in
Pass 2, `interp_way_count as u32` and `admin_polygons.len() as u32` in the
header, `total_cells += 1` in Stage B, `cell_ids.len() as u32` in the admin
index, and `chunk_start + STREET_CHUNK as u32` near `u32::MAX`. Other counts
use checked conversion with a "bump FORMAT_VERSION" error. Narrow the
function-level `cast_possible_truncation` allows across `pass3.rs`, `admin.rs`,
`interp.rs` and `mod.rs` to expression level, and consider
`arithmetic_side_effects` on the module, so each site has to choose.

Reported by: geocode index.

## BUG-054 - `inspect --show` prints to stderr on the exit-1 path the contract says is silent

Anger: 7 - CLI behaviour.

`reference/cli-reference.md` and the `ExitStatus` doc in `main.rs` say exit 1
prints nothing to stderr beyond normal output. `run_show_element` prints
`Element not found: <spec>` to stderr before exiting 1. Either the message or
the contract is wrong. The exit-code table is also duplicated in
`docs/cli/commands.md`; keep it in one document and link. A CLI test asserting
empty stderr on every negative-answer path would hold it.

Reported by: CLI and plumbing.

## BUG-056 - `bench-merge` deletes the user-named output before running

Anger: 7 - CLI behaviour.

`run_bench_merge` does `drop(std::fs::remove_file(output))` up front and is
not routed through `guarded_output`. Every other command moved to
`OutputFile` so a pre-existing file at `-o` survives failure.
`bench-merge -o important.pbf` destroys it before the first byte is read, then
writes directly to the final path; the remove error is swallowed, and
`--force` is hard-wired `true`. Fix: `guarded_output`, or bench into a scratch
path the command owns; test that a pre-existing output survives failure.
`disallowed_methods` on `fs::remove_file` in `cli/` would have caught it.

Reported by: CLI and plumbing.

## BUG-063 - Platform-skip predicates turn real failures into passes

Anger: 7 - CLI behaviour.

- `CliOutput::is_uring_unsupported` treats any stderr containing
  `"not supported"` as io_uring unavailable, including unrelated refusals.
- `CliOutput::is_o_direct_unsupported` matches `"Invalid argument"` anywhere,
  including clap-style argument errors.
- `tests/common/mod.rs::is_uring_unavailable` returns true for any
  `std::io::Error` and any `pbfhogg::ErrorKind::Io`, so ENOSPC, EIO and real
  bugs in io_uring library tests are skips.
- The only `DirectWriter` test returns early (passes) when the open fails, so
  on a tmpfs `TMPDIR` it is green without testing anything.

String matching for the CLI and type matching for the library disagree on what
counts. The root cause is that production emits no machine-checkable "platform
does not support this". Fix: `ErrorKind::PlatformUnsupported { feature,
source }`, a distinct CLI exit code, and both predicates checking that.
`BROKKR_TEST_PLATFORM = "1"` in the `platform` profile is set and read by
nothing; wiring it so the predicates fail rather than skip under that profile
would stop a platform run that skips everything from passing green.

Reported by: test harness, write path, CLI and plumbing.

## BUG-070 - `uring_writer` shutdown is not guaranteed to finish

Anger: 7 - CLI behaviour.

`uring_writer`'s `drain_best_effort` gives up after a fixed number of error
rounds, and each `submit_and_wait(1)` can block indefinitely, so its
shutdown is not guaranteed clean. Split out of the former `PbfWriter` framing
entry, whose deadlock half the library-use executor work fixed; this half is
not a process-ownership question and was out of that scope.

Reported by: write path.

## BUG-072 - Index-generating callers turn a malformed block into "no index" instead of an error

Anger: 9 - malformed blocks refused in cat and the writer.

The `blob_meta` scanners now come in two forms: `try_scan_block_ids_checked`
and `try_scan_block_tags` return `Err` for a block they cannot parse, while
the old `scan_block_ids`, `scan_block_ids_checked` and `scan_block_tags`
keep their signatures and map that `Err` to `None`. The writer's passthrough
reframe (`src/write/writer.rs`) and plain `cat` (`src/commands/cat/mod.rs`)
still call the old forms, so a malformed payload is written out without an
index rather than refused. `check --index` likewise does not report a
scanner `Err` as its own violation, so a malformed payload with no stored
sidecar metadata passes. Switch these to the `try_` forms. `sort` and
`cat --dedupe` already turn `None` into an error.

Related: duplicate tagdata reads as `None` rather than a placeholder,
because sort, dedupe and apply-changes copy those bytes through, so
`check --index` cannot see duplicate tagdata on its input; a presence flag
in `read/frame.rs` and `raw_frame.rs` would close it. The read-path warning
in `src/read/blob.rs` still spells the accepted lengths as "26 or 42".

Reported by: the blob_meta fix for the former strict-scanner entry.

## BUG-073 - `BlockBuilder` leaves empty-string keys out of tagdata

Anger: 9 - tagdata change in the block builder.

`src/write/block_builder.rs` drops the empty key from the tag index it
writes, so a blob whose elements carry a tag with key `""` advertises a key
set without it, and `tags-filter -e ',highway'` style expressions that
require the empty key skip the blob: a wrong answer. The passthrough
scanner now includes empty keys, so `check --index` flags such blobs as
`TagKeysMissing`.

Reported by: the blob_meta fix for the former strict-scanner entry.

## BUG-086 - Decompression buffers under-reserve

Anger: 9 - per-blob decompression reserve.

`src/read/decompress.rs::pool_get` calls
`buf.reserve(capacity.saturating_sub(buf.capacity()))` on a cleared
(`len == 0`) pooled buffer. `reserve(n)` only guarantees
`capacity >= len + n`, so a pooled 1 MiB buffer asked for 1.5 MiB reserves
nothing and grows during decompression instead. `buf.reserve(capacity)` is
the intended call. Other reservation calls in the same file subtract the
existing capacity the same way. It is the only decompression-buffer path, so
this is a performance change on every blob: measure before and after.

Reported by: the wave-0 reviews.

## BUG-088 - The check detail cap does not bound check memory

Anger: 8 - check aggregation.

`--max-errors` now caps the retained detail lists, but `verify_ids` still
collects per-blob violations uncapped before merging, and `check_refs` keeps
its counting and deferred-reference vectors whole. Total memory on a hostile
input still grows with the number of violations. Bounding it needs the
exact counts carried through a different aggregation.

Reported by: the wave-0 review.

## BUG-064 - Unit tests use fixed scratch names and collide across parallel sweeps

Anger: 3 - unit-test scratch and test timeouts.

The integration-test sites are fixed (unique dirs under
`common::test_scratch()`). Still fixed-name in unit tests:
`src/osc/parse.rs::make_test_dir` and `src/osc/write.rs`
(`temp_dir().join("pbfhogg_osc_test_{suffix}")`, `remove_dir_all` first, so
two sweeps delete each other's directory), and
`src/read/direct_reader.rs::direct_reader_reads_file` (a fixed file under
`data/bench-tmp`, then `remove_dir` of brokkr's scratch dir when empty). The
fix is HYG-071's shared scratch.

Reported by: change application, filtering and spatial, read path, test
harness.

## BUG-089 - The CliInvoker 10 s default can kill legitimately slow CLI tests

Anger: 3 - unit-test scratch and test timeouts.

`tests/common/cli.rs` now defaults `CliInvoker` to a 10 s timeout so it fires
before the 20 s `brokkr check` watchdog. Tests run individually via
`brokkr test --timeout N` (the denmark roundtrip, the osmium
cross-validations) that drive a slow CLI command through the default would
now fail at 10 s. They need explicit timeouts, or the default should follow
the watchdog brokkr sets.

Reported by: the wave-1a coordinator.

## BUG-090 - The apply-changes scanner can hold every post-node descriptor until EOF

Anger: 8 - apply-changes internals.

Under `--locations-on-ways`, `apply_changes/scanner.rs` holds back way and
relation descriptors until the node-to-way barrier fires. Nothing bounds how
many it holds, so on an input where the barrier fires late the scanner can
buffer every descriptor after the node phase until end of file, separately
from the drain's reorder map. Unverified beyond the fixer's reading.

Reported by: the wave-1 drain fixer.

## BUG-091 - Way, relation and dense-decode delta arithmetic still differs between debug and release

Anger: 8 - builder and decoder deltas.

`BlockBuilder`'s dense-node deltas now use explicit wrapping arithmetic. The
same class remains elsewhere: `src/write/encode.rs` computes way-ref and
relation-member deltas with plain subtraction, and `src/read/dense.rs`
reconstructs dense IDs and metadata with plain addition. Release builds
wrap, which is what modular varint deltas need; debug builds panic on the
same extreme but legal input. Use `wrapping_sub` / `wrapping_add` and say
why.

Reported by: the wave-1 write-path fixer.

## BUG-092 - Single-pass multi-extract still reports each region twice

Anger: 7 - CLI behaviour.

Multi-extract diagnostics now name the final output paths rather than the
scratch siblings, but a single-pass run still prints each region twice: the
library's `extract.multi.slot_counts` event and the CLI's own summary from
`final_paths`. Drop one of them.

Reported by: the wave-1 reviewer.

## BUG-093 - Multi-extract config path resolution deviates from osmium, undocumented

Anger: 7 - CLI behaviour.

A relative `"directory"` in a multi-extract config now resolves against the
config file's directory; osmium resolves it against the working directory.
With `-d`, an absolute `output` in the config keeps only its file name under
the override directory; osmium concatenates directory and output. Both are
deliberate and tested, and DEVIATIONS.md mentions neither. Document them, or
change the absolute-output rule if the maintainer prefers osmium's.

Reported by: the wave-1 reviewer.

## BUG-094 - The apply-changes `-j 1` refusal may rest on a fixed hazard

Anger: 7 - CLI behaviour.

`apply_changes/rewrite.rs` refuses `--jobs 1` because a single worker had a
deadlock hazard on a mid-stream worker panic. Scanner and worker panics are
now caught into the shared failure slot and the drain drops its channels, so
the lone worker's exit drops the candidate receiver and the scanner's send
fails; the hazard looks gone. The error text and the `MergeOptions::jobs` doc
still state the old rationale. The node-to-way barrier deadlock recorded
under HYG-081 is a separate `-j 1` case and must be ruled out before the
refusal is lifted.

Reported by: the wave-1 reviewer.
