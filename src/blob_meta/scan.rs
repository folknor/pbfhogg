//! Strict PrimitiveBlock scanner for blob sidecar metadata: element kind, ID
//! range, count, spatial bbox, intra-blob sortedness, and the tag key set.
//!
//! # Contract
//!
//! Both entry points return `Result<Option<_>>`, and the three outcomes are
//! distinct facts about the block:
//!
//! * `Err` - the bytes the scan reads are malformed. Never a partial result.
//! * `Ok(None)` - the block is well-formed (as far as the scan reads it) but no
//!   single index can describe it: no groups, no elements (ID scan only), a
//!   group carrying more than one element field, a second `DenseNodes` in one
//!   group, or (ID scan only) different element kinds across groups.
//! * `Ok(Some(_))` - the exact metadata of the whole block.
//!
//! A scan never stops early. An unsupported shape is recorded and the walk
//! continues, so a mixed group followed by truncated bytes is an `Err`, not an
//! `Ok(None)`; and a group that fails to parse is an `Err` for the whole block
//! rather than a group that silently drops out of the result. The previous
//! scanners did the latter, which made an under-claiming index - a `count` or
//! ID range missing a group, a tag key set missing every key after the first
//! bad tag - and an under-claiming index is a wrong answer for every consumer
//! that skips blobs on it (`tags-filter`, the spatial filter, `cat --type`,
//! `renumber`).
//!
//! # Agreement with the decoder
//!
//! Element messages go through the decoder's own parsers
//! ([`WireNode`]/[`WireWay`]/[`WireRelation`]/[`WireDenseNodes`]), so field
//! decoding is identical by construction: `Node.id` is `sint64` (zigzag), a
//! repeated scalar keeps its last occurrence, an absent `id` is 0, and packed
//! fields are accepted in both the packed and the unpacked wire form. Every
//! packed array the scan reads is decoded with fallible varint reads.
//!
//! Both scanners run the same walk, so they agree on what is an `Err`.
//! What the walk validates is the index-relevant structure, not full decoder
//! acceptance. Not checked here, and accepted where the decoder
//! (`validate_groups_strict` in `read/block.rs`) rejects: `Info`/`DenseInfo`
//! contents, way geometry and relation member arrays, string-table UTF-8, and
//! equal `keys`/`vals` counts on Node/Way/Relation. None of those can make an
//! index omit something a successful decode yields - the decoder refuses such
//! a block outright - so the gap can only cost an index over a block nobody
//! can read, never a wrong skip.

use std::collections::HashSet;

use protohoggr::Cursor;

use super::{BlobBbox, BlobIndex, ElemKind, TagIndex};
use crate::error::{ErrorKind, Result, new_error, new_wire_error};
use crate::read::wire::{WIRE_LEN, WIRE_VARINT, WireDenseNodes, WireNode, WireRelation, WireWay};

/// Scan a PrimitiveBlock for its [`BlobIndex`] and whether its elements are
/// internally sorted in canonical OSM ID order.
///
/// The flag is `true` iff every element, in wire order across all groups, is
/// `>=` its predecessor under `osm_id_cmp`. This is what a blob-level
/// permutation sort's passthrough fast path implicitly assumes; `sort` uses it
/// to route an internally unsorted blob into decode + re-encode.
///
/// See the module docs for what `Err` and `Ok(None)` mean.
#[hotpath::measure]
pub(crate) fn try_scan_block_ids_checked(raw: &[u8]) -> Result<Option<(BlobIndex, bool)>> {
    let walk = Walk::run(raw, false)?;
    if walk.unsupported_group || walk.kinds_differ {
        return Ok(None);
    }
    let Some(kind) = walk.kind else {
        return Ok(None);
    };
    let bbox = walk.bbox(kind);
    Ok(Some((
        BlobIndex {
            kind,
            min_id: walk.min_id,
            max_id: walk.max_id,
            count: walk.count,
            bbox,
        },
        walk.sorted,
    )))
}

/// Scan a PrimitiveBlock for the set of unique tag keys its elements carry.
///
/// Empty-string keys are included: the decoder yields them, and a tag
/// expression can require one (`-e ',highway'` keeps the empty alternative),
/// so leaving them out would make the index under-claim. Groups of different
/// element kinds are fine here - each group is scanned in full - so only a
/// group mixing element fields, or a second `DenseNodes` in one group, makes
/// the result `Ok(None)`.
#[hotpath::measure]
pub(crate) fn try_scan_block_tags(raw: &[u8]) -> Result<Option<TagIndex>> {
    let walk = Walk::run(raw, true)?;
    if walk.groups == 0 || walk.unsupported_group {
        return Ok(None);
    }
    let mut keys: Vec<Box<[u8]>> = walk
        .key_sids
        .iter()
        // In range by construction: `Walk::run` checked the largest sid.
        .filter_map(|&sid| usize::try_from(sid).ok())
        .filter_map(|sid| walk.strings.get(sid))
        .map(|&s| s.into())
        .collect();
    keys.sort();
    keys.dedup();
    Ok(Some(TagIndex::from_sorted_keys(keys)))
}

/// Raw coordinate extremes of every node seen, before the block's
/// granularity/offset transform.
#[derive(Clone, Copy)]
struct RawExtent {
    min_lat: i64,
    max_lat: i64,
    min_lon: i64,
    max_lon: i64,
}

