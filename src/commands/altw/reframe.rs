//! Wire-format way-blob reframe for the dense / sparse pass 2 way arm.
//!
//! Lifted from `external/stage4.rs::reframe_way_blob_with_locations`. Same
//! structure (StringTable byte-pass, group iteration, way reframe) but the
//! coordinate source is a `NodeIndex` lookup instead of a stage-3
//! pre-encoded `coord_payload`.
//!
//! Hot path: decompress -> walk wire -> per-way `NodeIndex::get` per ref ->
//! zigzag-delta-encode lat / lon -> append fields 9 / 10 to original way
//! bytes -> compress. No `BlockBuilder`, no `StringTable::add`, no Info
//! decode / encode, no ref redelta, no tag re-intern.
//!
//! If the input way already declares fields 9 and 10 (input is itself a
//! `LocationsOnWays` PBF), those fields are stripped before append - we
//! own the coordinates from `NodeIndex`, the input's are stale at best.

use protohoggr::{
    Cursor, PackedSint64Iter, WIRE_LEN, WIRE_VARINT, encode_bytes_field, encode_int64_field,
    encode_tag, encode_varint, zigzag_encode_64,
};

use super::NodeIndex;

/// Per-blob scratch buffers reused across way-blob reframes within a worker.
#[derive(Default)]
pub(super) struct WayReframeScratch {
    pub group_ranges: Vec<(usize, usize)>,
    pub scalar_fields: Vec<u8>,
    pub group_out: Vec<u8>,
    pub reframed_way: Vec<u8>,
    pub packed_lats: Vec<u8>,
    pub packed_lons: Vec<u8>,
    pub refs: Vec<i64>,
    pub member_ways: Vec<bool>,
    pub pins: Vec<u8>,
    /// This blob's field-20 tally under `--inject-prepass`; reset at the
    /// start of every reframe, read by the worker once it succeeds.
    pub pin_tally: super::inject_metrics::PinTally,
}

/// Per-blob counters that the caller folds into `Stats` after the reframe.
#[derive(Default)]
pub(super) struct WayReframeStats {
    pub way_count: u64,
    pub min_way_id: i64,
    pub max_way_id: i64,
    pub missing_locations: u64,
}

/// Reframe one decompressed way-blob `PrimitiveBlock` by appending packed
/// `lat` / `lon` fields (9 / 10) to each way using `node_index` for
/// coordinate lookups. Writes the resulting `PrimitiveBlock` bytes into
/// `output` (caller compresses + writes).
///
/// Errors are returned as `String` to match the existing par_iter result
/// channel in `process_slot_batch`.
#[hotpath::measure]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn reframe_way_blob_with_locations(
    decompressed: &[u8],
    node_index: &NodeIndex,
    output: &mut Vec<u8>,
    scratch: &mut WayReframeScratch,
    shared_node_ids: Option<&crate::idset::IdSet>,
    member_way_ids: Option<&crate::idset::IdSet>,
    inject_prepass: bool,
) -> std::result::Result<WayReframeStats, String> {
    if inject_prepass {
        scratch.pin_tally = super::inject_metrics::PinTally::default();
    }
    let (st_offset, st_len) = parse_block_top(decompressed, scratch)?;
    let stringtable_bytes = &decompressed[st_offset..st_offset + st_len];

    output.clear();
    encode_bytes_field(output, 1, stringtable_bytes);

    let mut stats = WayReframeStats {
        min_way_id: i64::MAX,
        max_way_id: i64::MIN,
        ..WayReframeStats::default()
    };
    scratch.member_ways.clear();

    for i in 0..scratch.group_ranges.len() {
        let (gr_offset, gr_len) = scratch.group_ranges[i];
        let group_bytes = &decompressed[gr_offset..gr_offset + gr_len];
        process_group(
            group_bytes,
            node_index,
            output,
            scratch,
            &mut stats,
            shared_node_ids,
            member_way_ids,
            inject_prepass,
        )?;
    }

    output.extend_from_slice(&scratch.scalar_fields);

    if stats.way_count == 0 {
        stats.min_way_id = 0;
        stats.max_way_id = 0;
    }

    Ok(stats)
}

