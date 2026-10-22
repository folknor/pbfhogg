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
```

---

## BUG-005 - Indexdata claims are folded into observed element counts

`sort`, `getid` and `cat --dedupe` keep a separate `passthrough_claimed`
count, and extract verifies a passthrough blob by decoding it before
copying it raw, so its counts are observed. Apply-changes still trusts
indexdata `count` for its stats on passthrough blobs, so a lying indexdata
moves its headline number with no way to tell (see HYG-061 for the trust
model).

Reported by: re-encoding, filtering and spatial, change application.

## BUG-006 - `degrade --block-cap N` drives an unbounded preallocation

`repack` uses `BlockBuilder::with_element_cap_prealloc(cap, file_len)`, and
its comment plus `block_builder.rs` explain why: a CLI integer must not drive
preallocation (`--elements-per-blob usize::MAX/2` was several huge
allocations). `degrade::run_kind_phase` uses
`BlockBuilder::with_element_cap(block_cap)`, which is
`prealloc(cap, usize::MAX)`. `--block-cap` is hidden but accepted. Same fix,
missed copy. Enforce by deleting `with_element_cap` or making it
`#[cfg(test)]`, so every caller has to name a bound.
`BlockBuilder::with_element_cap(0)` also panics (see HYG-047).

Reported by: re-encoding.

## BUG-007 - Degrade's "needs at least one flag" error omits `--drop-ids`

`DegradeFlags::any()` includes `drop_ids`; the error text listing the flags
does not. The flag list is spelled at four sites (`any()`, `needs_decode()`,
the error text, the `applied` list in `write_summary`) plus the CLI, and they
have already diverged. Generate the text from one `(field, flag name)` table,
or test that the error text names every field.

Reported by: re-encoding.

## BUG-009 - `apply-changes --locations-on-ways` does not update untouched base ways

`reference/cli-reference.md` documents the flag as "Preserve and update
way-node locations through the merge". Only ways the OSC touches get fresh
coordinates: `NodeLocationIndex::build_from_diff` collects refs of OSC ways
only; `rewrite_block_parallel` writes a surviving base way through
`write_base_way_local_with_locations`, which copies the old coordinate bytes;
way blobs with no diff overlap take the splice fast path. If the OSC moves
node N and does not touch way W, W goes out with N's old position, and a
deleted N leaves W with N's last coordinate. `loc_missing` cannot see it.
osmium's `apply-changes -l` recomputes from the merged node set.
`reference/pipeline.md` says the production loop re-runs altw after each
merge instead, which is probably why nobody has hit it.

Fixing it needs the set of base ways referencing any modified or deleted node
(a reverse index or a full way pass), routed through rewrite instead of
splice; otherwise the doc has to say "OSC ways only". Enforce with a
cross-validation test against `osmium apply-changes -l` on a diff that moves
a node referenced only by an untouched way.

Reported by: change application.

## BUG-013 - The apply-changes drain's "byte-budget reorder buffer" has no budget

`descriptor.rs` ("merged into a byte-budget reorder buffer"),
`DrainItem::byte_cost` ("for the reorder buffer's byte-budget backpressure")
and `streaming.rs` describe backpressure. `run_drain` only records
`bytes_in_buffer` into a high-water counter, always pulls the next item and
inserts it into the `BTreeMap`. When one worker stalls on seq N, the drain
keeps emptying the 256-slot channel, the scanner and other workers never
block, and every `Rewritten { framed_chunks }` after N piles up; the limit is
how far ahead the scanner can get, which is the whole file.

Fix: stop `recv` while `bytes_in_buffer > budget` and the head seq is
missing, or have workers take a budget permit before decompressing, as the
writer does. A `test-hooks` delay at seq N can assert the high-water stays
under budget.

Reported by: change application.

## BUG-014 - Apply-changes scanner "bytes high water" counters record the largest single item