/// One pass over a whole block, accumulating everything both scanners need.
struct Walk<'a> {
    collect_keys: bool,
    /// Entries of the last `StringTable` occurrence (the decoder keeps the
    /// last one too).
    strings: Vec<&'a [u8]>,
    granularity: i32,
    lat_offset: i64,
    lon_offset: i64,
    groups: usize,

    kind: Option<ElemKind>,
    kinds_differ: bool,
    unsupported_group: bool,
    count: u64,
    min_id: i64,
    max_id: i64,
    prev_id: Option<i64>,
    sorted: bool,

    /// `None` until the first node with a usable coordinate.
    extent: Option<RawExtent>,
    /// Set when any node's coordinate could not be determined (delta
    /// accumulation overflowed). The blob then gets no bbox at all - a box
    /// over the other nodes would exclude this one.
    extent_unknown: bool,

    key_sids: HashSet<u64>,
    /// Largest key sid referenced anywhere, checked against the string table
    /// in both modes so the two scanners agree on what is an `Err`.
    max_key_sid: Option<u64>,
}

impl<'a> Walk<'a> {
    fn run(raw: &'a [u8], collect_keys: bool) -> Result<Self> {
        let mut walk = Walk {
            collect_keys,
            strings: Vec::new(),
            granularity: 100, // PrimitiveBlock default
            lat_offset: 0,
            lon_offset: 0,
            groups: 0,
            kind: None,
            kinds_differ: false,
            unsupported_group: false,
            count: 0,
            min_id: i64::MAX,
            max_id: i64::MIN,
            prev_id: None,
            sorted: true,
            extent: None,
            extent_unknown: false,
            key_sids: HashSet::new(),
            max_key_sid: None,
        };

        let (stringtable, groups) = walk.block_fields(raw)?;

        let mut st = Cursor::new(stringtable);
        while let Some((field, wire_type)) = st.read_tag()? {
            if field == 1 && wire_type == WIRE_LEN {
                walk.strings.push(st.read_len_delimited()?);
            } else {
                st.skip_field(wire_type)?;
            }
        }

        walk.groups = groups.len();
        for group in groups {
            walk.group(group)?;
        }

        if let Some(sid) = walk.max_key_sid {
            let index = usize::try_from(sid).unwrap_or(usize::MAX);
            if index >= walk.strings.len() {
                return Err(new_error(ErrorKind::StringtableIndexOutOfBounds { index }));
            }
        }
        Ok(walk)
    }

