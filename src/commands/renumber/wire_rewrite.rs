//! Wire-format rewriters for DenseNodes, Ways, and Relations.
//!
//! These are the renumber-specific fast path: renumber only changes IDs,
//! so we avoid the full decode -> BlockBuilder -> re-encode cycle.
//! Per-node / per-way / per-relation cost drops from ~113 ns (HashMap
//! lookups, delta arrays, metadata) to ~10-15 ns (varint decode of old
//! ID + varint encode of new delta) plus verbatim byte-range copies for
//! all other fields.

use crate::idset::IdSet;

/// Error text for a packed field this rewriter cannot splice.
///
/// Unlike `src/scan/*` and `src/blob_meta/scan.rs`, these rewriters cannot
/// route through `src/read/wire.rs`. `wire.rs` yields a packed field's *value*
/// as a borrowed slice; a byte-level splice needs the field's *byte range*
/// inside the enclosing message - the tag start and the value end - so that
/// every other field can be copied verbatim without a parse / re-encode. That
/// is information the shared parser deliberately does not expose. The
/// duplicate-occurrence half of the policy is therefore re-decided here rather
/// than inherited - see [`repeated_field_error`], which refuses a second
/// data-bearing occurrence of the same packed field for reasons a splice makes
/// sharper than the read path's.
///
/// So the policy is the same as `wire.rs` in its *observable* effect - never
/// silently mis-handle a legal encoding - but reached by refusing rather than
/// accepting. Concretely, an unpacked `DenseNodes.id`, `Way.refs`,
/// `Relation.memids` or `Relation.types` is spec-legal and rare (no known
/// writer emits it), and the old code's handling ranged from a misleading
/// error to genuine silent corruption: unpacked `memids` left `memids_range`
/// as `None`, so the relation was treated as memberless, the id was patched,
/// and the unpacked member IDs were copied through **un-renumbered** into
/// output that claims to be renumbered.
fn unpacked_field_error(message: &str, field: &str) -> String {
    format!(
        "renumber: {message}.{field} is written in the unpacked wire form \
         (individually tagged varints). That encoding is spec-legal but the \
         renumber wire-splice rewriter can only patch the length-delimited \
         packed form. Re-encode the input first, e.g. `pbfhogg cat --clean`."
    )
}

/// Error text for a packed field that occurs more than once carrying data.
///
/// Protobuf gives a repeated packed field *concatenation* semantics across
/// occurrences: two length-delimited segments of `Way.refs` are one ref array,
/// not two candidate arrays. `src/read/wire.rs::take_packed` refuses that shape
/// because the read path is built on a single borrowed slice; these rewriters
/// have to refuse it for a stronger reason still. A splice replaces one
/// contiguous byte range and copies everything else verbatim, so with two
/// segments the earlier one is copied through **unrenumbered** while only the
/// last is patched - partially renumbered output, silently, from a file that
/// is entirely legal protobuf. For `Relation.types` the damage is worse than
/// partial: `types` drives the per-member node/way/relation dispatch, so
/// keeping only the last segment makes `memids` and `types` different lengths
/// and the count cross-check fires with a misleading message, or (equal
/// lengths, different values) every member is looked up in the wrong ID space.
///
/// Empty occurrences are ignored in either position, matching `take_packed`:
/// a zero-length segment carries no elements and cannot participate in a
/// concatenation.
fn repeated_field_error(message: &str, field: &str) -> String {
    format!(
        "renumber: {message}.{field} appears twice with data; packed field split \
         across segments. Protobuf concatenates such segments, which a byte-level \
         splice cannot express as one contiguous range - the earlier segment would \
         be copied through unrenumbered. Re-encode the input first, e.g. \
         `pbfhogg cat --clean`."
    )
}

