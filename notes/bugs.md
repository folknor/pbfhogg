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

## BUG-001 - Renumber hangs instead of failing on any mid-stream error

`renumber/pass1.rs`, `renumber/stage2.rs` and `renumber/relations.rs` share
one shape: `sync_channel(32)` created outside `thread::scope`, workers
`tx.send(...)` in a work-stealing loop, and a consumer inside the scope that
returns early via `?` on the first `Err` popped from the reorder buffer. The
receiver is borrowed, not moved, so it lives until after `thread::scope`
returns, and `thread::scope` joins every worker first. There is no cancel
flag, so the workers keep claiming tasks, fill the 32-slot channel and block
in `send` forever.

Any error with more than about 32 + worker-count blobs left in the phase
becomes a hang: a corrupt blob mid-node-section, `ENOSPC` on the output, or
the "indexdata understates its own contents" refusal in `wire_rewrite.rs`.
`brokkr test` reports it as "did not finish within 20s" with a futex wchan.
Fixture tests have far fewer than 32 blobs per kind, and truncation is caught
in the header scan before workers start, so nothing covers this.

Fix: move the receiver into the scope and drop it on the error path, plus an
abort flag the workers check before claiming. The three loops are one
function (see HYG-011) and should be written once with that property.
Enforce with a fault test: more than 64 node blobs, one corrupt in the
middle, assert an error exit and not a timeout.

Reported by: re-encoding.

## BUG-002 - Renumber writes wrong way refs when node IDs are not strictly increasing

Pass 1 (`wire_rewrite.rs::reframe_dense_with_new_ids`) assigns each node its
new ID by position (running count). Stage 2d and R2d resolve refs by rank in
the bitset (`start_id + rank_if_set(old_id)`). The two agree only if node IDs
are strictly increasing across the whole node section in schedule order with
no duplicates. Nothing checks that: `require_sorted` trusts the header's
`Sort.Type_then_ID`, the delta decode accepts zero and negative deltas (only
`old_id < 0` is refused), and a duplicate sets the same bit twice. A file that
lies about being sorted (what `degrade --unsort` produces) or carries a
duplicate node gets ways pointing at the wrong nodes, exit 0. The same
reasoning applies to way IDs in stage 2d versus relation member resolution.

The check is cheap: `delta > 0` per node inside a group, and per-blob
`min_id > previous max_id` in the schedule, where indexdata is already in hand.
Enforce with a test that runs `degrade --unsort` output, or a duplicate-ID
fixture, through renumber and asserts refusal.

Renumber's `--start-id` is also not range-checked: negative starts are
accepted and can produce a renumbered ID 0.

Reported by: re-encoding.

## BUG-003 - Unchecked delta accumulation in the renumber wire rewriter

`wire_rewrite.rs` accumulates sint64 deltas with plain `old_id += delta`, in
the node, way-ref and memids decode loops. A crafted delta overflows: a panic
in debug, a wrap in release, which may land negative (caught by the `< 0`
check) or large positive (hits `set_atomic_checked`). The new-ID prefix sum
next door uses `checked_add`. Fix: `checked_add` mapped to an input error.
Enforce with `#![deny(clippy::arithmetic_side_effects)]` scoped to
`wire_rewrite.rs`, the one file where every integer comes off the wire.

Reported by: re-encoding.

## BUG-004 - `cat --dedupe` builds the output header from the first input only

`cat/mod.rs::cat_output_header_bytes` documents that all cat paths used to
build the header from `files[0]` alone (a bbox not covering the file's own
contents, replication fields from whichever input came first) and now
reconciles bbox, replication fields and `HistoricalInformation`.
`cat/dedupe.rs::build_blob_index`, the fourth path, still does
`if header.is_none() { header = Some(hdr) }`. The fix was applied per call
site and missed the one in the same directory. Fix: `merge_pbf` calls
`cat_output_header_bytes`. Enforce structurally: make a multi-input header
constructor take `&[HeaderBlock]`, so a single-header call on a multi-input
command does not type-check. The "only ever understate" rule deserves to be
the crate-wide policy for any merged header (merge-changes included).

Reported by: re-encoding.

## BUG-005 - Indexdata claims are folded into observed element counts

`sort` separates `index_scanned` from `passthrough_claimed`, and `getid`
separates `passthrough_claimed` from `*_written`, both explaining that
folding an indexdata claim into an observed count reports hearsay as
observation. Three sites do the folding anyway:

- `cat/dedupe.rs::count_entry` adds `entry.index.count` from unverified
  indexdata into "Merged N elements".
