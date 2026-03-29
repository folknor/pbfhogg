# Raw group passthrough for extract

## Problem

Simple extract is 2.5x slower than osmium at Japan scale (18s vs 7.2s).
The gap is on the write side: pbfhogg fully decodes every matching element
(tags, refs, metadata) into Rust types, then re-encodes via BlockBuilder
(string table construction, delta encoding, wire-format serialization).
osmium copies raw protobuf group bytes for matching elements, modifying
only the string table.

## How osmium does it

libosmium (via protozero) identifies matching PrimitiveGroups within a
PrimitiveBlock. For groups where all elements match, it copies the raw
protobuf bytes into the output block. Only the StringTable is rebuilt
(subset of referenced entries). No per-element decode or re-encode.

For groups with partial matches (some elements match, some don't), osmium
falls back to element-level processing.

## Proposed approach

After classification (which elements match), the write pass examines each
PrimitiveGroup in the block:

**All-match group:** Every element in the group is selected for output.
Copy the group's raw protobuf bytes. No decode, no BlockBuilder.

**Partial-match group:** Some elements match. Fall back to current path:
decode elements, filter, re-encode via BlockBuilder.

**No-match group:** Skip entirely.

In sorted PBFs, node groups are geographically local (~0.01° spread per
group). For regional extracts, most node groups in the bbox interior are
all-match. Way and relation groups are less predictable (sorted by way/
relation ID, not geography).

## String table handling

Each PrimitiveBlock has one StringTable shared by all groups. If we copy
raw group bytes, the output block needs a StringTable containing at least
the entries referenced by the copied groups.

Three options:
1. **Copy the full StringTable.** Simple, correct, slightly wasteful
   (unused entries add ~1-5 KB per block). No string table analysis needed.
2. **Subset the StringTable.** Scan copied groups for string table indices,
   build a minimal table, remap indices in copied group bytes. Complex —
   requires rewriting varint-encoded indices in the raw bytes.
3. **Keep original + append new.** For partial-match groups that are
   re-encoded, BlockBuilder creates new string table entries. Merge with
   the original table. Complex.

Option 1 is the right starting point. The waste is negligible (<1% of
output size). Option 2 is a later optimization if output size matters.

## Implementation sketch

```rust
fn write_block_with_passthrough(
    block: &PrimitiveBlock,
    // ... IDs, clean, etc ...
) -> Vec<u8> {
    // Classify each group: all-match, partial, no-match
    for group_idx in 0..block.group_count() {
        let group_data = block.raw_group_bytes(group_idx);
        match classify_group(group_idx, &ids) {
            GroupMatch::All => {
                // Copy raw group bytes to output
                output_groups.push(group_data);
            }
            GroupMatch::Partial => {
                // Decode + filter + re-encode (current path)
                // BlockBuilder produces new group bytes
            }
            GroupMatch::None => {
                // Skip
            }
        }
    }
    // Assemble output: StringTable (copied) + output groups
    frame_raw_block(string_table_bytes, &output_groups)
}
```

## What needs to exist

1. **`PrimitiveBlock::raw_group_bytes(index)`** — access raw protobuf bytes
   for a group. WireBlock already stores `(offset, length)` per group.
   This is a slice into the decompressed buffer.

2. **`PrimitiveBlock::raw_stringtable_bytes()`** — raw StringTable protobuf
   bytes. WireBlock already has this during parse (field 1).

3. **`classify_group()`** — determine all-match/partial/none for a group.
   For node groups: check all node IDs against bbox_node_ids. For way
   groups: check all way IDs against matched_way_ids. This requires
   iterating the group's elements but only reading IDs (not tags/metadata).

4. **`frame_raw_block()`** — assemble a PrimitiveBlock protobuf from raw
   StringTable bytes + raw/re-encoded group bytes + scalar fields
   (granularity, lat_offset, lon_offset). Write the protobuf framing
   (field tags + length delimiters) around the raw content.

5. **Integration with `--clean`** — if metadata cleaning is active, raw
   passthrough must be disabled (raw bytes preserve original metadata).

## Expected impact

At Japan scale (Tokyo bbox): most node groups in the bbox interior are
all-match. ~60% of node blobs could use raw passthrough. Way and relation
groups less likely to be all-match. Estimate: 40-60% of output bytes
written via raw passthrough, saving the decode + re-encode cost for those.

Simple extract: 18s → ~10-12s estimated (closing to within 1.5x of osmium).

## Dependencies

None on current infrastructure. This is a write-path optimization,
independent of the read-path pread-from-workers work. Can be prototyped
on top of the current 3-phase simple extract.
