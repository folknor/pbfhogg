//! Wire-format rewriters for DenseNodes, Ways, and Relations.
//!
//! These are the renumber-specific fast path: renumber only changes IDs,
//! so we avoid the full decode -> BlockBuilder -> re-encode cycle.
//! Per-node / per-way / per-relation cost drops from ~113 ns (HashMap
//! lookups, delta arrays, metadata) to ~10-15 ns (varint decode of old
//! ID + varint encode of new delta) plus verbatim byte-range copies for
//! all other fields.

use super::super::id_set_dense::IdSetDense;

// ---------------------------------------------------------------------------
// DenseNodes wire-format rewriter for pass 1
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
/// Reframe a decompressed PrimitiveBlock by replacing only the DenseNodes
/// ID deltas while copying everything else (string table, lat/lon, tags,
/// metadata) verbatim at the byte level.
///
/// This is the renumber-specific fast path: renumber only changes IDs,
/// so we avoid the full decode→BlockBuilder→re-encode cycle. Per-node
/// cost drops from ~113 ns (HashMap lookups, delta arrays, metadata) to
/// ~10-15 ns (varint decode of old ID + varint encode of new delta).
///
/// Returns the number of nodes in the block. Sets old node IDs
/// directly in `id_set` as they are decoded.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn reframe_dense_with_new_ids(
    decompressed: &[u8],
    base_new_id: i64,
    id_set: &IdSetDense,
    check_negative_ids: bool,
    output: &mut Vec<u8>,
    // Reusable scratch buffers - hoisted to worker level.
    group_ranges_scratch: &mut Vec<(usize, usize)>,
    scalar_fields_scratch: &mut Vec<u8>,
    other_fields_scratch: &mut Vec<u8>,
    new_id_packed_scratch: &mut Vec<u8>,
    dense_out_scratch: &mut Vec<u8>,
    group_out_scratch: &mut Vec<u8>,
) -> std::result::Result<u64, String> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    group_ranges_scratch.clear();
    scalar_fields_scratch.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(|e| e.to_string())? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                group_ranges_scratch.push((offset, data.len()));
            }
            (17..=20, WIRE_VARINT) => {
                let raw = cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
            _ => cursor.skip_field(wire_type).map_err(|e| e.to_string())?,
        }
    }

    let (st_offset, st_len) = stringtable_range
        .ok_or("reframe: no StringTable in PrimitiveBlock")?;
    if group_ranges_scratch.is_empty() {
        return Err("reframe: no PrimitiveGroup in PrimitiveBlock".into());
    }
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    // Phase 2-5: process each PrimitiveGroup, reframing its DenseNodes.
    output.clear();

    // PrimitiveBlock field 1: StringTable (copy verbatim)
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    let mut total_nodes: u64 = 0;
    let mut current_new_id = base_new_id;

    for &(gr_offset, gr_len) in group_ranges_scratch.iter() {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];

        let mut dense_data: Option<&[u8]> = None;
        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 2 && wire_type == WIRE_LEN {
                dense_data = Some(gr_cursor.read_len_delimited().map_err(|e| e.to_string())?);
            } else {
                gr_cursor.skip_field(wire_type).map_err(|e| e.to_string())?;
            }
        }

        let dense_bytes = dense_data.ok_or("reframe: no DenseNodes in PrimitiveGroup")?;

        let mut id_field: Option<&[u8]> = None;
        other_fields_scratch.clear();

        let mut dn_cursor = Cursor::new(dense_bytes);
        while let Some((field, wire_type)) = dn_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 1 && wire_type == WIRE_LEN {
                id_field = Some(dn_cursor.read_len_delimited().map_err(|e| e.to_string())?);
            } else {
                let raw = dn_cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(other_fields_scratch, field, wire_type);
                other_fields_scratch.extend_from_slice(raw);
            }
        }

        let id_bytes = id_field.ok_or("reframe: no packed ID field in DenseNodes")?;

        // Decode old ID deltas → absolute old IDs. Set bits in
        // id_set inline - no intermediate Vec.
        let mut old_id: i64 = 0;
        let mut id_cursor = Cursor::new(id_bytes);
        let mut group_node_count: u64 = 0;
        while id_cursor.remaining() > 0 {
            let delta = id_cursor.read_sint64().map_err(|e| e.to_string())?;
            old_id += delta;
            if check_negative_ids && old_id < 0 {
                return Err(format!(
                    "renumber requires non-negative input ids. \
                     Input contains node id {old_id}. \
                     Negative ids are JOSM editor-local staging identifiers \
                     that should be resolved before processing."
                ));
            }
            id_set.set_atomic(old_id);
            group_node_count += 1;
        }
        total_nodes += group_node_count;

        // Build new packed ID field for this group.
        let gnc = usize::try_from(group_node_count)
            .map_err(|_| "group node count > usize")?;
        new_id_packed_scratch.clear();
        protohoggr::encode_varint(
            new_id_packed_scratch,
            protohoggr::zigzag_encode_64(current_new_id),
        );
        new_id_packed_scratch.extend(std::iter::repeat_n(0x02u8, gnc.saturating_sub(1)));
        #[allow(clippy::cast_possible_wrap)]
        {
            current_new_id += group_node_count as i64;
        }

        dense_out_scratch.clear();
        protohoggr::encode_bytes_field(dense_out_scratch, 1, new_id_packed_scratch);
        dense_out_scratch.extend_from_slice(other_fields_scratch);

        group_out_scratch.clear();
        protohoggr::encode_bytes_field(group_out_scratch, 2, dense_out_scratch);
        protohoggr::encode_bytes_field(output, 2, group_out_scratch);
    }

    output.extend_from_slice(scalar_fields_scratch);

    Ok(total_nodes)
}