- `extract/common.rs::pread_execute` adds the passthrough blob's indexdata
  `count` to `stats.nodes_in_bbox`.
- `extract/multi.rs` does the same on its passthrough path.

A lying indexdata moves the headline number with no way to tell. Fix: adopt
the getid/sort split in `ExtractStats` and the dedupe stats. Apply-changes has
the same shape: `indexdata count` is trusted for stats on passthrough (see
HYG-061 for the trust model).

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
the error text, the `applied` list in `print_summary`) plus the CLI, and they
have already diverged. Generate the text from one `(field, flag name)` table,
or test that the error text names every field.

Reported by: re-encoding.

## BUG-008 - Renumber timing counters truncate to zero per blob

Every `StageCounters` field accumulates `elapsed().as_millis() as u64` per
blob. A planet node blob's pread is well under 1 ms, so `pass1_pread_ms` and
its siblings sum truncated zeros and under-report by close to 100 %;
`consumer_recv_ms` per message likewise. Same in `relations.rs` and
`stage2.rs`. Accumulate nanoseconds and divide at emit, or follow the
`<category>_wait_ns` convention so `--stalls` sees them.

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

## BUG-010 - Under LocationsOnWays, a way rewrite with no loc map silently strips coordinates

`handle_candidate` reads `loc_map_handle.and_then(|h| h.get().cloned())`. If
that is `None` (barrier protocol broken, or a future routing change),
`rewrite_block_parallel` falls through to `write_base_way_local` and drops
inline coords from every base way in the blob. The `--force` guard in
`rewrite.rs` exists because this silent strip was reachable once; the guard
closes one route and leaves the fallback. With LOW on and a non-node kind, a
missing map should be an error. Enforce with a `LocMode` enum
(`Off` / `Required(&Map)`) instead of `Option<&Map>`.

Reported by: change application.

## BUG-011 - Node coordinate extraction failures are swallowed under LocationsOnWays

`streaming.rs::handle_candidate` calls `extract_node_tuples(...).is_ok()`. On
`Err`, which the comment says happens for non-dense `Node` messages, the whole
blob's coordinates are dropped with no log. The fallout is a later
"cannot write LocationsOnWays: N referenced node(s) could not be resolved",
naming no blob, node or cause, and pointing at `--ignore-missing-nodes`, which
then writes a silent `(0,0)`. The comment claims parity with
`node_locations.rs` prefill semantics; that prefill no longer exists. Fix:
handle plain `Node` groups, or fail with blob seq and offset. Pin with a test
on a non-dense node blob under LOW.

Reported by: change application.

## BUG-012 - `loc_node_blobs_scanned` counts every blob, not node extractions

`rewrite.rs` sets `final_stats.loc_node_blobs_scanned =
worker_counters.blobs_processed`, which `handle_candidate` and
`handle_owned_passthrough` bump for every candidate and owned passthrough of
every kind. The stats doc says "Passthrough node blobs decompressed for
coordinate extraction". Use a dedicated counter on the extraction branch and
assert it in an existing LOW CLI test.

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

## BUG-016 - `merge-changes --allow-version-regression` is a silent no-op without `--simplify`

The clap arg has no `requires = "simplify"`, and the flag reaches only
`write_simplified`; the streaming path never checks versions. The help text
mentions `--simplify` but does not stop the combination. Fix with clap
`requires`, and a test expecting a usage error.

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

## BUG-018 - `BlobOrderGuard` reports the wrong ID for negative IDs

`merge_join.rs::BlobOrderGuard::observe` formats `prev_last.1` as an ID, but
`prev_last` is an `osm_id_key`, whose `.1` for a negative ID is
`id.saturating_neg()`. A trip after blob `-5..-3` says "already reaching id
5". The drain's twin guard (`check_output_order`) stores the real ID beside
the key for exactly this reason. The two guards are one rule (HYG-025);
merging them fixes this.

Reported by: change application.

## BUG-019 - Negative timestamps become year 584 billion in OSC output

`osc/write.rs::push_info_attrs` does
`format_epoch_secs((milli_timestamp / 1000).cast_unsigned())`. A pre-1970 or
corrupt negative timestamp formats as a nonsense date with no error. The
parse side (`parse_rfc3339_utc`) refuses unparsable timestamps, so the two
ends of the round trip disagree on validity. `cast_sign_loss` is denied but
`cast_unsigned()` is a method, so the lint does not fire; a
`disallowed-methods` entry for `i64::cast_unsigned` outside a vetted helper
would catch it.

