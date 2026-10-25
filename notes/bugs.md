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
8. Nothing is measured - no benchmark, A/B, planet run or profiling cell -
   until this document and `notes/hygiene.md` are both drained. An entry
   whose only remaining work is a measurement is removed, not kept waiting.
```

Each entry carries an `Anger:` rating from 0 to 10: how likely resolving it
is to make the maintainer angry. 10 is rebuilds or contract changes on
measured paths; 9 is hot-path or contract changes that need benchmarks or
the maintainer's decision; 8 is altw, renumber and apply-changes internals
and observability contracts; 7 is geocode output and format, and CLI
behaviour; below that the work is progressively more contained.

---

## BUG-190 - The header walker sizes a block-device input as empty

Anger: 6 - pinned inputs accept block devices, but no command was ever
shown to read one.

`HeaderWalker::open` (`read/header_walker.rs`) takes `file_size` from the
opened descriptor's `metadata().len()`, which is `st_size`: 0 for a Linux
block device. `PinnedInput` deliberately accepts block devices ("a regular
file or a block device"), as do the contract in `reference/pipeline.md` and
every adopting command's docs, so a block-device input passes the pin and
then meets a walker that believes the file is empty - presumably a
missing-header refusal on every command that walks headers (the classify
schedule builders, sort, getid, getparents, time-filter's snapshot path,
and the rest). Either the walker (and any other `st_size` reader) learns
the device size (`BLKGETSIZE64`, or seeking to the end), or the pin refuses
block devices and the docs say so. Raised by codex in the time-filter pin
spar; not verified with a test.

Maintainer (2026-10-25): the pin refuses block devices, and the docs that
promise them (`reference/pipeline.md`, the adopting commands' docs, the
CHANGELOG) say so. The walker does not learn device sizes.

Reported by: the HYG-186 resolution.

## BUG-191 - Narrow packed values that do not fit their type alias silently

Anger: 9 - the fix is construction-time validation on every read path.

Block construction now guarantees that every packed array decodes in full
with `Cursor::read_varint` and that the parallel arrays agree in length,
but not that each value fits the type the iterator narrows it to.
`protohoggr`'s typed wrappers narrow after decoding: `PackedInt32Iter`
casts the `u64` to `i32`, `PackedSint32Iter` truncates to `u32` before the
zigzag step, and `PackedBoolIter` maps any nonzero value to `true`. So a
`DenseInfo.version` of `2^32` reads as version 0, a `DenseInfo.uid` or
`user_sid` delta of zigzag `2^32` reads as delta 0, a relation member type
of `2^32` reads as a node member, and a `roles_sid` of `2^32 + n` aliases
string `n`. The `uint32` tag-index arrays are already bounded in
`packed_uint32_count`; the fix for the others is the same single-scan
width check, for `int32` keeping sign-extended ten-byte negatives legal
and for `sint32` bounding the raw varint to `u32`. Raised by codex in the
dense-scanner spar; the narrowing casts were read in protohoggr 0.4.0, not
verified with a test.

Reported by: the BUG-183 resolution.

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