/// Walk the `PrimitiveBlock` top-level fields. Captures the StringTable
/// byte range (no parse), group offsets, and scalar fields (17-20)
/// verbatim into `scratch`. Returns the StringTable's `(offset, len)` in
/// `decompressed`.
fn parse_block_top(
    decompressed: &[u8],
    scratch: &mut WayReframeScratch,
) -> std::result::Result<(usize, usize), String> {
    scratch.group_ranges.clear();
    scratch.scalar_fields.clear();
    let mut stringtable_range: Option<(usize, usize)> = None;

    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|e| format!("reframe block tag: {e}"))?
    {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("reframe st: {e}"))?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                stringtable_range = Some((offset, data.len()));
            }
            (2, WIRE_LEN) => {
                let data = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("reframe group: {e}"))?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                scratch.group_ranges.push((offset, data.len()));
            }
            (17..=20, WIRE_VARINT) => {
                let raw = cursor
                    .read_raw_field(wire_type)
                    .map_err(|e| format!("reframe scalar: {e}"))?;
                check_default_coord_framing(field, raw)?;
                encode_tag(&mut scratch.scalar_fields, field, wire_type);
                scratch.scalar_fields.extend_from_slice(raw);
            }
            _ => cursor
                .skip_field(wire_type)
                .map_err(|e| format!("reframe skip: {e}"))?,
        }
    }

    stringtable_range.ok_or_else(|| "reframe: no StringTable in PrimitiveBlock".to_string())
}

/// Refuse a way blob whose coordinate framing is not the PBF default.
///
/// The reframe path writes `Way.lat` / `Way.lon` (fields 9 / 10) straight
/// from `NodeIndex`, which stores decimicrodegrees, while copying the source
/// block's `granularity` (17), `lat_offset` (19) and `lon_offset` (20)
/// verbatim into the output. The reader reconstructs
/// `lat = lat_offset + granularity * value`, so the round trip is only
/// correct when `granularity == 100` and both offsets are zero. On a block
/// declaring, say, `granularity = 1000` every way coordinate this path emits
/// is off by a factor of ten - and it is emitted as well-formed PBF that
/// reads back cleanly, so nothing downstream ever notices.
///
/// Dropping the fields instead is not safe in general: a mixed blob's
/// DenseNodes are copied through verbatim and would be reinterpreted under
/// the new framing. Refusing is the honest option. `write/block_builder.rs`
/// omits 17 / 19 / 20 entirely and relies on the defaults, so every pbfhogg
/// and osmium producer passes this check; field 18 (`date_granularity`) does
/// not affect coordinates and is copied unexamined.
pub(in crate::commands::altw) fn check_default_coord_framing(
    field: u32,
    raw: &[u8],
) -> std::result::Result<(), String> {
    let (name, default) = match field {
        17 => ("granularity", 100_u64),
        19 => ("lat_offset", 0),
        20 => ("lon_offset", 0),
        // 18 is date_granularity - irrelevant to coordinates.
        _ => return Ok(()),
    };
    let mut value: u64 = 0;
    for (i, byte) in raw.iter().enumerate() {
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            break;
        }
    }
    if value == default {
        return Ok(());
    }
    Err(format!(
        "reframe: PrimitiveBlock.{name} is {value}, not the default {default}. \
         add-locations-to-ways writes way coordinates in decimicrodegrees and \
         cannot honour a non-default coordinate framing on this path; the \
         output would be silently wrong. Re-encode the input first, e.g. \
         `pbfhogg cat --clean`."
    ))
}

/// Walk one `PrimitiveGroup`. Way submessages get reframed (locations
/// appended); non-way fields are copied verbatim. Builds the
/// `PrimitiveGroup` bytes in `scratch.group_out`, then emits as field 2
/// of the `PrimitiveBlock` in `output`.
#[allow(clippy::too_many_arguments)]
fn process_group(
    group_bytes: &[u8],
    node_index: &NodeIndex,
    output: &mut Vec<u8>,
    scratch: &mut WayReframeScratch,
    stats: &mut WayReframeStats,
    shared_node_ids: Option<&crate::idset::IdSet>,
    member_way_ids: Option<&crate::idset::IdSet>,
    inject_prepass: bool,
) -> std::result::Result<(), String> {
    scratch.group_out.clear();

    let mut gr_cursor = Cursor::new(group_bytes);
    while let Some((field, wire_type)) = gr_cursor
        .read_tag()
        .map_err(|e| format!("reframe gtag: {e}"))?
    {
        if field == 3 && wire_type == WIRE_LEN {
            let way_bytes = gr_cursor
                .read_len_delimited()
                .map_err(|e| format!("reframe way: {e}"))?;
            splice_way_locations(
                way_bytes,
                node_index,
                scratch,
                stats,
                shared_node_ids,
                member_way_ids,
                inject_prepass,
            )?;
            encode_bytes_field(&mut scratch.group_out, 3, &scratch.reframed_way);
        } else {
            let raw = gr_cursor
                .read_raw_field(wire_type)
                .map_err(|e| format!("reframe gskip: {e}"))?;
            encode_tag(&mut scratch.group_out, field, wire_type);
            scratch.group_out.extend_from_slice(raw);
        }
    }

    encode_bytes_field(output, 2, &scratch.group_out);
    Ok(())
}