/// Reframe a decompressed way-blob PrimitiveBlock by replacing way IDs
/// and node refs while copying everything else verbatim.
///
/// For each way: decode old way id, assign new sequential way id,
/// resolve each ref's new node id via `node_id_set.rank()`, delta-encode
/// the new refs, and copy keys/vals/info raw bytes verbatim.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn reframe_ways_with_new_ids(
    decompressed: &[u8],
    base_new_way_id: i64,
    node_id_set: &IdSetDense,
    start_node_id: i64,
    way_id_set: &mut IdSetDense,
    output: &mut Vec<u8>,
    refs_scratch: &mut Vec<u8>,
    group_scratch: &mut Vec<u8>,
    reframed_way_scratch: &mut Vec<u8>,
    check_negative_ids: bool,
    group_ranges_scratch: &mut Vec<(usize, usize)>,
    scalar_fields_scratch: &mut Vec<u8>,
) -> std::result::Result<(u64, u64), String> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    group_ranges_scratch.clear();
    scalar_fields_scratch.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(|e| e.to_string())? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                group_ranges_scratch.push((offset, data.len()));
            }
            _ => {
                let raw = cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
        }
    }

    let (st_offset, st_len) = stringtable_range
        .ok_or("reframe_ways: no StringTable in PrimitiveBlock")?;
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    output.clear();
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    let mut total_ways: u64 = 0;
    let mut orphan_refs: u64 = 0;
    let mut current_new_id = base_new_way_id;

    for &(gr_offset, gr_len) in group_ranges_scratch.iter() {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];
        group_scratch.clear();

        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 3 && wire_type == WIRE_LEN {
                // Way submessage - splice-reframe it.
                // Find byte positions of field 1 (id) and field 8 (refs)
                // in way_bytes. Everything else is copied as contiguous
                // verbatim byte ranges - no per-field parse+re-encode.
                let way_bytes = gr_cursor.read_len_delimited().map_err(|e| e.to_string())?;

                // (tag_start, value_end) for fields we're replacing.
                let mut id_range: Option<(usize, usize)> = None;
                let mut refs_range: Option<(usize, usize)> = None;
                let mut old_way_id: i64 = 0;
                let mut old_refs_data: &[u8] = &[];

                let mut way_cursor = Cursor::new(way_bytes);
                while let Some((wf, wt)) = way_cursor.read_tag().map_err(|e| e.to_string())? {
                    // tag_start = position before read_raw_field consumed the value
                    let val_start = way_bytes.len() - way_cursor.remaining();
                    if wf == 1 && wt == WIRE_VARINT {
                        let tag_start = val_start - 1; // field 1 varint tag = 1 byte
                        old_way_id = way_cursor.read_varint_i64().map_err(|e| e.to_string())?;
                        let val_end = way_bytes.len() - way_cursor.remaining();
                        id_range = Some((tag_start, val_end));
                    } else if wf == 8 && wt == WIRE_LEN {
                        let tag_start = val_start - 1; // field 8 varint tag = 1 byte
                        old_refs_data = way_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                        let val_end = way_bytes.len() - way_cursor.remaining();
                        refs_range = Some((tag_start, val_end));
                    } else {
                        way_cursor.read_raw_field(wt).map_err(|e| e.to_string())?;
                    }
                }

                if check_negative_ids && old_way_id < 0 {
                    return Err(format!(
                        "renumber requires non-negative input ids. \
                         Input contains way id {old_way_id}. \
                         Negative ids are JOSM editor-local staging identifiers \
                         that should be resolved before processing."
                    ));
                }
                way_id_set.set(old_way_id);

                // Decode old ref deltas, resolve via rank(), delta-encode new refs.
                refs_scratch.clear();
                let mut prev_old_ref: i64 = 0;
                let mut prev_new_ref: i64 = 0;
                let mut refs_cursor = protohoggr::Cursor::new(old_refs_data);
                while !refs_cursor.is_empty() {
                    let raw = refs_cursor.read_varint().map_err(|e| e.to_string())?;
                    let delta = protohoggr::zigzag_decode_64(raw);
                    prev_old_ref += delta;
                    let old_node_id = prev_old_ref;
                    if old_node_id < 0 {
                        return Err(format!(
                            "renumber requires non-negative input ids. \
                             Way references negative node id {old_node_id}. \
                             Negative ids are JOSM editor-local staging \
                             identifiers that should be resolved before \
                             processing."
                        ));
                    }
                    let new_ref = node_id_set.resolve(old_node_id, start_node_id);
                    if !node_id_set.get(old_node_id) {
                        orphan_refs += 1;
                    }
                    protohoggr::encode_varint(
                        refs_scratch,
                        protohoggr::zigzag_encode_64(new_ref - prev_new_ref),
                    );
                    prev_new_ref = new_ref;
                }

                // Splice: emit way_bytes with id and refs replaced.
                // Sort the two replacement ranges by start position to
                // handle any field order in the wire format.
                let id_r = id_range.ok_or("reframe_ways: no id field")?;
                let refs_r = refs_range.ok_or("reframe_ways: no refs field")?;
                let (first, second) = if id_r.0 < refs_r.0 {
                    (id_r, refs_r)
                } else {
                    (refs_r, id_r)
                };

                reframed_way_scratch.clear();
                // Bytes before first replaced field.
                reframed_way_scratch.extend_from_slice(&way_bytes[..first.0]);
                // First replacement.
                if first.0 == id_r.0 {
                    protohoggr::encode_int64_field(reframed_way_scratch, 1, current_new_id);
                } else {
                    protohoggr::encode_bytes_field(reframed_way_scratch, 8, refs_scratch);
                }
                // Bytes between first and second replaced fields.
                reframed_way_scratch.extend_from_slice(&way_bytes[first.1..second.0]);
                // Second replacement.
                if second.0 == refs_r.0 {
                    protohoggr::encode_bytes_field(reframed_way_scratch, 8, refs_scratch);
                } else {
                    protohoggr::encode_int64_field(reframed_way_scratch, 1, current_new_id);
                }
                // Bytes after second replaced field.
                reframed_way_scratch.extend_from_slice(&way_bytes[second.1..]);

                protohoggr::encode_bytes_field(group_scratch, 3, reframed_way_scratch);

                current_new_id += 1;
                total_ways += 1;
            } else {
                // Non-way field in the group - copy verbatim.
                let raw = gr_cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(group_scratch, field, wire_type);
                group_scratch.extend_from_slice(raw);
            }
        }

        protohoggr::encode_bytes_field(output, 2, group_scratch);
    }

    output.extend_from_slice(scalar_fields_scratch);

    Ok((total_ways, orphan_refs))
}