`run_scanner` does `*bytes_hw_fast = (*bytes_hw_fast).max(approx_cost)` under
a comment saying it tracks bytes in flight.
`merge_scanner_to_drain_bytes_high_water` and
`merge_scanner_to_workers_bytes_high_water` therefore report the largest
descriptor seen (about 64 bytes plus tagdata). Anyone sizing channels from
the sidecar gets a meaningless number. Rename or delete.

Reported by: change application.

## BUG-015 - OSC decompress counter is attributed to merge-changes for every caller

`xml_parse::parse_osc_path` emits `merge_changes_decompress_ns` for every
caller: apply-changes, tags-filter OSC and merge-changes. Brokkr therefore
books apply-changes' OSC decompress time under a merge-changes name. On
multi-input merge-changes it is emitted once per file under one name from
parallel workers, so the max-wins view keeps one file's time. Apply-changes
uses `merge_*` counters (named after the internal function), merge-changes
uses `merge_changes_*`, and the markers are `MERGE_*` vs `MERGECHANGES_*`;
`MERGECHANGES_WRITE_FINISH_START/END` are emitted back to back with nothing
between them. Fix: pass the counter prefix or a typed counters handle into
`parse_osc_path`; a const table of counter names per command lets a test
assert uniqueness.

Reported by: change application.

## BUG-017 - The version-advance rule differs between apply-changes and `merge-changes --simplify`

`CompactDiffOverlay::check_version_advance` (apply-changes) compares against
the max of the live record's version and a versioned tombstone map, so after
`delete v9`, an unversioned change, then `modify v8`, the v8 is refused.
`merge_changes::check_version_advance` compares only against the previous
change for the key in `last_by_object`; that change has no version, so v8 is
accepted. The same three-file input is refused by one command and accepted by
the other. The error texts are separate prose with different remediation
advice. The `xml_parse.rs` module doc argues that two paths answering one
question differently is the recurring defect; this is that defect one layer
down.

Fix: a `VersionFloor` type (live max plus tombstone max per `(kind, id)`)
owned by `osc`, used by both. Enforce by extending
`tests/osc_parser_parity.rs` with a version-gate case fed through both
commands.

Reported by: change application.


## BUG-020 - Apply-changes discards or escalates a scanner error or worker panic when the drain fails

Worker errors now reach the caller: workers publish into a shared
first-error slot (`apply_changes/failure.rs`) before releasing their drain
channel, and the drain returns that error. Two paths remain.
`drain::run_drain(...)?` still returns early out of the scope closure, so
the explicit `scanner_handle.join()` never runs and a scanner that returned
`Err` has its error dropped. A worker that panics bypasses the slot; the
worker pool's inner scope turns the panic into a `scope` panic after the
join, so the cause becomes a process panic, and under `--locations-on-ways`
a panicking node-blob worker can still hang the barrier.

Fix: have the scanner publish into the same slot, catch worker panics into
it, and always join both before picking the root cause. The existing
`panic_at_blob_seq` hook test should assert which message the caller gets.

Reported by: change application.

## BUG-021 - `apply_changes::merge` does not refuse output equal to the base input

The library entry point never checks whether `output_pbf` is the same file as
`base_pbf`; the writer would truncate the input it is about to
`copy_file_range` from. The CLI guards it, a library caller does not. Fix:
refuse `same_file(base, output)` in the library. See HYG-030 for the wider
output-publication policy.

Reported by: change application.

## BUG-022 - The altw external backend rejects any input with a zero-ref way (likely)

`scan::way::parse_way_refs` returns without calling back when
`way.refs_data.is_empty()`. Stage 1 pass A pushes `per_way_rcs.push(refs.len())`
inside that callback, so a zero-ref way gets no entry in the per-way refcount
sidecar. Stage 4's `reframe_way_blob_with_locations` reads one sidecar varint
per `Way` message, so it reads the next way's count, then fails with
"encountered more way messages than the per-way sidecar record declares".
Sparse accepts the same input (`reframe.rs` handles empty refs explicitly).
`tests/edge_cases.rs::zero_ref_way_roundtrips` covers plain read/write only.
Which backend runs, including what `--index-type auto` picks, decides whether
a legal input is accepted.

