# Audit of the "Review once drained" list

Started 2026-10-26. The reading review of `notes/hygiene.md`'s "Review once
drained" list, done by six read-only agents, one per slice of the list,
before the measurement campaign. Each section below is that agent's report,
verbatim and unfiltered. Line ranges refer to `notes/hygiene.md` as of
commit `060c2f74`. Findings by the orchestrating session, where it checked
the agents' claims, are kept apart at the end.

Slices:

- A: lines 81-198 (preamble, first drain round, through diff's
  path-independent input checks)
- B: lines 199-422 (O_DIRECT alignment through the sequential diff and
  derive `DecompressPool`)
- C: lines 423-696 (indexdata trust model through the OSC arena panics)
- D: lines 697-888 (input pinning and its per-command adoptions)
- E: lines 889-1104 (`BlockBuilder` poisoning through the OSC gzip writer)
- F: lines 1105-1273 (`expect_used` through the merge engine)

---

## Slice A

I've reviewed all 20 items in lines 81-198. One is a defect, none needs reverting, three need your ruling, three need measurement, and the rest are fine. Two items describe code that `2c12a955` later replaced. Everything was read-only: `git log -S` / `show` plus reading the current code, no cargo or brokkr.

### Verdict per item

**1. altw external refuses an overstated `max_id`** (`1db71d8a`). This does reverse the tolerance `2fabb026` built into stage 2 on purpose. The reason you kept it is in `scan_blob_metadata`'s doc and matches stage 2's code. Cost is one `OrderWatermark` compare per blob in a header-only scan. **Fine as is.**

**2. Node blobs whose range holds negative IDs are refused** (`1db71d8a`). The refusal itself is correct. Before, a wholly negative node blob was already refused (in `max_node_id_from_blob_meta`). A mixed-sign one passed and silently broke stage 2: `(b.min_id as u64)` wraps, so `partition_point` runs over a slice that is no longer partitioned and present nodes come out as missing locations. No cost. But it contradicts a binding record:
- ADR-0002 still says altw external is "silent inheritance" and that no fourth hard-reject site is planned. Its 2026-10-25 amendment covers diff only.
- DEVIATIONS.md "Negative input IDs rejected project-wide" still says `add-locations-to-ways` (both backends) is silent, and that the `any_in_range` clamp covers straddling node blobs in stage 4. That clamp is now unreachable for node blobs.
- CHANGELOG's `16e36941` entry claims the ALTW external mixed-sign fix.

**Needs a ruling:** record external as a hard-reject site in ADR-0002 and DEVIATIONS, or fix stage 2 for mixed-sign ranges instead.

**3. `blob_body_within_file` and getparents' duplicate deleted** (`1db71d8a`, `1dd2ea09`). Not reverted to anything deliberate. The walker's `PreadSource::skip_bytes` refuses a payload past the captured size, and every builder walks with it. **Fine.**

**4. `frame_in_scratch` refactor** (`1db71d8a`, then `f9625b94`). The sync path now has the pipelined path's shape: the entry trim sits inside `compress_ns` and an oversize `compress_buf` is freed before the write. Both trims are capacity compares. A free happens only above the 4 MiB retain cap, and it happened per blob before too, just later. The swap-vs-clone history in `frame_blob_into`'s doc is untouched. The only side effect is that sync-path `compress_ns` now includes the trim. **Fine.**

**5. `acquire_permit` before spawn, guard moved into the closure** (`1db71d8a`, now `reserve` / `write_osm_data`). Still the same ordering: take the permit, then copy the bytes (the planet OOM bound). It is strictly safer: an early return or a dropped closure now gives the permit back, where the old code leaked it. No cost. **Fine.**

**6. `EPERM` and `EINVAL` added to the copy fallback** (`1dd2ea09`). You kept this. The extra `fstat` x2 and `fcntl` only run after the kernel refuses a range. On a cross-filesystem run, which already fell back on `EXDEV`, that is three syscalls per range against a bounce copy. **Fine.**

**7. io_uring CopyRange accepts a short pread** (`1dd2ea09`). The strict check came from `fa8251d4` with no stated reason. It was carried over from the linked read+write design, which could not retry; the pread design can. The O_DIRECT alignment is still kept by `submit_current`, which only fires on a full buffer. **Fine.**

**8. New public APIs and `blob_from_offset` after a failed read** (`1dd2ea09`). Both APIs are additive and in the CHANGELOG. The `blob_from_offset` change is a bug fix. No hot-path cost. **Fine** (adding public API is your call).

**9. cat `fd_allowance` and `CAT_EXTRA_FDS`** (`56f8d9a9`). The text is accurate: two `getrlimit`, one `setrlimit` to the hard cap and one `/proc/self/fd` count per run. Plain cat now holds one pin per distinct input for the whole run, which matters only for runs with thousands of inputs. **Fine.** It is already inside a cat passthrough A/B if the campaign runs one.

**10. `reframe_raw_with_index` through `encode_frame_header`** (`f9625b94`). The "argued unreachable" claim holds: the reader refuses the same bound (`datasize >= MAX_BLOB_DATASIZE` in `frame.rs`). The deleted `reframe_raw_with_index_scratch` was an F27 optimisation (`4b924eaa`) that, as far as I traced, no production code has called since `f5050296`. The `HeaderWayMembers` attribution change only runs on the error path. **Fine.**

**11. `CliInvoker::child_limits` deleted** (`9cce24b3`). It was added for the B0 host-identity goldens (`6867a6b2`), whose caller `94453ae6` deliberately removed. Test-only. **Fine.**

**12. Degrade guard also checks `pending`** (`9cce24b3`). The "always empty" argument holds: the `write_single_*_local` helpers flush before adding, so `bb` is non-empty after any write, and the unsort hold never flushes. Cost is one length check per blob. **Fine.**

**13. `max_node_id_from_blob_meta` and `build_node_blob_mapping` lost their checks** (`1dd2ea09`, `1db71d8a`). The only production caller feeds them from `scan_blob_metadata`, and the fold starts at 0. **Fine.**

**14. Diff and derive delete and merge-stats helpers merged** (`1dd2ea09`). The bodies were identical. Counter names are unchanged. Both copies are in one crate built with fat LTO and `codegen-units = 1`. Moot now anyway, since `2c12a955` deleted `derive_parallel.rs`. **Fine.**

**15. Geocode build no longer runs `create_dir_all`** (`c5ff9b13`). `build_generation` is only reached with `OutputDir`'s staging path. Not on a measured path. **Fine.**

**16. `read_to_string` reports non-UTF-8 as `InvalidData`** (`c5ff9b13`). This is a library kind change, already in the CHANGELOG. The CLI exit mapping doesn't depend on that kind, so CLI behaviour is unchanged. **Fine.**