/// Reframe a single Way submessage. Copies every existing field verbatim
/// EXCEPT fields 9 (lat) and 10 (lon) - those are stripped if present
/// (input was already a LocationsOnWays PBF) since we own the
/// coordinates. Then appends fresh fields 9 / 10 from `node_index`
/// lookups.
#[allow(clippy::too_many_lines)]
fn splice_way_locations(
    way_bytes: &[u8],
    node_index: &NodeIndex,
    scratch: &mut WayReframeScratch,
    stats: &mut WayReframeStats,
    shared_node_ids: Option<&crate::idset::IdSet>,
    member_way_ids: Option<&crate::idset::IdSet>,
    inject_prepass: bool,
) -> std::result::Result<(), String> {
    scratch.reframed_way.clear();
    scratch.refs.clear();
    scratch.packed_lats.clear();
    scratch.packed_lons.clear();
    scratch.pins.clear();

    let mut way_id: i64 = 0;
    let mut have_id = false;
    let mut have_refs = false;

    let mut way_cursor = Cursor::new(way_bytes);
    while let Some((wf, wt)) = way_cursor
        .read_tag()
        .map_err(|e| format!("reframe wtag: {e}"))?
    {
        match (wf, wt) {
            (1, WIRE_VARINT) => {
                way_id = way_cursor
                    .read_varint_i64()
                    .map_err(|e| format!("reframe wid: {e}"))?;
                have_id = true;
                encode_int64_field(&mut scratch.reframed_way, 1, way_id);
            }
            // Both legal wire forms of the packed `refs` field. Matching only
            // WIRE_LEN meant a spec-legal unpacked `refs` fell to the verbatim
            // `_` arm: `scratch.refs` stayed empty, so no lat / lon fields were
            // appended and the way came out of add-locations-to-ways with no
            // locations at all - silently, since the raw bytes still copied
            // through. A single unpacked varint is byte-identical to a
            // one-element packed segment, so it is normalized to the packed
            // spelling on output. Mirrors `src/read/wire.rs::take_packed`.
            (8, WIRE_LEN | WIRE_VARINT) => {
                let refs_data = if wt == WIRE_LEN {
                    way_cursor
                        .read_len_delimited()
                        .map_err(|e| format!("reframe wrefs: {e}"))?
                } else {
                    way_cursor
                        .read_raw_field(WIRE_VARINT)
                        .map_err(|e| format!("reframe wrefs: {e}"))?
                };
                if !refs_data.is_empty() {
                    if have_refs {
                        // Same policy, and the same boundary, as the read
                        // path: two data-bearing occurrences of a packed field
                        // mean concatenation, which this single-slice rewriter
                        // cannot express. That covers both a packed field
                        // split across segments and more than one unpacked
                        // value, since each unpacked value is its own
                        // occurrence - which is why unpacked support here is
                        // exact for a one-element array and an error beyond
                        // it, matching `src/read/wire.rs::take_packed`.
                        return Err(
                            "reframe: Way.refs appears twice with data - a packed field \
                             split across segments, or more than one unpacked value. Re-encode \
                             the input first, e.g. `pbfhogg cat --clean`."
                                .to_string(),
                        );
                    }
                    have_refs = true;
                    let mut cum: i64 = 0;
                    for delta in PackedSint64Iter::new(refs_data) {
                        cum += delta;
                        scratch.refs.push(cum);
                    }
                    encode_tag(&mut scratch.reframed_way, 8, WIRE_LEN);
                    encode_varint(&mut scratch.reframed_way, refs_data.len() as u64);
                    scratch.reframed_way.extend_from_slice(refs_data);
                } else if wt == WIRE_LEN {
                    // An empty packed occurrence carries no refs, but dropping
                    // it would change the output bytes for an input this
                    // rewriter used to copy through unchanged. `add-locations-
                    // to-ways` output is compared byte-wise against osmium in
                    // `brokkr verify`, so the normalization must be a no-op on
                    // every packed input, including the degenerate one.
                    encode_tag(&mut scratch.reframed_way, 8, WIRE_LEN);
                    encode_varint(&mut scratch.reframed_way, 0);
                }
            }
            // Stripped: we own the coordinates. Both wire forms, or an
            // unpacked stale lat / lon would survive alongside the fresh
            // packed one we append below and the way would carry two
            // occurrences of field 9.
            (9 | 10 | 20, WIRE_LEN | WIRE_VARINT) => {
                let _ = way_cursor
                    .read_raw_field(wt)
                    .map_err(|e| format!("reframe wstrip: {e}"))?;
            }
            _ => {
                let raw = way_cursor
                    .read_raw_field(wt)
                    .map_err(|e| format!("reframe wskip: {e}"))?;
                encode_tag(&mut scratch.reframed_way, wf, wt);
                scratch.reframed_way.extend_from_slice(raw);
            }
        }
    }

    if !have_id {
        return Err("reframe: way without field 1 (id)".to_string());
    }

    if way_id < stats.min_way_id {
        stats.min_way_id = way_id;
    }
    if way_id > stats.max_way_id {
        stats.max_way_id = way_id;
    }
    stats.way_count += 1;
    scratch
        .member_ways
        .push(inject_prepass && member_way_ids.is_some_and(|ids| ids.get(way_id)));

    // Field 20 is a fixed-width ceil(ref_count/8) bitmap; size it up front so
    // trailing unpinned refs still contribute their bytes (the consumer
    // validates the length, and the external backend emits full width too).
    if inject_prepass {
        scratch.pins.resize(scratch.refs.len().div_ceil(8), 0);
    }
    let mut last_lat: i64 = 0;
    let mut last_lon: i64 = 0;
    for (idx, &node_id) in scratch.refs.iter().enumerate() {
        let (lat, lon, resolved) = match node_index.get(node_id) {
            Some(loc) => (i64::from(loc.0), i64::from(loc.1), true),
            None => {
                stats.missing_locations += 1;
                (0, 0, false)
            }
        };
        encode_varint(&mut scratch.packed_lats, zigzag_encode_64(lat - last_lat));
        encode_varint(&mut scratch.packed_lons, zigzag_encode_64(lon - last_lon));
        last_lat = lat;
        last_lon = lon;
        if inject_prepass && resolved && shared_node_ids.is_some_and(|ids| ids.get(node_id)) {
            scratch.pins[idx / 8] |= 1 << (idx % 8);
        }
    }

    if inject_prepass {
        scratch.pin_tally.record_way(&scratch.pins);
    }
    if !scratch.refs.is_empty() {
        encode_bytes_field(&mut scratch.reframed_way, 9, &scratch.packed_lats);
        encode_bytes_field(&mut scratch.reframed_way, 10, &scratch.packed_lons);
        if inject_prepass && scratch.pins.iter().any(|&byte| byte != 0) {
            encode_bytes_field(&mut scratch.reframed_way, 20, &scratch.pins);
        }
    }

    Ok(())
}

