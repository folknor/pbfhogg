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

## BUG-063 - Platform-skip residue: `src/` unit tests and pre-5.12 ring ENOMEM

Anger: 7 - CLI behaviour.

Platform refusals are now typed (`ErrorKind::PlatformUnsupported`, CLI exit
3) and the shared test predicates check that, failing rather than skipping
under `BROKKR_TEST_PLATFORM`. Left:

- The unit tests inside `src/read/direct_reader.rs` still skip on a refusal
  whatever `BROKKR_TEST_PLATFORM` says: library code may not read the
  environment, and the `no-env-read` textlint rule covers test code in
  `src/` too (brokkr#10 is the open ticket for that blind spot). Moving those
  tests out of `src/`, or brokkr scoping the rule, closes it.
- io_uring ring creation failing with `ENOMEM` is now an ordinary I/O error.
  Kernels before 5.12 charge ring memory to `RLIMIT_MEMLOCK`, so on such a
  host with a low memlock limit the uring tests fail instead of skipping and
  the CLI exits 2 instead of 3. `register_buffers` already handles this with
  a getrlimit check; ring creation does not. No current host is affected.
- CLI exit 3 depends on every command carrying the `io::Error` through `?`,
  `From` or `map_io_preserving_platform`. No audit has checked that no
  command stringifies it (`map_err(|e| format!(..))`), which would exit 2
  and turn its platform test red rather than skipped.
- Now that the `platform` profile refuses to skip, it needs at least 16 MB
  of locked memory for the io_uring registered-buffer pool. On bygg the hard
  limit is 8 MB (`ulimit -Hl`), so `cli_sort::platform::sort_overlapping_blobs_uring`
  and `cli_apply_changes::platform::merge_basic_create_modify_delete_uring`
  fail there with a correct exit-3 report (seen 2026-10-22). Raise the host
  limit (LimitMEMLOCK or limits.conf); both used to pass by skipping.

Reported by: test harness, write path, CLI and plumbing.

## BUG-108 - Raw string-table indices are unchecked in release, and the interner trusts FxHash on input strings

Anger: 9 - string-table remap in the rewrite hot path.

`StringTable::pre_seed` now preserves every source slot, and `BlockBuilder`
checks raw sids against the table in debug builds only; a release build
writes an out-of-range raw sid into the output unchecked. Separately, the
interner hashes input-derived strings with FxHash, and the 8000-element
block cap does not bound how many strings `pre_seed` inserts, so an
adversarial string table can force collision-heavy interning. Decide
whether the raw sid bound belongs in block validation (once per block) and
whether a collision-resistant hasher is worth its cost on the rewrite path.

Reported by: the wave-9 string-table fixer.