/// Single-lookup `resolve`-equivalent: returns `(new_id, is_orphan)`.
/// On hit, `new_id = start_id + rank`. On miss, passes the old id through
/// unchanged (matches `IdSet::resolve`'s orphan return). Used by the
/// way-ref and relation-member-ref renumber hot loops to avoid the
/// `resolve(id) + get(id)` double chunk lookup.
#[inline]
#[allow(clippy::cast_possible_wrap)]
fn resolve_with_orphan(set: &IdSet, old_id: i64, start_id: i64) -> (i64, bool) {
    match set.rank_if_set(old_id) {
        Some(r) => (start_id + r as i64, false),
        None => (old_id, true),
    }
}

/// Count varints in `data`, erroring on a truncated trailing varint or a
/// malformed mid-stream byte. Counting continuation-bit-clear bytes is
/// faster but silently accepts truncation (no terminator means the last
/// byte is missed; extra `0x80` bytes after the last terminator are
/// silently ignored). A full walk catches both classes at the boundary.
fn count_varints_strict(data: &[u8]) -> std::result::Result<usize, String> {
    use protohoggr::Cursor;
    let mut cursor = Cursor::new(data);
    let mut count = 0_usize;
    while cursor.remaining() > 0 {
        cursor.read_varint().map_err(|e| e.to_string())?;
        count += 1;
    }
    Ok(count)
}

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
    id_set: &IdSet,
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
                let raw = cursor
                    .read_raw_field(wire_type)
                    .map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
            _ => cursor.skip_field(wire_type).map_err(|e| e.to_string())?,
        }
    }

    let (st_offset, st_len) =
        stringtable_range.ok_or("reframe: no StringTable in PrimitiveBlock")?;
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

        // `id_field` holds only a data-bearing occurrence; `saw_id_field`
        // records that field 1 was present at all, so a DenseNodes carrying
        // one empty packed `id` still means "zero nodes" rather than "no id
        // field", exactly as before this guard existed.
        let mut id_field: Option<&[u8]> = None;
        let mut saw_id_field = false;
        other_fields_scratch.clear();

        let mut dn_cursor = Cursor::new(dense_bytes);
        while let Some((field, wire_type)) = dn_cursor.read_tag().map_err(|e| e.to_string())? {
            if field == 1 && wire_type == WIRE_VARINT {
                return Err(unpacked_field_error("DenseNodes", "id"));
            }
            if field == 1 && wire_type == WIRE_LEN {
                let seg = dn_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                saw_id_field = true;
                if !seg.is_empty() {
                    if id_field.is_some() {
                        return Err(repeated_field_error("DenseNodes", "id"));
                    }
                    id_field = Some(seg);
                }
            } else {
                let raw = dn_cursor
                    .read_raw_field(wire_type)
                    .map_err(|e| e.to_string())?;
                protohoggr::encode_tag(other_fields_scratch, field, wire_type);
                other_fields_scratch.extend_from_slice(raw);
            }
        }

        if !saw_id_field {
            return Err("reframe: no packed ID field in DenseNodes".into());
        }
        let id_bytes = id_field.unwrap_or_default();

        // Decode old ID deltas → absolute old IDs. Set bits in
        // id_set inline - no intermediate Vec.
        let mut old_id: i64 = 0;
        let mut id_cursor = Cursor::new(id_bytes);
        let mut group_node_count: u64 = 0;
        while id_cursor.remaining() > 0 {
            let delta = id_cursor.read_sint64().map_err(|e| e.to_string())?;
            old_id += delta;
            // Unconditional check: trusting indexdata `min_id >= 0` to
            // skip this gate was a bug - stale or hand-edited indexdata
            // that understates the real range lets a negative id flow
            // into `set_atomic`, which panics in `chunk_for_atomic` with
            // an opaque "pre_allocate only covers..." message rather
            // than the clear error users expect. See
            // DEVIATIONS.md > renumber for the policy rationale.
            if old_id < 0 {
                return Err(format!(
                    "renumber requires non-negative input ids. \
                     Input contains node id {old_id}. \
                     Negative ids are JOSM editor-local staging identifiers \
                     that should be resolved before processing."
                ));
            }
            // `set_atomic` panics when `old_id` falls outside what
            // `pre_allocate` covered, and that budget comes from indexdata -
            // which nothing verifies. A stale or hand-edited index that
            // understates the real maximum therefore turned a bad input file
            // into a worker panic. Report it as the input error it is; the
            // caller already surfaces these `String`s as command failures.
            if !id_set.set_atomic_checked(old_id) {
                return Err(format!(
                    "renumber: node id {old_id} exceeds the id range the input's \
                     indexdata declared, so the node map cannot record it. The \
                     input's blob indexdata understates its own contents; \
                     re-index it (pbfhogg cat) before renumbering."
                ));
            }
            group_node_count += 1;
        }
        total_nodes += group_node_count;

        // Build new packed ID field for this group.
        let gnc = usize::try_from(group_node_count).map_err(|_| "group node count > usize")?;
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
    node_id_set: &IdSet,
    start_node_id: i64,
    way_id_set: &mut IdSet,
    output: &mut Vec<u8>,
    refs_scratch: &mut Vec<u8>,
    group_scratch: &mut Vec<u8>,
    reframed_way_scratch: &mut Vec<u8>,
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
                let raw = cursor
                    .read_raw_field(wire_type)
                    .map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
        }
    }

    let (st_offset, st_len) =
        stringtable_range.ok_or("reframe_ways: no StringTable in PrimitiveBlock")?;
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
                    // tag_start = position before read_raw_field consumed the value.
                    // The `val_start - 1` single-byte tag is only correct for
                    // protobuf field numbers <= 15 (tag byte continuation-bit
                    // clear). If a future PBF schema adds a field >= 16 the
                    // rewriter has to splice here, the debug_assert traps the
                    // class in test rather than letting it splice mid-varint.
                    let val_start = way_bytes.len() - way_cursor.remaining();
                    if wf == 1 && wt == WIRE_VARINT {
                        debug_assert!(
                            way_bytes[val_start - 1] < 0x80,
                            "way field 1 tag byte has continuation bit set; field >= 16 schema extension?"
                        );
                        let tag_start = val_start - 1; // field 1 varint tag = 1 byte
                        old_way_id = way_cursor.read_varint_i64().map_err(|e| e.to_string())?;
                        let val_end = way_bytes.len() - way_cursor.remaining();
                        id_range = Some((tag_start, val_end));
                    } else if wf == 8 && wt == WIRE_VARINT {
                        return Err(unpacked_field_error("Way", "refs"));
                    } else if wf == 8 && wt == WIRE_LEN {
                        debug_assert!(
                            way_bytes[val_start - 1] < 0x80,
                            "way field 8 tag byte has continuation bit set; field >= 16 schema extension?"
                        );
                        let tag_start = val_start - 1; // field 8 varint tag = 1 byte
                        let seg = way_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                        let val_end = way_bytes.len() - way_cursor.remaining();
                        if !seg.is_empty() && !old_refs_data.is_empty() {
                            return Err(repeated_field_error("Way", "refs"));
                        }
                        // An empty first occurrence is recorded so a
                        // zero-ref way still splices; a later data-bearing
                        // one takes over the range.
                        if refs_range.is_none() || !seg.is_empty() {
                            old_refs_data = seg;
                            refs_range = Some((tag_start, val_end));
                        }
                    } else {
                        way_cursor.read_raw_field(wt).map_err(|e| e.to_string())?;
                    }
                }

                // Unconditional check: see the node-side rationale above.
                // A negative way id here that slipped past stale indexdata
                // would be silently dropped by `way_id_set.set` (which
                // guards `id < 0`), producing phantom orphan refs in
                // relations that still point at the old negative id.
                if old_way_id < 0 {
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
                    // Single chunk lookup per ref via `resolve_with_orphan`.
                    let (new_ref, is_orphan) =
                        resolve_with_orphan(node_id_set, old_node_id, start_node_id);
                    if is_orphan {
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
                let raw = gr_cursor
                    .read_raw_field(wire_type)
                    .map_err(|e| e.to_string())?;
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
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
pub(super) fn reframe_relations_with_new_ids(
    decompressed: &[u8],
    relation_id_set: &IdSet,
    start_relation_id: i64,
    node_id_set: &IdSet,
    start_node_id: i64,
    way_id_set: &IdSet,
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
                let raw = cursor
                    .read_raw_field(wire_type)
                    .map_err(|e| e.to_string())?;
                protohoggr::encode_tag(scalar_fields_scratch, field, wire_type);
                scalar_fields_scratch.extend_from_slice(raw);
            }
        }
    }

    let (st_offset, st_len) =
        stringtable_range.ok_or("reframe_relations: no StringTable in PrimitiveBlock")?;
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
                    // Same single-byte-tag invariant as the way branch above:
                    // `val_start - 1` is only correct for field numbers <= 15.
                    let val_start = rel_bytes.len() - rel_cursor.remaining();
                    match (rf, rt) {
                        (1, WIRE_VARINT) => {
                            debug_assert!(
                                rel_bytes[val_start - 1] < 0x80,
                                "relation field 1 tag byte has continuation bit set; field >= 16 schema extension?"
                            );
                            let tag_start = val_start - 1; // field 1 tag = 0x08, 1 byte
                            old_rel_id = rel_cursor.read_varint_i64().map_err(|e| e.to_string())?;
                            let val_end = rel_bytes.len() - rel_cursor.remaining();
                            id_range = Some((tag_start, val_end));
                        }
                        (9, WIRE_LEN) => {
                            debug_assert!(
                                rel_bytes[val_start - 1] < 0x80,
                                "relation field 9 tag byte has continuation bit set; field >= 16 schema extension?"
                            );
                            let tag_start = val_start - 1; // field 9 tag = 0x4A, 1 byte
                            let seg = rel_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                            let val_end = rel_bytes.len() - rel_cursor.remaining();
                            if !seg.is_empty() && !old_memids_data.is_empty() {
                                return Err(repeated_field_error("Relation", "memids"));
                            }
                            if memids_range.is_none() || !seg.is_empty() {
                                old_memids_data = seg;
                                memids_range = Some((tag_start, val_end));
                            }
                        }
                        (10, WIRE_LEN) => {
                            // Not patched - just captured for dispatch. Still
                            // guarded: keeping only the last segment silently
                            // re-types every member.
                            let seg = rel_cursor.read_len_delimited().map_err(|e| e.to_string())?;
                            if !seg.is_empty() {
                                if !types_data.is_empty() {
                                    return Err(repeated_field_error("Relation", "types"));
                                }
                                types_data = seg;
                            }
                        }
                        // The silent-corruption case: an unpacked `memids`
                        // left `memids_range` as None, so the splice below
                        // took the "zero-member relation" branch and copied
                        // the member IDs through un-renumbered. An unpacked
                        // `types` would leave the dispatch blind instead.
                        (9, WIRE_VARINT) => {
                            return Err(unpacked_field_error("Relation", "memids"));
                        }
                        (10, WIRE_VARINT) => {
                            return Err(unpacked_field_error("Relation", "types"));
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
                    //
                    // Counting continuation-bit-clear bytes is a proxy for
                    // the varint count that silently miscounts on truncated
                    // input (a trailing varint with no terminator, or
                    // extraneous bytes after the last terminator, both pass
                    // the proxy but break the decode loop below). A full
                    // varint walk catches every malformation here, at the
                    // boundary, rather than letting it surface as a
                    // misaligned decode further in.
                    let memids_count = count_varints_strict(old_memids_data).map_err(|e| {
                        format!("reframe_relations: relation {old_rel_id} memids: {e}")
                    })?;
                    let types_count = count_varints_strict(types_data).map_err(|e| {
                        format!("reframe_relations: relation {old_rel_id} types: {e}")
                    })?;
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

                        // Unconditional reject: same policy as the node and
                        // way rewriter entry points. A negative member ref
                        // that slipped past the node / way paths (e.g.
                        // inconsistent input where nodes/ways are positive
                        // but relation members are not, or a malformed PBF)
                        // would otherwise flow through `resolve` unchanged
                        // (cid-out-of-bounds early-return) and land in
                        // output as a phantom orphan ref.
                        if old_abs_id < 0 {
                            return Err(format!(
                                "renumber requires non-negative input ids. \
                                 Input contains relation {old_rel_id} with \
                                 member ref {old_abs_id}. Resolve JOSM-style \
                                 negative staging IDs before renumbering.",
                            ));
                        }

                        // Look up new absolute id by member type. Single
                        // chunk lookup per ref via `resolve_with_orphan`.
                        let (new_abs_id, is_orphan) = match member_type {
                            0 => resolve_with_orphan(node_id_set, old_abs_id, start_node_id),
                            1 => resolve_with_orphan(way_id_set, old_abs_id, start_way_id),
                            2 => {
                                resolve_with_orphan(relation_id_set, old_abs_id, start_relation_id)
                            }
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
                gr_cursor
                    .read_raw_field(wire_type)
                    .map_err(|e| e.to_string())?;
            }
        }

        if !group_scratch.is_empty() {
            protohoggr::encode_bytes_field(output, 2, group_scratch);
        }
    }

    output.extend_from_slice(scalar_fields_scratch);

    Ok((total_relations, min_new_id, max_new_id, orphan_refs))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            #[allow(clippy::cast_possible_truncation)]
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }

    fn len_delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
        varint(u64::from((field << 3) | 2), out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    /// PrimitiveBlock: an empty StringTable plus one group holding one
    /// Relation with id `rel_id` and a single member (node `memid`).
    /// `memids` / `types` are written packed when `packed`, unpacked
    /// otherwise. Both are legal protobuf.
    fn relation_block(rel_id: i64, memid: i64, packed: bool) -> Vec<u8> {
        let mut rel = Vec::new();
        rel.push(0x08); // field 1 (id), varint
        #[allow(clippy::cast_sign_loss)]
        varint(rel_id as u64, &mut rel);
        #[allow(clippy::cast_sign_loss)]
        let zz = ((memid << 1) ^ (memid >> 63)) as u64;
        if packed {
            let mut seg = Vec::new();
            varint(zz, &mut seg);
            len_delimited(9, &seg, &mut rel); // memids
        } else {
            rel.push(0x48); // field 9 (memids), VARINT - the unpacked form
            varint(zz, &mut rel);
        }
        // `types` stays packed in BOTH fixtures on purpose: it isolates the
        // memids guard. If types were unpacked too, the sibling types guard
        // would fire first and the message assertion below would pass while
        // testing the wrong thing.
        len_delimited(10, &[0x00], &mut rel); // types: [0] = node

        let mut group = Vec::new();
        len_delimited(4, &rel, &mut group); // PrimitiveGroup.relations

        let mut block = Vec::new();
        len_delimited(1, &[], &mut block); // StringTable
        len_delimited(2, &group, &mut block); // PrimitiveGroup
        block
    }

    struct Scratch {
        output: Vec<u8>,
        memids: Vec<u8>,
        group: Vec<u8>,
        rel: Vec<u8>,
        ranges: Vec<(usize, usize)>,
        scalars: Vec<u8>,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                output: Vec::new(),
                memids: Vec::new(),
                group: Vec::new(),
                rel: Vec::new(),
                ranges: Vec::new(),
                scalars: Vec::new(),
            }
        }
    }

    fn run(block: &[u8], s: &mut Scratch) -> std::result::Result<(u64, i64, i64, u64), String> {
        let mut node_ids = IdSet::new();
        node_ids.set(7);
        node_ids.build_rank_index();
        let mut rel_ids = IdSet::new();
        rel_ids.set(5);
        rel_ids.build_rank_index();
        let mut way_ids = IdSet::new();
        way_ids.build_rank_index();

        reframe_relations_with_new_ids(
            block,
            &rel_ids,
            1_000,
            &node_ids,
            100,
            &way_ids,
            500,
            &mut s.output,
            &mut s.memids,
            &mut s.group,
            &mut s.rel,
            &mut s.ranges,
            &mut s.scalars,
        )
    }

    #[allow(clippy::cast_sign_loss)]
    fn zz(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    /// PrimitiveBlock whose single Relation writes its two members across
    /// `memids_segments` occurrences of field 9 and `types_segments`
    /// occurrences of field 10. Both spellings are legal protobuf and mean
    /// exactly the same two-member relation - protobuf concatenates repeated
    /// packed segments.
    fn relation_block_segmented(
        rel_id: i64,
        memids: &[i64],
        memids_segments: usize,
        types_segments: usize,
    ) -> Vec<u8> {
        let mut rel = Vec::new();
        rel.push(0x08); // field 1 (id), varint
        #[allow(clippy::cast_sign_loss)]
        varint(rel_id as u64, &mut rel);

        // Delta-encode the member ids, then split the varint stream into the
        // requested number of length-delimited occurrences.
        let mut per_member: Vec<Vec<u8>> = Vec::new();
        let mut prev = 0_i64;
        for &m in memids {
            let mut v = Vec::new();
            varint(zz(m - prev), &mut v);
            prev = m;
            per_member.push(v);
        }
        let chunk = per_member.len().div_ceil(memids_segments.max(1));
        for seg in per_member.chunks(chunk.max(1)) {
            let mut bytes = Vec::new();
            for v in seg {
                bytes.extend_from_slice(v);
            }
            len_delimited(9, &bytes, &mut rel);
        }

        // types: one 0 (node) per member, split the same way.
        let types: Vec<u8> = vec![0x00; memids.len()];
        let tchunk = types.len().div_ceil(types_segments.max(1));
        for seg in types.chunks(tchunk.max(1)) {
            len_delimited(10, seg, &mut rel);
        }

        let mut group = Vec::new();
        len_delimited(4, &rel, &mut group);
        let mut block = Vec::new();
        len_delimited(1, &[], &mut block);
        len_delimited(2, &group, &mut block);
        block
    }

    /// Splitting a packed field across two data-bearing occurrences is legal
    /// protobuf with concatenation semantics. The splice rewriter patches one
    /// contiguous byte range, so it used to keep only the LAST occurrence's
    /// range: the earlier segment's member IDs were copied through verbatim
    /// and un-renumbered while the tail was renumbered, producing partially
    /// renumbered output with no error. The module comment already promised a
    /// refusal here; this is the guard that keeps that promise.
    /// PrimitiveBlock holding one Way whose `refs` are written across
    /// `segments` occurrences of field 8.
    fn way_block_segmented(way_id: i64, refs: &[i64], segments: usize) -> Vec<u8> {
        let mut way = Vec::new();
        way.push(0x08);
        #[allow(clippy::cast_sign_loss)]
        varint(way_id as u64, &mut way);

        let mut per_ref: Vec<Vec<u8>> = Vec::new();
        let mut prev = 0_i64;
        for &r in refs {
            let mut v = Vec::new();
            varint(zz(r - prev), &mut v);
            prev = r;
            per_ref.push(v);
        }
        let chunk = per_ref.len().div_ceil(segments.max(1));
        for seg in per_ref.chunks(chunk.max(1)) {
            let mut bytes = Vec::new();
            for v in seg {
                bytes.extend_from_slice(v);
            }
            len_delimited(8, &bytes, &mut way);
        }

        let mut group = Vec::new();
        len_delimited(3, &way, &mut group);
        let mut block = Vec::new();
        len_delimited(1, &[], &mut block);
        len_delimited(2, &group, &mut block);
        block
    }

    fn run_ways(block: &[u8]) -> std::result::Result<(u64, u64), String> {
        let mut node_ids = IdSet::new();
        node_ids.set(7);
        node_ids.build_rank_index();
        let mut way_ids = IdSet::new();
        let (mut out, mut refs, mut group, mut way) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut ranges, mut scalars) = (Vec::new(), Vec::new());
        reframe_ways_with_new_ids(
            block,
            1_000,
            &node_ids,
            100,
            &mut way_ids,
            &mut out,
            &mut refs,
            &mut group,
            &mut way,
            &mut ranges,
            &mut scalars,
        )
    }

    /// Same class as the relation case, and the most damaging of the three:
    /// with two `refs` segments the splice replaced only the last one, so the
    /// earlier segment's node refs were copied through pointing at OLD node
    /// IDs inside a file that claims every ID was renumbered. No error, no
    /// orphan count, nothing.
    #[test]
    fn way_refs_split_across_two_segments_is_refused() {
        let err = run_ways(&way_block_segmented(3, &[7, 9], 2))
            .expect_err("two data-bearing refs segments must be refused");
        assert!(
            err.contains("Way.refs appears twice with data"),
            "expected the Way.refs repeated-occurrence guard specifically - no \
             other guard in this rewriter names Way.refs and a repeated \
             occurrence together, and without it the rewrite SUCCEEDS with \
             half the refs unrenumbered. Got: {err}"
        );
    }

    #[test]
    fn a_single_segment_two_ref_way_is_still_accepted() {
        let (ways, orphans) = run_ways(&way_block_segmented(3, &[7, 9], 1))
            .expect("one refs segment must be accepted");
        assert_eq!(ways, 1);
        assert_eq!(
            orphans, 1,
            "node 7 is in the set and node 9 is not, so exactly one ref is an \
             orphan - proving both refs were actually resolved"
        );
    }

    /// PrimitiveBlock holding one DenseNodes whose `id` deltas are written
    /// across `segments` occurrences of field 1.
    fn dense_block_segmented(ids: &[i64], segments: usize) -> Vec<u8> {
        let mut per_id: Vec<Vec<u8>> = Vec::new();
        let mut prev = 0_i64;
        for &id in ids {
            let mut v = Vec::new();
            varint(zz(id - prev), &mut v);
            prev = id;
            per_id.push(v);
        }
        let mut dense = Vec::new();
        let chunk = per_id.len().div_ceil(segments.max(1));
        for seg in per_id.chunks(chunk.max(1)) {
            let mut bytes = Vec::new();
            for v in seg {
                bytes.extend_from_slice(v);
            }
            len_delimited(1, &bytes, &mut dense);
        }

        let mut group = Vec::new();
        len_delimited(2, &dense, &mut group);
        let mut block = Vec::new();
        len_delimited(1, &[], &mut block);
        len_delimited(2, &group, &mut block);
        block
    }

    fn run_dense(block: &[u8]) -> std::result::Result<u64, String> {
        let mut ids = IdSet::new();
        ids.pre_allocate(1_000);
        let mut out = Vec::new();
        let (mut ranges, mut scalars, mut others) = (Vec::new(), Vec::new(), Vec::new());
        let (mut packed, mut dense, mut group) = (Vec::new(), Vec::new(), Vec::new());
        reframe_dense_with_new_ids(
            block,
            1,
            &ids,
            &mut out,
            &mut ranges,
            &mut scalars,
            &mut others,
            &mut packed,
            &mut dense,
            &mut group,
        )
    }

    /// The DenseNodes half. Keeping only the last `id` segment understated the
    /// node count, so every downstream ID base computed from it was wrong and
    /// the earlier segment's nodes were never entered into the node map - they
    /// became orphan refs in every way that pointed at them.
    #[test]
    fn dense_node_ids_split_across_two_segments_are_refused() {
        let err = run_dense(&dense_block_segmented(&[10, 20], 2))
            .expect_err("two data-bearing DenseNodes.id segments must be refused");
        assert!(
            err.contains("DenseNodes.id appears twice with data"),
            "expected the DenseNodes.id repeated-occurrence guard specifically; \
             without it the call SUCCEEDS and simply reports one node instead \
             of two. Got: {err}"
        );
    }

    #[test]
    fn a_single_segment_dense_block_is_still_accepted() {
        assert_eq!(
            run_dense(&dense_block_segmented(&[10, 20], 1)).expect("one id segment is accepted"),
            2,
            "both node ids must be counted, so the guard cannot have become a \
             refuse-every-dense-block"
        );
    }

    #[test]
    fn relation_memids_split_across_two_segments_is_refused() {
        let mut s = Scratch::new();
        let err = run(&relation_block_segmented(5, &[7, 9], 2, 1), &mut s)
            .expect_err("two data-bearing memids segments must be refused");
        assert!(
            err.contains("Relation.memids appears twice with data"),
            "expected the memids repeated-occurrence guard specifically. Only \
             that guard names Relation.memids together with a repeated \
             occurrence; without it the sibling memids/types count cross-check \
             fires instead, with a misleading message about a member count \
             mismatch. Got: {err}"
        );
    }

    /// `types` is not spliced - it is only read for the per-member dispatch -
    /// so keeping the last segment gives a member array typed against the
    /// wrong ID spaces, or a spurious count mismatch. Guarded too.
    #[test]
    fn relation_types_split_across_two_segments_is_refused() {
        let mut s = Scratch::new();
        let err = run(&relation_block_segmented(5, &[7, 9], 1, 2), &mut s)
            .expect_err("two data-bearing types segments must be refused");
        assert!(
            err.contains("Relation.types appears twice with data"),
            "expected the types repeated-occurrence guard specifically; the \
             count cross-check would otherwise claim a memids/types length \
             mismatch. Got: {err}"
        );
    }

    /// Passing counterpart for both guards: the single-segment spelling of the
    /// same two-member relation must still be accepted and renumbered. A
    /// refuse-every-relation regression fails here.
    #[test]
    fn a_single_segment_two_member_relation_is_still_accepted() {
        let mut s = Scratch::new();
        let (count, _min, _max, orphans) = run(&relation_block_segmented(5, &[7, 9], 1, 1), &mut s)
            .expect("one memids segment and one types segment must be accepted");
        assert_eq!(count, 1);
        assert_eq!(
            orphans, 1,
            "node 7 is in the set and node 9 is not, so exactly one member is \
             an orphan - proving both members were actually walked"
        );
    }

    #[test]
    fn unpacked_relation_memids_are_rejected_not_silently_passed_through() {
        // This is the silent-corruption case. With `memids` unpacked, the
        // splice rewriter found no `memids_range`, took its "zero-member
        // relation" branch, patched only the id, and copied the member IDs
        // through UN-RENUMBERED - into output that claims to be renumbered.
        let mut s = Scratch::new();
        let err = run(&relation_block(5, 7, false), &mut s)
            .expect_err("an unpacked memids field must be refused, not mis-spliced");
        assert!(
            err.contains("Relation.memids is written in the unpacked wire form"),
            "expected the memids unpacked-form guard specifically; only that \
             guard names Relation.memids and the unpacked form together. Got: {err}"
        );

        // Passing counterpart: the packed spelling of the identical relation
        // must succeed and actually renumber the member. node 7 is the only
        // node in the set, so rank 0 -> new id 100.
        let mut s = Scratch::new();
        let (count, _min, _max, orphans) =
            run(&relation_block(5, 7, true), &mut s).expect("the packed form must be accepted");
        assert_eq!(count, 1);
        assert_eq!(orphans, 0, "node 7 is present, so its ref is not an orphan");
        assert!(
            !s.output.is_empty(),
            "the packed path must actually emit a rewritten block"
        );
    }
}