Fix: call back with an empty slice, or count Way messages instead of
callbacks. Enforce with a cross-backend test: a zero-ref-way fixture through
sparse and external must give byte-identical way output.

Reported by: altw and scanning.

## BUG-023 - Sparse altw fails when `-o` is a bare relative filename (likely)

`add_locations_to_ways` computes
`scratch_dir = output.parent().unwrap_or(Path::new("."))`. For `out.osm.pbf`,
`Path::parent()` returns `Some("")`, not `None`, so the fallback never fires.
`sparse.rs::preallocate_sparse_file` passes that to `statvfs`, and
`statvfs("")` fails with ENOENT: "failed to query free space for sparse index
in : No such file or directory". External's `ScratchDir::new` survives because
`"".join(name)` is relative. The same idiom is in `external/mod.rs` and the
CLI. A library caller writing to `/dev/stdout` or a FIFO likewise gets
`/dev` as derive-changes' scratch dir. Integration tests use absolute paths.
Fix: one helper mapping an empty parent to `.`, used by every scratch
derivation (HYG-028). Pin with a CLI test using `current_dir` and a bare `-o`.

Reported by: altw and scanning, re-encoding.

## BUG-024 - `IdSet::pre_allocate` sizes from an unverified `max_id`

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

## BUG-026 - Altw stage 3's empty-bucket error path reports a panic that did not happen

In `stage3_slot_reorder`, the non-empty-bucket error path records the error,
calls `router.abort(...)` with the real message, and returns. The empty-bucket
branch (`entry_counts[bucket_idx] == 0`) records the error and returns `Err`
without `router.abort`; the still-armed `AbortOnDrop` guard then fires
`router.abort("stage 3 worker panicked (AbortOnDrop guard fired)")`. Stage 4
waiters see a panic message for a plain error; the top-level message is right
only because `external_join` prefers the stage 3 result. `AbortOnDrop` stands
in for both "panicked" and "returned early", and its message assumes the
first.

Reported by: altw and scanning.

## BUG-027 - `s2_orphan_records` undercounts