    /// Read the block-level fields, returning the last StringTable and every
    /// group in order. Groups are walked only afterwards because granularity
    /// and the offsets may follow them on the wire.
    fn block_fields(&mut self, raw: &'a [u8]) -> Result<(&'a [u8], Vec<&'a [u8]>)> {
        let mut stringtable: &[u8] = &[];
        let mut groups: Vec<&[u8]> = Vec::new();
        let mut cur = Cursor::new(raw);
        while let Some((field, wire_type)) = cur.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_LEN) => stringtable = cur.read_len_delimited()?,
                (2, WIRE_LEN) => groups.push(cur.read_len_delimited()?),
                (17, WIRE_VARINT) => {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        self.granularity = cur.read_varint()? as i32;
                    }
                }
                (19, WIRE_VARINT) => self.lat_offset = cur.read_varint_i64()?,
                (20, WIRE_VARINT) => self.lon_offset = cur.read_varint_i64()?,
                _ => cur.skip_field(wire_type)?,
            }
        }
        Ok((stringtable, groups))
    }

    /// Walk one `PrimitiveGroup`, every field of it.
    fn group(&mut self, data: &'a [u8]) -> Result<()> {
        // Bit n set = element field n (1 nodes, 2 dense, 3 ways, 4 relations)
        // occurred in this group.
        let mut seen: u8 = 0;
        let mut cur = Cursor::new(data);
        while let Some((field, wire_type)) = cur.read_tag()? {
            match (field, wire_type) {
                (1, WIRE_LEN) => {
                    let node = WireNode::parse(cur.read_len_delimited()?)?;
                    self.packed_keys(node.keys_data, node.vals_data)?;
                    self.coordinate(Some((node.lat, node.lon)));
                    self.element(ElemKind::Node, node.id);
                }
                (2, WIRE_LEN) => {
                    // The decoder reads only the first DenseNodes of a group
                    // (`WireGroup::dense`); protobuf would merge them. No index
                    // can be right for both readings, so describe neither.
                    if seen & (1 << 2) != 0 {
                        self.unsupported_group = true;
                    }
                    let dense = WireDenseNodes::parse(cur.read_len_delimited()?)?;
                    self.dense(&dense)?;
                }
                (3, WIRE_LEN) => {
                    let way = WireWay::parse(cur.read_len_delimited()?)?;
                    self.packed_keys(way.keys_data, way.vals_data)?;
                    self.element(ElemKind::Way, way.id);
                }
                (4, WIRE_LEN) => {
                    let rel = WireRelation::parse(cur.read_len_delimited()?)?;
                    self.packed_keys(rel.keys_data, rel.vals_data)?;
                    self.element(ElemKind::Relation, rel.id);
                }
                _ => {
                    cur.skip_field(wire_type)?;
                    continue;
                }
            }
            seen |= 1 << field;
        }
        // The OSM PBF spec says a group holds exactly one kind; nothing on the
        // wire enforces it. A group mixing element fields cannot be described
        // by one (kind, count) pair - `cat --type way` would pass its
        // relations through raw, `renumber` would leave them unrenumbered -
        // so it is not described at all.
        if seen.count_ones() > 1 {
            self.unsupported_group = true;
        }
        Ok(())
    }

    /// Record one element's kind and ID.
    fn element(&mut self, kind: ElemKind, id: i64) {
        match self.kind {
            None => self.kind = Some(kind),
            // Mixed-kind blobs cannot be indexed: the fast paths (raw
            // passthrough, ID-range skip) trust `kind` as exact.
            Some(k) if k != kind => self.kinds_differ = true,
            Some(_) => {}
        }
        if let Some(prev) = self.prev_id
            && crate::osm_id::osm_id_cmp(id, prev).is_lt()
        {
            self.sorted = false;
        }
        self.prev_id = Some(id);
        self.min_id = self.min_id.min(id);
        self.max_id = self.max_id.max(id);
        self.count += 1;
    }

    /// Fold one node's raw coordinate into the extent; `None` means the
    /// coordinate is unknown.
    fn coordinate(&mut self, raw: Option<(i64, i64)>) {
        let Some((lat, lon)) = raw else {
            self.extent_unknown = true;
            return;
        };
        self.extent = Some(match self.extent {
            None => RawExtent {
                min_lat: lat,
                max_lat: lat,
                min_lon: lon,
                max_lon: lon,
            },
            Some(e) => RawExtent {
                min_lat: e.min_lat.min(lat),
                max_lat: e.max_lat.max(lat),
                min_lon: e.min_lon.min(lon),
                max_lon: e.max_lon.max(lon),
            },
        });
    }

    /// Validate a Node/Way/Relation `keys` and `vals` pair, recording keys.
    ///
    /// Both arrays are decoded with fallible reads. A key above `u32::MAX` is
    /// an error rather than truncated: the decoder's `PackedUint32Iter`
    /// truncates it to a different, possibly valid, sid, so no key set can
    /// be trusted to match what decoding yields. The two counts are not
    /// compared - see the module docs.
    fn packed_keys(&mut self, keys: &[u8], vals: &[u8]) -> Result<()> {
        let mut cur = Cursor::new(keys);
        while !cur.is_empty() {
            let sid = u64::from(cur.read_varint_u32()?);
            self.key(sid);
        }
        let mut cur = Cursor::new(vals);
        while !cur.is_empty() {
            cur.read_varint()?;
        }
        Ok(())
    }

    fn key(&mut self, sid: u64) {
        self.max_key_sid = Some(self.max_key_sid.map_or(sid, |m| m.max(sid)));
        if self.collect_keys {
            self.key_sids.insert(sid);
        }
    }

    /// Walk one `DenseNodes` message: IDs, coordinates, and `keys_vals`.
    ///
    /// The decoder zips `id`, `lat` and `lon` and refuses a group whose three
    /// arrays differ in length, so they must agree here too - otherwise
    /// `count` would claim nodes no decode yields. `keys_vals` must describe
    /// exactly one tag run per node, under the decoder's own rules (see
    /// `validate_dense_keys_vals` in `read/block.rs`).
    fn dense(&mut self, dense: &WireDenseNodes<'a>) -> Result<()> {
        let mut ids = Cursor::new(dense.id_data);
        let mut lats = Cursor::new(dense.lat_data);
        let mut lons = Cursor::new(dense.lon_data);
        let mut id: i64 = 0;
        let mut lat: i64 = 0;
        let mut lon: i64 = 0;
        let mut coords_known = true;
        let mut nodes: u64 = 0;
        while !ids.is_empty() {
            let did = ids.read_sint64()?;
            if lats.is_empty() || lons.is_empty() {
                return Err(dense_length_mismatch());
            }
            let dlat = lats.read_sint64()?;
            let dlon = lons.read_sint64()?;
            id = id
                .checked_add(did)
                .ok_or_else(|| new_wire_error("dense node id delta overflows i64"))?;
            if coords_known {
                match (lat.checked_add(dlat), lon.checked_add(dlon)) {
                    (Some(a), Some(b)) => {
                        lat = a;
                        lon = b;
                    }
                    _ => coords_known = false,
                }
            }
            self.coordinate(coords_known.then_some((lat, lon)));
            self.element(ElemKind::Node, id);
            nodes += 1;
        }
        if !lats.is_empty() || !lons.is_empty() {
            return Err(dense_length_mismatch());
        }
        self.dense_keys_vals(dense.keys_vals_data, nodes)
    }

    /// Walk `keys_vals`: an empty array means no node has tags; otherwise one
    /// zero-terminated run of (key, value) pairs per node, where the last run
    /// may omit its terminator. Zero is a delimiter only in key position - a
    /// value of 0 is a value.
    fn dense_keys_vals(&mut self, keys_vals: &[u8], nodes: u64) -> Result<()> {
        if keys_vals.is_empty() {
            return Ok(());
        }
        let mut kv = Cursor::new(keys_vals);
        let mut runs: u64 = 0;
        let mut in_run = false;
        while !kv.is_empty() {
            let key = kv.read_varint()?;
            if key == 0 {
                runs += 1;
                in_run = false;
                continue;
            }
            if kv.is_empty() {
                return Err(new_wire_error(
                    "dense keys_vals ends with a key that has no value",
                ));
            }
            kv.read_varint()?;
            self.key(key);
            in_run = true;
        }
        if in_run {
            runs += 1;
        }
        if runs != nodes {
            return Err(new_wire_error(
                "dense keys_vals describes a different number of nodes than the id array",
            ));
        }
        Ok(())
    }

    /// The blob's bbox in decimicrodegrees, or `None` when it has none or any
    /// node's coordinate is unknown.
    fn bbox(&self, kind: ElemKind) -> Option<BlobBbox> {
        if kind != ElemKind::Node || self.extent_unknown {
            return None;
        }
        let e = self.extent?;
        let (min_lat, max_lat) =
            transform_range(e.min_lat, e.max_lat, self.lat_offset, self.granularity)?;
        let (min_lon, max_lon) =
            transform_range(e.min_lon, e.max_lon, self.lon_offset, self.granularity)?;
        Some(BlobBbox {
            min_lat,
            max_lat,
            min_lon,
            max_lon,
        })
    }
}

