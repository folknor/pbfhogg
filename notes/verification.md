# Verification findings

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

Findings are as reported by the hunters and have not been verified. Hunters read
source; costs are derived from the code unless an entry says measured. "Reported
by" records which scopes raised a finding and is a fact about the reports, not a
verdict on the code. Entries are in no order of importance.

## VER-163 - The apply-changes base replacement cell runs buffered and one arm only

`fault_apply_changes.rs` `base_replaced_after_the_splice_open_is_never_read` is a hand-rolled
pinned-input replacement cell outside `pinned_cell::check`: it arms the replacement after the
splice-source open only, with `OutputBackend::Parallel` and buffered reads, and asserts no
transport list. The file's `direct_reads_reach_every_base_open` cell pins the direct transports
of an unreplaced run, so no cell holds apply-changes to its pin under a replacement on the direct
read path, or at any open other than the one chosen. Every other pinned-input command now runs
its whole matrix, both read modes and the caller-pin arm through the driver; apply-changes would
need a driver cell whose run takes the base and the OSC, with the splice route's per-open list
(front read, splice source, the scanner's walker, the worker pool's descriptor).

## VER-164 - `opens_carry_o_direct_and_count_once` skips without announcing

`direct_io.rs` `opens_carry_o_direct_and_count_once` returns on a typed direct-io refusal of its
write open (`Err(e) if is_direct_io_refusal(&e) => return`) without calling `skip`, though
`test_support::refused` leaves announcing the skip to its caller and every other predicate site
announces. The gate still fails it through `fail_if_required`; outside the gate the test passes
silently on a host that refuses O_DIRECT, and this is the test the construction exemption in
`reference/testing.md` "Platform-dependent tests" rests on.
