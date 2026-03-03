# Passthrough Coalescing Memcpy Investigation (2026-03-03)

## Scope

Investigate TODO item:

- `P1 performance: passthrough coalescing currently memcpy-copies full frames`

No code changes were made during this investigation.

## Current Behavior

### Merge path

- `src/commands/merge.rs::coalesce_passthrough` appends each passthrough frame into a single `Vec<u8>` via `extend_from_slice`.
- Flush path writes the coalesced bytes once with `write_raw_owned`.
- This reduces channel-send overhead, but performs a userspace memcpy of all passthrough bytes into the coalescing buffer.

### Add-locations-to-ways path

- `src/commands/add_locations_to_ways.rs::coalesce_passthrough` uses the same `extend_from_slice` pattern.
- When copy-range is unavailable/inactive, passthrough-heavy runs pay the same userspace memcpy cost.

## Writer Capability Constraint

`PbfWriter` currently accepts:

- single raw blob payloads (`write_raw` / `write_raw_owned`) as one contiguous `Vec<u8>`, or
- `CopyRange` passthrough items.

It does not currently accept segmented passthrough chunks (`Vec<Vec<u8>>` / iovec-style raw groups), which is why coalescing code concatenates bytes first.

## Measurement Notes

Environment snapshot (`brokkr env`):

- host: `plantasjen`
- commit: `795f59b`
- memory: 30 GB
- storage: source/data/scratch on NVMe, target HDD

Observed runs:

- Merge Denmark (`bench merge`, none): 6766 passthrough / 630 rewritten blobs.
- Merge Germany (`bench merge`, none): 50981 passthrough / 11480 rewritten blobs.
- Add-locations Denmark (`--keep-untagged-nodes --compression none`): 6568 passthrough / 828 decoded blobs.

Important instrumentation caveat in current merge stats:

- `bytes_passthrough` can read `0` even in passthrough-heavy runs because frame length is sampled after `mem::take(frame.frame_bytes)` in the coalescing path.
- This should be fixed before using `bytes_passthrough` for before/after validation.

## Candidate Fixes Considered

1. Writer-side chunk API (recommended)
- Add a raw payload mode that carries multiple owned frame chunks in one sequence item.
- Coalescers collect owned frame vectors instead of concatenating bytes.
- Writer thread drains chunk list in order.
- Benefits: removes coalescing memcpy while preserving low channel-send count and ordering.

2. Vectored writes only (`writev`) from command side
- Keep command-owned `Vec<Vec<u8>>`, write via vectored API.
- Harder to integrate cleanly with existing pipeline thread and O_DIRECT/io_uring variants.

3. Keep concatenation and rely on copy-range where possible
- Does not address current non-copy-range paths.
- Leaves known TODO issue unresolved for many practical runs.

## Correctness Constraints for Any Fix

- Preserve exact output ordering across passthrough and rewritten blobs.
- Preserve existing flush boundaries at type transitions and batch boundaries.
- Keep compatibility with:
  - buffered writer,
  - copy-range payload path,
  - io_uring writer path.
- Avoid increasing channel pressure by regressing to one send per passthrough blob.

## Recommended Next Step

Implement writer-side chunked raw payload support, then migrate:

1. `merge` coalescer
2. `add_locations_to_ways` coalescer

Validate with:

- `brokkr check`
- `brokkr bench merge --dataset denmark --runs 1 --compression none`
- `brokkr bench merge --dataset germany --runs 1 --compression none`
- `brokkr run add-locations-to-ways ... --keep-untagged-nodes --compression none`