fn dense_length_mismatch() -> crate::error::Error {
    new_wire_error("dense group has mismatched id/lat/lon array lengths")
}

/// Map a raw coordinate range through `offset + granularity * raw`
/// (nanodegrees) to decimicrodegrees, rounding outward.
///
/// Both endpoints are transformed and then ordered, because a negative
/// granularity maps the raw minimum to the coordinate maximum. `granularity`
/// and the offsets are attacker-controllable, so every step is checked; an
/// overflow, or a result outside `i32`, means no bbox rather than a wrapped
/// one that every spatial filter would trust.
fn transform_range(lo: i64, hi: i64, offset: i64, granularity: i32) -> Option<(i32, i32)> {
    let gran = i64::from(granularity);
    let a = offset.checked_add(gran.checked_mul(lo)?)?;
    let b = offset.checked_add(gran.checked_mul(hi)?)?;
    let (min_nano, max_nano) = if a <= b { (a, b) } else { (b, a) };
    Some((
        i32::try_from(floor_div(min_nano, 100)).ok()?,
        i32::try_from(ceil_div(max_nano, 100)).ok()?,
    ))
}

/// Floor division for signed integers (rounds toward negative infinity).
fn floor_div(a: i64, b: i64) -> i64 {
    let d = a / b;
    let r = a % b;
    if (r != 0) && ((r ^ b) < 0) { d - 1 } else { d }
}