Reported by: change application.

## BUG-020 - Apply-changes discards or escalates the real error when the drain fails

Comments at three sites (`rewrite.rs`, `drain.rs`, `streaming.rs`) accept a
"double error": the drain reports "Producer dropped a seq" first and the real
cause surfaces at the scope join. But `drain::run_drain(...)?` returns early
out of the scope closure, so the explicit `scanner_handle.join()` and
`workers_handle.join()` never run; `std::thread::scope` joins implicitly. A
scanner or worker that returned `Err` has its error dropped. A thread that
panicked makes `scope` itself panic after the join, and the worker pool's
inner scope turns any worker panic into exactly that. So the documented
"both errors surface, real one second" is wrong both ways: the cause is
discarded or becomes a process panic.

Fix: run the drain, always join both, then pick the root cause through the
shared first-error slot the comment calls unneeded. The existing
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

## BUG-024 - Input-controlled IDs drive unbounded `IdSet` growth or panics

`IdSet::set` grows `chunks` to `id >> 25` entries. Sparse altw pass 0
(`collect_way_referenced_node_ids`) and the relation-member scans
`referenced.set(id)` every non-negative ref, so one ref near `i64::MAX`
allocates about 2^38 slots (around 2 TB of vector) and aborts. External is
protected by `BucketLayout::locate` bounding IDs by indexdata `max_id`;
sparse has no bound. A plausible but large ID still allocates a full 4 MB
chunk per distinct 2^25 range, so memory is bounded by ID spread, not element
count.

Related panics on the same type:
- `IdSet::rank` indexes `chunk_prefix[cid]` unchecked, so it panics for any ID
  past the last chunk or any negative ID cast to `u64`; `count_below` guards
  this, `rank` (which is `pub`) does not.
- `IdSet::set_atomic` on a negative ID panics with a message blaming
  `pre_allocate`, because `-1 as u64` lands in chunk 2^39.
- Geocode pass 1.5: with `--force` on input whose node blobs lack indexdata,
  `max_node_id` is under-reported (0 if none has indexdata), and the
  comment in `run_pass1_5` says `set_atomic` "would panic via its
  diagnostic path". A node ID above a lying or stale indexdata `max_id` does
  the same. The negative-ID guard was added; the too-large guard was not. The
  panic takes out a worker inside `thread::scope` and propagates out of the
  library.

Fix: an `IdSet` constructor taking a bound plus a fallible `try_set`, as
`set_atomic_checked` already does; the untrusted-input callers get a set that
cannot grow past its bound. For geocode, check `r <= max_node_id` and record
an error, or refuse `--force` without indexdata for that command.

Reported by: altw and scanning, geocode index, re-encoding.

## BUG-025 - `BucketLayout::new` panics on a large indexdata `max_id`

`BucketLayout::new` asserts `bucket_width <= 1 << 30`. `max_node_id` comes
from indexdata via `max_node_id_from_blob_meta`, which validates sign and
order but not size, so a node blob advertising `max_id` above about 2.7e11
(256 x 2^30) panics the process instead of returning an error. Return a
`Result`.

Reported by: altw and scanning.

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

## BUG-033 - Zero-padding a 26-byte indexdata can manufacture a real Null Island bbox

`WireBlobHeader::parse` and the frame path copy a 26-byte (v1) indexdata into
a zeroed 42-byte buffer. `BlobIndex::deserialize` trusts the version byte,
not the original length: a 26-byte field whose version byte says v3 and kind
Node reads bbox words `[0, 0, 0, 0]`, which under v3 is a real box at (0, 0)
(`BBOX_ABSENT` is the inverted box). A spatial filter would then skip that
blob for any region not containing (0, 0). No conforming writer produces this,
but the padding undoes the reason v3 exists. `deserialize` also accepts
`min_id > max_id`, which makes every range-based skip wrong in the skip
direction.

Related: the accepted-length check is `len == INDEX_SIZE || len == 26`, with
"expected 26 or 42" in the warning, while `INDEX_SIZE_V1 = 26` exists in
`blob_meta` but is private. Fix: carry the original length
(`RawIndex { bytes, len }`, constructed only in `blob_meta`), reject a v2/v3
version byte on a 26-byte field, reject inverted ranges. Unit-testable.

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

## BUG-036 - A truncated tag index from `blob_meta` makes tag filters skip blobs