**17. Text diff drivers take an optional writer** (`dfe45a35`). The item text is **partly stale**. `2c12a955` left two drivers, not three: the sharded block-pair engine (with an inline single shard) and the element stream. The optional writer now lives in `DiffSink`. The cost is still one predictable branch per write site, including per common element when `-c` is off.
- **Needs measurement:** `brokkr diff --dataset planet`, text output, matched `--bench 1` pairs of `dfe45a35^` against `dfe45a35`, at the default `-j` and at `-j 1`.
- **Related, for your ruling:** `-q` refuses `-c` and `diff_stats` hard-codes `suppress_common: false`, so a quiet diff never takes the byte-equal skip. `diff -q` on base vs apply-changes output decodes every blob pair, while `diff -c >/dev/null` skips the identical ones. That is deliberate (counts equal an unsuppressed diff's), but it could be a large cost on exactly the brokkr diff case.

**18. `CommandPaths` and the geocode alias refusal** (`58228260`). CLI preflight only. The symlink rule is correct: `remove_dir_all` does not follow links, and the hard-link case is safe. **Fine.**

**19. Diff's input checks made path-independent** (`8881415e`):
- **(a) `next_frame` plus `into_blob`.** Swapping eviction before the envelope parse is cost-neutral: the parse is CPU-only on an owned buffer. But the split's reason is gone: since `2c12a955`, `next_frame`'s only caller is `next` itself. This is a dead seam, not a cost. **Fine**; a `brokkr read` A/B would settle it if anyone wants proof.
- **(b) `OrderWatermark` mixed-sign change.** Correct: indexdata ranges are exact, so `min_id` is present. A few compares per blob. **Fine.**
- **(c) Per-blob order guard plus trailing `HeaderWalker`.** **Stale.** `2c12a955` deleted the sequential block-pair merge, `verify_rest` and `resume_at`. What exists now is an up-front `walk_pair` on every indexed run, including `-j 1`. Retire or rewrite this sub-item. The real `-j 1` cost (a full header walk of both files, pread with SEQUENTIAL advice, `--direct-io` not honoured) belongs with `2c12a955`'s own entry.
- **(d) Unindexed element stream.** The text is accurate: per-element `admit`, `element_kinds` classification, excluded kinds dropped. The old `drain_phase` also decoded every block, so nothing new is decompressed. **Needs measurement:** `brokkr diff --dataset denmark --variant raw` (europe if it shows), with and without `--type way`, `8881415e^` against `8881415e`. `94743e65` touched this path again, so measure it separately.
- **(e) `WireBlobHeader` refuses a wrong-wire-type known field.** Cost is about five compares per header. But the unification picked the stricter side. libosmium's `decode_blob_header` (`research/libosmium/.../pbf_input_format.hpp`) skips any (tag, type) it doesn't expect, and protobuf treats such a field as unknown. So pbfhogg now refuses files osmium reads whenever field 2, 4 or 5 carries a varint; 4 and 5 are pbfhogg's own extensions, which another producer could reuse. There is no DEVIATIONS entry. **Needs a ruling**, in line with the BUG-191 "protobuf's specified decoding" stance: refuse, or skip as unknown the way the reader used to.

### The defect: diff blob-order message mislabels its input
`BlobOrderGuard::observe` in `src/osc/merge_join.rs` checks diff's **input** but its message reads "follows {kind} output already reaching id ...". The wording was copied from apply-changes' output guard. A reword fixes it.

### Other things found on the way
- **`diff_block_pair` ignores `--direct-io`.** It now takes `_direct_io` with a "Not honoured yet" comment: the parameter was kept, but `-j 1` no longer honours it. This is the round-one direct_io pattern again; `2c12a955`'s commit message says it was recorded in hygiene.md for review.
- **Comments in altw stage 2** (`build_node_blob_mapping`'s doc and the `partition_point` comment) still say blob ranges are "non-overlapping". The scan admits equal boundaries, which the partition handles; only the wording is stale.
- **`notes/handoff.md`** changes none of these verdicts. Its "observability is a contract" check passes for the slice: no counter or marker was renamed, and `writer_copy_range_fallback_*` is additive.

---

## Slice B

**Bottom line:** none of the seven entries plus the two standalone items in this slice undid a measured choice, documented contract or deliberately kept plumbing. Nothing needs reverting. Only one item sits on a measured default path (planet `diff --format osc -j 16`), and its change there is cost-neutral by construction. Three things need a ruling, all small. Two entries need their text corrected: item 8 (DecompressPool) and item 9 (shared descriptor).

### 1. O_DIRECT alignment from statx (`ea17fd86`)
- **Deliberate choice undone?** No. The old `PAGE_SIZE = 4096` was a constant with a comment ("universally safe"), not a measured value; the perf docs hold nothing on direct-I/O alignment. At an alignment of 4096 or less, or none reported, buffer alignment, padding and skip arithmetic are unchanged.
- **Accuracy:** the entry is accurate.
  - Direct opens happen only in `FileReader::direct_from`, `DirectWriter::create` and the uring writer. Nothing opens O_DIRECT per worker or per blob; multi-extract costs one `statx` per region writer.
  - The final-fill change saves one syscall. The old misaligned read at EOF still returned 0, because the kernel checks `pos >= i_size` before alignment.
  - The uring pool's drop order is kept by declaring it first.
- **Cost:** none plausible.
- **Verdict:** fine as is.
- **Stale docs:** `FileReader::direct`'s doc still says "page-aligned buffers" (`src/read/file_reader.rs`). `DirectReader::skip`'s doc still says "aligned down to a page boundary ... page-aligned file offsets" (`src/read/direct_reader.rs`). The CLI's `libc = "0.2.186"` (`cli/Cargo.toml`) is below the library's 0.2.189; unification makes it harmless.

### 2. Unindexed diff and derive merge borrowed elements (`94743e65`)
- **Deliberate choice undone?** No. The version-only metadata was BUG-146, a filed bug, and the commit fixes it (removed from `notes/bugs.md`, CHANGELOG entry added).
- **Memory:** the old per-side peak was the block plus its owned conversion buffer (strings per tag), which is larger than the block. The new peak is at most two decoded blocks in total, stashes included, so the entry's memory claim holds.
- **Checked lookups on the parallel derive shards (the measured planet path):** `TagIter` already bounds-checked every index (out of range ended iteration). `CheckedTagIter` adds one `failed` flag branch and a `Result` wrapper, so "cost-neutral" is a fair claim.
- **Perf record:** has no entry for the unindexed path at all.
- **Verdict:** fine as is. If anyone wants a number: a matched `brokkr diff --dataset denmark --variant raw` A/B of HEAD against `--commit 94743e65^`, if brokkr's `diff` takes `--variant raw`. It should come out faster (no string copies).

### 3. Fault injection on the cancellation token (`8aa10c65`)
- **Deliberate choice undone?** The static hooks (`3a664c7b`, `fa94c6c7`) were a test-isolation design, not a performance one. A per-token `FaultPlan` gets the same isolation another way, and a textlint rule bans statics in `test_hooks` modules.
- **Product builds:** bench builds use `linux-direct-io` + `linux-io-uring` without `test-hooks` (`brokkr.toml`), so none of the hook code is compiled there. Every new hook site is `cfg`-gated, e.g. the region-grid hook in `extract/multi.rs`.
  - The `ParallelGzipWriter::new(&Cancel)` change is crate-private (`pub(crate) mod parallel_gzip`).
  - `Output::pbf_writer`'s `if let` is on a constant `None`.
- **Accuracy:** matches the commit, including the two tests that had passed without their hook firing. The removed `MergeOptions::with_panic_at_blob_seq` and `PbfWriter::test_panic_in_framing_at` existed only under `test-hooks`.
- **Verdict:** fine as is.

### 4. `counter_set!`, `record_max`, `Stopwatch` (`9dcf53a7`)
- **Values:** I spot-checked every `/ 1_000_000` divisor against the parent. All of them (stage 1 `s1a_id_shard_write_ms`, stage 3 `tmp_pwrite`, stage 4 readiness and way timers, the `altw_pass2_*` set) already divided accumulated nanoseconds at emission. Per-sample millisecond truncation is kept, which the handoff calls deliberate.
- **Cache lines:** the old code had no `CachePadded` or `align(64)` anywhere, so no padding choice was undone. The stage 4 worker set is bumped once per blob.
- **Verdict:** fine as is.
- **Optional observability check (not performance):** compare `brokkr sidecar <uuid> --counters` name sets from an altw external Denmark run at HEAD and at `--commit 9dcf53a7^`.
- **Public API:** `pbfhogg::debug::emit_counter` is public and its signature changed to `impl CounterValue`. It stays source-compatible for `i64` callers; it is not in the CHANGELOG.

### 5. The rest of the counters (`13dd02aa`)
- **Layout:** the "may change field layout" call is moot. Before and after, `DrainCounters` and `WorkerCounters` are default-repr structs of the same `AtomicU64` fields in the same order.
- **Renumber names:** the old call-site prefixes were exactly `pass1`, `stage2d`, `r1` and `r2d` (`pass1.rs`, `stage2.rs`, `relations.rs`), and the prefix was used only for counter names. The 28 literal names therefore match what was emitted before. The unit test alone could not prove that, because it supplies its own prefixes.
- **Other changes:** the degrade `cast_signed` and the `merge_changes_decompress_ns` saturating add are as described.
- **Verdict:** fine as is.

### 6. Read-metrics emission after join, fsync funnel, `with_sync_observer` (`8384fa29`)
- **Counts:** fsync counts are unchanged on every path: `FileWriter::sync` gives one record, as before. Without `test-hooks`, `with_sync_observer` is an `#[inline(always)] f()`.
- **Values:** `writer_sync_all_ns` shrinks a little on the buffered path (the `EINVAL` fstat now falls outside the timed span), and failed fsyncs now count.
- **Ruling needed (small):** a stage panic now emits no `PIPELINE_METRICS`; before, they were emitted and then the panic re-raised. That is a small observability loss on exactly the crash runs that `brokkr sidecar dirty` exists to diagnose. A drop guard that emits on unwind would restore it.
- **Coverage note:** the fsync-count tests now compile only under `test-hooks`, so the `consumer`, `musl` and `minimal` sweeps no longer run them.
- **Verdict:** fine; the panic-path emission is the one ruling.

### 7. One front read per input (`a3f1a1bc`)
- **Deliberate choice undone?** No. The commit kept the separate full-pass descriptors on purpose and commented them; only repeated precondition probes merged.
- **Descriptors:** `InputFront` drops its descriptor as soon as `indexed()` runs. altw external calls `require_indexdata` before stage 1, so no extra fd is held while external budgets its descriptors (`ca15f441`). The sparse path drops the front before pass 0.
- **Cost:** none. The per-frame `saw_header` bool in inspect is noise.
- **Ruling needed:** `merge_header_read_ms` changed meaning on a measured command. It now times open + header frame + probe to the first data blob + decode, not the header read alone. That is documented in a code comment, but the handoff calls counter units and meaning a contract. The maintainer should either accept the new meaning or put the timer back around the header-only portion.
- **Behaviour changes, all listed in the entry and fine:**
  - A library build without `linux-direct-io` now accepts `direct_io = true` on inspect's index-only path.
  - Auto selection reads the hint from the first data blob.
  - The bench-write and inspect verdicts differ on a malformed first envelope.

### 8. Sequential merges recycle decode buffers (`126cbce2`; pooling for block-pair removed again by `2c12a955`)
- **Deliberate choice undone?** No. The old `decompress_buf` field never recycled anything: `std::mem::take` handed it to `from_vec_with_scratch` every block, so every block was a fresh allocation. The "avoid cross-thread retention" comment on `new_sequential` concerns the pipelined reader; this pool lives on one thread.
- **The entry's text is stale:**
  - Its opening sentence still lists the indexed block-pair merge under `-j 1` / `--verbose`; since `2c12a955`, `BlockPairSide::decode` allocates fresh again, which is what the pre-drain code did.
  - "The block alive while `drain_remaining` decodes the next" no longer applies: `drain_remaining` is block-pair only. The code comment on `MergeBuffers` is already correct ("a current block per side and a stashed one").
  - The last bullet does state the current truth.
- **Nuance for "cannot plausibly allocate more" (relevant to the decode-buffer growth item):**
  - `decompress_wire_blob_into` calls `buf.reserve(est)` on the cleared pooled buffer. When `est` exceeds the pooled capacity, Vec's amortized growth takes `max(2*cap, est)`: it doubles the pooled capacity rather than growing to `est`.
  - The result lands near the fresh path anyway, because a fresh buffer doubles at the inline reserve. Individual buffers can still end up larger or smaller than fresh, and crossing the 4 MiB retain cap gets them dropped and counted in `DECOMPRESS_POOL_METRICS`.
  - Those counters are new for diff and derive runs, which matters only for cross-run counter comparisons.
- **Verdict:** fine as is; correct the entry's text. Only the unindexed path is affected, and it has never been measured.

### 9. Shared walker descriptor versus separate ones (no commit; a standing question)
- **Verdict:** needs measurement, as the entry says. The suggested A/B is right: altw external stage 4 at Europe scale, one variant handing the walker's descriptor to the workers, matched `--bench 1` cells.
- **The entry's evidence is partly wrong:**
  - **Renumber is not on the shared-walker side.** Its schedule walk keeps its own `FADV_RANDOM` descriptor (`renumber/schedule.rs`, `renumber/mod.rs` "opened three times"). What `db49c929` shares is the plain body descriptor across phases, which is a different question.
  - **`InputFront` does not belong in the list either:** it is a precondition probe closed before any pass runs.
- **What to measure:** the split may track access pattern rather than accident. The eight sharing commands are selective classify-style readers using `FADV_RANDOM` plus the measured `WILLNEED` prefetch over the schedule. The separate-descriptor commands keep default readahead for their body reads (altw external stage 4 reads every blob). Measure both shapes, not just one command.
- **Stale comment:** renumber's comment says the walker's advice "must not reach the workers' shared body descriptor". That is stronger than the evidence; `input_front.rs` and `apply_changes/rewrite.rs` both say the cost is unmeasured. Reword it to match.

### What needs a ruling
1. Restore `PIPELINE_METRICS` emission on a stage panic (item 6)?
2. Accept the new meaning of `merge_header_read_ms`, or time the header read alone again (item 7)?
3. Correct the text of items 8 and 9 as described above.

---

## Slice C

I've reviewed all 15 items in my slice. One is wrong: the cat `--clean` rewrite silently renamed sidecar markers, which breaks a contract. A few need a ruling or a measurement, and the rest look fine. Nothing in the slice undid measured plumbing the way the earlier `direct_io` deletion did. Every item text matches what its commit did, with two small caveats noted under the copy-loops and un-gating items.

### Item-by-item

**1. Trust model page and `check --ids` node/relation comparison (`4c254750`)**
- **Undid something deliberate?** No. The code changes are comments, plus one new comparison in `check_type_order` (`src/commands/check/verify_ids.rs`). The old comment said two comparisons were enough "transitively", which is false when a file has no ways. The record-closure argument swap fixes violations that read backwards: the old code printed "way after node" for a node blob found after a way blob. Text and JSON output change, and the CHANGELOG has it.
- **I checked the doc corrections against the code.** `pass1_5.rs` does cap the node set by file size (`file_bounded_max_id`) and refuses a node above the claimed max.
- **The stage 2 order comment is consistent with ADR-0013.** It documents the intra-blob order as believed, not checked.
- **Cost:** none on any measured path.
- **Verdict: fine as is.** Two calls in the item still need your ruling: extending ADR-0013 to `Sort.Type_then_ID`, and whether to add the intra-blob monotonicity pass.

**2. `apply-changes --force` writes a wholly indexed output (`f703b02b`)**
- **Indexed hot path:** the only additions are a `desc.index.is_none()` branch per candidate, a `(&derived, id_range)` match, and `take_copy_range` returning an `Option`. `scan_block_ids_and_tags` runs only on unindexed blobs.
- **Forced path:** an untouched unindexed blob is now reframed from the body the worker has already read and decompressed. Before, it was spliced, which read it a second time. So you trade a second read for holding the frame bytes in the drain channel.
- **The item text is accurate,** including the fake type transition to Node: the old `infer_kind_and_range` returned Node with a reversed range for an empty block.
- **I checked one worry and it is closed.** On `--force` the scanner gives every descriptor a placeholder `Node` kind, so the node-to-way barrier would never close. But `--force` together with `--locations-on-ways` is refused before any input is read (test `merge_refuses_force_with_locations_on_ways_before_any_input`).
- **Verdict: fine as is.** It only affects the forced path, which no production benchmark exercises.

**3. getid accepts partial indexing (`720682a1`)**
- **No getid path decides from indexdata a blob lacks.** The invert walker raw-copies only under `if let Some(ref idx) = meta.index`. The include arms skip only on a present index. The `-I` reader and `--add-referenced` pass 1 work on actual elements.
- **Cost:** one branch at walker open.
- **Verdict: fine as is.** The policy question is in "Needs your ruling" below.

**4. getparents (`a05b3d25`)**
- **No code change:** only a test and doc text. The test checks output bytes, element order and stats.
- **Verdict: fine as is.**

**5. cat `--clean` as one ordered pass (`8da26b23`)**
- **Shape:** each blob is decoded once, `BlockBuilder` is still fresh per blob (the measured choice in handoff.md is kept), and two phase barriers are gone.
- **Memory:** the old path was an unordered pool feeding a `ReorderBuffer`, which could grow without bound behind a slow head blob. The new ordered pool keeps at most 64 results alive (`POOL_REORDER_CAPACITY_WIDE`). Peak anon memory could move either way.
- **Defect - sidecar markers were renamed.** `CAT_NODES_*`, `CAT_WAYS_*` and `CAT_RELATIONS_*` were replaced by one `CAT_FILTERED_START`/`END` pair. That breaks phase alignment with the recorded baseline (`f2315551`, 5m34s, 750 MB) in `sidecar --compare`, and handoff.md treats marker names as a contract. Either restore per-kind markers where they still mean something or record the rename in `reference/performance.md`. The item text does not mention it.
- **CLI break:** `--clean --force` without `--type` is now refused by clap (`cat_indexed_paths` no longer includes `clean`). I couldn't read the brokkr tree to check whether brokkr ever passes that combination.
- **The summary's "Decoded N blobs" now counts input blobs, not output frames.** The CHANGELOG has it.
- **Verdict: needs measurement.** Run a matched, interleaved planet A/B of `cat --clean version` at `8da26b23` against its parent `a05b3d25`, reading wall time, peak anon and the stall counters. Do it before `56a50e0d` moved the pool onto `ordered_pool::run_ordered`, so the two changes don't blur. Then repeat at HEAD.

**6. tags-filter accepts partial indexing (`54e04055`)**
- **The claims check out.** Pass 2's `filter_block_pass2` writes by ID per actual kind, and the closure and way-dependency walks read decoded elements.
- **Cost:** on a wholly indexed file a per-header guard goes away.
- **The item's lateral question has an answer.** tags-count's workers filter by actual kind (`type_filter.admits(kind)` per element), and the tag-key skip applies only when tagdata parses. So accepting a partial file would be safe there; its remaining refusal is not owed under the new rule.
- **Verdict: fine as is.**

**7. time-filter (`063ea4d0`)**
- **The commit's correction of the old premise is accurate:** the history path is index-blind. The only code change is `Refuse` to `Accept` on the snapshot walk.
- **Verdict: fine as is.**

**8. Five copy loops become two (`bd2de78d`)**
- **Same semantics as the old pool loop and pwrite fallback.** EINTR is retried, a short copy counts as progress, the whole remaining range goes to each call, and the decision to fall back is made only before any byte has moved.
- **On cost:** the extra `Option` is on `off_out` once per call (about one call per 2 GiB). The zeroed `DirectReader` buffer is paid once per open. `DirectWriter`'s `write_all` keeps the deliberate misaligned retry.
- **Verdict: fine as is for performance.** One open risk is correctness, not speed: the io_uring `ReadSink` has never run end to end. It needs a platform sweep on a host where io_uring can run (memlock). The "fallback bytes" Note is new user-visible output and is documented.

**9. Splice, SEQUENTIAL and DONTNEED un-gated from the I/O features (`d510bbd7`)**
- **Behaviour change:** brokkr builds with both features, so the measured binary should behave the same. The change lands on default and consumer builds.
- **Old and new paths match.** The `fadvise::advise` clamping is equivalent to the old `try_into` skip. `evict_consumed_prefix` now returns whether it actually advised, and its only caller is `BlobReader`'s eviction hook.
- **A slot-bucket `sync_data` failure now fails the run instead of being dropped.**
- **Item text caveat:** "a whole coalesced run lands on one parallel-writer worker" is also true on Linux, not only off Linux.
- **Verdict: fine as is.**

**10. Geocode format codec (`64141296`)**
- **This path has no measurement record:** `performance.md` says query latency has not been benchmarked.
- **`find_by_key` decodes a whole 32-byte record per probe.** That record sits in the same cache line, so the cost is negligible next to page faults.
- **Verdict: fine as is.**

**11. Geocode builder reads through the codec (`067be2d8`)**
- **The algorithm is the same.** Stage B's `iter().collect()` over `chunks_exact` is trusted-length, so it still allocates once. The decoded `BucketRecord` is padded to 16 bytes, like `ParsedBucketEntry` was.
- **The CSR build now uses a range plus a checked `get`,** a few integer operations against an S2 cell computation per point.
- **`cover_way_segments` numbers segments correctly.**
- **Verdict: fine; low-priority measurement.** If a planet `build-geocode-index` run happens anyway, compare `067be2d8` against `d510bbd7` to cover both geocode commits (planet baseline 347 s).

**12. `validated_info` plus `take_message` (`e28fdf69`)**
- **The panic replaces the old default fallback at the same branch count.**
- **What the refusal actually protects.** `take_message` adds a slot check per element parse; each element is parsed twice (once in validation, once when iterating). It refuses a legal protobuf encoding: a repeated singular message, which protobuf merges. Without `take_message` the panic would still be unreachable, because keep-last never looked at the bad copy. So the refusal does not protect the panic. It only replaces keep-last's silent loss of fields from the earlier copy.
- **Verdict: needs your ruling.** Either refuse this legal-but-unsupported encoding, keep the old keep-last, or implement the merge. The cost is probably below noise; fold it into the read-path A/B below.

**13. Block iterators panic through `validated` (`9c85d37e`)**
- **Branch counts are equal or lower:** the element arms lost their skip loops. The hottest site is `DenseTagIter::next`, with two `validated` calls per tag.
- **Codex's codegen caution stands:** `validated` is generic, `#[inline]` and `#[track_caller]`.
- **Verdict: needs measurement.** Run one matched planet A/B of `brokkr read` plus one tag-heavy re-encoding command (`tags-filter` or `inspect --tags`) at `85c1428c` against `5a9d40e4`. That covers items 12 to 14 in one comparison; bisect only on a delta above 3%.

**14. Re-encoding refuses a bad user or role (`85c1428c`)**
- **No new lookups:** the lookups already ran. What's new is a `Result` per kept element and a push loop over a reused, cleared buffer instead of `extend`. Old and new metadata field mappings match.
- **This does prevent a silently wrong answer** (an empty user or role written out), so it passes your rule if it's free.
- **Public API:** `Info::user` and `DenseNodeInfo::user` now return a wire error for a negative index, not `StringtableIndexOutOfBounds(0)`. That is visible to library users; the CHANGELOG has it.
- **Verdict:** measurement is covered by the A/B above. Your ruling is needed on the item's `cat --clean user` call (refuse a bad user even when it would be dropped).

**15. OSC arena panics (`20d10610`)**
- **`str::get` replaces `from_utf8` on every key, role and user,** which is cheaper. `le_at` costs the same as the old slice conversion.
- **Verdict: fine as is.** Optionally fold it into the planned planet apply-changes runs.

### Needs your ruling
- **Remaining partial-index refusals.** They are altw (way refs, relation members, sparse node index), extract smart, inspect `--nodes` and tags-count. tags-count at least would be safe to accept.
- **Repeated `Info` / `DenseInfo`** (item 12): refuse, keep-last, or merge.
- **`cat --clean user`** refusing a bad user it would drop (item 14).
- **ADR-0013's extension to `Sort.Type_then_ID`,** and whether to add altw external's intra-blob order check (item 1).

### Other things I noticed
- **Two counter meanings changed quietly.** `merge_streaming_blobs_forced_rewrite` now also counts unindexed blobs re-encoded on `--force` (item 2). cat's `blobs_decoded` now counts input blobs. Anyone comparing old sidecar runs should know.
- **Empty roles are still written on two paths.** `parse_str_attr_optional` (`src/osc/xml_parse.rs`) still reads a malformed optional `role` as absent, and `merge-changes` and the tags-filter OSC path then write an empty role. That contradicts the stance items 14 and 15 take (an empty role is a silent wrong answer). It is pre-existing and commented as deliberate. `e.attributes().flatten()` also swallows malformed attributes.
- **One item 14 call site to recheck.** cat's `reencode_unclassifiable` dropped its separate `check_user`. `element_metadata` now covers it, but only when a version is present, which matches the old `kept` guard. It's equivalent; it is just the one place where the old check was explicit.

I edited nothing and ran no cargo or brokkr. The reading needed is in the commits named above, plus `src/commands/cat/mod.rs`, `src/write/copy_range.rs`, `src/read/block.rs`, `src/read/dense.rs`, `src/owned.rs`, `src/osc/interner.rs` and `src/commands/apply_changes/streaming.rs`.

---

## Slice D

I reviewed the input-pinning item (`notes/hygiene.md` lines 697-888) and its adoption commits. Summary: pinning left every open file description where it was. Every derived open comes from a pin taken once per run. No per-blob syscall was added. The extra cost is a few syscalls per pass. I found one real defect, several places where the item's text is wrong or incomplete, and a few contract choices for you to rule on.

### The core: `926d5183`, plus `b8acda7c`

**Cost.** Taking a pin costs one `O_PATH` open held for the run, an `fstat`, and one probe open of `/proc/self/fd/<pin>` with its own `fstat`. Each later open costs an open through procfs, an `fstat` and two `fcntl`.
- I searched every `.open()`, `metadata()` and `pin` call in `src` and `cli/src`. All of them run once per pass or phase; none runs per blob.
- The `test-hooks` hook wrapper compiles away in bench builds: `brokkr.toml` bench features are `linux-direct-io` and `linux-io-uring` only.
- `ErrorKind` is `#[non_exhaustive]`, so adding `InputReplaced` breaks nothing.

**Descriptions.** A procfs reopen creates a new open file description, so `FADV_RANDOM`/`SEQUENTIAL` and readahead state stay per description, as before. `WILLNEED`/`DONTNEED` work on the page cache, so they are unaffected.

**Doc softening.** Comments in `input_front.rs` and apply-changes used to say the descriptions "must not share one". They now say that whether sharing would cost anything "is unmeasured". The performance record (`reference/performance-history.md`, on the HeaderWalker swap to `FADV_RANDOM` and the `WILLNEED` prefetch) does back the reasoning for separate descriptions. The new wording reads as an invitation to merge them.

**The item's text is wrong** where it says "(block devices pass)". `b8acda7c` refuses block devices: their `st_size` is 0, which the walker would read as an empty file. The CHANGELOG, `reference/pipeline.md` and `src/read/input.rs` were all updated; only this item was not.

**Verdict: fine as is.** No measurement is owed for pinning itself; the cost is far below the 2-3% noise floor.

**Edge cases, not verified:** every derived open goes through `O_NONBLOCK` before it is cleared.
- On a file holding a lease, the open returns EWOULDBLOCK instead of waiting for the lease to break.
- Some FUSE filesystems record open flags at open time.

### Per sub-item

**apply-changes (`11189dd4`)**
- All four descriptions are kept: front read, splice source, walker, pool.
- The splice source is now opened only on the splice route. Under a `--direct-io` output it used to be opened and dropped.
- Text accurate. **Fine as is.**

**add-locations-to-ways (`36674a0f`)**
- Descriptions unchanged. The external route goes through `external_join_in` with the same pin, so it is not pinned twice.
- The external route holds one more descriptor; stage 1's fd allowance is computed from a snapshot of open descriptors, so it already counts the pin.
- Text accurate. **Fine as is.**

**sort (`b11909e9`)**
- The swap from `require_indexdata` to `InputFront` adds one read of a few hundred bytes. Negligible.
- **The item is stale:** "one per overlap-run worker" no longer exists. `79d2e923` (the blob-merge engine, outside this slice) replaced those opens. Sort now opens three times: the front read, the walker in `blob_merge.rs`, and one pass-2 `InputFile`. Overlap sweeps now share that description, which is `79d2e923`'s change, not pinning's.
- **Fine as is**, but the item text needs correcting.

**tags-filter (`0f64d8f4`): defect.** The commit message says: "The unused direct_io parameters of the closure and way-dependency helpers are gone." That drops `_direct_io` from `collect_relation_member_closure` and `collect_way_node_dependencies`. This is exactly the mistake the preamble describes. The rule already existed when the commit landed: `8192aa07` (2026-10-24) restored the diff parameter and recorded why, and `0f64d8f4` is from 2026-10-25. `notes/todo.md` now says "do NOT remove the ignored `direct_io` parameters".
- The item says nothing about it, and claims "descriptions unchanged" as if nothing else changed.
- Runtime cost is zero, but restoring the two parameters is cheap and should be done now.
- Separately, pinning in every mode makes `-R` with `--invert-match` or `--distrust-metadata` refuse a FIFO where it used to read one. That is a ruling, below.

**getid (`37a7b777`) and getparents (`dff514b2`)**
- Descriptions unchanged; `parse_ids_from_pbf` kept its `_direct_io`.
- The CLI still reserves the output first, then pins the input, then pins `-I` in `resolve`.
- A FIFO `-I` could never have worked: the `-I` read goes through a header walker.
- Text accurate. **Fine as is.**

**renumber (`85949bff`)**
- The deliberate shared description is preserved: all data phases still read one `pin.open()`. The header read (`O_DIRECT` under direct reads) and the `FADV_RANDOM` schedule walk keep their own descriptions. The file length now comes from an `fstat` of the pin.
- The edit to `reference/performance.md` ("data phases (the header read and the schedule walk have their own)") matches the code before and after the change.
- **Fine as is.**

**tags-count (`58248f19`), repack (`3cfb380c`), time-filter (`3a3da612`)**
- Descriptions unchanged and the text is accurate. Repack's failed stat now fails the run instead of falling back to an unbounded ceiling.
- **Fine as is**, apart from time-filter's history route refusing a FIFO or `/dev/stdin` even though it reads once (a ruling, below).

**inspect (`a57c490f`)**
- Descriptions unchanged.
- `--get` moved from `ElementReader::open` to `open_in(host)`. Under the opt-in `Evict` policy that adds a `DONTNEED` over the header prefix.
- The CLI's added cancellation checks are sound.
- **Fine as is.**

**digest (`c0f67d9e`)**
- The `FADV_RANDOM` envelope walk is gone, merged into the decoding pass. That is fewer reads and no `Vec` of indexes, so it should be faster.
- The change touches the shared fused engine (ADR-0009). I read it as cost-neutral:
  - `BlockOnly::apply` is inlined and monomorphized.
  - The deferred `decode` closure runs the same scratch code, and the scratch-capacity counter is still recorded at the same point.
  - The transform now runs after the thread-local scratch borrow ends, which costs nothing.
- **Unlisted observability change:** the `DIGEST_ENVELOPE_START`/`_END` markers were removed. The phase no longer exists, but per the handoff note markers are a contract. I could not check whether brokkr reads them, because reading `~/Programs/brokkr` was denied.
- **Needs measurement, low priority.** One matched, interleaved `--bench 1` A/B of `c0f67d9e^` against `c0f67d9e` on a fused consumer, either `getid` (the pass-2 route) or `tags-filter -R` on planet. Expect it inside noise.

### Missing from the item

Eight adoption commits have no entry in the review list:
- `d8494ff8` extract and multi-extract
- `56f8d9a9` cat and cat --dedupe
- `9dd4516e` build-geocode-index
- `f3258b7e` diff
- `5c9d66ed` derive-changes
- `d9b3943c` degrade
- `2529f73f` check
- `b8acda7c` the block-device refusal

Reading them, I found descriptions preserved: diff kept its `_direct_io`, and extract's three opens per run are still one per call. Two changes do belong on the list:
- **cat:** plain cat now holds one pin per distinct input for the whole run and checks the fd allowance first. A run with more inputs than the allowance (counted as the inputs plus a fixed `CAT_EXTRA_FDS` extra) is refused with `InvalidArgument`. It used to hold one input at a time. The CLI raises its soft limit, but a library caller under a soft limit of 1024 cannot cat around 1000 files any more. This needs a ruling.
- **degrade passthrough:** its two sequential opens became one. That is harmless and saves re-reading the header frame.

The check commit's CLI env-var replacement hooks are inside `#[cfg(feature = "test-hooks")]`, so they are not in product builds.

### For you to rule on

1. **Streams refused on single-read routes:** the time-filter history route, tags-filter `-R` with invert or distrust, and any two-check `check` run. These used to read a FIFO or `/dev/stdin` and now refuse it.
2. **Block devices refused** on every pinning command. `DirectReader` deliberately supports them (its `read_limit` doc), and a single-read route could read one. The refusal message also says "this command reads its input more than once", which is misleading on those routes.
3. **cat's fd admission** for library callers (above).
4. **API consistency:** about a dozen entries now take `impl Into<InputSource>`, while `sort`, `tags_filter`, `add_locations_to_ways`, `extract`, `cat` and `merge` still take `&Path`. Callers passing `&&Path`, `&Box<Path>` or `&Cow<Path>` lose deref coercion. The CHANGELOG records this, which is acceptable for 0.6.0, but the two styles are now mixed.

### Smaller notes
- The scheduled header walks are passed to classify pools that preread through the walker's own `FADV_RANDOM` description. That predates pinning and relies on the measured `WILLNEED` prefetch. I mention it only because the softened comments cite it as a reason sharing might be fine.

---

## Slice E

I've audited all 11 items in lines 889-1104. One is a likely defect against your own ruling (item 3). Two dropped a `--direct-io` path or other plumbing. Five item texts are wrong or incomplete about what the commit did, and the rest look cost-neutral.

I edited nothing. One slip: a `git show --output=` call created an empty file at `research/.audit_pipeline_old.rs`; I deleted it straight away, so the tree is as I found it. The cost estimates below come from reading the code and std/protohoggr internals. I compiled and measured nothing.

### Measurement priority (likeliest planet impact first)

1. **sort / dedupe engine** (`79d2e923`, follow-up `060c2f74`) - item 5
2. **`packed_varint_count`** (`e61e0ad7`) - item 3. Also needs your ruling.
3. **diff/derive one engine** (`2c12a955`) - item 6
4. **Raw-frame and BlobReader reads under `Frames`** (`307982d7`) - item 7
5. **Raw node/way scanners** (`e61e0ad7`) - item 2
6. **Single `run_pipeline`** (`dd312160`) - item 4
7. **Writer cluster** (`ae646053`, `62046948`, `7abba0df`, `52690f1e`) - items 8-11. One A/B of `52690f1e` against `ae646053^` covers all four; bisect only if it moves.
8. **BlockBuilder refusal** (`ce6f20d9`) - item 1

Cheaper way to run it: one `brokkr read --dataset planet` A/B of `ce6f20d9^` against `307982d7` covers items 1-4 and 7 on the read side. Bisect only if it moves.

### Per item

**1. BlockBuilder asserts become a cold refusal** (`ce6f20d9`)
- **Deliberate undone:** no.
- **Cost:** the same predicted branch as the old `assert!`; the refusal is `#[cold] #[inline(never)]` where the panic used to be.
- **Text:** accurate.
- **Verdict:** fine as is. One residual risk: the "no block after a refusal" guarantee depends on every caller ending with a `take`. A poisoned builder that is dropped without one loses the element silently. A debug assert in `Drop` would make that structural.

**2. Raw node and way scanners read fallibly** (`e61e0ad7`)
- **Deliberate undone:** yes, in a sense. protohoggr's `PackedIter::next` documents that swallowing errors is on purpose: "A fallible iterator would add per-element overhead for a case that cannot happen with valid wire data."
- **Cost:** the new loop has fewer branches than the old one: one emptiness check plus three inlined `read_varint` calls, against three emptiness checks before. The error constructors are out of line.
- **Text:** accurate.
- **Verdict:** needs measurement, low priority: `add-locations-to-ways --dataset planet`, sparse and external, `e61e0ad7` against its parent. That A/B also includes item 3, so it can't isolate this one.

**3. `packed_varint_count` gained a per-byte state machine** (`e61e0ad7`)
- **Deliberate undone:** yes, two ways.
  - The function runs on every `PrimitiveBlock` construction, over dense `id`/`lat`/`lon`, all five denseinfo arrays plus `visible`, way refs/lat/lon and relation arrays.
  - BUG-191's width check was rejected (`bc38ad55`) because it "put a per-byte state machine on the per-node dense metadata arrays of every block to refuse only malformed input". `validate_dense_info_arrays`' doc and `reference/truncation-handling.md` now say exactly that. This change adds a per-byte position counter to those same arrays, to refuse only a malformed ten-byte varint.
- **Cost:** the old loop (count bytes below 0x80, remember the last continuation bit) is very likely auto-vectorizable. The new one has an early `return Err` and a loop-carried `pos` recurrence, so it can't vectorize. Rough estimate: 2-5 % on decode-bound planet commands, more per byte on dense data.
- **Text:** misses the vectorization loss and the BUG-191 conflict.
- **Verdict:** needs your ruling first. It does prevent a silently wrong answer (one fewer node, or ids paired with the wrong coordinates in the columnar decoder), but only for a writer bug. Then measure: `brokkr read --dataset planet --bench 1`, interleaved `e61e0ad7` against `e61e0ad7^`, plus one decode-bound command such as `cat --clean` on europe. If it costs, the cheaper route keeps the vectorizable terminal-byte count and runs the slow scan only when a long run of continuation bytes exists.

**4. One `run_pipeline` engine** (`dd312160`)
- **Deliberate undone:** no. I checked the plain engine body before and after: it is unchanged; the fused copy was dropped. The plain path gains one duplicate blob-type compare per blob.
- **Text:** accurate.
- **Verdict:** fine as is. Ride along with the getid/getparents planet runs.

**5. sort and `cat --dedupe` on one engine** (`79d2e923`)
- **Deliberate undone:** yes, the `--direct-io` plumbing.
  - Old dedupe pass 1 was `FileReader::open(pin, direct_io)`. Now `opts.direct_reads` reaches only `require_indexdata`, so `cat --dedupe --direct-io` silently ignores the flag on its index pass. The item text doesn't mention this; it's the same mistake the preamble describes, and it conflicts with the "no-op flags refused" rule.
  - `SORT_OVERLAP_PARALLEL_START`/`_END` are gone. That's an observability-contract question, though the phase itself no longer exists and performance-history records the change.
- **Cost:**
  - Pass 1 now uses `HeaderWalker` - sort's measured trade: a planet win, a dense-europe pass-1 regression.
  - Sweep workers are dedicated threads sized like the decode pools. Sort used to run its overlap runs on the host executor, and PbfWriter compression still runs there, so dedupe can now oversubscribe the cores.
  - All pass-2 workers pread one shared descriptor per input with no advice, so readahead state is shared across workers. Old sort opened one descriptor per run; old dedupe read serially.
  - Heaps: one per worker, plus `window x 4` re-encoded blocks.
- **Text:** otherwise accurate.
- **Verdict:** needs measurement, and needs your ruling on dedupe `--direct-io`.
  - Cells: `cat --dedupe` on denmark and japan, interleaved against `79d2e923^`; `sort --dataset planet` against its parent (expect no change); sort on a `degrade --unsort` snapshot; then one planet dedupe cell, watching peak RSS.
  - Before spending 133 minutes on that cell: the merge engine has no markers inside `MERGEPBF_START/END` and no `*_wait_ns` counters. Add a pass-1/pass-2 split and a write-loop run-receive wait counter first.
  - `060c2f74` also adds a tag scan per unindexed blob in pass 1. That matters for sort on raw inputs.

**6. diff and derive on one block-pair engine** (`2c12a955`)
- **Deliberate undone:**
  - `-j 1` lost `--direct-io` (`_direct_io` is now unused at every shard count, after `8192aa07` put that parameter back on purpose), page-cache eviction, and `MergeBuffers` recycling. The item text says so.
  - Verbose diff now shards, so its output is buffered in scratch files and copied, where it used to stream straight to the output.
- **Cost:** neutral or better on the default sharded path.
  - Per-element window compares are gone (one clip walk per boundary block instead).
  - The byte-equal probe costs three integer compares per fresh overlapping pair.
  - The decode count is unchanged.
- **Text, two errors:**
  - "computes each head's ID once per step (the old copies twice)" is wrong. `element_merge_pair` still calls `element_id` twice per head per step, through the `in_range` filter and then `.map(element_id)`, the same as before.
  - It misses a behaviour change: single-sided runs now add the blob's claimed `index.count` (`DiffSink`: `stats.deleted += run.count`) while the text path prints the actual elements. The summary, and a quiet run's counts, follow wrong indexdata, where the old shard copies counted each element. Under ADR-0013 that needs your ruling.
- **Verdict:** needs measurement: `diff-snapshots --dataset planet --from base --to 20260411` in both formats, `2c12a955` against its parent; `diff` on planet (should show the byte-equal win); one `-j 1` cell on europe. Needs your ruling on `-j 1 --direct-io` and on claimed counts.

**7. One frame decoder** (`307982d7`)
- **Deliberate undone:** no.
- **Text, wrong on one point:** the raw-frame walks have not dropped their zero-fill. `FileReader` implements only `read`, so std's default `read_buf` zero-initialises each chunk. An unsized `Take::read_to_end` also starts at 8 KiB and doubles, so each 1.8 MB planet payload takes about 4 read calls instead of 1-2, and ~250-450 KiB of it passes through the BufReader as an extra copy. That's about 25 us per blob - a few seconds of reader-thread time on cat passthrough at planet, probably hidden if it is I/O-bound. The `HeaderWalker` header copy and the front-probe cancellation checks are negligible.
- **Verdict:** needs measurement, medium-low: plain `brokkr cat --dataset planet`, `307982d7` against its parent. If it costs, implement `read_buf` on `FileReader`, or go back to `resize` + `read_exact`.

**8. PbfWriter per-block dispatch** (`ae646053`)
- **Deliberate undone:** no.
- **Cost:** one fewer copy per owned block. Every caller already owned the block, so nothing lost a reused scratch buffer. The closure box is a few words bigger.
- **Text:** accurate; calling it "the bisect candidate for any write-side delta" overstates the risk.
- **Verdict:** fine as is.

**9. Writer counters given one meaning** (`62046948`)
- **Cost:** one stopwatch and a few relaxed atomics per chunk or raw write. Negligible.
- **Deliberate undone:** retiring `writer_write_ns` is against the "observability is a contract" rule, though `reference/performance.md` documents it.
- **Text:** accurate.
- **Verdict:** your ruling on the rename only.

**10. One ordered-delivery loop** (`7abba0df`)
- **Deliberate undone:** no. Every backend already flattened with `into_vec` before this commit.
- **Text:** accurate.
- **Verdict:** fine as is. The io_uring sink still needs a run on a host with enough `RLIMIT_MEMLOCK` - that's a correctness check, not a speed one.

**11. OSC gzip writer reorder buffer** (`52690f1e`)
- **Deliberate undone:** no. I checked the window proof: every sequence number, barriers included, takes a permit, and the permit comes back only after the head has advanced. In-window sequence numbers can't be falsely refused.
- **Cost:** `push` is a wrapper over `try_push`; the success path is unchanged.
- **Text:** accurate.
- **Verdict:** fine as is.

### Other findings outside the items
- `notes/todo.md` "Raw-frame passthrough errors name neither file nor blob" is stale: `raw_frame.rs` and dedupe's byte-level reader are both gone.
- The comment in `BlobReader::next_frame` ("no reallocation and no zero-fill") is false for `BlobReader<FileReader>`, the reader every command uses. Adding `read_buf` to `FileReader` would remove a memset of every payload on every BlobReader path. That's an older cost, worth measuring separately.
- The `FramedBlobParts` doc says the parts stay split so a backend can `writev` them; no backend does, and all of them concatenate the blob first.

Files most relevant to the open questions:
- `src/read/block.rs`
- `src/commands/blob_merge.rs`
- `src/commands/cat/dedupe.rs`
- `src/osc/merge_join.rs`
- `src/commands/diff/mod.rs`
- `src/read/frame.rs`
- `src/read/file_reader.rs`
- `src/read/blob.rs`

---

## Slice F

I've reviewed every item in my slice, lines 1105-1273. I found one defect that should be fixed before the measurement campaign: it breaks the counters the campaign depends on. I edited nothing and ran no commands besides read-only git.

### Ranking for the campaign (my slice)

1. **`56a50e0d` ordered-pool unification.** The main measurement target, plus the counter defect below.
2. **Decode-buffer growth (`1f572f14`).** Comment-only; this measures existing behaviour, it doesn't bisect anything. Its own text needs one correction.
3. **`060c2f74` merge engine.** Only the forced-unindexed `sort` / `cat --dedupe` path can cost anything.
4. **`ff0b0a37` + `37e6ab1e` + `887fe273` + `9744a9fc`, the apply-changes barrier.** Probably neutral; confirm with a `-l` run on planet.
5. **`003fe717` stall counters.** Cost is near zero by construction; no dedicated measurement.
6. **`11becae7`, `95eea09b`, `00ed5044`, `285df56d`.** Fine as they are; no measurement needed.

### 1. `clippy::expect_used` denied (`11becae7`)

- **Undid anything deliberate?** No.
  - Stage 1's id-shard writers: still the same 256 `BufWriter`s per worker, now built all-or-nothing as a plain `Vec`.
  - Framing compressor caches: the guarded match keeps the same failure behaviour. If `Compressor::new` fails, `?` returns before the cache is replaced, exactly as before.
  - `assemble_rings_search`: I checked it is traversal-identical. The start candidates come from `segments[start_idx].last()`, which equals the ring's end because the ring starts as that segment. `new_end` equals the ring's last point in both append branches, one-point segments included. The exhausted-frame handling only changes the pop order.
  - Degrade's `take_due` matches the old `should_inject_after` plus `take`.
- **Cost?** None plausible. The same work on the same per-blob paths.
- **Item text:** accurate, but it leaves out two restructurings: geocode pass 3's bucket writers (`Option::insert`), and `collect_relation_member_ids` now silently skipping a kind with no set (it used to panic).
- **Verdict: fine as is.**

### 2. Every ordered pool on `src/ordered_pool.rs` (`56a50e0d`)

#### Defect: the result channel is sized to the window, which kills the send-side counters and loosens the memory bound

`run_ordered` creates `sync_channel(window)`. A task is only worked when `task < consumed + window`, so the channel can never hold more than `window` items and a send never blocks. That has three consequences.

**Counters that are now structurally dead:**
- `s4_send_ms`, the `WAIT_S4_SEND` span, the `WAIT_P2_SEND` span and `altw_pass2` send timing can no longer record anything.
- `s4_send_ms` is the measured planet diagnostic: 705 s and 1587 s cumulative in `reference/performance.md` and `notes/altw-external.md`, the evidence behind "planet ALTW streaming is not compression-bound".
- `s4_channel_high_water` can now only reach `window`, never the old 32, so its doc ("reaching `decoded_tx` capacity means...") no longer describes it.
- The backpressure moved into the admission wait. Stage 4 and sparse pass 2 don't record that wait (`Waits.window` is `None`). Neither does any `parallel_classify_phase_ordered` caller: they all pass `Waits::default()`.
- So a fresh planet altw run would show `s4_send_ms` near 0, which reads as a win when the wait has only moved.
- AGENTS.md still calls `WAIT_P2_SEND` / `WAIT_S4_SEND` "real and useful".
- The hygiene item's "sidecar counters keep their names and units" is true of the names only.

**Memory loosened on consumer-bound pools:**
- Before: workers blocked sending into a 32-slot result channel (`POOL_RESULT_CHANNEL_DEPTH`; its doc records the stage 4 256-depth probe: deeper only moves the wait). In flight was roughly 32 + one result per worker + 16 descriptors, about 54 to 70.
- Now: up to `8 x decode_threads` results, which is 176 at 22 threads.
- Where the consumer is the bottleneck, that is about 3x more decoded results alive: stage 4 under zlib:6, sparse pass 2, extract `pread_execute`, multi-extract (each slot holds N regions' blocks, which the `ordered_pool` doc itself flags), repack and degrade.
- In stage 4 this overlaps stage 3's memory-heavy work, so the peaks can coincide.
- The item says the reverse: it presents the window as new bounding where "before only the channel and an effectively unbounded reorder buffer limited it". In practice the 32-slot channel was the binding limit.

**Suggested fix:** size the result channel at `min(window, POOL_RESULT_CHANNEL_DEPTH)`. I believe this cannot deadlock, because the consumer always receives. It restores the measured shape and the send counters. Also wire `Waits.window` (a `*_wait_ns` counter) at every ordered site, so the campaign can see workers parked at the gate.

#### Did it undo something deliberate?

- **The 32-deep result channel**, which was measured: the stage 4 probe. See above.
- **The window value has no measurement anywhere.** "Carried over from renumber's pool, where it was set": that pool was itself drain work (`fa69b667`, 2026-10-22, unmeasured), and its old comment was a design argument. The renumber planet numbers on file (191.1 s, 2026-07-14) predate it.
- **WILLNEED moved from the dispatcher thread to the consumer** (`ClassifyConsumer::on_advance`, one `posix_fadvise` per consumed item).
  - `posix_fadvise(WILLNEED)` allocates pages and can enter reclaim at planet scale, where the page cache is always full. It now runs on the serial writer thread of getparents, tags-filter pass 2, time-filter, repack, degrade, cat `--clean`, geocode pass 2 and multi-extract.
  - The advised edge is also further ahead. Tags-filter pass 2 had no window, so its dispatcher ran about 70 blobs ahead of the consumer; now it's 176 ahead plus 256 MiB.
- **Multi-extract's region writer gained WILLNEED for the first time.** Its old dispatcher had none. The item doesn't mention this, and the record says WILLNEED was a Europe-only win.
- **Hotpath names moved.** getparents, time-filter, repack, degrade and multi-extract's writer now report under `parallel_classify_phase_ordered`. `performance.md` cites "`parallel_classify_phase` 17.66 s of the 22.5 s wall" for getparents.

#### Can it cost wall time or memory?

- **A thundering herd on the gate.** `OrderedGate::advance` takes the mutex and calls `notify_all` on every consumed item.
  - std's futex condvar issues that wake syscall even with no waiters: about 1 us per blob on the serial consumer, in every pool, including producer-bound ones. Negligible on primary planet (about 50k blobs), about 1.5 s on the 1.45M-blob 8k encoding.
  - When the window is full (consumer-bound, which is stage 4's steady state), every advance wakes all parked workers, about 21. Only one becomes admissible.
  - The woken workers contend for the mutex the consumer needs for its next advance, and that consumer is the critical path. They also steal CPU from the writer's compression pool.
  - The old classify-ordered pool used `notify_one` per contiguous run with a single waiting dispatcher.
- **Altw stage 1 parking behind a slow head.** Results there are tiny (`Vec<u32>`), so the window buys no memory. It only adds head-of-line parking behind a worker held up in dirty-page throttling on its 256 x 256 KiB shard writers. Low on primary planet (big blobs), more plausible on 8k encodings.
- **Stage 4 with `--keep-untagged-nodes`.** The passthrough node prefix is now copied only after decode result 0 arrives. With node blobs passthrough, result 0 is way blob 0, which waits for stage 3 to publish its first bucket (about 1 s at planet). The old consumer copied the prefix immediately. That's a small lost overlap; the default (`false`) is unaffected.
- **Fine:** cat `--clean` (64) and geocode pass 2 keep their own windows with the same bound. Renumber is unchanged except for the off-by-one, which is negligible. getparents and tags-filter `-R` at planet are producer-bound, so the window seldom binds.

#### Item-text accuracy

- "Every converted pool now admits at most `default_window`": cat `--clean` and geocode pass 2 keep their own windows and had windows before.
- It doesn't mention that the channel depth went from 32 to the window, which is the actual memory change.
- It omits multi-extract gaining WILLNEED.
- "Counters keep their names and units": the meaning is lost (above).
- The rest (claim workers, the `Interleaved` prefix, the stage 1 flush after the join, failure order, lost latches) matches the code.

#### Verdict

- **Defect** for the channel sizing and the counters.
- **Needs measurement** (top of the slice). Matched A/B, `--commit 60e1b691` (the parent) vs `56a50e0d`, alternating `--bench 1` cells, phase split read first. Order:
  1. `add-locations-to-ways --index-type external` on planet, zlib:6: stage 1 and stage 4 walls, peak anon, `s4_*`. Read the counters with the defect in mind.
  2. `extract --strategy smart` (Europe bbox) on planet: peak anon (the record has the arena-residency history) and wall.
  3. `multi-extract --regions 5` on Japan and Europe: peak RSS.
  4. `repack` on Europe or planet.
  5. tags-filter two-pass on planet, and getparents on planet: WILLNEED on the consumer.
  6. time-filter on planet.

### 3. Decode-buffer growth (`1f572f14`)

- **Undid anything?** No. Comments only, in `read/wire.rs` and `osc/merge_join.rs`.
- **Item text is partly inaccurate.**
  - "Pooled paths pay it on a buffer's first use" doesn't hold for the classify and extract pools. `pool_get` reserves `4 x compressed size` (`DECOMPRESS_EXPANSION_GUESS`), so a first-use buffer is only full when the compression ratio exceeds 4.
  - The 4 MiB retain-cap drop does not need the doubling: any blob over 1 MiB compressed gets a guess past 4 MiB and is dropped on return (`DecompressPool::put`). On primary planet (about 1.6 MB compressed per blob, per cat `--clean`'s doc) the classify pools may retain nothing at all.
  - The "886.6 MB getparents share is probably this growth" attribution is therefore shaky.
- **Verdict: needs measurement**, lower priority. First read the decompress pool's oversize-drop counter (`DECOMPRESS_POOL_METRICS`) on a planet classify command. Then compare a fresh-buffer path (diff shards) before and after a pooling or `reserve_exact` experiment.

### 4. Accumulate pool stop flag (`95eea09b`)

- **Undid anything?** No. One relaxed load per blob. The stop and unwind logic is sound (a stopped worker sends nothing; the dispatcher's send fails once all workers exit).
- The rewritten doc dropped the measured figure "14.59 GB peak RSS (planet)" from the history in the comment. Minor.
- **Verdict: fine as is.** Item text accurate.

### 5. `run_unordered_per_blob` (`00ed5044`)

- **Undid anything?** No. Same dispatcher thread, WILLNEED placement, channel depths, `pool_get` sizing (`raw.len()` equals `data_size`), and worker-thread init.
- **Verdict: fine as is.** With fat LTO and one codegen unit, there is no codegen concern.
- **Item-text nuance:** "every caller of the two functions" is true, but `extract/simple.rs` still has two hand-built `Mutex<Receiver>` unordered pools. Those get none of `003fe717`'s counters, so "the any-order pools were invisible to `--stalls`" remains true for them. performance-history's Worker-pool unification section doesn't mention them.

### 6. Altw reframe shared walk (`285df56d`)

- **Undid anything?** No. The walk is identical. Two clears moved inside the `parse_block_ns` stopwatch (negligible). The context string changed from "reframe block" to "reframe block tag" (noted in the item).
- **Verdict: fine as is.** Text accurate.

### 7. Unordered-pool stall counters (`003fe717`)

- **Cost?** The `try_*` form first, then the blocking form: same atomics on the fast path, clock reads only when blocked. Emitted once per run; the only looping caller is tags-filter's closure convergence, a few iterations. Stage 2's twin counter is fine.
- **Verdict: fine as is.** Text accurate.
- **Notes:**
  - `UNORDERED_POOL_METRICS` is a process-global static in library code. `PIPELINE_METRICS` sets the precedent, but it sits awkwardly with ADR-0016 and mixes totals if two library calls run concurrently.
  - The ordered pools, now the majority of pools, report nothing to `--stalls` (see item 2).

### 8. apply-changes node-to-way barrier (`ff0b0a37`, `37e6ab1e`, `887fe273`, `9744a9fc`)

- **Equivalence holds.**
  - Checking after the contiguous run instead of inside it is safe: the first held-back way has seq equal to `node_end_seq`, so the run can't cross the boundary. On unsorted input, late nodes sit above it.
  - The boundary in the shared 256-slot channel can't deadlock. The drain always receives; the comment at `drain.rs` says it never stops at a byte threshold.
  - The scanner only waits for one free slot.
  - Test hooks are all `#[cfg(feature = "test-hooks")]`.
  - The leftover-coordinates error can't fire on the success path, because every node candidate flushes before sending.
- **Item text omits `9744a9fc`:** `-j 1` and `Parallelism::Fixed(1)` are now accepted, a CLI and library contract change. It's in the CHANGELOG; it isn't on the review list.
- **Verdict: needs measurement**, low priority. `apply-changes -l` on a planet base with LocationsOnWays, A/B `ff0b0a37^` vs `ff0b0a37`. Denmark never exercises `-l` (per the handoff).

### 9. Merge engine (`060c2f74`)

- **Production sort is untouched in its scans:** the claimed-sorted indexed fast path still pushes without scanning. `plan` adds four `Vec<bool>`s per frame and two extra walks; that is O(blobs) and negligible.
- **The memory claim is half right.** Heap tagdata per indexed blob is gone. But the inline size grows: `Frame` (about 96 B including an inline `Option<Reindex>` of about 72 B) plus `BlobEntry` (about 64 B), versus the old `BlobEntry` at about 96 B. That's +60 B per blob, which only matters at 8k-encoding blob counts.
- **Forced unindexed path:** the item covers memory, but pass 1 is serial, and every unindexed blob now pays the combined ID-and-tag scan, so pass-1 wall time grows too. Retained tagdata scales with blob count, up to GBs at 1.45M blobs if key sets are a few KB.
- **Inaccuracy: `claim_holds` is not "the `check --index` rule".** `check --index` raises no violation when a bbox is claimed but the scan produced none (`check/index.rs`, `if let (Some, Some)`). `claim_holds` treats that case as false and reframes. It's rare (only when bbox scaling overflows), but the commit message, the `index_inputs` doc and CHANGELOG all claim equivalence.
- `SortStats` and `MergePbfStats` gain a public field. That breaks struct-literal construction and is listed under "Fixed"; ruling optional.
- **Verdict:**
  - Needs a maintainer ruling on the bbox divergence (pick a rule, or reword the claim).
  - Needs measurement: `sort --force` on an unindexed Europe or planet input, A/B `060c2f74^` vs `060c2f74`, comparing the `SORT_INDEX_BUILD` phase wall and peak RSS.

### Other things noticed on the way

- **Stale docs after `56a50e0d`:**
  - `tunables.rs` `POOL_RESULT_CHANNEL_DEPTH` ("Altw stage 4 records why 32 is right there"): stage 4 no longer uses it.
  - `POOL_REORDER_CAPACITY_WIDE` ("time-filter and altw stage 1's per-way stream"): only cat `--clean` uses it now.
  - `stage4.rs` `channel_high_water` doc and the flush comment still refer to `decoded_tx`.
  - AGENTS.md's `WAIT_P2_SEND` / `WAIT_S4_SEND` sentence.
- **The router comment** in `coord_payloads.rs` ("`notify_all` is cheap... <=6 stage-4 waiters") is out of date: up to `decode_threads` (22) can wait. Pre-existing.
- **Two judgement calls missing from the review list:** multi-extract gaining WILLNEED, and `-j 1` acceptance.

Files most relevant: `src/ordered_pool.rs`, `src/commands/altw/external/stage4.rs`, `src/commands/altw/passthrough.rs`, `src/scan/classify.rs`, `src/tunables.rs`, `src/commands/blob_merge.rs`, `src/commands/check/index.rs`, `src/read/decompress.rs`, `src/commands/apply_changes/drain.rs`, `AGENTS.md`.

---

## Orchestrator's checks

The agents' claims have not been re-verified against the code yet, except
the ones below.

- Slice F, item 9 (`060c2f74`, `claim_holds` versus `check --index`):
  confirmed. `claim_holds` in `src/commands/blob_merge.rs` returns false
  for a claimed bbox with no scanned one, a case `check --index`'s
  `diff_blob` does not report, while the `index_inputs` doc, the
  CHANGELOG entry, `reference/indexdata-trust.md` and
  `reference/cli-reference.md` all call it the `check --index` rule. The
  stricter choice was deliberate (sparred with codex: a box the scan
  cannot confirm is not believed); the defect is the claimed equivalence.
  Either the docs say "the `check --index` rule, stricter in one case",
  or the rule follows `check --index`.

- Slice F, item 2 (`56a50e0d`, the result channel sized to the window):
  confirmed and **resolved** in `c2409c45`, with the lateral stage 4 fix
  below in the commit after it (sparred with codex to consensus, the diff
  reviewed by a fresh codex session).
  - Confirmed in the code: `run_ordered` made `sync_channel(window)`, and
    every pool it replaced had used `POOL_RESULT_CHANNEL_DEPTH` (32) at
    `60e1b691` (stage 4, sparse passthrough, stage 1, extract
    `pread_execute`, both multi-extract pools, tags-filter pass 2,
    classify's ordered phase); renumber's three stages used 32 before
    `fa69b667`, whose own pool, unmeasured, introduced the window-deep
    channel. Confirmed on data: Denmark altw external on bygg at 30 workers,
    parent commit `6592af1f` (`e5e9b3fd`): `s4_send_ms` 10,
    `s4_channel_high_water` 237 of a 240 window; with the fix (forced run,
    not stored): `s4_send_ms` 7,425, high water 62 (32 plus 30 blocked
    senders), `WAIT_S4_SEND` spans present again.
  - Fix: the channel is `min(window, POOL_RESULT_CHANNEL_DEPTH)`. One
    correction to the audit's memory claim: the total bound is still the
    window. The channel restores the consumer-bound shape (about 32 plus one
    result per worker), but a slow head can still fill the reorder buffer up
    to the window, and that backlog stays until consumed.
  - The suggested "wire `Waits.window` at every ordered site" was done as one
    engine-wide gated stall set instead of per-site names, following the
    unordered classify pools' precedent: `ordered_pool_window_wait_ns`,
    `ordered_pool_result_send_wait_ns`, `ordered_pool_result_recv_wait_ns`,
    emitted after every run. Every ordered pool now reports to `--stalls`,
    the classify-ordered callers, stage 1, extract and multi-extract
    included. The per-site legacy timers keep their names and units, and no
    longer read the clock when not configured.
  - Lateral, found while sparring and resolved in the same change: stage 4
    never aborted the router on its own failure. The `stage3.rs` comment
    ("a stage 4 failure aborts the router") and the `AbortOnDrop` doc were
    false; a stage 4 output or input failure left stage 3 running every
    remaining bucket, and the ordered pool's join waited for stage 3 to
    publish the slots its admitted workers were parked on. Now the pool
    calls a new `OrderedConsumer::abandon` before it joins (stage 4 aborts
    the router there), stage 4's consumer callbacks guard panics with
    `AbortOnDrop`, and the stage 4 thread aborts on any other error or
    panic. Three fault tests (`tests/fault_altw_stage4.rs`) hold stage 3's
    first bucket and fail stage 4 at start, in its consumer, and by a
    consumer panic; each was mutation-checked (removing the matching abort
    makes it fail).
  - Docs corrected: `POOL_RESULT_CHANNEL_DEPTH` and
    `POOL_REORDER_CAPACITY_WIDE` in `tunables.rs`, stage 4's
    `channel_high_water` doc and flush comment, classify's ordered-phase and
    unordered-metrics docs, the router's wakeup comment,
    `reference/pipeline.md`, `reference/testing.md`, a
    `reference/performance-history.md` note that sidecar rows from
    `56a50e0d` until the fix are not comparable on the send-side counters,
    and the `hygiene.md` item (which also now names multi-extract gaining
    WILLNEED). The `notes/todo.md` stall follow-up about the classify-ordered
    phase recording no waits is gone. Unmeasured, per the drain rule; the
    audit's measurement order for `56a50e0d` stands, now with live counters.

- Slice C, item 5 (`8da26b23`, the cat `--clean` markers): confirmed and
  **resolved** (sparred with codex to consensus).
  - Confirmed in the code: the per-kind `CAT_NODES_*`, `CAT_WAYS_*` and
    `CAT_RELATIONS_*` pairs went with the per-kind phases, replaced by one
    `CAT_FILTERED_*` pair per input; `brokkr sidecar --compare` matches
    rows by name, so the planet baseline `f2315551` had nothing to line up
    with. No result was ever recorded on the `CAT_FILTERED_*`-only shape.
    One correction to the audit's framing: `handoff.md` names counters,
    units and the `WAIT_*` spans as the contract, not every phase marker;
    the rename still broke the baseline compare, which is the real cost.
  - Fix: the per-kind names are back, nested inside the kept
    `CAT_FILTERED_*` pair, driven from the ordered merge (`KindPhase` in
    `src/commands/cat/mod.rs`): a phase opens at the first written blob of
    its kind and closes at the first of a greater kind or at the pass end;
    a lower kind on unsorted input changes nothing (at most three pairs per
    input); both pairs are closed before an error propagates.
  - What they mean now, settled in the spar: write-side kind transitions,
    not barriers. Decode of the next kind overlaps a row by up to the
    admission window of tasks, and the wait for the first next-kind result
    lands in the row before, with no time bound the code enforces, so a
    per-kind row against the baseline is approximate. Seen on Denmark
    (forced run, not stored): the relation row is 0 ms because every
    relation blob was decoded while the last ways were written.
    `reference/performance.md` beside the baseline and
    `reference/pipeline.md` say this; the wall stays the exact comparison
    and `CAT_FILTERED_*` the exact ordered-pass total.
  - Pinned by `cli_cat::clean_marks_each_kind_phase` (sorted, two inputs,
    a two-kind `--type`, unsorted, a pass writing nothing, a failing pass),
    mutation-checked on the monotone rule and on the error-path close.
  - Not verified (brokkr's source was not readable here): codex reported
    that the human compare view matches a repeated name to its first
    occurrence, so a multi-input run's per-input rows do not align by
    input. Pre-existing either way. Also seen: when run A lacks a marker
    (the baseline has no `CAT_FILTERED_*`) the compare shows no row for it.
  - For the item's measurement: a cell built at `8da26b23` or `56a50e0d`
    carries only `CAT_FILTERED_*`, so the A/B against the parent there can
    compare wall, peak anon and stall counters but not per-kind rows; the
    HEAD repeat has them, approximately.

- Slice D, tags-filter (`0f64d8f4`, the removed `_direct_io` parameters):
  confirmed and **resolved** (sparred with codex to consensus).
  - Confirmed in the code and history: `0f64d8f4` dropped `_direct_io`
    from `collect_relation_member_closure` and
    `collect_way_node_dependencies`. The parameter dates from `9f73bef7`,
    which moved both scans from a sequential `BlobReader` that honoured the
    flag onto the classify pools, so it is the same deliberate plumbing
    `8192aa07` restored for diff. Codex checked every Slice D adoption
    commit and the eight unlisted ones: these two are the only removals
    there. Degrade's `passthrough_header_bytes` lost its `direct_io` in
    `d9b3943c` legitimately, since it now takes the reader its caller
    opened with `write.direct_reads`.
  - Fix: both helpers take `_direct_io` again, right after `input`, with
    diff's "not honoured yet" comment, and `tags_filter_two_pass` passes
    its `direct_io`. No behaviour change and no test: the flag still does
    not reach these reads. Honouring it was not taken: the pools pread
    frame offsets and lengths through the walker's shared buffered
    descriptor, which would need alignment handling throughout, the result
    is correct today, and the flag's reach is the deferred CLI-surface
    redesign.
  - Notes corrected: the `notes/todo.md` ignored-parameter inventory names
    the two helpers, and its reach paragraph says which tags-filter reads
    honour the flag (the probe, both leading-header reads, `-R`'s reader)
    and which do not. The `hygiene.md` pinning item records the removal
    and restoration.
  - Same family, left to its own fix: `79d2e923` also deleted sort's
    `build_blob_index`, whose `_direct_io` was ignored, along with the
    dedupe index pass that honoured the flag (Slice E item 5). The merge
    engine in `src/commands/blob_merge.rs` has no `direct_io` parameter at
    all now, so the sort side needs the plumbing back as well as dedupe.
    Resolved in the next entry.

- Slice E, item 5 (`79d2e923`, the dedupe `--direct-io` index pass):
  confirmed and **resolved** (sparred with codex to consensus, the diff
  reviewed by a fresh codex session).
  - Confirmed in the code and history: `79d2e923^` dedupe pass 1 walked
    each input through `FileReader::open(pin, direct_io)` (sequential
    `O_DIRECT` reads of the whole file); the engine's `index_inputs` walked
    with `HeaderWalker::open`, always buffered, and took no flag. Old sort
    pass 1 already walked buffered (`build_blob_index(_direct_io)`). Pass 2
    of both commands read buffered before and after. The change never
    shipped (the engine entry is under Unreleased in the CHANGELOG).
  - Fix: the flag is honoured, not just plumbed. `HeaderWalker` is generic
    over a `Positional` transport (a plain `File` by default, so every
    other caller is unchanged), and `HeaderWalker::open_direct` reads
    through `direct_reader::DirectPositional`, which serves any byte range
    from aligned preads (`read_aligned`: offset, address and length aligned;
    a short transfer rebased to the aligned offset below the missing bytes;
    stops once the requested bytes are held; a zero or no-progress transfer
    is `UnexpectedEof`). The captured-extent policy stays in `PreadSource`,
    so alignment padding neither widens a probe nor turns a shrunk file into
    a clean end. `input()` and `shared_file()` exist only on the buffered
    walker, so a direct descriptor cannot reach the pools that pread
    unaligned. The open goes through `InputSource::open_direct`, so pinned
    inputs and the shared `direct_io_open_error_at` classification are
    reused (no new message). `index_inputs` takes `direct_io`; sort and
    `cat --dedupe` both pass `direct_reads`, so `sort --direct-io` pass 1 is
    now `O_DIRECT` too (one engine). A no-feature build refuses with the
    message `FileReader` uses, now one shared function.
  - Not the old shape: the old dedupe pass 1 read every payload byte
    `O_DIRECT`; the new one reads only header probes (one aligned read of
    the blocks covering the 4 KiB window) and the payloads it scans.
    Unmeasured; no `--direct-io` number exists for either command.
  - Pass 2 stays buffered and is recorded (the `open_sources` doc, the
    `notes/todo.md` reach inventory, the CHANGELOG engine entry): the
    splicing backends, `--io-uring` among them, copy from the same
    descriptor at unaligned offsets, so a direct pass 2 there needs a
    second descriptor per input. `DirectPositional` keeps one scratch
    buffer behind a `RefCell`, so it is single-thread; the pools of the
    other walker users would need one transport per worker.
  - Tests: `read_aligned` over injected transfer sequences (partial,
    unaligned, interrupted, no-progress, EOF, overflow refused before any
    read); a real `O_DIRECT` file including truncation after open; the
    frame conformance table gains the direct walk as a source; buffered and
    direct walks give identical metas and payloads; both walks keep the
    extent captured at open (growth not walked, shrink an error); sort and
    `cat --dedupe` unit tests count direct walk opens (one per input) and
    compare plans and output bytes with the buffered run. Mutation-checked:
    opening the buffered walker in the direct branch fails both command
    tests. `brokkr check` (tier1, platform) passes; `cat --dedupe` and
    `sort` with `--direct-io` run on Denmark.
  - Doc corrections: the `HeaderWalker::open` comment that justified
    ignoring the flag ("buffered reads amortise that overhead") was wrong
    under `FADV_RANDOM`; it now says what buffered actually buys (cache
    residency for a later walk). Also `crate::direct_io`'s backend list,
    `alloc_aligned_zeroed`'s safety argument (now a checked zero size),
    `reference/testing.md`'s list of classified direct opens, the sort
    front-read comment and the frame conformance module doc.
  - Lateral: `DirectReader::fill` read again from an unaligned offset
    after a short transfer. Resolved in the next entry.

- Lateral from the entry above (`DirectReader::fill`, the sequential
  `O_DIRECT` reader behind `FileReader::open(.., true)`): confirmed and
  **resolved**.
  - Confirmed in the code: `fill` issued `read(2)` at the fd offset, which
    a short transfer had advanced by its count. `read_limit` (from
    `ea17fd86`) only stopped the read at a regular file's own length, so a
    short transfer before it whose count is not a multiple of the alignment
    (a filesystem whose real alignment is finer than the statx-floored one,
    or a partial transfer) made the next read unaligned, `EINVAL`. No
    recorded run hit it; it needs a filesystem that returns a short count
    before the end of the file.
    Two more in `skip`'s unaligned path: a refill that fell short of the
    target inside its block was reported as a skip past the end, and an
    `Interrupted` refill escaped after the seek, leaving the reader at the
    wrong logical position.
  - Fix, fitted to `read_aligned`: the reader reads by `pread` at its own
    tracked `file_pos` (the fd offset is no longer used or moved; the one
    `raw_fd` contract already said explicit offsets). Each fill starts at
    the aligned offset at or below `file_pos`, re-reading at most one
    partial block, and the read position skips the bytes already served; a
    transfer adding nothing past `file_pos` is the end of the file (a file
    that shrank reads as one ending there, as before). `read_limit` stays
    as the no-syscall stop at a regular file's length. `skip`'s unaligned
    path refills from the aligned offset below the target until the buffer
    covers it, retrying `Interrupted`; the aligned path is unchanged. Stale
    docs from Slice B item 1 corrected on the way: `FileReader::direct`,
    the `Direct` variant and `DirectReader::skip` no longer say page
    alignment or lseek.
  - Tests in `src/read/direct_reader.rs` drive `read` and `skip` over an
    injected `pread` that asserts every call is aligned (offset, address,
    length): whole-file reads under unaligned short transfers and
    interruptions, a no-progress transfer ending the read, skips within
    the buffer and to aligned and unaligned targets after short transfers
    (one falling short inside the target's block), a skip to the unaligned
    end, and skips past the end. Mutation-checked: dropping the rebase
    fails three tests on the alignment assertion, a single refill in
    `skip` fails the skip test. `brokkr check` (tier1, platform) passes;
    `cat --clean` and `degrade --strip-indexdata` with `--direct-io` run
    on Denmark.
  - Left open, pre-existing: on a block device `file_len` is 0, so
    `skip`'s aligned path refuses every aligned target past the buffer as
    past the end, while the unaligned path reads through. `read_limit`
    already treats block devices as unbounded; `skip` does not. Whether
    block-device inputs are supported at all is the question to settle
    first.

- Slice A, the defect (`BlobOrderGuard::observe`, the diff blob-order
  message): confirmed and **resolved** (sparred with codex to consensus).
  - Confirmed in the code: the guard's only caller is `RawWalk::bucket` in
    `src/commands/diff/shard.rs`, which runs it over each input's blobs in
    file order for text diff, `diff_stats` and derive alike, yet the message
    said "follows {kind} output already reaching id". The wording came from
    apply-changes' `check_output_order`, where "output" is right. No other
    copy of the mislabel exists.
  - Fix: "follows {kind} blobs already reaching id {id} earlier in this
    file", plus "according to its indexdata" before "its blobs are not in
    canonical order": the guard reads only the indexdata ranges, so a lying
    range (sorted contents, overstated `max_id`) trips it too, the
    distinction `reference/indexdata-trust.md` draws. The rest was checked
    and kept: both entry points refuse an unsorted header before the walk,
    so "The input's header claims Sort.Type_then_ID" holds; the caller
    locates the error with path, blob index and frame offset, so the
    message need not say old or new; `previous_id` is the furthest id in
    OSM order (not numerically greatest when negative) that earlier blobs
    of that kind reached. The `block-pair merge:` prefix stays: it names
    the engine whose precondition failed, the convention every other
    message in `merge_join.rs` follows (codex first proposed
    `input blob order:` and withdrew it). The substring the trust tests
    match is unchanged; `node_blob_after_way_blobs_is_rejected_not_dropped`,
    which pinned the old clause, now pins the new one. `brokkr check`
    (tier1) passes.
  - Laterals from the spar, all **open** (checked against the code, not
    fixed):
    - `require_sorted` (`src/commands/mod.rs`) says "{file} is not sorted"
      when it only knows the header does not declare `Sort.Type_then_ID`.
      A sorted file without the flag gets that message.
    - diff and derive give "Streaming diff requires sorted inputs to
      operate in constant memory" as the reason, but the indexed engine
      holds one descriptor per blob, so its memory grows with blob count.
    - `walk_file`'s "requires indexdata on every data blob" also fires for
      indexdata that is present but unreadable: `BlobIndex::deserialize`
      returns `None` for an unknown version, kind or an inverted range.
    - `diff -v` relation member details compare member sets
      (`write_member_diff_borrowed`), so a reorder (`[A,B]` to `[B,A]`) or
      a dropped duplicate (`[A,A]` to `[A]`) prints the modification with
      no member lines. The modification itself is still reported.

Notes made while the audit ran, not findings of it:

- protohoggr 0.4.0 to 0.5.0 (bz `pbfhogg#15`) was applied after slices C,
  A, D and B reported and while E and F were finishing; none of the
  reports depends on it. `brokkr check` and `brokkr verify all --dataset
  denmark` passed on 0.5.0.