// ---------------------------------------------------------------------------
// Wire-format relation rewriter
// ---------------------------------------------------------------------------

/// Wire-format splice rewriter for relations. Patches field 1 (id) and
/// field 9 (memids) in each Relation submessage; copies all other fields
/// (keys, vals, info, roles_sid, types) verbatim as raw bytes.
///
/// The memids field (packed sint64, delta-encoded) interleaves node, way,
/// and relation member IDs in one stream. Field 10 (types, packed int32)
/// tells us which lookup to use for each member:
///   0 = node  → `node_id_set.resolve(old_id, start_node_id)`
///   1 = way   → `way_id_set.resolve(old_id, start_way_id)`
///   2 = relation → `relation_id_set.resolve(old_id, start_relation_id)`
///   other = unknown (preserve old absolute ID unchanged)
///
/// One `prev_new_id` accumulator tracks across ALL member types - the
/// delta encoding is over the interleaved stream, not per-type.
///
/// Returns `(relation_count, min_new_id, max_new_id)`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::cast_possible_truncation)]
pub(super) fn reframe_relations_with_new_ids(
    decompressed: &[u8],
    relation_id_set: &IdSetDense,
    start_relation_id: i64,
    node_id_set: &IdSetDense,
    start_node_id: i64,
    way_id_set: &IdSetDense,
    start_way_id: i64,
    output: &mut Vec<u8>,
    memids_scratch: &mut Vec<u8>,
    group_scratch: &mut Vec<u8>,
    reframed_rel_scratch: &mut Vec<u8>,
    group_ranges_scratch: &mut Vec<(usize, usize)>,
    scalar_fields_scratch: &mut Vec<u8>,
) -> std::result::Result<(u64, i64, i64, u64), String> {
    use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

    // ---- Level 1: PrimitiveBlock ----
    group_ranges_scratch.clear();
    scalar_fields_scratch.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(|e| e.to_string())? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(|e| e.to_string())?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                group_ranges_scratch.push((offset, data.len()));
            }
            _ => {
                let raw = cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
        }
    }

    let (st_offset, st_len) = stringtable_range
        .ok_or("reframe_relations: no StringTable in PrimitiveBlock")?;
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    output.clear();
    protohoggr::encode_bytes_field(output, 1, stringtable_bytes);

    let mut total_relations: u64 = 0;
    let mut orphan_refs: u64 = 0;
    let mut min_new_id: i64 = i64::MAX;
    let mut max_new_id: i64 = i64::MIN;

    // ---- Level 2: PrimitiveGroup ----
    for &(gr_offset, gr_len) in group_ranges_scratch.iter() {
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];
        group_scratch.clear();

        let mut gr_cursor = Cursor::new(group_bytes);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 4 && wire_type == WIRE_LEN {
                // Relation submessage - splice-reframe it.
                let rel_bytes = gr_cursor.read_len_delimited().map_err(|e| e.to_string())?;

                // Scan relation fields to find byte ranges for id and memids.
                let mut id_range: Option<(usize, usize)> = None;
                let mut memids_range: Option<(usize, usize)> = None;
                let mut old_rel_id: i64 = 0;
                let mut old_memids_data: &[u8] = &[];
                let mut types_data: &[u8] = &[];

                let mut rel_cursor = Cursor::new(rel_bytes);
                while let Some((rf, rt)) = rel_cursor.read_tag().map_err(|e| e.to_string())? {
                    let val_start = rel_bytes.len() - rel_cursor.remaining();
                    match (rf, rt) {
                        (1, WIRE_VARINT) => {
                            let tag_start = val_start - 1; // field 1 tag = 0x08, 1 byte
                            old_rel_id = rel_cursor.read_varint_i64().map_err(|e| e.to_string())?;
                            let val_end = rel_bytes.len() - rel_cursor.remaining();
                            id_range = Some((tag_start, val_end));
                        }
                        (9, WIRE_LEN) => {
                            let tag_start = val_start - 1; // field 9 tag = 0x4A, 1 byte
                            old_memids_data = rel_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                            let val_end = rel_bytes.len() - rel_cursor.remaining();
                            memids_range = Some((tag_start, val_end));
                        }
                        (10, WIRE_LEN) => {
                            types_data = rel_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                            // Not patched - just captured for dispatch.
                        }
                        _ => {
                            rel_cursor.read_raw_field(rt).map_err(|e| e.to_string())?;
                        }
                    }
                }

                // Look up new relation id.
                let new_rel_id = relation_id_set.resolve(old_rel_id, start_relation_id);

                if new_rel_id < min_new_id {
                    min_new_id = new_rel_id;
                }
                if new_rel_id > max_new_id {
                    max_new_id = new_rel_id;
                }

                // ---- Patch memids: decode old deltas + types, look up new ids, re-encode ----
                memids_scratch.clear();

                if !old_memids_data.is_empty() || !types_data.is_empty() {
                    // Validate: both must have the same varint count.
                    let memids_count = old_memids_data.iter().filter(|&&b| b & 0x80 == 0).count();
                    let types_count = types_data.iter().filter(|&&b| b & 0x80 == 0).count();
                    if memids_count != types_count {
                        return Err(format!(
                            "reframe_relations: relation {old_rel_id} has {memids_count} memids \
                             but {types_count} types"
                        ));
                    }

                    let mut memids_cursor = Cursor::new(old_memids_data);
                    let mut types_cursor = Cursor::new(types_data);
                    let mut prev_old_id: i64 = 0;
                    let mut prev_new_id: i64 = 0;

                    for _ in 0..memids_count {
                        // Decode member type.
                        let member_type = types_cursor
                            .read_varint()
                            .map_err(|e| format!("types varint: {e}"))?;

                        // Decode old memid delta → absolute old id.
                        let raw_delta = memids_cursor
                            .read_varint()
                            .map_err(|e| format!("memids varint: {e}"))?;
                        let delta = protohoggr::zigzag_decode_64(raw_delta);
                        prev_old_id += delta;
                        let old_abs_id = prev_old_id;

                        // Look up new absolute id by member type.
                        let (new_abs_id, is_orphan) = match member_type {
                            0 => (node_id_set.resolve(old_abs_id, start_node_id), !node_id_set.get(old_abs_id)),
                            1 => (way_id_set.resolve(old_abs_id, start_way_id), !way_id_set.get(old_abs_id)),
                            2 => (relation_id_set.resolve(old_abs_id, start_relation_id), !relation_id_set.get(old_abs_id)),
                            _ => (old_abs_id, false), // unknown type - preserve
                        };
                        if is_orphan {
                            orphan_refs += 1;
                        }

                        // Delta-encode the new id.
                        protohoggr::encode_varint(
                            memids_scratch,
                            protohoggr::zigzag_encode_64(new_abs_id - prev_new_id),
                        );
                        prev_new_id = new_abs_id;
                    }
                }

                // ---- Splice: emit rel_bytes with id and memids replaced ----
                let id_r = id_range.ok_or_else(|| {
                    format!("reframe_relations: no id field in relation {old_rel_id}")
                })?;

                reframed_rel_scratch.clear();

                if let Some(memids_r) = memids_range {
                    // Two replacement fields - sort by position, splice.
                    let (first, second) = if id_r.0 < memids_r.0 {
                        (id_r, memids_r)
                    } else {
                        (memids_r, id_r)
                    };

                    // Bytes before first replaced field.
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[..first.0]);
                    // First replacement.
                    if first.0 == id_r.0 {
                        protohoggr::encode_int64_field(reframed_rel_scratch, 1, new_rel_id);
                    } else {
                        protohoggr::encode_bytes_field(reframed_rel_scratch, 9, memids_scratch);
                    }
                    // Bytes between first and second replaced fields.
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[first.1..second.0]);
                    // Second replacement.
                    if second.0 == memids_r.0 {
                        protohoggr::encode_bytes_field(reframed_rel_scratch, 9, memids_scratch);
                    } else {
                        protohoggr::encode_int64_field(reframed_rel_scratch, 1, new_rel_id);
                    }
                    // Bytes after second replaced field.
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[second.1..]);
                } else {
                    // No memids field (zero-member relation) - only patch id.
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[..id_r.0]);
                    protohoggr::encode_int64_field(reframed_rel_scratch, 1, new_rel_id);
                    reframed_rel_scratch.extend_from_slice(&rel_bytes[id_r.1..]);
                }

                protohoggr::encode_bytes_field(group_scratch, 4, reframed_rel_scratch);
                total_relations += 1;
            } else {
                // Non-relation field in the group - drop it to match
                // current R2d behavior (only relations are emitted).
                gr_cursor.read_raw_field(wire_type).map_err(|e| e.to_string())?;
            }
        }

        if !group_scratch.is_empty() {
            protohoggr::encode_bytes_field(output, 2, group_scratch);
        }
    }

    output.extend_from_slice(scalar_fields_scratch);

    Ok((total_relations, min_new_id, max_new_id, orphan_refs))
}
