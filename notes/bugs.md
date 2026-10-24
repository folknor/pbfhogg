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

## BUG-133 - A metadata-less node in a dense block deletes as `version="0"`

Anger: 7 - derive-changes output; the root is the block builder.

`BlockBuilder` (`backfill_default_dense_metadata` /
`push_default_dense_metadata`) stores version 0 for a node without metadata
in a dense block whose other nodes carry metadata, so the reader sees 0, and
`derive.rs::extract_delete_info`'s `!= -1` filter lets it through:
derive-changes emits `<node id=".." version="0"/>`, which
`write_delete_element`'s own comment calls a claim the input never made. A way
without metadata correctly gets no version. Sequential and parallel paths
agree, so it is not drift. Seen on `cli_diff`'s sharded-equivalence fixture
(node 65 there is excluded from the checks with a comment). Open: whether the
builder should write the dense "absent" sentinel, or readers treat 0 as
absent (OSM versions start at 1).

Needs research before a ruling:

- What the PBF spec and `osmformat.proto` say a dense node without metadata
  in a metadata-carrying block should store, and whether `-1` is the
  sentinel for `DenseInfo.version` or only for `Info.version`.
- What osmium (`research/libosmium/`) writes in that position, and what it
  reads back from `0` and from `-1` (its version type is unsigned).
- What osmium's OSC and XML writers emit for a version-0 object, so the
  output convention matches the oracle.
- Every pbfhogg output that prints a version (derive deletes,
  `write_element_xml` creates and modifies, the text diff's `vN`, `inspect`,
  `export`) and what each prints for these nodes today.
- Whether anything in pbfhogg distinguishes absent from 0 on the read side
  (`DenseNodeInfo::version`, `owned.rs`'s defaults) that a builder change
  would break.

Recommendation on the table, pending that: treat version 0 as absent on
output (emit a version only above 0), leaving the encoding alone.

Reported by: the diff helper dedupe.

## BUG-138 - Sequential indexed `diff --type` without nodes fails on any file with nodes

Anger: 7 - CLI behaviour on the diff core's sequential path.

`diff_block_pair` (`commands/diff/mod.rs`) skips a disabled kind's phase
outright, but `osc/merge_join.rs::next_blob_for_kind` treats an earlier-kind
blob met during a later phase as an unsorted file. So `diff --type way`
(or `way,relation`) with `-j 1` or `-v`, on indexed inputs that hold nodes,
reads the first node blob in the way phase and fails with "found a node
blob during the way phase ... not in canonical order". The parallel path
buckets blobs by kind and is fine, and the unindexed element stream drains
excluded phases. An indexed input holding negative IDs falls back to the
same sequential merge, so it fails the same way at every `-j`. `-t node`
alone is unaffected (the later kinds are stashed, never reached).
Confirmed on a test fixture (probe in the parity test below, since
removed). `diff_stats` inherits the same behaviour by design; the parity test
`roundtrip_invariants::diff_stats_matches_text_diff_on_every_path` runs
these cases and sees both fail identically. Open: how an excluded phase
is skipped - drained through the merge (decoding, so the claimed-max and
decode checks apply to kinds the user excluded) or skipped by blob header
alone (what the parallel path effectively does).

Reported by: the quiet-diff count-only work.

## BUG-142 - `--tmp-dir` help misstates what parallel diff writes

Anger: 2 - help text.

`TmpDirArg`'s doc in `cli/src/lib.rs` says parallel diff "writes a copy of
the changed text" to scratch. Without `-c` (suppress common) each shard
writes all of its text, unchanged lines included, so scratch use is the size
of the full text output, not of the changes. Anyone sizing `--tmp-dir` from
the help underestimates it. Fix: reword the help (and any reference/docs
page that repeats it) to say each shard writes its full text output.

Reported by: the quiet-diff count-only work.