`blob_meta/tag_index.rs` and `scan_ids.rs` loop on
`read_tag().ok().flatten()` and end on the first bad tag, returning `None` or
a truncated result. There is no upfront validation in `blob_meta`. A truncated
tag index written by the write path is an under-claiming index, which a tag
filter then uses to skip blobs: a wrong answer, not a degradation. A parse
failure while building an index must be an error, never a short index. See
HYG-034 for the general decode-to-default pattern.

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
report every region twice under two names. Falls out of HYG-019; short of
that, carry a display name in `ExtractSlot`. Testable via a `CliInvoker`
stderr assertion.

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

## BUG-043 - The tags-filter expressions file strips tabs, contradicting the osmium parity claim

`tag_expr::read_expressions_file` uses `trim()`, which strips tabs, while
`tag_expr::strip_spaces` documents that osmium strips spaces only and "a tab
stays part of the key there and stays part of the key here". A tab-led key
from an expressions file is stripped. Separately, the two "one item per line
with comments" readers disagree: `getid::parse_ids_from_file_with_default_type`
treats `#` only at line start, `read_expressions_file` strips inline `#`
anywhere. Fix: one `read_list_file` helper with the comment rule decided once,
and `trim_matches(' ')` plus `\r` only in the expressions path. Testable
against osmium in `brokkr verify tags-filter`.

Reported by: filtering and spatial.

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

## BUG-047 - `CoordMmapShared` is a data race on input with a duplicated node

The `unsafe impl Sync` and `write_coord` contract require that no two
concurrent calls pass the same rank, justified by "sorted PBF guaranteeing
every node ID appears in at most one blob". Nothing checks the input is sorted
or deduplicated: `require_indexdata` checks indexdata, not
`Sort.Type_then_ID`, and `--force` skips even that. A PBF with a node
duplicated across two blobs (naive concatenation, un-deduped merge) makes two
workers write one slot concurrently, which is UB. `write_coord`'s bounds check
is `debug_assert!` only, so in release an out-of-range rank is an
out-of-bounds write.

Fix: `AtomicU64` slots with relaxed stores (same codegen on x86-64, no UB, no
`unsafe impl`), or check `is_sorted()` at entry and refuse; make the bounds
check a real check.

Reported by: geocode index.

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

## BUG-055 - `osm_id_key(i64::MIN)` collides with `-i64::MAX`

`saturating_neg` maps `i64::MIN` to `(1, i64::MAX)`, the key of
`-i64::MAX`. Two distinct IDs compare equal in canonical order, so dedupe or
merge-join over a crafted file collapses them. The agreement test uses
`i64::MIN + 1` and steps around it. Enforce with a proptest over full `i64`
asserting injectivity.

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
decode before clearing, and the brokkr textlint rule
`static-test-hooks-only-in-fault-targets` refuses a `*_test_hooks` import
outside `tests/fault_*.rs`. What remains: nothing checks that each fault
target holds a single `#[test]`, so a second test added to one would share
the armed static with the first. HYG-062 covers removing the globals outright.

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

## BUG-066 - `PbfWriter`'s rayon framing tasks can abort the process or deadlock `flush()`

The write pools now join every thread they start before returning (the
parallel_writer detach is fixed), but `writer.rs` hands blob framing to three
`rayon::spawn` sites that are not joined. They never touch the output file, so
they cannot corrupt it, but: a panic inside a framing task goes to rayon's panic
handler, which aborts the process by default; and a normal `flush()` can
deadlock on a single-thread rayon pool, because the writer thread's channel only
closes once the framing tasks finish and those tasks cannot run while the pool's
one thread is blocked in `flush()`. Waiting for the tasks from `flush` or `Drop`
is not the fix - it deadlocks the same way when the `PbfWriter` is driven from
inside rayon. Needs cooperative scheduling or structured execution of framing
work. Relevant to library use (bz pbfhogg#1): a caller choosing its own rayon
pool size can hit it.

Also in the write path: `uring_writer`'s `drain_best_effort` gives up after a
fixed number of error rounds, and each `submit_and_wait(1)` can block
indefinitely, so its shutdown is not guaranteed clean.

Reported by: write path.

## BUG-067 - `eprintln!` panics when stderr's reader goes away

`eprintln!` panics on a write error, so `pbfhogg sort ... 2>&1 | head` exits 101
once `head` closes the pipe. The stdout half of this class is fixed (the CLI
writes through `cli/src/output.rs`, where EPIPE becomes a silent exit 0); the
library still calls `eprintln!` from about 40 files (see HYG-019). The audit of
the library's stderr use for library consumers is tracked in bz as pbfhogg#1.

Reported by: CLI and plumbing.