The counter is `bkt.records.len() - record_ptr` after the blob walk. Records
the walk passed over without resolving (an ID in a gap between tuples inside a
blob's range) are not counted, so it covers only orphans past the last blob.
It is the counter you would check when `missing_locations` is non-zero.

Reported by: altw and scanning.

## BUG-028 - Page-fault deltas underflow, and `/proc/self/stat` parsing breaks on a spaced comm

`external_join` computes `s1_majflt_after - s1_majflt_before` (and s2, s3) on
`u64`. `read_page_faults` returns `(0, 0)` on any read failure, so a failure
on the second read underflows: a panic in debug, a huge counter in release.
The parser uses `split_whitespace`, so fields shift if the comm field contains
a space, which a library consumer's binary name can. Use `saturating_sub` and
parse from the last `)`. See HYG-064 for the three `/proc` parsers.

Reported by: altw and scanning.

## BUG-029 - Lost wakeup in the altw stage 4 location router

`ConcurrentBlobLocationRouter` waits on `notify` with `notify_mu: Mutex<()>`,
while the predicate lives under per-slot mutexes and atomics. The comment
argues waiters re-check under `notify_mu`, but `publish_worker` sets the slot,
drops the slot lock and calls `notify_all()` without taking `notify_mu`. A
publish between the waiter's re-check and its `wait` is lost, and the waiter
sleeps until the next publish, `mark_producer_done` or `abort`. The final
`mark_producer_done` guarantees one more wakeup, so it should not hang, but a
stage 4 worker can stall for an arbitrary stretch of stage 3. Fix: publishers
take `notify_mu` around the update and notify, or put the predicate under it.
A loom test of `wait_ready` against `publish_worker` would hold it.

Reported by: altw and scanning.

## BUG-030 - `with_blob_filter` is silently ignored by `for_each` and `par_map_reduce`

`ElementReader::with_blob_filter` documents that the pipeline skips
decompressing blobs whose type does not match. `for_each` destructures
`Self { blob_iter, header, .. }` and drops the filter; `par_map_reduce` never
reads it. A library user who sets a filter and calls either gets every
element, with no error. Fix: apply it in all three engines (the check is
`should_skip_blob`), or make the filter a parameter of the pipelined methods
only. Enforceable by type: a `FilteredReader` offering only the methods that
honour it.

Reported by: read path.

## BUG-031 - `HeaderWalker::open` destroys the `io::ErrorKind`

`header_walker.rs` wraps the open error as
`io::Error::other(format!("failed to open {}: {e}"))`. `file_reader.rs` has
`open_error`, whose doc says it exists to keep `ErrorKind` "so callers
matching on `NotFound` still match". The two implementations have diverged:
a missing input opened through the walker reports `Other`, through
`FileReader` reports `NotFound`. Fix: one `open_error` used by both.

Reported by: read path.

## BUG-032 - `read_raw_frame` advances the caller's offset before the read succeeds

`*file_offset += frame_len` precedes `reader.read_exact(...)?`. On a truncated
payload the error propagates with the offset already past the frame, so any
caller that reports or resumes from `file_offset` points past the failure.
Move the increment after the read.

Reported by: read path.

## BUG-034 - `DirectReader::skip` misses skip-past-EOF on an aligned target

When `aligned == target`, `skip` seeks and clears the buffer with no EOF
check. `FileReader::skip`'s doc says `DirectReader::skip` already validates
EOF, which is true only for the unaligned branch. A file truncated at a
page-aligned point passes `skip` and surfaces later, or never for callers like
`has_indexdata`. This is the shape-4 hole the buffered arm closes with its
sentinel byte. Fix: the same sentinel, or compare against file size. Testable
with an aligned truncated fixture on the `platform` profile.

Reported by: read path.

## BUG-035 - Sequential `for_each` fires its sort debug-assert on sorted history files

Sequential `for_each` debug-asserts strictly increasing node IDs
(`id > last_node_id`) whenever the file is sorted; `for_each_pipelined` uses
`>=` for history files. The sequential path therefore asserts on any sorted
history file in debug builds. Extract one `check_sorted_order(header)` used by
both; testable with a sorted history fixture in debug.

Reported by: read path.

## BUG-037 - `StringTable::pre_seed` identity mapping is not structural

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

## BUG-038 - `ParallelGzipWriter` with `chunk_size == 0` loops forever

In `ParallelGzipWriter::new(inner, 0, n)`, `write` loops with `space == 0`
and `take == 0`. `worker_count` is clamped to 1; `chunk_size` is not. Clamp
it or take `NonZeroUsize`.

Reported by: write path.

## BUG-039 - `HeaderBuilder::bbox` accepts NaN, infinities and inverted boxes

`bbox` takes raw `f64`s and converts with `(x * 1e9).round() as i64`
(saturating, NaN to 0), with no check for NaN, infinity, `left > right` or
out-of-range latitude. A bad `--bbox` or a broken input header round-trips
into a header claiming `0,0,0,0`. `HeaderBuilder::build` returns `io::Result`
but cannot fail. Validate in `bbox()` or a checked `Bbox` type, and make
`build` the place that returns the error.

Reported by: write path.

## BUG-040 - BlockBuilder delta encoding behaves differently in debug and release

`BlockBuilder` computes `id - last_dense_id`, `meta.uid - last_dense_uid` on
`i32`, and user-sid deltas. Extreme but legal inputs (uid `i32::MAX` followed
by a negative uid from an OSC) overflow. Release has no `overflow-checks`, so
it wraps, which is what a wrapping varint delta decoder needs, but debug
builds panic: the two builds disagree on the same input. Use `wrapping_sub`
explicitly and say why.

Reported by: write path.

## BUG-041 - Multi-extract prints scratch file names as if they were the outputs

`run_extract_config` redirects every `slot.output` to its `OutputFile`
scratch sibling (`.pbfhogg-tmp-<pid>-<seq>.<name>`) before calling
`extract_multi`. Inside the library, `extract/mod.rs`
("`[i/N] Extracting to {}`", sequential fallback) and `extract/multi.rs`
("`  [i] {name}: N elements`", single-pass) print `slot.output`. The operator
sees dot-prefixed temporary names that no longer exist after commit; the CLI
then prints a second, correct summary from `final_paths`, so single-pass runs
report every region twice under two names. The library lines are now
`Diagnostics` events (`extract.multi.slot`, `extract.multi.slot_counts`), but
their text still carries the scratch name; carry a display name in
`ExtractSlot`. Testable via a `CliInvoker` stderr assertion.

Reported by: filtering and spatial.

## BUG-042 - Multi-extract output locations depend on the working directory and on how `-d` strips

In `parse_extract_config`, `polygon_file` resolves against `config_dir` and
`output` against `directory.unwrap_or(config_dir)`, but a relative
`"directory"` is used as-is, relative to the process working directory. The
same config writes to different places depending on where it is run.
Separately, `run_extract_config` rewrites `slot.output` for the `-d` override
only `if let Ok(relative) = slot.output.strip_prefix(old_dir)`; an absolute
`output` in the config stays where the config put it, silently. Fix: join a
relative `directory` onto `config_dir`, and pass the override into
`parse_extract_config` as the resolve root.

Reported by: filtering and spatial.

## BUG-077 - Two osmium parity gaps not yet checked against DEVIATIONS.md

Found while fixing the list-file readers, not verified further:

- osmium's `tags-count` (`add_matcher`) has no type-prefix syntax, while
  pbfhogg parses `w/highway` as typed in its tags-count expressions.
- osmium `getid` treats a bare ID as a node (`--default-type` defaults to
  node); pbfhogg rejects bare IDs unless `--default-type` is given.

Each is either a bug or an undocumented deviation; DEVIATIONS.md mentions
neither.

Reported by: the list-file reader fix.

## BUG-044 - Degenerate polygon rings produce an empty extract with exit 0

`extract::parse_ring` accepts rings of any length, including 0 to 2 points
and unclosed rings; `point_in_ring` then returns `false`. A polygon file with
a degenerate exterior yields an empty extract and success: the wrong answer
reported as success that `parse_bbox`'s comment says was fixed for bboxes.

Reported by: filtering and spatial.

## BUG-045 - Geocode header radii are not validated on read

`BuildConfig::validate` rejects non-finite or negative radii, but
`Header::from_bytes` accepts any `f32`. A NaN radius from another producer or
corruption makes `fine_max_dist_sq` NaN and every `dist_sq < max_dist_sq`
false: the index opens and answers "nothing here" everywhere. Level checks
were mirrored into the reader; radius checks were not. Fix: one
`validate_radius` shared by builder and reader, tested by poking header bytes
16..24 like the existing level tests.

Reported by: geocode index.

## BUG-046 - The geocode reader skips out-of-range entries, and one off-by-one reads the next way

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

## BUG-078 - Renumber output carries no tagdata

Renumber writes its re-framed blobs with tagdata `None`, so `tags-filter`
and `tags-count` cannot skip blobs of a renumbered file by tag key, while
indexdata is written. Its rewriters already walk every element's keys.

Reported by: the renumber fix.

## BUG-075 - A repeated OSMHeader is skipped by some commands and refused by others

`sort` and plain `cat` silently skip an OSMHeader blob that is not the
first; `digest` and `cat --dedupe` refuse one. A file with a second header
(naive concatenation of two PBFs) is therefore accepted or rejected
depending on the command. Decide once, in the reader, and have every
command inherit it.

Reported by: the cat-dedupe header fix.

## BUG-074 - Geocode builder output differs between runs of the same input

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

## BUG-049 - Admin flood fill is unbounded at the admin levels validation permits

`MAX_BUILD_S2_LEVEL` bounds per-segment sampling, not the interior flood fill
in `admin_cells_for_polygon`, whose work and memory (`visited`, `queue`,
`entries`) grow 4x per level. At the default 10 a country is about 10^5 cells;
`--admin-level 19` passes validation and asks for about 10^10 cells for
Russia: a hang and an OOM, the failure the segment budget was introduced to
prevent. Fix: a separate `MAX_ADMIN_S2_LEVEL` derived from a cell-count budget
over the largest legal polygon, checked in `validate`, tested like
`max_build_level_is_exactly_the_sampling_budget_boundary`. The structural fix
is the region coverer tracked in ADR-0015.

Reported by: geocode index.

## BUG-050 - Geocode Stage A writes duplicate coarse entries for long segments

The fused fine/coarse cover closure in `bucketed_cell_assignment_fused` dedups
coarse cells with `coarse_seen[4]`, which remembers four. A segment crossing
more than four coarse cells re-emits the fifth onward for each fine sample,
inflating per-cell counts toward the u16 limit (the reader tolerates
duplicates). The closure appears verbatim twice and the bucket write loop three
times, so any fix has to be made more than once; extract `cover_way_fused` and
a `BucketSink`.

Reported by: geocode index.

## BUG-051 - The admin edge cover skips the closing segment of an open ring

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

## BUG-053 - The sorted-input error tells non-diff commands that diff needs sorting

`commands/mod.rs` has `require_sorted` and `require_sorted_err` with
byte-identical format strings ending "Streaming diff requires sorted inputs to
operate in constant memory." That sentence is emitted for `renumber`,
`time-filter` and `cat --dedupe` too. Fix: one function taking a reason, as
`require_indexdata` does.

Reported by: CLI and plumbing.

## BUG-054 - `inspect --show` prints to stderr on the exit-1 path the contract says is silent

`reference/cli-reference.md` and the `ExitStatus` doc in `main.rs` say exit 1
prints nothing to stderr beyond normal output. `run_show_element` prints
`Element not found: <spec>` to stderr before exiting 1. Either the message or
the contract is wrong. The exit-code table is also duplicated in
`docs/cli/commands.md`; keep it in one document and link. A CLI test asserting
empty stderr on every negative-answer path would hold it.

Reported by: CLI and plumbing.

## BUG-056 - `bench-merge` deletes the user-named output before running

`run_bench_merge` does `drop(std::fs::remove_file(output))` up front and is
not routed through `guarded_output`. Every other command moved to
`OutputFile` so a pre-existing file at `-o` survives failure.
`bench-merge -o important.pbf` destroys it before the first byte is read, then
writes directly to the final path; the remove error is swallowed, and
`--force` is hard-wired `true`. Fix: `guarded_output`, or bench into a scratch
path the command owns; test that a pre-existing output survives failure.
`disallowed_methods` on `fs::remove_file` in `cli/` would have caught it.

Reported by: CLI and plumbing.

## BUG-057 - `check --max-errors` never reaches the refs check

`--max-errors` (default 100, documented as "per check") bounds `verify_ids`
and `check_index` but is not passed to `check_refs`
(`check_refs(path, check_relations, show_ids, direct_io)`).
`result.missing_refs` collects every missing reference, so on an extract with
millions of dangling refs `--show-ids` grows without bound. Pass it through via
an options struct shared by all three checks.

Reported by: CLI and plumbing.

## BUG-058 - Small CLI parsing defects

- `inspect tags` takes its own `file` positional, so
  `pbfhogg inspect x.pbf tags y.pbf` parses two files and silently uses the
  second.
- `run_extract`'s region group `area` is not marked `required`, so
  `(None, None)` is reachable and handled at run time instead of by clap.
- `Inspect` has `file: Option<PathBuf>` with `required = true` plus
  `subcommand_negates_reqs`, then `file.ok_or("Input PBF file is required")`, a
  second check clap already does.
- `diff -q` has `conflicts_with = "output"`, while the `--format osc` arm
  requires `--output` and rejects `--quiet` by hand; the hand check is
  unreachable.
- `detect_input_kind` (tags-filter) falls back to extension for a gzipped
  non-OSC file or an OSC with a BOM or leading whitespace, defaults unknown
  extensions to PBF, and lists `.osc.bz2`, which the OSC reader cannot read,
  so a bz2 input fails inside the XML parser. Refuse ambiguous input naming
  `--input-kind`; drop `.bz2`.
- `run_check` uses `"input.osm.pbf"` as the file name when the path has no
  UTF-8 file name and emits it into JSON; `inspect` uses `to_string_lossy`.
  Use `to_string_lossy` everywhere.

Reported by: CLI and plumbing.

## BUG-059 - Nothing enforces one test per static-hook target

The read-pipeline decode hook itself is fixed: it is a locked gate with a
bounded park that panics with a named message, and `reset()` frees a parked
decode before clearing, and the statics now exist only under the
`static-test-hooks` feature, which empties `tests/all.rs` and is absent from
the library's unit-test harness, so neither of the root package's shared
harnesses can arm one
(reference/testing.md, "Test binaries"). What remains: nothing checks that
each fault target holds a single `#[test]`, so a second test added to one
would share the armed static with the first. HYG-062 covers removing the
globals outright.

Reported by: test harness.

## BUG-061 - The `minimal` sweep runs CLI tests against a binary another sweep built

`brokkr.toml` says `build_packages` is "Required for tests/cli_*.rs: without
it ... CliInvoker silently hits a stale binary". `minimal` has no
`build_packages` by design, but `cli_cat.rs`, `cli_sort.rs`, `cli_diff.rs`,
`cli_repack.rs`, `cli_degrade.rs`, `cli_exit_codes.rs` and
`fixture_helpers.rs::cli_invoker_runs_version_command` carry no
`#![cfg(feature = "commands")]`, so they run in `minimal` against whichever
binary `all` or `consumer` left, with different features. `cli_sort.rs`'s
`cfg(not(feature = "linux-..."))` feature-missing tests are true in `minimal`
while the binary may have the features. With parallel sweeps this is order-
and timing-dependent. Fix: gate `tests/common/cli.rs` itself out of `minimal`
so an ungated user fails to compile, or have brokkr not export
`BROKKR_TEST_BIN_DIR` for a sweep that did not build the binary. See HYG-070
for moving CLI tests into `cli/tests/`.

Reported by: test harness.

## BUG-062 - A tier-1 test requires `osmium` on PATH with no skip

`tests/geocode_index.rs::legal_non_dense_nodes_build_a_queryable_index` is a
plain `#[test]` that shells out to `osmium cat ... pbf_dense_nodes=false` and
`expect`s it. `reference/testing.md` says contributors do not need external
tools for a clean `brokkr check`; the other osmium users are
`#[ignore = "external"]` with skip-on-missing. On a host without osmium the
default check fails. The fixture could be emitted by pbfhogg's own
`BlockBuilder`. Enforce with a text rule: `Command::new("osmium")` only in a fn
with `#[ignore = "external"]`.

Reported by: test harness.

## BUG-063 - Platform-skip predicates turn real failures into passes

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

## BUG-064 - Tests use fixed scratch names in shared locations and collide across parallel sweeps

Commit `b353de8` made the `all`, `consumer` and `minimal` sweeps run
concurrently. Several tests use fixed or weakly unique scratch paths:

- `src/tag_expr.rs` test: `std::env::temp_dir().join("pbfhogg_test_expr")`, a
  fixed name, then `remove_dir_all`. A live cross-sweep collision.
- `src/osc/parse.rs::make_test_dir` and `src/osc/write.rs`:
  `temp_dir().join("pbfhogg_osc_test_{suffix}")`, no pid or random part,
  `remove_dir_all` first, so two sweeps delete each other's directory
  mid-test.
- `src/read/direct_reader.rs::direct_reader_reads_file` writes
  `$CARGO_MANIFEST_DIR/data/bench-tmp/direct_reader_test.bin`, brokkr's
  configured scratch on some hosts, with a fixed name, then `remove_dir(&dir)`,
  which deletes brokkr's scratch dir when it happens to be empty.
- `tests/geocode_index.rs::ensure_index_exists` does
  `remove_dir_all(target/geocode-test)` before rebuilding, so two concurrent
  geocode tests can delete each other's index mid-read.

The `path_guard.rs` test comment already says never to use
`std::env::temp_dir()` because of exactly this, and the project forbids
`/tmp`. Fix and location policy are HYG-071; enforce with `clippy.toml`
`disallowed-methods = ["std::env::temp_dir"]`.

Reported by: change application, filtering and spatial, read path, test
harness.

## BUG-065 - The scratch leak check is vacuous when the scratch root is wrong

`tests/common/mod.rs::snapshot_dir_inner` swallows `read_dir` errors and
returns an empty set; `assert_scratch_unchanged` then compares two empty sets
and passes. A typo or uncreated root makes the leak check meaningless. The
root call should fail; only subdirectories removed mid-walk merit tolerance.
Related: `ensure_index_exists` in `geocode_index.rs` turns any build failure
into `None` via `.ok()?`, which callers treat as a skip, so a builder
regression on Denmark reads as "data missing".

Reported by: test harness.

## BUG-068 - apply-changes prints its summary twice

`apply_changes::merge` prints the summary in `rewrite.rs`, and the CLI's
`run_apply_changes` prints it again. Remove the latter print after a separate
CLI output change is approved. bench-merge also gets the library summary.

Reported by: library-use implementation survey.

## BUG-070 - `uring_writer` shutdown is not guaranteed to finish

`uring_writer`'s `drain_best_effort` gives up after a fixed number of error
rounds, and each `submit_and_wait(1)` can block indefinitely, so its
shutdown is not guaranteed clean. Split out of the former `PbfWriter` framing
entry, whose deadlock half the library-use executor work fixed; this half is
not a process-ownership question and was out of that scope.

Reported by: write path.

## BUG-071 - Renumber orphan refs can collide with new IDs

DEVIATIONS.md "renumber: orphan-reference handling" says orphan refs (to
objects absent from the input) pass through with their old ID, and claims
"the output never contains ids for objects that aren't in the output" and
that "a ref to old id X always means the thing that had id X in the input".
Both are false. An orphan ref is by definition an ID for an object not in
the output, and its old ID can equal a new ID assigned to a different
object: with the default start ID, an orphan ref to old node 5 and the
renumbered node 5 are the same number, so the way silently points at the
wrong node. Derived from the documented behaviour, not reproduced.

Options: assign orphans fresh IDs past the in-input range (osmium), refuse,
or keep pass-through and document the collision. The DEVIATIONS rationale
needs rewriting whichever is chosen.

Reported by: cross-reference of the notes against DEVIATIONS.md.

## BUG-072 - Index-generating callers turn a malformed block into "no index" instead of an error

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

`src/write/block_builder.rs` drops the empty key from the tag index it
writes, so a blob whose elements carry a tag with key `""` advertises a key
set without it, and `tags-filter -e ',highway'` style expressions that
require the empty key skip the blob: a wrong answer. The passthrough
scanner now includes empty keys, so `check --index` flags such blobs as
`TagKeysMissing`.

Reported by: the blob_meta fix for the former strict-scanner entry.
