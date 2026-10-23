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

## BUG-110 - `merge-changes` reports a missing input without naming it

Anger: 4 - error text, contained.

`pbfhogg merge-changes missing.osc.gz -o out.osc.gz` exits 2 with only
`Error: No such file or directory (os error 2)`: the open of an input OSC
loses its path on the way out, unlike the PBF commands, whose open errors name
the file (`file_reader::open_error`). Found by the `-j` table in
`tests/cli_flag_contract.rs`, which had to judge merge-changes' acceptance by
the absence of a usage error instead of by the input's name for this reason.
Likely the `std::fs::metadata` / `File::open` calls in
`commands/merge_changes/mod.rs` or the OSC parser's open. Fix: wrap the open
with the path, and switch that table row back to checking the input's name.

Reported by: the CLI flag consistency batch.

## BUG-085 - `HeaderBuilder` drops an empty-string base URL or `source`

Anger: 4 - header bytes, contained.

Reported, not run. `HeaderBuilder::build` in `src/write/header_builder.rs`
writes `source` and `osmosis_replication_base_url` through protohoggr's
`encode_bytes_field`, which skips an empty value. So a header that carries an
empty string comes out with the field absent, while comments in the header
code treat empty and absent as distinct. Whether any input in the wild
carries an empty value is unknown; the effect is that a passthrough or
rebuilt header loses the distinction silently. Fix: encode the presence
decision from the `Option`, as fields 32 and 33 already do with
`encode_int64_field_always`, and pin it with a round-trip test of an empty
`source` and base URL.

Reported by: the output-header policy work.

