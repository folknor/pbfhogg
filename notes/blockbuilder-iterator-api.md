# BlockBuilder: remaining slice-to-iterator migration

## What shipped

`add_node`, `add_way`, `add_way_with_locations` tags params changed from
`&[(&str, &str)]` to `impl IntoIterator<Item = (&str, &str)>`. Dual
packed buffer (`packed_vals_scratch`) enables single-pass encoding —
no Clone needed.

## What remains

`add_relation` members parameter is still `members: &[MemberData<'_>]`.
The plan (Approach 5) called for triple packed buffers (`member_roles`,
`member_ids`, `member_types`) so members could also be an iterator with
single-pass encoding into three separate packed fields (roles field 8,
memids field 9, types field 10).

This eliminates per-element `Vec<MemberData>` allocations in callers like
`write_single_relation` in `elements_pbf.rs`. Same pattern as the tag
iterator migration — callers that currently `.collect()` into a temp Vec
would pass owned member iterators directly.

Impact depends on whether relation-heavy paths (merge, sort, diff) are
bottlenecked on member allocation. The tag iterator migration eliminated
~80 GB cumulative alloc on Japan (diff command); the member side is
smaller but follows the same pattern.