/// Ceiling division for signed integers (rounds toward positive infinity).
fn ceil_div(a: i64, b: i64) -> i64 {
    let d = a / b;
    let r = a % b;
    if (r != 0) && ((r ^ b) >= 0) { d + 1 } else { d }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;
    use crate::blob_meta::{BlobFilter, scan_block_ids, scan_block_ids_checked, scan_block_tags};

    // --- Minimal protobuf wire-format builders --------------------------

    fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
    }

    fn zigzag(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    fn write_tag(field: u32, wire: u32, out: &mut Vec<u8>) {
        encode_varint(u64::from((field << 3) | wire), out);
    }

    fn write_len_delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
        write_tag(field, 2, out);
        encode_varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    fn packed_delta_sint64(values: &[i64]) -> Vec<u8> {
        let mut packed = Vec::new();
        let mut prev = 0i64;
        for &v in values {
            encode_varint(zigzag(v - prev), &mut packed);
            prev = v;
        }
        packed
    }

    fn packed_uint(values: &[u64]) -> Vec<u8> {
        let mut packed = Vec::new();
        for &v in values {
            encode_varint(v, &mut packed);
        }
        packed
    }

    /// DenseNodes message with ids and matching (lat, lon) per node, plus an
    /// optional raw `keys_vals` array.
    fn dense_msg(ids: &[i64], coords: &[(i64, i64)], keys_vals: &[u64]) -> Vec<u8> {
        let lats: Vec<i64> = coords.iter().map(|c| c.0).collect();
        let lons: Vec<i64> = coords.iter().map(|c| c.1).collect();
        let mut msg = Vec::new();
        write_len_delimited(1, &packed_delta_sint64(ids), &mut msg);
        write_len_delimited(8, &packed_delta_sint64(&lats), &mut msg);
        write_len_delimited(9, &packed_delta_sint64(&lons), &mut msg);
        if !keys_vals.is_empty() {
            write_len_delimited(10, &packed_uint(keys_vals), &mut msg);
        }
        msg
    }

    /// PrimitiveGroup body wrapping a DenseNodes for `ids`, each node at
    /// (id, id) raw coordinates.
    fn dense_group_body(ids: &[i64]) -> Vec<u8> {
        let coords: Vec<(i64, i64)> = ids.iter().map(|&i| (i, i)).collect();
        let mut group_body = Vec::new();
        write_len_delimited(2, &dense_msg(ids, &coords, &[]), &mut group_body);
        group_body
    }

    /// Way/Relation message: id (plain int64) plus optional packed keys and
    /// vals.
    fn element_msg(id: i64, keys: &[u64], vals: &[u64]) -> Vec<u8> {
        let mut msg = Vec::new();
        write_tag(1, 0, &mut msg);
        encode_varint(id as u64, &mut msg);
        if !keys.is_empty() {
            write_len_delimited(2, &packed_uint(keys), &mut msg);
        }
        if !vals.is_empty() {
            write_len_delimited(3, &packed_uint(vals), &mut msg);
        }
        msg
    }

    /// Non-dense Node message: `Node.id`, `lat` and `lon` are `sint64`.
    fn node_msg(id: i64, lat: i64, lon: i64) -> Vec<u8> {
        let mut msg = Vec::new();
        write_tag(1, 0, &mut msg);
        encode_varint(zigzag(id), &mut msg);
        write_tag(8, 0, &mut msg);
        encode_varint(zigzag(lat), &mut msg);
        write_tag(9, 0, &mut msg);
        encode_varint(zigzag(lon), &mut msg);
        msg
    }

    /// PrimitiveGroup body holding repeated way (3) or relation (4) messages.
    fn repeated_group_body(field: u32, ids: &[i64]) -> Vec<u8> {
        let mut group_body = Vec::new();
        for &id in ids {
            write_len_delimited(field, &element_msg(id, &[], &[]), &mut group_body);
        }
        group_body
    }

    fn node_group_body(ids: &[i64]) -> Vec<u8> {
        let mut group_body = Vec::new();
        for &id in ids {
            write_len_delimited(1, &node_msg(id, 0, 0), &mut group_body);
        }
        group_body
    }

    fn stringtable(strings: &[&[u8]]) -> Vec<u8> {
        let mut st = Vec::new();
        for s in strings {
            write_len_delimited(1, s, &mut st);
        }
        st
    }

    /// PrimitiveBlock wrapping the given PrimitiveGroup bodies in order.
    fn block_from_groups(groups: &[&[u8]]) -> Vec<u8> {
        let mut block = Vec::new();
        for g in groups {
            write_len_delimited(2, g, &mut block);
        }
        block
    }

    fn block_with_strings(strings: &[&[u8]], groups: &[&[u8]]) -> Vec<u8> {
        let mut block = Vec::new();
        write_len_delimited(1, &stringtable(strings), &mut block);
        block.extend_from_slice(&block_from_groups(groups));
        block
    }

    // --- DenseNodes: order checks ---------------------------------------

    #[test]
    fn dense_two_groups_in_order_stay_sorted() {
        let a = dense_group_body(&[1, 2]);
        let b = dense_group_body(&[3, 4]);
        let block = block_from_groups(&[&a, &b]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(sorted, "two ascending same-kind groups must be sorted");
        assert_eq!(index.kind, ElemKind::Node);
        assert_eq!(index.min_id, 1);
        assert_eq!(index.max_id, 4);
        assert_eq!(index.count, 4);
    }

    #[test]
    fn dense_cross_group_inversion_detected() {
        // Each group is internally monotone, but group B's first id (15) sorts
        // before group A's last id (20): the cross-group seam check must fire.
        let a = dense_group_body(&[10, 20]);
        let b = dense_group_body(&[15, 25]);
        let block = block_from_groups(&[&a, &b]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(
            !sorted,
            "an inversion across two same-kind groups must be detected"
        );
        assert_eq!(index.min_id, 10);
        assert_eq!(index.max_id, 25);
        assert_eq!(index.count, 4);
    }

    #[test]
    fn dense_within_group_inversion_detected() {
        let g = dense_group_body(&[1, 3, 2, 4]);
        let block = block_from_groups(&[&g]);
        let (_, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(
            !sorted,
            "a within-group DenseNodes inversion must be detected"
        );
    }

    #[test]
    fn dense_canonical_negative_order_not_flagged() {
        // Canonical OSM order for negatives is ascending absolute value.
        let g = dense_group_body(&[-1, -2, -3]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(sorted, "canonical -1,-2,-3 order must not be flagged");
        assert_eq!(index.min_id, -3);
        assert_eq!(index.max_id, -1);
        assert_eq!(index.count, 3);
    }

    #[test]
    fn dense_negative_inversion_detected() {
        let g = dense_group_body(&[-2, -1, -3]);
        let block = block_from_groups(&[&g]);
        let (_, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(!sorted, "a negative-ID inversion must be detected");
    }

    #[test]
    fn dense_cross_group_negative_seam_detected() {
        let a = dense_group_body(&[-2, -3]);
        let b = dense_group_body(&[-1, -4]);
        let block = block_from_groups(&[&a, &b]);
        let (_, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(
            !sorted,
            "a negative cross-group seam inversion must be detected"
        );
    }

    // --- Repeated Way/Relation/non-dense-Node message path --------------

    #[test]
    fn way_repeated_in_order_sorted() {
        let g = repeated_group_body(3, &[1, 2, 3]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(sorted);
        assert_eq!(index.kind, ElemKind::Way);
        assert_eq!((index.min_id, index.max_id, index.count), (1, 3, 3));
        assert!(index.bbox.is_none(), "ways carry no bbox");
    }

    #[test]
    fn way_repeated_inversion_detected() {
        let g = repeated_group_body(3, &[1, 3, 2]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(!sorted);
        assert_eq!(index.kind, ElemKind::Way);
    }

    #[test]
    fn relation_repeated_inversion_detected() {
        let g = repeated_group_body(4, &[5, 4]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(!sorted);
        assert_eq!(index.kind, ElemKind::Relation);
    }

    #[test]
    fn non_dense_node_repeated_inversion_detected() {
        let g = node_group_body(&[2, 1]);
        let block = block_from_groups(&[&g]);
        let (index, sorted) = scan_block_ids_checked(&block).unwrap();
        assert!(!sorted);
        assert_eq!(index.kind, ElemKind::Node);
    }

    /// `Node.id` is `sint64`, so its wire value is zigzag-encoded. The old
    /// scanner read it as a plain varint and indexed node 3 as 6 and node -1
    /// as 1 - a wrong ID range written into every `cat`-reframed non-dense
    /// node blob, and trusted by sort/dedupe overlap detection.
    #[test]
    fn non_dense_node_ids_are_zigzag_decoded() {
        let g = node_group_body(&[3, 7]);
        let block = block_from_groups(&[&g]);
        let index = scan_block_ids(&block).unwrap();
        assert_eq!((index.min_id, index.max_id, index.count), (3, 7, 2));

        let g = node_group_body(&[-1, -5]);
        let block = block_from_groups(&[&g]);
        let index = scan_block_ids(&block).unwrap();
        assert_eq!((index.min_id, index.max_id), (-5, -1));
    }

    /// Non-dense nodes now get a bbox, and it is derived exactly like the
    /// dense one.
    #[test]
    fn non_dense_nodes_get_a_bbox() {
        let mut g = Vec::new();
        write_len_delimited(1, &node_msg(1, 10, 20), &mut g);
        write_len_delimited(1, &node_msg(2, 30, -40), &mut g);
        let block = block_from_groups(&[&g]);
        let bbox = scan_block_ids(&block).unwrap().bbox.unwrap();
        // granularity 100: decimicro = raw.
        assert_eq!(
            (bbox.min_lat, bbox.max_lat, bbox.min_lon, bbox.max_lon),
            (10, 30, -40, 20)
        );
    }

    // --- Unpacked wire forms --------------------------------------------

    #[test]
    fn unpacked_dense_ids_are_indexed_not_read_as_absent() {
        // DenseNodes with every array in the UNPACKED form (individually
        // tagged varints). A single unpacked value is byte-identical to a
        // one-element packed segment.
        let mut dense = Vec::new();
        for field in [1, 8, 9] {
            write_tag(field, 0, &mut dense);
            encode_varint(zigzag(7), &mut dense);
        }
        let mut g = Vec::new();
        write_len_delimited(2, &dense, &mut g);
        let block = block_from_groups(&[&g]);
        let (index, _) = scan_block_ids_checked(&block)
            .expect("an unpacked DenseNodes.id array must still produce a BlobIndex");
        assert_eq!(
            (index.kind, index.count, index.min_id),
            (ElemKind::Node, 1, 7)
        );

        let packed = block_from_groups(&[&dense_group_body(&[7])]);
        let (packed_index, _) = scan_block_ids_checked(&packed).unwrap();
        assert_eq!(packed_index.count, index.count);
        assert_eq!(packed_index.min_id, index.min_id);
    }

    /// Unpacked `Way.keys` used to be skipped by the tag scanner, which then
    /// emitted an index without that key.
    #[test]
    fn unpacked_way_keys_are_in_the_tag_index() {
        let mut way = Vec::new();
        write_tag(1, 0, &mut way);
        encode_varint(1, &mut way);
        write_tag(2, 0, &mut way); // keys, unpacked
        encode_varint(1, &mut way);
        write_tag(3, 0, &mut way); // vals, unpacked
        encode_varint(2, &mut way);
        let mut g = Vec::new();
        write_len_delimited(3, &way, &mut g);
        let block = block_with_strings(&[b"", b"highway", b"primary"], &[&g]);
        let ti = scan_block_tags(&block).unwrap();
        assert!(ti.has_any_key(&[b"highway"[..].into()]));
    }

    // --- Mixed kinds and unsupported shapes -----------------------------

    fn mixed_way_relation_group_body(way_id: i64, rel_id: i64) -> Vec<u8> {
        let mut group_body = Vec::new();
        write_len_delimited(3, &element_msg(way_id, &[], &[]), &mut group_body);
        write_len_delimited(4, &element_msg(rel_id, &[], &[]), &mut group_body);
        group_body
    }

    #[test]
    fn a_group_mixing_ways_and_relations_is_not_indexed() {
        let g = mixed_way_relation_group_body(1, 2);
        let block = block_from_groups(&[&g]);
        assert!(
            matches!(try_scan_block_ids_checked(&block), Ok(None)),
            "a group holding both a way and a relation cannot be described by a \
             single (kind, count) pair, so it must yield no indexdata at all"
        );
    }

    #[test]
    fn a_mixed_group_poisons_the_whole_blob_not_just_itself() {
        let clean = repeated_group_body(3, &[10, 11]);
        let mixed = mixed_way_relation_group_body(12, 2);
        let block = block_from_groups(&[&clean, &mixed]);
        assert!(scan_block_ids_checked(&block).is_none());
    }

    #[test]
    fn single_kind_groups_still_index() {
        let a = repeated_group_body(3, &[1, 2]);
        let b = repeated_group_body(3, &[3, 4]);
        let block = block_from_groups(&[&a, &b]);
        let (index, sorted) = scan_block_ids_checked(&block)
            .expect("two single-kind way groups must still be indexable");
        assert!(sorted);
        assert_eq!((index.kind, index.count), (ElemKind::Way, 4));
    }

    #[test]
    fn different_kinds_across_groups_block_ids_but_not_tags() {
        let ways = repeated_group_body(3, &[1]);
        let rels = repeated_group_body(4, &[1]);
        let block = block_with_strings(&[b""], &[&ways, &rels]);
        assert!(matches!(try_scan_block_ids_checked(&block), Ok(None)));
        // Each group is scanned in full for tags, so the key set is complete.
        assert!(matches!(try_scan_block_tags(&block), Ok(Some(_))));
    }

    #[test]
    fn a_second_dense_in_one_group_is_not_described() {
        let mut g = Vec::new();
        write_len_delimited(2, &dense_msg(&[1], &[(0, 0)], &[]), &mut g);
        write_len_delimited(2, &dense_msg(&[2], &[(0, 0)], &[]), &mut g);
        let block = block_with_strings(&[b""], &[&g]);
        assert!(matches!(try_scan_block_ids_checked(&block), Ok(None)));
        assert!(matches!(try_scan_block_tags(&block), Ok(None)));
    }

    /// An unsupported shape must not end the walk: what follows it still has
    /// to parse, or the answer is `Err`, not `Ok(None)`.
    #[test]
    fn an_unsupported_shape_does_not_hide_later_malformed_bytes() {
        let mixed = mixed_way_relation_group_body(1, 2);
        let mut block = block_from_groups(&[&mixed]);
        assert!(matches!(try_scan_block_ids_checked(&block), Ok(None)));
        block.extend_from_slice(&[0x12, 0x05, 0x1A]); // group claims 5 bytes, has 1
        assert!(try_scan_block_ids_checked(&block).is_err());
        assert!(try_scan_block_tags(&block).is_err());
    }

    // --- BUG-036: a parse failure is an error, never a short result ------

    /// A group that fails to parse used to vanish, and the blob was indexed
    /// from the remaining groups with an understated `count` and range.
    #[test]
    fn a_malformed_group_is_an_error_not_a_dropped_group() {
        let good = repeated_group_body(3, &[1, 2]);
        // A way message whose id varint is truncated (continuation bit set,
        // nothing after it).
        let mut bad = Vec::new();
        write_len_delimited(3, &[0x08, 0x80], &mut bad);
        let block = block_from_groups(&[&good, &bad]);
        assert!(try_scan_block_ids_checked(&block).is_err());
        assert!(
            scan_block_ids(&block).is_none(),
            "the wrapper must not index a prefix"
        );

        // Passing counterpart: the good group alone indexes.
        let block = block_from_groups(&[&good]);
        assert_eq!(scan_block_ids(&block).unwrap().count, 2);
    }

    /// The tag scanner used to stop at the first bad tag inside a group and
    /// return the keys it had seen so far - an index that under-claims, which
    /// makes `tags-filter` skip blobs that do carry the key.
    #[test]
    fn a_malformed_key_array_is_an_error_not_a_short_index() {
        let w1 = element_msg(1, &[1], &[2]);
        // keys array whose second varint is truncated.
        let mut w2 = Vec::new();
        write_tag(1, 0, &mut w2);
        encode_varint(2, &mut w2);
        write_len_delimited(2, &[0x03, 0x80], &mut w2);
        let mut g = Vec::new();
        write_len_delimited(3, &w1, &mut g);
        write_len_delimited(3, &w2, &mut g);
        let block = block_with_strings(&[b"", b"highway", b"primary", b"amenity"], &[&g]);
        assert!(try_scan_block_tags(&block).is_err());
        assert!(scan_block_tags(&block).is_none());
        assert!(
            try_scan_block_ids_checked(&block).is_err(),
            "both scanners agree"
        );
    }

    #[test]
    fn a_malformed_dense_keys_vals_is_an_error() {
        // One node, keys_vals [1] - a key with no value.
        let mut g = Vec::new();
        write_len_delimited(2, &dense_msg(&[1], &[(0, 0)], &[1]), &mut g);
        let block = block_with_strings(&[b"", b"highway"], &[&g]);
        assert!(try_scan_block_tags(&block).is_err());

        // Two nodes but only one tag run.
        let mut g = Vec::new();
        write_len_delimited(
            2,
            &dense_msg(&[1, 2], &[(0, 0), (0, 0)], &[1, 1, 0]),
            &mut g,
        );
        let block = block_with_strings(&[b"", b"highway"], &[&g]);
        assert!(try_scan_block_tags(&block).is_err());

        // Passing counterparts: one run per node, with and without the final
        // terminator, and a value of 0 is a value, not a delimiter.
        for kv in [&[1u64, 1, 0, 1, 0][..], &[1, 1, 0, 1, 0, 0][..]] {
            let mut g = Vec::new();
            write_len_delimited(2, &dense_msg(&[1, 2], &[(0, 0), (0, 0)], kv), &mut g);
            let block = block_with_strings(&[b"", b"highway"], &[&g]);
            let ti = try_scan_block_tags(&block).unwrap().unwrap();
            assert!(ti.has_any_key(&[b"highway"[..].into()]), "{kv:?}");
        }
    }

    #[test]
    fn dense_arrays_of_different_lengths_are_an_error() {
        let mut dense = Vec::new();
        write_len_delimited(1, &packed_delta_sint64(&[1, 2]), &mut dense);
        write_len_delimited(8, &packed_delta_sint64(&[0]), &mut dense);
        write_len_delimited(9, &packed_delta_sint64(&[0, 0]), &mut dense);
        let mut g = Vec::new();
        write_len_delimited(2, &dense, &mut g);
        let block = block_from_groups(&[&g]);
        assert!(try_scan_block_ids_checked(&block).is_err());
    }

    #[test]
    fn a_key_outside_the_string_table_is_an_error() {
        let mut g = Vec::new();
        write_len_delimited(3, &element_msg(1, &[5], &[1]), &mut g);
        let block = block_with_strings(&[b"", b"highway"], &[&g]);
        assert!(try_scan_block_tags(&block).is_err());
        assert!(
            try_scan_block_ids_checked(&block).is_err(),
            "both scanners agree"
        );
    }

    #[test]
    fn the_last_string_table_wins() {
        let mut g = Vec::new();
        write_len_delimited(3, &element_msg(1, &[1], &[1]), &mut g);
        let mut block = Vec::new();
        write_len_delimited(1, &stringtable(&[b"", b"stale"]), &mut block);
        write_len_delimited(2, &g, &mut block);
        write_len_delimited(1, &stringtable(&[b"", b"amenity"]), &mut block);
        let ti = scan_block_tags(&block).unwrap();
        assert_eq!(ti.keys(), &[Box::<[u8]>::from(&b"amenity"[..])]);
    }

    /// The decoder yields empty-string keys, and `-e ',highway'` requires
    /// one; an index without it makes the filter skip a matching blob.
    #[test]
    fn empty_string_keys_are_in_the_tag_index() {
        let mut g = Vec::new();
        write_len_delimited(3, &element_msg(1, &[1], &[2]), &mut g);
        let block = block_with_strings(&[b"", b"", b"v"], &[&g]);
        let ti = scan_block_tags(&block).unwrap();
        let filter = BlobFilter::new(true, true, true)
            .with_required_tag_keys(vec![String::new(), "highway".to_string()]);
        assert!(filter.wants_tag_index(&ti));
    }

    // --- bbox -----------------------------------------------------------

    /// A node blob whose second group's coordinates cannot be determined must
    /// not carry the first group's box alone: a query near the second group
    /// would skip the blob.
    #[test]
    fn an_unknown_coordinate_drops_the_whole_bbox() {
        let a = dense_group_body(&[1]);
        let mut dense = Vec::new();
        write_len_delimited(1, &packed_delta_sint64(&[2, 3]), &mut dense);
        // lat deltas: i64::MAX then 1 - the second accumulation overflows.
        let mut lats = Vec::new();
        encode_varint(zigzag(i64::MAX), &mut lats);
        encode_varint(zigzag(1), &mut lats);
        write_len_delimited(8, &lats, &mut dense);
        write_len_delimited(9, &packed_delta_sint64(&[0, 0]), &mut dense);
        let mut b = Vec::new();
        write_len_delimited(2, &dense, &mut b);
        let block = block_from_groups(&[&a, &b]);
        let index = scan_block_ids(&block).unwrap();
        assert_eq!(index.count, 3);
        assert!(index.bbox.is_none());

        // Counterpart: two ordinary groups union their boxes.
        let block = block_from_groups(&[&dense_group_body(&[1]), &dense_group_body(&[50])]);
        let bbox = scan_block_ids(&block).unwrap().bbox.unwrap();
        assert_eq!((bbox.min_lat, bbox.max_lat), (1, 50));
    }

    #[test]
    fn negative_granularity_orders_the_bbox() {
        let g = dense_group_body(&[10, 20]); // raw coordinates 10 and 20
        let mut block = block_from_groups(&[&g]);
        write_tag(17, 0, &mut block);
        encode_varint(-100i64 as u64, &mut block);
        let bbox = scan_block_ids(&block).unwrap().bbox.unwrap();
        assert_eq!((bbox.min_lat, bbox.max_lat), (-20, -10));
        assert!(bbox.min_lon <= bbox.max_lon);
    }

    #[test]
    fn floor_ceil_div_correctness() {
        assert_eq!(floor_div(7, 2), 3);
        assert_eq!(floor_div(-7, 2), -4);
        assert_eq!(floor_div(6, 2), 3);
        assert_eq!(floor_div(-6, 2), -3);
        assert_eq!(ceil_div(7, 2), 4);
        assert_eq!(ceil_div(-7, 2), -3);
        assert_eq!(ceil_div(6, 2), 3);
        assert_eq!(ceil_div(-6, 2), -3);
        assert_eq!(floor_div(510_000_050, 100), 5_100_000);
        assert_eq!(ceil_div(510_000_050, 100), 5_100_001);
        assert_eq!(floor_div(-510_000_050, 100), -5_100_001);
        assert_eq!(ceil_div(-510_000_050, 100), -5_100_000);
    }
}