/// Field-5 payload for a way blob. Bits are in file order and identify ways
/// that participate in a multipolygon or boundary relation.
pub(super) fn way_members_payload(way_count: u64, members: &[bool]) -> Vec<u8> {
    let mut payload = vec![1];
    encode_varint(&mut payload, way_count);
    let mut bits = vec![0; members.len().div_ceil(8)];
    for (i, member) in members.iter().enumerate() {
        if *member {
            bits[i / 8] |= 1 << (i % 8);
        }
    }
    payload.extend_from_slice(&bits);
    payload
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{NodeIndex, WayReframeScratch, reframe_way_blob_with_locations};

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

    #[allow(clippy::cast_sign_loss)]
    fn zz(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    /// PrimitiveBlock: empty StringTable, one group, one Way with `refs`
    /// written packed or unpacked. Both spellings are legal protobuf and mean
    /// the same way.
    fn way_block(way_id: i64, refs: &[i64], packed: bool) -> Vec<u8> {
        let mut way = Vec::new();
        way.push(0x08); // field 1 (id), varint
        #[allow(clippy::cast_sign_loss)]
        varint(way_id as u64, &mut way);

        let mut deltas = Vec::new();
        let mut prev = 0_i64;
        for &r in refs {
            deltas.push(zz(r - prev));
            prev = r;
        }
        if packed {
            let mut seg = Vec::new();
            for &d in &deltas {
                varint(d, &mut seg);
            }
            len_delimited(8, &seg, &mut way);
        } else {
            for &d in &deltas {
                way.push(0x40); // field 8, VARINT - the unpacked spelling
                varint(d, &mut way);
            }
        }

        let mut group = Vec::new();
        len_delimited(3, &way, &mut group);
        let mut block = Vec::new();
        len_delimited(1, &[], &mut block);
        len_delimited(2, &group, &mut block);
        block
    }

    fn try_run(block: &[u8]) -> std::result::Result<(Vec<u8>, u64, u64), String> {
        let index = NodeIndex::InMemory([(7_i64, (100_i32, 200_i32))].into_iter().collect());
        let mut out = Vec::new();
        let mut scratch = WayReframeScratch::default();
        let stats = reframe_way_blob_with_locations(
            block,
            &index,
            &mut out,
            &mut scratch,
            None,
            None,
            false,
        )?;
        Ok((out, stats.way_count, stats.missing_locations))
    }

    fn run(block: &[u8]) -> (Vec<u8>, u64, u64) {
        try_run(block).expect("reframe must succeed")
    }

    /// The silent-output bug. `refs` written in the unpacked wire form fell to
    /// the verbatim-copy arm, so `scratch.refs` stayed empty, the `lat`/`lon`
    /// append was skipped entirely, and `add-locations-to-ways` emitted a way
    /// with NO locations - no error, no missing-location count, nothing to
    /// notice.
    ///
    /// The single ref is deliberate. Exactly as in `src/read/wire.rs`, a
    /// one-element unpacked array is byte-identical to a one-element packed
    /// segment and is therefore accepted; two or more unpacked values are two
    /// data-bearing occurrences of the same field, which is the refused shape
    /// (see `unpacked_multi_value_refs_are_refused_not_half_handled`). Node 9
    /// is absent from the index, so a correct walk reports it missing - a zero
    /// there is exactly the bug, and indistinguishable from a way with no refs.
    #[test]
    fn unpacked_way_refs_still_get_locations_appended() {
        let (out, ways, missing) = run(&way_block(42, &[9], false));
        assert_eq!(ways, 1);
        assert_eq!(
            missing, 1,
            "the ref must be resolved against the index; a zero means the refs \
             were never walked at all"
        );
        assert!(
            out.contains(&0x4A),
            "the reframed way must carry a packed field 9 (lat); appending no \
             locations is the observable symptom of the bug"
        );
    }

    /// The normalization must be exact, in both directions.
    ///
    /// `add-locations-to-ways` output is compared byte-wise against osmium in
    /// `brokkr verify`, so re-emitting the packed form must be a no-op for
    /// every packed input - and the unpacked spelling of the same way must
    /// normalize to precisely the bytes the packed spelling produces.
    #[test]
    fn packed_and_unpacked_spellings_produce_identical_output_bytes() {
        let (packed_out, _, packed_missing) = run(&way_block(42, &[9], true));
        let (unpacked_out, _, unpacked_missing) = run(&way_block(42, &[9], false));
        assert_eq!(packed_missing, unpacked_missing);
        assert_eq!(
            packed_out, unpacked_out,
            "the two legal spellings of the same way must reframe to the same \
             bytes"
        );

        // And the packed input's own refs segment survives verbatim: the
        // rewriter re-emits field 8 itself, so a normalization that changed
        // the encoding would silently diverge from osmium byte-for-byte. This
        // is also the assertion that a multi-ref packed way must not disturb:
        // the packed path is untouched by the unpacked-form acceptance.
        let refs_segment = [0x42_u8, 0x01, 0x12]; // field 8, len 1, zigzag(9) = 18
        assert!(
            packed_out
                .windows(refs_segment.len())
                .any(|w| w == refs_segment),
            "the packed refs field must be re-emitted byte-identically"
        );

        // Multi-ref packed ways - the overwhelmingly common shape - are
        // likewise untouched: two refs, both walked, one resolvable.
        let (multi_out, ways, missing) = run(&way_block(42, &[7, 9], true));
        assert_eq!(ways, 1);
        assert_eq!(missing, 1, "node 7 hits and node 9 misses");
        let multi_segment = [0x42_u8, 0x02, 0x0E, 0x04]; // len 2, zigzag(7), zigzag(2)
        assert!(
            multi_out
                .windows(multi_segment.len())
                .any(|w| w == multi_segment),
            "a two-ref packed segment must survive byte-identically too"
        );
    }

    /// Two unpacked values are two data-bearing occurrences of `refs`, which
    /// protobuf concatenates and this single-slice rewriter cannot express.
    /// Refusing is the point: keeping only one occurrence would append
    /// coordinates for half the way's nodes, which is worse than appending
    /// none. Same policy, and same boundary, as `src/read/wire.rs`.
    #[test]
    fn unpacked_multi_value_refs_are_refused_not_half_handled() {
        let err = try_run(&way_block(42, &[7, 9], false))
            .expect_err("two unpacked refs values must be refused");
        assert!(
            err.contains("Way.refs appears twice with data"),
            "expected the repeated-occurrence guard specifically. Got: {err}"
        );
    }

    /// Append a `PrimitiveBlock` scalar varint field (17 / 19 / 20) to a
    /// block built by `way_block`.
    #[allow(clippy::cast_sign_loss)]
    fn with_scalar(mut block: Vec<u8>, field: u32, value: i64) -> Vec<u8> {
        varint(u64::from(field << 3), &mut block); // VARINT wire type = 0
        varint(value as u64, &mut block);
        block
    }

    /// A block whose coordinate framing is not the PBF default must be
    /// refused, not reframed.
    ///
    /// This path writes fields 9 / 10 straight from `NodeIndex`, which holds
    /// decimicrodegrees, while copying `granularity` / `lat_offset` /
    /// `lon_offset` verbatim into the output. The reader reconstructs
    /// `lat = lat_offset + granularity * value`, so on a `granularity = 1000`
    /// block every way coordinate came out ten times too large - written as
    /// well-formed PBF that reads back cleanly, with no downstream check able
    /// to notice.
    #[test]
    fn non_default_coordinate_framing_is_refused() {
        for (field, value, name) in [
            (17_u32, 1000_i64, "granularity"),
            (19, 500_000_000, "lat_offset"),
            (20, -500_000_000, "lon_offset"),
        ] {
            let block = with_scalar(way_block(42, &[7], true), field, value);
            let err = try_run(&block).expect_err("non-default framing must be refused");
            assert!(
                err.contains(name) && err.contains("not the default"),
                "expected the coordinate-framing guard naming {name}. Got: {err}"
            );
        }
    }

    /// The guard must not fire on the framing every real producer writes.
    /// Explicit defaults are legal protobuf and must reframe exactly as an
    /// absent field does, and `date_granularity` (18) must pass untouched -
    /// it has nothing to do with coordinates.
    #[test]
    fn default_coordinate_framing_and_date_granularity_still_pass() {
        let plain = run(&way_block(42, &[7], true)).0;

        let explicit = with_scalar(
            with_scalar(with_scalar(way_block(42, &[7], true), 17, 100), 19, 0),
            20,
            0,
        );
        let (explicit_out, ways, missing) = run(&explicit);
        assert_eq!((ways, missing), (1, 0));
        // The reframed output is the group bytes followed by the copied
        // scalar fields, so an explicitly-default block must produce exactly
        // the implicit-default output plus those three fields verbatim.
        let mut expected = plain.clone();
        expected.extend_from_slice(&[0x88, 0x01, 100, 0x98, 0x01, 0, 0xA0, 0x01, 0]);
        assert_eq!(
            explicit_out, expected,
            "explicitly-default framing must reframe identically and copy \
             fields 17 / 19 / 20 through unchanged"
        );

        let dated = with_scalar(way_block(42, &[7], true), 18, 1000);
        let (_, ways, missing) = run(&dated);
        assert_eq!(
            (ways, missing),
            (1, 0),
            "date_granularity is not a coordinate framing field"
        );
    }

    /// Passing counterpart: a way whose refs are genuinely absent still gets
    /// no locations, so the fix is not "append locations unconditionally".
    #[test]
    fn a_way_with_no_refs_gets_no_locations() {
        let (_out, ways, missing) = run(&way_block(42, &[], true));
        assert_eq!(ways, 1);
        assert_eq!(missing, 0);
    }
}
